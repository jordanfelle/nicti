<#
.SYNOPSIS
Stages the 50 hero-scenario files (docs/benchmarks/hero-set.txt) into their own folder via NTFS
hardlinks, so setup.ahk can do a plain whole-folder import instead of scripting a 50-item
multi-select in the Windows file picker.

Hardlinks (not copies) so this doesn't duplicate multi-GB Z8 NEFs on disk -- both names point at
the same data, and both must stay on the same NTFS volume as the frozen ref-10k source.

.PARAMETER RefRoot
Path to the frozen ref-10k copy to stage from (see NICTI_REF10K / docs/benchmarks.md).

.PARAMETER HeroSetFile
Path to docs/benchmarks/hero-set.txt (one file id per line).

.PARAMETER DestDir
Folder to create the hardlinks in.
#>
param(
    [Parameter(Mandatory)] [string]$RefRoot,
    [Parameter(Mandatory)] [string]$HeroSetFile,
    [Parameter(Mandatory)] [string]$DestDir
)

$ErrorActionPreference = "Stop"

if (-not (Test-Path $RefRoot)) { throw "RefRoot not found: $RefRoot" }
if (-not (Test-Path $HeroSetFile)) { throw "HeroSetFile not found: $HeroSetFile" }

New-Item -ItemType Directory -Force -Path $DestDir | Out-Null

$ids = Get-Content $HeroSetFile | Where-Object { $_.Trim() -ne "" }
if ($ids.Count -ne 50) {
    Write-Warning "Expected 50 ids in $HeroSetFile, found $($ids.Count) -- continuing anyway."
}

$missing = @()
foreach ($id in $ids) {
    $src = Join-Path $RefRoot $id
    $dst = Join-Path $DestDir $id
    if (-not (Test-Path $src)) {
        $missing += $id
        continue
    }
    if (Test-Path $dst) {
        Remove-Item $dst -Force
    }
    New-Item -ItemType HardLink -Path $dst -Target $src | Out-Null
}

if ($missing.Count -gt 0) {
    throw "Missing $($missing.Count) source file(s) under $RefRoot`: $($missing -join ', ')"
}

Write-Output "Staged $($ids.Count) hardlinks into $DestDir"
