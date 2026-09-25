#Requires -Version 7.0
<#
.SYNOPSIS
Runs one measured pass of the #43 hero scenario: verifies the hero-set files against the
manifest, records hardware identity, optionally pre-navigates LRC's selection, captures the screen
while bench/lrc/hero.ahk drives LRC, then crops and hands the capture to whisker for analysis. See
docs/benchmarks/hero-scenario.md. Requires PowerShell 7+ (winget install Microsoft.PowerShell) --
this script's graceful-ffmpeg-stop logic uses a .NET-Core-only ProcessStartInfo API that silently
null-references under Windows PowerShell 5.1.

.PARAMETER Interaction
switch | crop | zoom -- must match bench/lrc/hero-config.ini's [general] Interaction (drives the
AHK script and hero-config.ini's own interaction-specific settings; independent of -Cold below).

.PARAMETER Config
originals | smart-previews -- which LRC configuration this run is against (naming only; the
correct catalog must already be open in LRC before this script runs).

.PARAMETER Cold
Marks this as a cold run: the `interaction` recorded in meta.json (and this run's output folder)
is "$Interaction-cold" instead of "$Interaction", so whisker's `analyze` pools cold results
separately from warm ones. Does not change what hero.ahk itself does -- the cold/warm distinction
is purely about catalog/cache state you've arranged before invoking this script (see
docs/benchmarks.md's cold-run rules and hero-scenario.md's Warm vs. cold section).

.PARAMETER RunLabel
Free-form label for this run's output folder, e.g. "warmup" or "run-1". whisker's `analyze`
excludes the run labeled "warmup" (case-insensitive, overridable via its own --warmup-label) from
pooled stats.

.PARAMETER CaptureFps
Capture rate in fps. Defaults to 60 per the documented capture-rate deviation in
hero-scenario.md -- pass 120 once run on a machine with a 120Hz+ display attached.

.PARAMETER IndicatorRect / RoiRect
"x,y,w,h" screen rects for the keypress indicator and the loupe ROI, matching hero-config.ini's
[indicator] section and the calibration step in bench/lrc/README.md.

.PARAMETER PreNavigate
Optional bench/lrc/navigate.ahk spec (e.g. "Left:49,Right:12"), run to completion *before* the
capture starts so navigation never lands inside the timed capture or gets scored as a spurious
switch event. See run-hero-series.ps1, which drives this for you across a full series.

