<#
.SYNOPSIS
Runs one measured pass of the #43 hero scenario: verifies the hero-set files against the
manifest, records hardware identity, captures the screen while bench/lrc/hero.ahk drives LRC, then
crops and hands the capture to whisker for analysis. See docs/benchmarks/hero-scenario.md.

.PARAMETER Interaction
switch | crop | zoom -- must match bench/lrc/hero-config.ini's [general] Interaction.

.PARAMETER Config
originals | smart-previews -- which LRC configuration this run is against (naming only; the
correct catalog must already be open in LRC before this script runs).

.PARAMETER RunLabel
Free-form label for this run's output folder, e.g. "warmup" or "run-1".

.PARAMETER CaptureFps
Capture rate in fps. Defaults to 60 per the documented capture-rate deviation in
hero-scenario.md -- pass 120 once run on a machine with a 120Hz+ display attached.

.PARAMETER IndicatorRect / RoiRect
"x,y,w,h" screen rects for the keypress indicator and the loupe ROI, matching hero-config.ini's
[indicator] section and the calibration step in bench/lrc/README.md.

.PARAMETER DurationSeconds
Safety timeout, not the target capture length (actual capture length is however long hero.ahk
takes to run, plus fixed buffers) -- if hero.ahk hasn't exited within this many seconds (a blocking
MsgBox dialog, LRC not found, a hung drag), the run is aborted rather than left to hang forever.
Must comfortably exceed hero.ahk's expected run time for the chosen interaction (see
bench/lrc/hero-config.ini.example's delay/duration settings).
#>
param(
    [Parameter(Mandatory)] [ValidateSet("switch", "crop", "zoom")] [string]$Interaction,
    [Parameter(Mandatory)] [ValidateSet("originals", "smart-previews")] [string]$Config,
    [Parameter(Mandatory)] [string]$RunLabel,
    [double]$CaptureFps = 60,
    [Parameter(Mandatory)] [string]$IndicatorRect,
    [Parameter(Mandatory)] [string]$RoiRect,
    [int]$DurationSeconds = 60,
    [string]$RefRoot = "H:\NictiBench\ref-10k",
    [string]$ManifestPath = "$PSScriptRoot\..\docs\ref-10k-manifest.csv",
    [string]$HeroSetFile = "$PSScriptRoot\..\docs\benchmarks\hero-set.txt",
    [string]$AhkConfigPath = "$PSScriptRoot\lrc\hero-config.ini",
    [string]$AhkExe = "",
    [string]$FfmpegExe = "",
    [string]$FfprobeExe = "",
    [string]$ResultsRoot = "$PSScriptRoot\..\bench-results\hero"
)

$ErrorActionPreference = "Stop"

function Get-Rect([string]$spec) {
    $parts = $spec -split "," | ForEach-Object { [int]$_.Trim() }
    if ($parts.Count -ne 4) { throw "Rect must be 'x,y,w,h', got: $spec" }
    return @{ X = $parts[0]; Y = $parts[1]; W = $parts[2]; H = $parts[3] }
}

# winget-installed tools don't reliably land on PATH for every process that launches this script
# (WSL interop in particular can inherit a stale PATH) -- fall back to a WinGet Packages search
# before giving up, rather than failing on a bare "ffmpeg"/"AutoHotkey64.exe" that Start-Process
# can't resolve.
function Resolve-Tool([string]$explicit, [string]$commandName, [string]$searchPattern) {
    if ($explicit) {
        if (-not (Test-Path $explicit)) { throw "$commandName override not found: $explicit" }
        return $explicit
    }
    $onPath = Get-Command $commandName -ErrorAction SilentlyContinue
    if ($onPath) { return $onPath.Source }
    $wingetRoot = Join-Path $env:LOCALAPPDATA "Microsoft\WinGet\Packages"
    if (Test-Path $wingetRoot) {
        $found = Get-ChildItem $wingetRoot -Recurse -Filter $searchPattern -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($found) { return $found.FullName }
    }
    throw "$commandName not found on PATH or under $wingetRoot -- pass -AhkExe/-FfmpegExe explicitly."
}

