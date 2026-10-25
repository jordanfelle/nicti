<#
.SYNOPSIS
  Idle frame-rate probe (#513): launch the release nicti.exe on an empty catalog, let it settle,
  measure frames per second over a window, and fail above a budget.

.DESCRIPTION
  Idle CPU% on a hosted runner is useless as a gate (34-47% baseline on WARP), but frames per
  second is deterministic. The frame log is the existing NICTI_REPAINT_TRACE output (#512): a
  `repaint-trace: frames=N relayout=M elapsed_ms=T ...` stderr line each time a frame runs at
  least a second after the previous line, so an idle app prints less often than once a second.
  The rate is therefore sum(frames) / sum(elapsed_ms) over the lines past the settle time, never
  a line count. Baseline on windows-latest is ~2.2 frames/s; a true repaint loop saturates near
  11, so the default budget leaves headroom for runner jitter while still catching a loop. This
  catches repaint loops only, not a worker burn.
#>
param(
    [string]$Exe = (Join-Path $PWD "target\release\nicti.exe"),
    [int]$SettleSeconds = 15,
    [int]$WindowSeconds = 10,
    [double]$BudgetFps = 6.0,
    [int]$GraceSeconds = 30
)
$ErrorActionPreference = "Stop"

$dir = Join-Path ([System.IO.Path]::GetTempPath()) ("nicti-idle-probe-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $dir | Out-Null
$err = Join-Path $dir "stderr.log"
$out = Join-Path $dir "stdout.log"
$env:NICTI_REPAINT_TRACE = "1"

# Parsed samples: frames and the span (ms) they cover, in arrival order.
function Read-Samples {
    if (-not (Test-Path $err)) { return @() }
    @(Get-Content $err | Where-Object { $_ -match "^repaint-trace: frames=(\d+) .*elapsed_ms=(\d+)" } | ForEach-Object {
        $null = $_ -match "^repaint-trace: frames=(\d+) .*elapsed_ms=(\d+)"
        [pscustomobject]@{ Frames = [int]$Matches[1]; Ms = [int]$Matches[2] }
    })
}

# The release binary is a GUI-subsystem exe; redirected handles still reach eprintln!.
# The catalog path is quoted so a temp dir with spaces stays one argument.
$proc = Start-Process -FilePath $Exe -ArgumentList ('"' + (Join-Path $dir "probe.nicti") + '"') `
    -RedirectStandardError $err -RedirectStandardOutput $out -PassThru
$started = Get-Date
$samples = @()
try {
    # Deadline loop, not a fixed sleep: stop once the samples cover settle + window, or when the
    # wall-clock deadline passes (an idle app legitimately prints fewer lines).
    $needMs = ($SettleSeconds + $WindowSeconds) * 1000
    $deadline = $started.AddSeconds($SettleSeconds + $WindowSeconds + $GraceSeconds)
    while ((Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 500
        if ($proc.HasExited) { break }
        $samples = Read-Samples
        if (($samples | Measure-Object -Property Ms -Sum).Sum -ge $needMs) { break }
    }
    $exitedEarly = $proc.HasExited
} finally {
    if (-not $proc.HasExited) { Stop-Process -Id $proc.Id -Force }
}
$samples = Read-Samples
$wallSeconds = ((Get-Date) - $started).TotalSeconds

if ($samples.Count -eq 0 -or $exitedEarly) {
    Write-Host "::error::idle probe: app exited early or printed no frame log ($($samples.Count) lines)"
    if (Test-Path $err) { Get-Content $err | Select-Object -Last 40 }
    exit 1
}

# Skip samples until their cumulative span passes the settle time, then take the rest (the loop
# stops once the window is covered; the deadline may end it sooner for a very quiet app).
$cum = 0; $frames = 0; $ms = 0
foreach ($s in $samples) {
    $cum += $s.Ms
    if ($cum -gt $SettleSeconds * 1000) { $frames += $s.Frames; $ms += $s.Ms }
}
if ($ms -le 0) {
    Write-Host "idle frame rate: no frames logged after the $SettleSeconds s settle ($($samples.Count) lines); well under budget"
    exit 0
}
$fps = $frames / ($ms / 1000.0)
Write-Host ("idle frame rate: {0:N2} frames/s ({1} frames over {2:N1} s measured after >= {3} s settle; budget {4})" -f $fps, $frames, ($ms / 1000.0), $SettleSeconds, $BudgetFps)
if ($fps -gt $BudgetFps) {
    Write-Host "::error::idle frame rate $fps/s exceeds budget $BudgetFps/s; frame log and repaint causes follow"
    Get-Content $err | Select-Object -Last 80
    exit 1
}