.PARAMETER ImageIndex
1-based hero-set image index this run targets (crop/zoom's "5 images per run" spread). Purely a
label: recorded in meta.json and used to nest this run's output under an "img-<N>" subfolder so
whisker's `analyze` can walk multiple per-image captures per run. Omit (0) for switch runs, which
always operate on the full 50-image set rather than one target image.

.PARAMETER DurationSeconds
Safety timeout, not the target capture length (actual capture length is however long hero.ahk
takes to run, plus fixed buffers) -- if hero.ahk (or -PreNavigate's navigate.ahk) hasn't exited
within this many seconds (a blocking dialog, LRC not found, a hung drag), the run is aborted rather
than left to hang forever. Must comfortably exceed hero.ahk's expected run time for the chosen
interaction (see bench/lrc/hero-config.ini.example's delay/duration settings).

.PARAMETER DdagrabOutputIdx
Optional ddagrab `output_idx` override, for a machine with multiple display adapters/outputs (this
one has three: a Parsec virtual display, an AMD iGPU, and the NVIDIA GPU) where ddagrab's default
output isn't the one LRC is actually on. Leave unset unless the calibration dry-run shows a capture
of the wrong screen.
#>
param(
    [Parameter(Mandatory)] [ValidateSet("switch", "crop", "zoom")] [string]$Interaction,
    [Parameter(Mandatory)] [ValidateSet("originals", "smart-previews")] [string]$Config,
    [switch]$Cold,
    [Parameter(Mandatory)] [string]$RunLabel,
    [double]$CaptureFps = 60,
    [Parameter(Mandatory)] [string]$IndicatorRect,
    [Parameter(Mandatory)] [string]$RoiRect,
    [string]$PreNavigate = "",
    [int]$NavigateDelayMs = 80,
    [int]$ImageIndex = 0,
    [int]$DurationSeconds = 60,
    [int]$DdagrabOutputIdx = -1,
    [string]$RefRoot = "H:\NictiBench\ref-10k",
    [string]$ManifestPath = "$PSScriptRoot\..\docs\ref-10k-manifest.csv",
    [string]$HeroSetFile = "$PSScriptRoot\..\docs\benchmarks\hero-set.txt",
    [string]$ProwlExe = "$PSScriptRoot\..\target\release\prowl.exe",
    [string]$AhkConfigPath = "$PSScriptRoot\lrc\hero-config.ini",
    [string]$NavigateAhkPath = "$PSScriptRoot\lrc\navigate.ahk",
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

# winget/user-scope installs don't reliably land on PATH for every process that launches this
# script (WSL interop in particular can inherit a stale PATH) -- fall back to a search of the
# usual install locations before giving up, rather than failing on a bare "ffmpeg"/
# "AutoHotkey64.exe" that Start-Process can't resolve.
function Resolve-Tool([string]$explicit, [string]$commandName, [string]$searchPattern) {
    if ($explicit) {
        if (-not (Test-Path $explicit)) { throw "$commandName override not found: $explicit" }
        return $explicit
    }
    $onPath = Get-Command $commandName -ErrorAction SilentlyContinue
    if ($onPath) { return $onPath.Source }
    $extraRoots = @(
        (Join-Path $env:LOCALAPPDATA "Programs\AutoHotkey\v2"),
        (Join-Path ${env:ProgramFiles} "AutoHotkey\v2"),
        (Join-Path $env:LOCALAPPDATA "Microsoft\WinGet\Packages")
    )
    foreach ($rootDir in $extraRoots) {
        if (Test-Path $rootDir) {
            $found = Get-ChildItem $rootDir -Recurse -Filter $searchPattern -ErrorAction SilentlyContinue | Select-Object -First 1
            if ($found) { return $found.FullName }
        }
    }
    throw "$commandName not found on PATH or under the usual install locations -- pass -AhkExe/-FfmpegExe explicitly."
}

$AhkExe = Resolve-Tool $AhkExe "AutoHotkey64.exe" "AutoHotkey64.exe"
$FfmpegExe = Resolve-Tool $FfmpegExe "ffmpeg" "ffmpeg.exe"
$FfprobeExe = Resolve-Tool $FfprobeExe "ffprobe" "ffprobe.exe"
Write-Host "Using AutoHotkey: $AhkExe"
Write-Host "Using ffmpeg: $FfmpegExe"
Write-Host "Using ffprobe: $FfprobeExe"

$metaInteraction = if ($Cold) { "$Interaction-cold" } else { $Interaction }
$indicator = Get-Rect $IndicatorRect
$roi = Get-Rect $RoiRect

# --- 1. Integrity check: hero-set files must match the committed manifest's SHA-256 column,
#        per docs/benchmarks.md's rule that any harness verify the frozen copy before trusting it.
#        #17 moved this from an inline Get-FileHash loop to `prowl verify`, so this script and
#        every other harness (the `prowl` binary itself, any future research ticket's own
#        tooling) share one verifier instead of each reimplementing it.
if (-not (Test-Path $ProwlExe)) {
    throw "prowl.exe not found at $ProwlExe -- build it first: cargo build --release -p nicti-prowl"
}
Write-Host "Verifying hero-set file integrity via prowl ..."
# @(...): without this, PowerShell unwraps a single-match Where-Object result to a scalar
# instead of a one-element array, which would make $heroIds.Count below misreport.
$heroIds = @(Get-Content $HeroSetFile | Where-Object { $_.Trim() -ne "" })
if ($heroIds.Count -eq 0) {
    # prowl verify --ids "" would otherwise report a clean, zero-file no-op (correct in
    # isolation -- see nicti-prowl's own tests) and let this script fall through to actually
    # driving and capturing a benchmark against zero verified reference files. An empty
    # hero-set.txt is always a configuration mistake, never an intentional zero-file run.
    throw "Hero-set file $HeroSetFile contains no non-blank ids -- nothing to verify or benchmark."
}
$idsArg = $heroIds -join ","
& $ProwlExe verify --manifest $ManifestPath --root $RefRoot --ids $idsArg
if ($LASTEXITCODE -ne 0) {
    throw "Hero-set integrity check failed (prowl verify exited $LASTEXITCODE) -- see output above."
}
Write-Host "Integrity check passed: $($heroIds.Count) files verified."

# --- 2. Hardware identity, per docs/benchmarks.md's "never compared across different hardware".
$cpu = (Get-CimInstance Win32_Processor).Name
$gpu = Get-CimInstance Win32_VideoController | Select-Object -First 1
$ram = [math]::Round((Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory / 1GB, 1)
$os = (Get-CimInstance Win32_OperatingSystem).Version
$driveInfo = Get-PSDrive -PSProvider FileSystem | Select-Object Name, @{n = "FreeGB"; e = { [math]::Round($_.Free / 1GB, 1) } }

$lrcVersion = $null
try {
    $lrcProc = Get-Process -Name "Lightroom" -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($lrcProc) { $lrcVersion = $lrcProc.MainModule.FileVersionInfo.ProductVersion }
} catch {
    Write-Warning "Could not read Lightroom Classic version: $_"
}

$outDir = Join-Path (Join-Path (Join-Path $ResultsRoot $Config) $metaInteraction) $RunLabel
if ($ImageIndex -gt 0) { $outDir = Join-Path $outDir "img-$ImageIndex" }
New-Item -ItemType Directory -Force -Path $outDir | Out-Null

$meta = [ordered]@{
    interaction      = $metaInteraction
    config           = $Config
    run_label        = $RunLabel
    image_index      = if ($ImageIndex -gt 0) { $ImageIndex } else { $null }
    capture_fps      = $CaptureFps
    capture_fps_deviation_note = if ($CaptureFps -lt 120) {
        "hero-scenario.md specifies 120fps; this run captured at $CaptureFps fps due to a <120Hz display on the machine used (see hero-scenario.md's Capture-rate deviation note)."
    } else { $null }
    timestamp_utc    = (Get-Date).ToUniversalTime().ToString("o")
    lrc_version      = $lrcVersion
    cpu              = $cpu
    gpu              = $gpu.Name
    gpu_driver       = $gpu.DriverVersion
    ram_gb           = $ram
    windows_build    = $os
    drives           = $driveInfo
    indicator_rect   = $IndicatorRect
    indicator_w      = $indicator.W
    indicator_h      = $indicator.H
    roi_rect         = $RoiRect
    roi_w            = $roi.W
    roi_h            = $roi.H
}
$meta | ConvertTo-Json -Depth 5 | Set-Content (Join-Path $outDir "meta.json")

# --- 3. Pre-navigate (if requested), before the capture starts -- see -PreNavigate above.
if ($PreNavigate) {
    Write-Host "Pre-navigating: $PreNavigate"
    $navProc = Start-Process -FilePath $AhkExe -ArgumentList @($NavigateAhkPath, $PreNavigate, $NavigateDelayMs) -PassThru
    if (-not $navProc.WaitForExit($DurationSeconds * 1000)) {
        Stop-Process -Id $navProc.Id -Force -ErrorAction SilentlyContinue
        throw "navigate.ahk did not exit within $DurationSeconds s for spec '$PreNavigate' -- aborting."
    }
    if ($navProc.ExitCode -ne 0) {
        throw "navigate.ahk exited with code $($navProc.ExitCode) for spec '$PreNavigate'."
    }
}

# --- 4. Capture + drive, concurrently.
$capturePath = Join-Path $outDir "capture.mkv"
Write-Host "Starting ddagrab capture at $CaptureFps fps -> $capturePath"
# NOT verified against real hardware yet (see hero-scenario.md/README.md): ddagrab's D3D11
# surfaces feed h264_nvenc with no explicit hwupload/hwmap filter. Most ffmpeg+NVENC builds
# negotiate this zero-copy path automatically, but if this errors on a format mismatch during the
# calibration dry-run, add an explicit hw-frames bridge filter here.
$ddagrabOpts = @("framerate=$CaptureFps")
if ($DdagrabOutputIdx -ge 0) { $ddagrabOpts += "output_idx=$DdagrabOutputIdx" }
$ddagrabFilter = "ddagrab=" + ($ddagrabOpts -join ":")
$ffmpegArgs = @(
    "-y", "-f", "lavfi", "-i", $ddagrabFilter,
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

# --- 5. Crop the two ROIs whisker needs (see bench/whisker/README.md).
$indicatorRaw = Join-Path $outDir "indicator.raw"
$roiRaw = Join-Path $outDir "roi.raw"

Write-Host "Extracting indicator crop ..."
& $FfmpegExe -y -i $capturePath -vf "crop=$($indicator.W):$($indicator.H):$($indicator.X):$($indicator.Y),format=gray" -f rawvideo $indicatorRaw

Write-Host "Extracting ROI crop ..."
& $FfmpegExe -y -i $capturePath -vf "crop=$($roi.W):$($roi.H):$($roi.X):$($roi.Y),format=gray" -f rawvideo $roiRaw

Write-Host "Run complete. Output: $outDir"
Write-Host "Next: run whisker (switch|drag|analyze) against indicator.raw/roi.raw -- see bench/whisker/README.md."
