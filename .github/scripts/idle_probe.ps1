$ErrorActionPreference = 'Stop'
$exe = Resolve-Path target\release\nicti.exe
$cat = Join-Path $env:RUNNER_TEMP 'idle.catalog.sqlite'
$runs = 3
for ($r = 1; $r -le $runs; $r++) {
  Remove-Item "$cat*" -ErrorAction SilentlyContinue
  $p = Start-Process -FilePath $exe -ArgumentList "`"$cat`"" -PassThru
  Start-Sleep -Seconds 15
  if ($p.HasExited) { Write-Host "run ${r}: EXITED early, code $($p.ExitCode)"; continue }
  for ($w = 1; $w -le 3; $w++) {
    $p.Refresh()
    $before = @{}
    foreach ($t in $p.Threads) { $before[$t.Id] = $t.TotalProcessorTime.TotalMilliseconds }
    $totBefore = $p.TotalProcessorTime.TotalMilliseconds
    Start-Sleep -Seconds 10
    $p.Refresh()
    $totAfter = $p.TotalProcessorTime.TotalMilliseconds
    $busy = 0
    $top = @()
    foreach ($t in $p.Threads) {
      $prev = if ($before.ContainsKey($t.Id)) { $before[$t.Id] } else { 0 }
      $d = $t.TotalProcessorTime.TotalMilliseconds - $prev
      if ($d -gt 0) { $busy++; $top += [pscustomobject]@{ Id = $t.Id; Ms = [math]::Round($d, 1) } }
    }
    $cpuMs = $totAfter - $totBefore
    $pct = [math]::Round(100 * $cpuMs / (10000 * [Environment]::ProcessorCount), 3)
    Write-Host ("run {0} window {1}: threads={2} busy_threads={3} cpu_ms={4:N0} pct_of_machine={5} cores={6}" -f $r, $w, $p.Threads.Count, $busy, $cpuMs, $pct, [Environment]::ProcessorCount)
    $top | Sort-Object Ms -Descending | Select-Object -First 5 | Format-Table | Out-String | Write-Host
  }
  Stop-Process -Id $p.Id -Force
  Start-Sleep -Seconds 2
}