$AhkExe = Resolve-Tool $AhkExe "AutoHotkey64.exe" "AutoHotkey64.exe"
$FfmpegExe = Resolve-Tool $FfmpegExe "ffmpeg" "ffmpeg.exe"
$FfprobeExe = Resolve-Tool $FfprobeExe "ffprobe" "ffprobe.exe"
Write-Host "Using AutoHotkey: $AhkExe"
Write-Host "Using ffmpeg: $FfmpegExe"
Write-Host "Using ffprobe: $FfprobeExe"

# --- 1. Integrity check: hero-set files must match the committed manifest's SHA-256 column,
#        per docs/benchmarks.md's rule that any harness verify the frozen copy before trusting it.
Write-Host "Verifying hero-set file integrity against $ManifestPath ..."
$manifest = Import-Csv $ManifestPath
$manifestById = @{}
foreach ($row in $manifest) { $manifestById[$row.id] = $row.sha256 }

$heroIds = Get-Content $HeroSetFile | Where-Object { $_.Trim() -ne "" }
$badIds = @()
foreach ($id in $heroIds) {
    $path = Join-Path $RefRoot $id
    if (-not (Test-Path $path)) { $badIds += "$id (missing at $path)"; continue }
    $expected = $manifestById[$id]
    if (-not $expected) { $badIds += "$id (not in manifest)"; continue }
    $actual = (Get-FileHash -Algorithm SHA256 -Path $path).Hash.ToLower()
    if ($actual -ne $expected.ToLower()) { $badIds += "$id (sha256 mismatch)" }
}
if ($badIds.Count -gt 0) {
    throw "Hero-set integrity check failed for $($badIds.Count) file(s):`n$($badIds -join "`n")"
}
Write-Host "Integrity check passed: $($heroIds.Count) files verified."

# --- 2. Hardware identity, per docs/benchmarks.md's "never compared across different hardware".
$cpu = (Get-CimInstance Win32_Processor).Name
$gpu = Get-CimInstance Win32_VideoController | Select-Object -First 1
$ram = [math]::Round((Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory / 1GB, 1)
$os = (Get-CimInstance Win32_OperatingSystem).Version
$driveInfo = Get-PSDrive -PSProvider FileSystem | Select-Object Name, @{n = "FreeGB"; e = { [math]::Round($_.Free / 1GB, 1) } }

$outDir = Join-Path (Join-Path (Join-Path $ResultsRoot $Config) $Interaction) $RunLabel
New-Item -ItemType Directory -Force -Path $outDir | Out-Null

$meta = [ordered]@{
    interaction      = $Interaction
    config           = $Config
    run_label        = $RunLabel
    capture_fps      = $CaptureFps
    capture_fps_deviation_note = if ($CaptureFps -lt 120) {
        "hero-scenario.md specifies 120fps; this run captured at $CaptureFps fps due to a <120Hz display on the machine used (see hero-scenario.md's Capture-rate deviation note)."
    } else { $null }
    timestamp_utc    = (Get-Date).ToUniversalTime().ToString("o")
    cpu              = $cpu
    gpu              = $gpu.Name
    gpu_driver       = $gpu.DriverVersion
    ram_gb           = $ram
    windows_build    = $os
    drives           = $driveInfo
    indicator_rect   = $IndicatorRect
    roi_rect         = $RoiRect
}
$meta | ConvertTo-Json -Depth 5 | Set-Content (Join-Path $outDir "meta.json")

# --- 3. Capture + drive, concurrently.
$capturePath = Join-Path $outDir "capture.mkv"
Write-Host "Starting ddagrab capture at $CaptureFps fps -> $capturePath"
# NOT verified against real hardware yet (see hero-scenario.md/README.md): ddagrab's D3D11
# surfaces feed h264_nvenc with no explicit hwupload/hwmap filter. Most ffmpeg+NVENC builds
# negotiate this zero-copy path automatically, but if this errors on a format mismatch during the
# calibration dry-run, add an explicit hw-frames bridge filter here.
$ffmpegArgs = @(
    "-y", "-f", "lavfi", "-i", "ddagrab=framerate=$CaptureFps",
    "-c:v", "h264_nvenc", "-preset", "p7", "-qp", "0",
    $capturePath
)
# Started via System.Diagnostics.Process (not Start-Process) so stdin can be redirected: ffmpeg's
# documented graceful-stop signal is 'q' on stdin, which lets it flush/finalize the container.
# A hard Stop-Process -Force (TerminateProcess) risks a truncated/corrupted tail on the mkv --
# exactly the trailing frames the last event's "settled" measurement depends on.
$ffmpegStartInfo = [System.Diagnostics.ProcessStartInfo]::new($FfmpegExe)
foreach ($a in $ffmpegArgs) { $ffmpegStartInfo.ArgumentList.Add($a) }
$ffmpegStartInfo.RedirectStandardInput = $true
$ffmpegStartInfo.UseShellExecute = $false
$ffmpegStartInfo.CreateNoWindow = $true
$ffmpegProc = [System.Diagnostics.Process]::Start($ffmpegStartInfo)

Start-Sleep -Milliseconds 500 # let the capture actually start before input begins
if ($ffmpegProc.HasExited) {
    throw "ffmpeg exited immediately after start (exit code $($ffmpegProc.ExitCode)) -- capture never began. Check ddagrab/NVENC availability on this display session."
}

Write-Host "Running hero.ahk ($Interaction) ..."
$ahkProc = Start-Process -FilePath $AhkExe -ArgumentList @($AhkConfigPath) -PassThru
if (-not $ahkProc.WaitForExit($DurationSeconds * 1000)) {
    Stop-Process -Id $ahkProc.Id -Force -ErrorAction SilentlyContinue
    $ffmpegProc.StandardInput.Write("q")
    $ffmpegProc.StandardInput.Flush()
    $ffmpegProc.WaitForExit(5000) | Out-Null
    if (-not $ffmpegProc.HasExited) { Stop-Process -Id $ffmpegProc.Id -Force -ErrorAction SilentlyContinue }
    throw "hero.ahk did not exit within $DurationSeconds s (a blocking dialog? LRC window not found? a hung drag?) -- aborting. See $outDir for whatever capture exists."
}

Start-Sleep -Seconds 1 # trailing settle buffer beyond hero.ahk's own trailing sleep

Write-Host "Stopping capture (graceful 'q') ..."
$ffmpegProc.StandardInput.Write("q")
$ffmpegProc.StandardInput.Flush()
if (-not $ffmpegProc.WaitForExit(5000)) {
    Write-Warning "ffmpeg didn't exit within 5s of 'q' -- force-killing. The capture's tail frames may be truncated."
    Stop-Process -Id $ffmpegProc.Id -Force -ErrorAction SilentlyContinue
}

if (-not (Test-Path $capturePath)) {
    throw "Capture file was not produced: $capturePath"
}

# Sanity-check the capture actually contains video, not just an empty/near-empty container left
# behind by a silent ddagrab/NVENC failure that Test-Path alone wouldn't catch.
$frameCountRaw = & $FfprobeExe -v error -select_streams v:0 -count_frames -show_entries "stream=nb_read_frames" -of "csv=p=0" $capturePath
$frameCount = 0
[void][int]::TryParse($frameCountRaw, [ref]$frameCount)
if ($frameCount -lt 10) {
    throw "Capture at $capturePath has only $frameCount decodable video frame(s) -- capture likely failed silently (check ddagrab/NVENC on this display session)."
}
Write-Host "Capture verified: $frameCount frames."

# --- 4. Crop the two ROIs whisker needs (see bench/whisker/README.md).
$indicator = Get-Rect $IndicatorRect
$roi = Get-Rect $RoiRect
$indicatorRaw = Join-Path $outDir "indicator.raw"
$roiRaw = Join-Path $outDir "roi.raw"

Write-Host "Extracting indicator crop ..."
& $FfmpegExe -y -i $capturePath -vf "crop=$($indicator.W):$($indicator.H):$($indicator.X):$($indicator.Y),format=gray" -f rawvideo $indicatorRaw

Write-Host "Extracting ROI crop ..."
& $FfmpegExe -y -i $capturePath -vf "crop=$($roi.W):$($roi.H):$($roi.X):$($roi.Y),format=gray" -f rawvideo $roiRaw

Write-Host "Run complete. Output: $outDir"
Write-Host "Next: run whisker (switch|drag) against indicator.raw/roi.raw -- see bench/whisker/README.md."
