#Requires -Version 7.0
<#
.SYNOPSIS
Runs one (config, interaction) pair's full #43 hero-scenario series -- a discarded warm-up pass
plus 5 measured runs (docs/benchmarks.md's "1 warm-up run discarded, then 5 measured runs" rule) --
by invoking run-hero.ps1 once per run, with the right bench/lrc/navigate.ahk pre-navigation spec so
every run starts from the right selection without that navigation landing inside the timed capture.

For switch, every run starts at image 1 (rewinds however far the set may have advanced). For
crop/zoom, hero-scenario.md's "5 images per run, spread across the set" spread is driven here too:
each run visits every target image in -TargetImages, one run-hero.ps1 invocation per image,
producing run-hero.ps1's own "img-<N>" output subfolder per image.

Stops on the first run that throws (run-hero.ps1's own ErrorActionPreference = Stop propagates
through the call operator) rather than continuing past a broken series and producing a partial,
silently-incomplete result set.

.PARAMETER Cold
Passed straight through to every run-hero.ps1 invocation -- see its own -Cold doc.

.PARAMETER TargetImages
1-based hero-set image indices for crop/zoom's "5 images per run" spread. Default 1,13,25,37,49:
evenly spaced across the 50-file set, per hero-scenario.md's "spread across the set, not just
image 1" (it doesn't mandate exact positions).
#>
param(
    [Parameter(Mandatory)] [ValidateSet("originals", "smart-previews")] [string]$Config,
    [Parameter(Mandatory)] [ValidateSet("switch", "crop", "zoom", "mixed")] [string]$Interaction,
    [switch]$Cold,
    [double]$CaptureFps = 60,
    [Parameter(Mandatory)] [string]$IndicatorRect,
    [Parameter(Mandatory)] [string]$RoiRect,
    [int]$TotalImages = 50,
    [int[]]$TargetImages = @(1, 13, 25, 37, 49),
    [int]$NavigateDelayMs = 80,
    # mixed (interaction D, #100) drives many more steps per capture than a single switch/crop/
    # zoom pass -- bump the safety timeout well past its default StepGapMs*stepCount*Repeats.
    [int]$DurationSeconds = 60,
    [int]$DdagrabOutputIdx = -1,
    [string]$ResultsRoot = "$PSScriptRoot\..\bench-results\hero"
)

if ($Interaction -eq "mixed" -and -not $PSBoundParameters.ContainsKey("DurationSeconds")) {
    $DurationSeconds = 240
}

$ErrorActionPreference = "Stop"
$runHero = Join-Path $PSScriptRoot "run-hero.ps1"
$runLabels = @("warmup", "run-1", "run-2", "run-3", "run-4", "run-5")

function Invoke-Run {
    param([string]$RunLabel, [string]$PreNavigate, [int]$ImageIndex = 0)

    $label = "$Config / $Interaction$(if ($Cold) { ' (cold)' }) / $RunLabel"
    if ($ImageIndex -gt 0) { $label += " / img-$ImageIndex" }
    Write-Host "=== $label ==="

    $params = @{
        Interaction      = $Interaction
        Config           = $Config
        RunLabel         = $RunLabel
        CaptureFps       = $CaptureFps
        IndicatorRect    = $IndicatorRect
        RoiRect          = $RoiRect
        DurationSeconds  = $DurationSeconds
        DdagrabOutputIdx = $DdagrabOutputIdx
        ResultsRoot      = $ResultsRoot
    }
    if ($Cold) { $params["Cold"] = $true }
    if ($PreNavigate) {
        $params["PreNavigate"] = $PreNavigate
        $params["NavigateDelayMs"] = $NavigateDelayMs
    }
    if ($ImageIndex -gt 0) { $params["ImageIndex"] = $ImageIndex }

    & $runHero @params
}

if ($Interaction -eq "switch" -or $Interaction -eq "mixed") {
    foreach ($label in $runLabels) {
        # Every switch/mixed run must start at image 1 -- rewind however far the set may have
        # advanced. Unlike crop/zoom, mixed has no per-image spread: one capture per run drives
        # the whole interleaved sequence across however many images its own Sequence/Repeats
        # touch (see hero-config.ini.example's [mixed] section).
        Invoke-Run -RunLabel $label -PreNavigate "Left:$TotalImages"
    }
} else {
    foreach ($label in $runLabels) {
        foreach ($k in $TargetImages) {
            # Rewind to image 1, then advance to image k -- always relative to image 1 rather than
            # wherever the previous target image left off, so a skipped/failed image never throws
            # off subsequent navigation.
            $preNav = "Left:$TotalImages,Right:$($k - 1)"
            Invoke-Run -RunLabel $label -PreNavigate $preNav -ImageIndex $k
        }
    }
}

Write-Host "Series complete: $Config / $Interaction$(if ($Cold) { ' (cold)' })"
