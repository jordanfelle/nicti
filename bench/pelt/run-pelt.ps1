<#
.SYNOPSIS
Runs one measured pass of a #68 (ADR-0006) GUI-framework candidate interaction: records hardware
identity, captures the screen while bench/pelt/pelt.ahk drives the already-running pelt-*
binary, then crops and hands the capture to whisker for analysis. See
docs/adr/0006-gui-framework.md and docs/benchmarks/hero-scenario.md (same method, applied here).

.PARAMETER Candidate
egui | iced | slint -- which pelt-<candidate> binary is already running and on screen.

.PARAMETER Interaction
grid | loupe | slider | pan -- must match pelt-config.ini's [general] Interaction.

.PARAMETER RunLabel
Free-form label for this run's output folder, e.g. "warmup" or "run-1".

.PARAMETER CaptureFps
Capture rate in fps. Defaults to 60, matching hero-scenario.md's own documented capture-rate
deviation -- pass 120 once run on a machine with a 120Hz+ display attached.

.PARAMETER IndicatorRect / RoiRect
"x,y,w,h" screen rects for the keypress indicator and the interaction's ROI (the grid, the loupe
image, or the develop viewport, depending on -Interaction).

.PARAMETER DurationSeconds
Safety timeout -- if pelt.ahk hasn't exited within this many seconds, the run is aborted rather
than left to hang forever.
#>
param(
    [Parameter(Mandatory)] [ValidateSet("egui", "iced", "slint")] [string]$Candidate,
    [Parameter(Mandatory)] [ValidateSet("grid", "loupe", "slider", "pan")] [string]$Interaction,
    [Parameter(Mandatory)] [string]$RunLabel,
    [double]$CaptureFps = 60,
    [Parameter(Mandatory)] [string]$IndicatorRect,
    [Parameter(Mandatory)] [string]$RoiRect,
    [int]$DurationSeconds = 60,
    [string]$AhkConfigPath = "$PSScriptRoot\pelt-config.ini",
    [string]$AhkExe = "",
    [string]$FfmpegExe = "",
    [string]$FfprobeExe = "",
    [string]$ResultsRoot = "$PSScriptRoot\..\bench-results\pelt"
)

$ErrorActionPreference = "Stop"

function Get-Rect([string]$spec) {
    $parts = $spec -split "," | ForEach-Object { [int]$_.Trim() }
    if ($parts.Count -ne 4) { throw "Rect must be 'x,y,w,h', got: $spec" }
    return @{ X = $parts[0]; Y = $parts[1]; W = $parts[2]; H = $parts[3] }
}

# See bench/run-hero.ps1's identical Resolve-Tool comment for why the WinGet fallback exists.
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

if (-not (Test-Path $AhkConfigPath)) {
    throw "$AhkConfigPath not found -- copy pelt-config.ini.example to pelt-config.ini and calibrate the indicator rect first."
}

