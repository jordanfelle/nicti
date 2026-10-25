<#
.SYNOPSIS
  Idle frame-rate probe (#513): launch the release nicti.exe on an empty catalog, let it settle,
  count frames over a window, and fail above a budget.

.DESCRIPTION
  Idle CPU% on a hosted runner is useless as a gate (34-47% baseline on WARP), but frames per
  second is deterministic. The frame log is the existing NICTI_REPAINT_TRACE output (#512): one
  `repaint-trace: frames=N ...` stderr line per second. Baseline on windows-latest is ~2.2
  frames/s; a true repaint loop saturates near 11, so the default budget leaves headroom for
  runner jitter while still catching a loop. This catches repaint loops only, not a worker burn.
#>
param(
    [string]$Exe = "target\release\nicti.exe",
    [int]$SettleSeconds = 15,
    [int]$WindowSeconds = 10,
    [double]$BudgetFps = 6.0
)
$ErrorActionPreference = "Stop"

$dir = Join-Path ([System.IO.Path]::GetTempPath()) ("nicti-idle-probe-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $dir | Out-Null
$err = Join-Path $dir "stderr.log"
$out = Join-Path $dir "stdout.log"
$env:NICTI_REPAINT_TRACE = "1"

# The release binary is a GUI-subsystem exe; redirected handles still reach eprintln!.
$proc = Start-Process -FilePath $Exe -ArgumentList (Join-Path $dir "probe.nicti") `
    -RedirectStandardError $err -RedirectStandardOutput $out -PassThru
try {
    # Deadline loop, not a fixed sleep: stop as soon as enough one-second samples exist, and
    # give up (fail) if the app exits or never gets there.
    $needed = $SettleSeconds + $WindowSeconds
    $deadline = (Get-Date).AddSeconds($needed + 45)
    $lines = @()
    while ((Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 500
        if ($proc.HasExited) { break }
        if (Test-Path $err) {
            $lines = @(Get-Content $err | Where-Object { $_ -like "repaint-trace:*" })
            if ($lines.Count -ge $needed) { break }
        }
    }
} finally {
    if (-not $proc.HasExited) { Stop-Process -Id $proc.Id -Force }
}

if (Test-Path $err) { $lines = @(Get-Content $err | Where-Object { $_ -like "repaint-trace:*" }) }
if ($lines.Count -lt ($SettleSeconds + $WindowSeconds)) {
    Write-Host "::error::idle probe got only $($lines.Count) frame-log lines (app exited or hung)"
    if (Test-Path $err) { Get-Content $err | Select-Object -Last 40 }
    exit 1
}

$window = $lines | Select-Object -Skip $SettleSeconds -First $WindowSeconds
$frames = 0
foreach ($l in $window) {
    if ($l -match "frames=(\d+)") { $frames += [int]$Matches[1] }
}
$fps = $frames / $WindowSeconds
Write-Host ("idle frame rate: {0:N2} frames/s over {1} s after {2} s settle (budget {3})" -f $fps, $WindowSeconds, $SettleSeconds, $BudgetFps)
if ($fps -gt $BudgetFps) {
    Write-Host "::error::idle frame rate $fps/s exceeds budget $BudgetFps/s; wake attribution and repaint causes follow"
    Get-Content $err | Select-Object -Skip $SettleSeconds -First 60
    exit 1
}
