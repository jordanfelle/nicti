<#
.SYNOPSIS
    #143's codec sweep: JPEG baseline, AVIF at ravif speeds 6-10, and lossy WebP at three
    qualities, all at T2 (screen tier), against the #37/#136 stratified NictiBench-subset (the
    ref-10k replacement -- see ADR-0020).

.DESCRIPTION
    Follows docs/benchmarks.md's protocol: 1 discarded warm-up run, then 5 measured runs per
    config, each into its own fresh output directory (a shared cache dir across configs would let
    a later config's payloads silently overwrite an earlier one's under the same asset-id keys).
    `--ssim` is passed on every run, not as a separate pass -- scoring happens after each
    (already-recorded) encode-timing measurement, so it doesn't distort the timing numbers, and
    running it once per run instead of via a 10th "ssim-only" pass halves the total wall time.

    Requires Windows PowerShell 5.1+ (not PowerShell 7-only syntax) -- this reference machine
    doesn't have pwsh.exe installed. Run natively on Windows, not through WSL's /mnt/h -- 9p I/O
    would distort the read+decode timings docs/benchmarks.md's protocol depends on.

.PARAMETER SniffExe
    Path to the cross-built sniff.exe (built with
    `cargo build --release -p sniff --target x86_64-pc-windows-gnu`).

.PARAMETER Roots
    Comma-separated NictiBench-subset folders to sample from (e.g.
    H:\NictiBench-subset\anthrocon2024,H:\NictiBench-subset\mff2024) -- a single string, not a
    PowerShell array literal: invoking a script with `-File` (the only way to run this from a
    non-interactive caller, including WSL interop) does not split a comma-joined command-line
    argument into an array the way an interactive `-Roots 'a','b'` call would, so this splits on
    `,` itself instead of declaring `[string[]]$Roots`. Passed through as repeated --root flags.

.PARAMETER OutRoot
    Scratch directory for per-run cache backends and result JSON. Created if missing.

.PARAMETER SampleLimit
    Optional cap on the file count (mainly for a quick smoke run); omitted means the full merged
    set across all -Roots.
#>
param(
    [Parameter(Mandatory = $true)]
    [string]$SniffExe,

    [Parameter(Mandatory = $true)]
    [string]$Roots,

    [Parameter(Mandatory = $true)]
    [string]$OutRoot,

    [int]$SampleLimit = 0
)

$ErrorActionPreference = "Stop"
$RootList = $Roots -split "," | Where-Object { $_.Trim().Length -gt 0 }

$configs = @(
    @{ Name = "jpeg_q85";        Codec = "jpeg"; Quality = 85; AvifSpeed = 6 },
    @{ Name = "avif_q75_speed6"; Codec = "avif"; Quality = 75; AvifSpeed = 6 },
    @{ Name = "avif_q75_speed7"; Codec = "avif"; Quality = 75; AvifSpeed = 7 },
    @{ Name = "avif_q75_speed8"; Codec = "avif"; Quality = 75; AvifSpeed = 8 },
    @{ Name = "avif_q75_speed9"; Codec = "avif"; Quality = 75; AvifSpeed = 9 },
    @{ Name = "avif_q75_speed10"; Codec = "avif"; Quality = 75; AvifSpeed = 10 },
    @{ Name = "webp_q75_m4";     Codec = "webp"; Quality = 75; AvifSpeed = 6 },
    @{ Name = "webp_q80_m4";     Codec = "webp"; Quality = 80; AvifSpeed = 6 },
    @{ Name = "webp_q85_m4";     Codec = "webp"; Quality = 85; AvifSpeed = 6 }
)

function Invoke-SniffRun {
    param(
        [hashtable]$Config,
        [string]$RunOutDir
    )

    New-Item -ItemType Directory -Path $RunOutDir -Force | Out-Null

    $rootArgs = @()
    foreach ($root in $RootList) {
        $rootArgs += "--root"
        $rootArgs += $root
    }

    $allArgs = $rootArgs + @(
        "--codec", $Config.Codec,
        "--quality", $Config.Quality,
        "--avif-speed", $Config.AvifSpeed,
        "--ssim",
        "--out-dir", $RunOutDir
    )
    if ($SampleLimit -gt 0) {
        $allArgs += @("--sample-limit", $SampleLimit)
    }

    $json = & $SniffExe tier-bench @allArgs
    if ($LASTEXITCODE -ne 0) {
        throw "sniff tier-bench failed for config '$($Config.Name)' (exit $LASTEXITCODE): $json"
    }
    return $json | ConvertFrom-Json
}

function Get-Median {
    param([double[]]$Values)
    $sorted = $Values | Sort-Object
    $n = $sorted.Length
    if ($n -eq 0) { return $null }
    if ($n % 2 -eq 1) {
        return $sorted[[int](($n - 1) / 2)]
    }
    $mid = $n / 2
    return ($sorted[$mid - 1] + $sorted[$mid]) / 2.0
}

New-Item -ItemType Directory -Path $OutRoot -Force | Out-Null
$summary = @()

foreach ($config in $configs) {
    Write-Host "=== $($config.Name) ==="
    $configDir = Join-Path $OutRoot $config.Name

    Write-Host "  warm-up (discarded)"
    Invoke-SniffRun -Config $config -RunOutDir (Join-Path $configDir "warmup") | Out-Null

    $runs = @()
    for ($i = 1; $i -le 5; $i++) {
        Write-Host "  measured run $i/5"
        $result = Invoke-SniffRun -Config $config -RunOutDir (Join-Path $configDir "run$i")
        $result | ConvertTo-Json -Depth 5 | Set-Content -Path (Join-Path $configDir "run$i.json")
        $runs += $result
    }

    $summaryRow = [ordered]@{
        config             = $config.Name
        codec              = $config.Codec
        quality            = $config.Quality
        avif_speed         = $config.AvifSpeed
        n_assets           = $runs[0].n_assets
        avg_encoded_bytes  = Get-Median ($runs | ForEach-Object { $_.avg_encoded_bytes })
        encode_p50_ms      = Get-Median ($runs | ForEach-Object { $_.encode_p50_ms })
        encode_p95_ms      = Get-Median ($runs | ForEach-Object { $_.encode_p95_ms })
        read_decode_p50_ms = Get-Median ($runs | ForEach-Object { $_.read_decode_p50_ms })
        read_decode_p95_ms = Get-Median ($runs | ForEach-Object { $_.read_decode_p95_ms })
        ssim_mean          = Get-Median ($runs | ForEach-Object { $_.ssim_mean })
        ssim_p5            = Get-Median ($runs | ForEach-Object { $_.ssim_p5 })
        ssim_min           = ($runs | ForEach-Object { $_.ssim_min } | Measure-Object -Minimum).Minimum
    }
    $summary += [pscustomobject]$summaryRow
}

$summaryPath = Join-Path $OutRoot "summary.json"
$summary | ConvertTo-Json -Depth 5 | Set-Content -Path $summaryPath
$summary | Format-Table -AutoSize
Write-Host "Summary written to $summaryPath"
