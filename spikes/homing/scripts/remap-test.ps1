<#
.SYNOPSIS
    #71/ADR-0020's VHDX-only drive-remapping proof: creates a small NTFS-formatted VHDX, attaches
    it, and runs `homing` through a letter change, a detach/reattach, a folder-mount-point remount,
    a duplicated-VHDX ambiguity case, and a reformat -- recording `homing resolve`'s output after
    each step into ADR-0020's Measured-results survival table.

.DESCRIPTION
    Per this session's scope decision: VHDX only, no physical-drive step. Requires an elevated
    (Administrator) PowerShell 7+ session -- diskpart's `create vdisk`/`attach vdisk`/`assign
    letter` and `Format-Volume` all need admin rights. NOT YET RUN -- this script was written in a
    Linux/WSL sandbox with no Windows box to test it against (see docs/research/
    homing-volume-identity.md's Sandbox constraint section). Expect to need to debug/adjust this
    on the real reference machine; treat every diskpart/PowerShell cmdlet call here as unverified
    until it's actually been run once.

.PARAMETER SampleDir
    A folder of real NEF files to copy into the VHDX for `homing build`/`resolve` to index. Small
    is fine -- this is testing remap survival, not throughput.

.PARAMETER WorkDir
    Scratch directory for the VHDX file, the mount-point folder, and homing.sqlite3. Defaults to
    a fresh temp folder.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$SampleDir,

    [string]$WorkDir = (Join-Path $env:TEMP "homing-remap-test-$(Get-Date -Format 'yyyyMMdd-HHmmss')")
)

$ErrorActionPreference = "Stop"

if (-not ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw "This script needs an elevated (Administrator) PowerShell session -- diskpart and Format-Volume both require it."
}

New-Item -ItemType Directory -Path $WorkDir -Force | Out-Null
$vhdxPath = Join-Path $WorkDir "homing-test.vhdx"
$dbPath = Join-Path $WorkDir "homing.sqlite3"
$homingExe = Join-Path $PSScriptRoot "..\..\..\target\release\homing.exe"

if (-not (Test-Path $homingExe)) {
    throw "homing.exe not found at $homingExe -- run 'cargo build --release -p homing' first."
}

function Invoke-Homing {
    param([string[]]$HomingArgs)
    Write-Host ">> homing $($HomingArgs -join ' ')" -ForegroundColor Cyan
    & $homingExe @HomingArgs
    if ($LASTEXITCODE -ne 0) {
        throw "homing $($HomingArgs -join ' ') failed with exit code $LASTEXITCODE"
    }
}

function New-TestVhdx {
    param([string]$Path, [int]$SizeMB = 512)
    $diskpartScript = @"
create vdisk file="$Path" maximum=$SizeMB type=expandable
select vdisk file="$Path"
attach vdisk
create partition primary
format fs=ntfs quick label="HomingTest"
assign
"@
    $scriptFile = [System.IO.Path]::GetTempFileName()
    Set-Content -Path $scriptFile -Value $diskpartScript
    diskpart /s $scriptFile
    if ($LASTEXITCODE -ne 0) {
        throw "diskpart failed with exit code $LASTEXITCODE"
    }
    Remove-Item $scriptFile
}

function Get-VhdxDriveLetter {
    param([string]$Path)
    # After `attach vdisk`, the newly assigned letter is the highest-numbered disk's volume --
    # Get-Disk/Get-Partition via the VHD's device path is more precise than "highest letter", but
    # needs the Hyper-V/Storage module's Get-VHD, not diskpart. UNVERIFIED on real hardware --
    # confirm this actually finds the right letter before trusting it for the later steps.
    $vhd = Get-VHD -Path $Path -ErrorAction SilentlyContinue
    if ($vhd -and $vhd.DiskNumber -ne $null) {
        $partition = Get-Partition -DiskNumber $vhd.DiskNumber | Where-Object { $_.DriveLetter }
        return "$($partition.DriveLetter):\"
    }
    throw "Could not determine the VHDX's drive letter -- inspect 'Get-VHD -Path $Path' and 'Get-Partition' manually."
}

Write-Host "=== Step 1: create + attach + format VHDX, assign a letter ===" -ForegroundColor Yellow
New-TestVhdx -Path $vhdxPath
$letter1 = Get-VhdxDriveLetter -Path $vhdxPath
Write-Host "VHDX mounted at $letter1"

Write-Host "=== Step 2: copy sample NEFs ===" -ForegroundColor Yellow
Copy-Item -Path (Join-Path $SampleDir "*") -Destination $letter1 -Recurse

Write-Host "=== Step 3: homing build ===" -ForegroundColor Yellow
Invoke-Homing @("build", $letter1, "--db", $dbPath, "--tier", "partial")

Write-Host "=== Step 4: reassign the drive letter, then resolve ===" -ForegroundColor Yellow
$vhd = Get-VHD -Path $vhdxPath
$partition = Get-Partition -DiskNumber $vhd.DiskNumber | Where-Object { $_.DriveLetter }
$newLetter = "Y"  # UNVERIFIED: confirm Y: is free on the reference machine before running, or parameterize this.
Set-Partition -DiskNumber $vhd.DiskNumber -PartitionNumber $partition.PartitionNumber -NewDriveLetter $newLetter
Write-Host "Reassigned to ${newLetter}: -- expect 100% resolved:"
Invoke-Homing @("resolve", "--db", $dbPath)

Write-Host "=== Step 5: detach + reattach, then resolve ===" -ForegroundColor Yellow
Dismount-VHD -Path $vhdxPath
Start-Sleep -Seconds 2
Mount-VHD -Path $vhdxPath
Write-Host "Reattached -- expect 100% resolved:"
Invoke-Homing @("resolve", "--db", $dbPath)

Write-Host "=== Step 6: remount at a folder mount point instead of a letter, then resolve ===" -ForegroundColor Yellow
Dismount-VHD -Path $vhdxPath
$mountFolder = Join-Path $WorkDir "mounted-volume"
New-Item -ItemType Directory -Path $mountFolder -Force | Out-Null
Mount-VHD -Path $vhdxPath
$vhd = Get-VHD -Path $vhdxPath
$partition = Get-Partition -DiskNumber $vhd.DiskNumber | Where-Object { $_.DriveLetter -or $_.AccessPaths }
Add-PartitionAccessPath -DiskNumber $vhd.DiskNumber -PartitionNumber $partition.PartitionNumber -AccessPath $mountFolder
if ($partition.DriveLetter) {
    # Otherwise `homing resolve` can pass via the volume's existing drive-letter mount point --
    # `enumerate`/`resolve` use the first mount point returned for the volume, not specifically
    # the folder path, so leaving the letter attached would let this step pass without actually
    # proving folder-only resolution.
    Remove-PartitionAccessPath -DiskNumber $vhd.DiskNumber -PartitionNumber $partition.PartitionNumber -AccessPath "$($partition.DriveLetter):\"
}
Write-Host "Mounted at folder path $mountFolder -- expect 100% resolved:"
Invoke-Homing @("resolve", "--db", $dbPath)

Write-Host "=== Step 7: clone the VHDX, attach both, check for an ambiguity flag ===" -ForegroundColor Yellow
$vhdxClone = Join-Path $WorkDir "homing-test-clone.vhdx"
Dismount-VHD -Path $vhdxPath
Copy-Item -Path $vhdxPath -Destination $vhdxClone
Mount-VHD -Path $vhdxPath
Mount-VHD -Path $vhdxClone
Write-Host "Both original and clone attached -- run 'homing enumerate' and manually confirm the ambiguity is flagged, not silently resolved:"
Invoke-Homing @("enumerate")
Write-Host "RECORD: did both copies report the same identity_key? Did homing resolve pick one silently, or flag it? (No automatic assertion here -- ADR-0020's ambiguity-guard follow-up isn't implemented yet, this step is meant to surface exactly that gap.)"
Dismount-VHD -Path $vhdxClone
Remove-Item $vhdxClone

# Step 6 removed the drive-letter access path (folder-only mount), and Step 8 below filters
# partitions by `DriveLetter` -- restore one if Step 6's removal left the volume without one.
$vhd = Get-VHD -Path $vhdxPath
$partition = Get-Partition -DiskNumber $vhd.DiskNumber | Where-Object { $_.AccessPaths }
if (-not $partition.DriveLetter) {
    Add-PartitionAccessPath -DiskNumber $vhd.DiskNumber -PartitionNumber $partition.PartitionNumber -AssignDriveLetter
}

Write-Host "=== Step 8: reformat, then check whether the old rows resolve or need relink ===" -ForegroundColor Yellow
$vhd = Get-VHD -Path $vhdxPath
$partition = Get-Partition -DiskNumber $vhd.DiskNumber | Where-Object { $_.DriveLetter }
Format-Volume -DriveLetter $partition.DriveLetter -FileSystem NTFS -NewFileSystemLabel "HomingTest2" -Confirm:$false
Write-Host "Reformatted -- expect a NEW identity (NTFS serial regenerates on reformat), old rows unresolved:"
Invoke-Homing @("resolve", "--db", $dbPath)
Copy-Item -Path (Join-Path $SampleDir "*") -Destination "$($partition.DriveLetter):\" -Recurse
Write-Host "Re-copied sample files onto the reformatted volume -- attempting relink:"
Invoke-Homing @("relink", "$($partition.DriveLetter):\", "--db", $dbPath)

Write-Host "=== Cleanup ===" -ForegroundColor Yellow
Dismount-VHD -Path $vhdxPath -ErrorAction SilentlyContinue
Write-Host "Done. Transcribe this run's output into docs/adr/0020-volume-identity-and-remapping.md's Measured-results tables and docs/research/homing-volume-identity.md."