# Hardware identity, per docs/benchmarks.md's "never compared across different hardware" rule --
# same fields bench/run-hero.ps1 records for the LRC baseline, so a pelt-* number and a hero-
# scenario number from the same run are directly comparable.
$cpu = (Get-CimInstance Win32_Processor).Name
$gpu = Get-CimInstance Win32_VideoController | Select-Object -First 1
$ram = [math]::Round((Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory / 1GB, 1)
$os = (Get-CimInstance Win32_OperatingSystem).Version

$outDir = Join-Path (Join-Path $ResultsRoot "$Candidate\$Interaction") $RunLabel
New-Item -ItemType Directory -Force -Path $outDir | Out-Null

$meta = [ordered]@{
    candidate        = $Candidate
    interaction      = $Interaction
    run_label        = $RunLabel
    capture_fps      = $CaptureFps
    capture_fps_deviation_note = if ($CaptureFps -lt 120) {
        "hero-scenario.md's own capture-rate deviation note applies here too: this run captured at $CaptureFps fps, not the documented 120fps."
    } else { $null }
    timestamp_utc    = (Get-Date).ToUniversalTime().ToString("o")
    cpu              = $cpu
    gpu              = $gpu.Name
    gpu_driver       = $gpu.DriverVersion
    ram_gb           = $ram
    windows_build    = $os
    indicator_rect   = $IndicatorRect
    roi_rect         = $RoiRect
}
$meta | ConvertTo-Json -Depth 5 | Set-Content (Join-Path $outDir "meta.json")

$capturePath = Join-Path $outDir "capture.mkv"
Write-Host "Starting ddagrab capture at $CaptureFps fps -> $capturePath"
$ffmpegArgs = @(
    "-y", "-f", "lavfi", "-i", "ddagrab=framerate=$CaptureFps",
    "-c:v", "h264_nvenc", "-preset", "p7", "-qp", "0",
    $capturePath
)
$ffmpegStartInfo = [System.Diagnostics.ProcessStartInfo]::new($FfmpegExe)
foreach ($a in $ffmpegArgs) { $ffmpegStartInfo.ArgumentList.Add($a) }
$ffmpegStartInfo.RedirectStandardInput = $true
$ffmpegStartInfo.UseShellExecute = $false
$ffmpegStartInfo.CreateNoWindow = $true
$ffmpegProc = [System.Diagnostics.Process]::Start($ffmpegStartInfo)

Start-Sleep -Milliseconds 500
if ($ffmpegProc.HasExited) {
    throw "ffmpeg exited immediately after start (exit code $($ffmpegProc.ExitCode)) -- capture never began."
}

Write-Host "Running pelt.ahk (candidate=$Candidate interaction=$Interaction) ..."
$ahkProc = Start-Process -FilePath $AhkExe -ArgumentList @($AhkConfigPath) -PassThru
if (-not $ahkProc.WaitForExit($DurationSeconds * 1000)) {
    Stop-Process -Id $ahkProc.Id -Force -ErrorAction SilentlyContinue
    $ffmpegProc.StandardInput.Write("q")
    $ffmpegProc.StandardInput.Flush()
    $ffmpegProc.WaitForExit(5000) | Out-Null
    if (-not $ffmpegProc.HasExited) { Stop-Process -Id $ffmpegProc.Id -Force -ErrorAction SilentlyContinue }
    throw "pelt.ahk did not exit within $DurationSeconds s -- aborting. See $outDir for whatever capture exists."
}

Start-Sleep -Seconds 1

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

$frameCountRaw = & $FfprobeExe -v error -select_streams v:0 -count_frames -show_entries "stream=nb_read_frames" -of "csv=p=0" $capturePath
$frameCount = 0
[void][int]::TryParse($frameCountRaw, [ref]$frameCount)
if ($frameCount -lt 10) {
    throw "Capture at $capturePath has only $frameCount decodable video frame(s) -- capture likely failed silently."
}
Write-Host "Capture verified: $frameCount frames."

$indicator = Get-Rect $IndicatorRect
$roi = Get-Rect $RoiRect
$indicatorRaw = Join-Path $outDir "indicator.raw"
$roiRaw = Join-Path $outDir "roi.raw"

Write-Host "Extracting indicator crop ..."
& $FfmpegExe -y -i $capturePath -vf "crop=$($indicator.W):$($indicator.H):$($indicator.X):$($indicator.Y),format=gray" -f rawvideo $indicatorRaw

Write-Host "Extracting ROI crop ..."
& $FfmpegExe -y -i $capturePath -vf "crop=$($roi.W):$($roi.H):$($roi.X):$($roi.Y),format=gray" -f rawvideo $roiRaw

Write-Host "Run complete. Output: $outDir"
Write-Host "Next: run whisker (switch for loupe/slider-settled, drag for grid/slider-drag/pan) against indicator.raw/roi.raw -- see bench/whisker/README.md."
