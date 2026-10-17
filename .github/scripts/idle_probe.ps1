$ErrorActionPreference = 'Stop'
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class Thr {
  [DllImport("kernel32.dll")] public static extern IntPtr OpenThread(uint a, bool i, uint id);
  [DllImport("kernel32.dll")] public static extern int GetThreadDescription(IntPtr h, out IntPtr d);
  [DllImport("kernel32.dll")] public static extern bool CloseHandle(IntPtr h);
  public static string Name(uint id) {
    IntPtr h = OpenThread(0x0400, false, id);
    if (h == IntPtr.Zero) return "?";
    IntPtr d; string n = "";
    if (GetThreadDescription(h, out d) >= 0 && d != IntPtr.Zero) n = Marshal.PtrToStringUni(d);
    CloseHandle(h);
    return n;
  }
}
'@
$exe = Resolve-Path target\release\nicti.exe
$cat = Join-Path $env:RUNNER_TEMP 'idle.catalog.sqlite'
$runs = 3
for ($r = 1; $r -le $runs; $r++) {
  $cat = Join-Path $env:RUNNER_TEMP "idle$r.catalog.sqlite"
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
      if ($d -gt 0) { $busy++; $top += [pscustomobject]@{ Id = $t.Id; Name = [Thr]::Name([uint32]$t.Id); Ms = [math]::Round($d, 1) } }
    }
    $cpuMs = $totAfter - $totBefore
    $pct = [math]::Round(100 * $cpuMs / (10000 * [Environment]::ProcessorCount), 3)
    Write-Host ("run {0} window {1}: threads={2} busy_threads={3} cpu_ms={4:N0} pct_of_machine={5} cores={6}" -f $r, $w, $p.Threads.Count, $busy, $cpuMs, $pct, [Environment]::ProcessorCount)
    $top | Sort-Object Ms -Descending | Select-Object -First 10 | Format-Table | Out-String | Write-Host
  }
  Stop-Process -Id $p.Id -Force
  Start-Sleep -Seconds 2
}

cargo run --release -p nicti-pounce --example idle_burn
