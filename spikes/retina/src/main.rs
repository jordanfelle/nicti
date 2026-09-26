mod compression;
mod frame;
mod libraw_ffi;
mod rawler_backend;
mod watch;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use clap::{Parser, Subcommand, ValueEnum};
use nicti_prowl::manifest::{Manifest, Scope};
use nicti_prowl::perf::Protocol;
use serde::Serialize;

#[derive(Parser)]
#[command(name = "retina", about = "Spike for #37: RAW decoder research")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum, Debug)]
enum Decoder {
    Libraw,
    Rawler,
}

#[derive(Subcommand)]
enum Command {
    /// Decode every file the manifest lists (or an --ids subset) with one backend, one JSONL row
    /// per file.
    Sweep {
        root: PathBuf,
        manifest: PathBuf,
        #[arg(long, value_enum)]
        decoder: Decoder,
        #[arg(long)]
        out: PathBuf,
        /// Verify each file's sha256 against the manifest before decoding it. Off by default for
        /// a fast sweep re-run; turn on for the run whose numbers go in the ADR.
        #[arg(long)]
        verify: bool,
    },
    /// Joins two sweep JSONL files (e.g. libraw vs rawler) by id and reports agreement/mismatch
    /// per compression bucket.
    Compare { a: PathBuf, b: PathBuf },
    /// Per-bucket decode latency (1 warmup + 5 measured runs, nicti-prowl's Protocol) plus rayon
    /// throughput at several thread counts.
    Bench {
        root: PathBuf,
        manifest: PathBuf,
        #[arg(long, value_enum)]
        decoder: Decoder,
        /// How many files per bucket to time individually (bench's per-file latency loop, not
        /// the throughput sweep).
        #[arg(long, default_value_t = 20)]
        sample_per_bucket: usize,
    },
    /// notify-based filesystem watch spike for #24's ingest question.
    Watch {
        dir: PathBuf,
        #[arg(long, default_value_t = 30)]
        seconds: u64,
    },
    /// Debug helper: decode one file and print the first N raw CFA samples plus metadata.
    /// Not part of #37's exit criteria, just for chasing a hash mismatch investigation.
    Peek {
        path: PathBuf,
        #[arg(long, value_enum)]
        decoder: Decoder,
        #[arg(long, default_value_t = 16)]
        n: usize,
    },
    /// Quantitative per-pixel CFA diff between LibRaw and rawler on one file -- the real
    /// correctness tool once `compare`'s exact-hash-match assumption turned out wrong for
    /// Lossless-compressed NEFs (see #37's write-up: a nonlinear curve-decode rounding
    /// difference, not a bug in either decoder).
    Diff { path: PathBuf },
    /// Recursively decodes every .NEF/.nef under `dir` with one backend -- no manifest CSV
    /// needed. Added when the frozen `ref-10k` copies (both NVMe and HDD) turned out to be
    /// unmaintainable at 393GB and vanished mid-research (see #37's write-up); this runs directly
    /// against the live source library or any ad hoc subset copied out of it instead.
    Scan {
        dir: PathBuf,
        #[arg(long, value_enum)]
        decoder: Decoder,
        #[arg(long)]
        out: PathBuf,
    },
}

#[derive(Serialize, serde::Deserialize)]
struct SweepRow {
    id: String,
    bucket: String,
    manifest_compression: String,
    ok: bool,
    error: Option<String>,
    ms: f64,
    frame: Option<frame::RawFrame>,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Sweep {
            root,
            manifest,
            decoder,
            out,
            verify,
        } => sweep(&root, &manifest, decoder, &out, verify),
        Command::Compare { a, b } => compare(&a, &b),
        Command::Bench {
            root,
            manifest,
            decoder,
            sample_per_bucket,
        } => bench(&root, &manifest, decoder, sample_per_bucket),
        Command::Watch { dir, seconds } => watch::run(&dir, seconds),
        Command::Peek { path, decoder, n } => peek(&path, decoder, n),
        Command::Diff { path } => diff(&path),
        Command::Scan { dir, decoder, out } => scan(&dir, decoder, &out),
    }
}

fn scan(dir: &Path, decoder: Decoder, out: &Path) -> anyhow::Result<()> {
    use rayon::prelude::*;

    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d)? {
            let entry = entry?;
            let path = entry.path();
            // `entry.file_type()` (unlike `path.is_dir()`, which follows symlinks via
            // `fs::metadata`) does NOT follow symlinks -- a hostile review caught that the
            // original `path.is_dir()` check here would walk into a symlinked directory and hang
            // forever on a self-referential or ancestor-pointing symlink loop (plausible from
            // backup tools or network-share reparse points in a real photo library). Skipping
            // symlinked directories entirely is the simplest safe fix; nothing in this research's
            // subset needed to follow one.
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file()
                && path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.eq_ignore_ascii_case("nef") || e.eq_ignore_ascii_case("dng"))
                    .unwrap_or(false)
            {
                files.push(path);
            }
        }
    }
    eprintln!("found {} raw files under {}", files.len(), dir.display());

    let rows: Vec<SweepRow> = files
        .par_iter()
        .map(|path| {
            let id = path
                .strip_prefix(dir)
                .unwrap_or(path)
                .to_string_lossy()
                .into_owned();
            let start = Instant::now();
            let result = fs::read(path)
                .map_err(|e| format!("read: {e}"))
                .and_then(|data| decode_one(decoder, &data));
            let ms = start.elapsed().as_secs_f64() * 1000.0;
            match result {
                Ok(frame) => SweepRow {
                    bucket: frame.compression_label.clone(),
                    manifest_compression: frame.compression_label.clone(),
                    id,
                    ok: true,
                    error: None,
                    ms,
                    frame: Some(frame),
                },
                Err(e) => SweepRow {
                    id,
                    bucket: "unknown".to_string(),
                    manifest_compression: "unknown".to_string(),
                    ok: false,
                    error: Some(e),
                    ms,
                    frame: None,
                },
            }
        })
        .collect();

    let mut writer = fs::File::create(out)?;
    use std::io::Write;
    let ok_count = rows.iter().filter(|r| r.ok).count();
    for row in &rows {
        writeln!(writer, "{}", serde_json::to_string(row)?)?;
    }
    eprintln!(
        "scan [{decoder:?}]: {ok_count}/{} decoded ok -> {}",
        rows.len(),
        out.display()
    );
    Ok(())
}

fn diff(path: &Path) -> anyhow::Result<()> {
    let data = fs::read(path)?;

    let mut lr_handle = libraw_ffi::LibRawHandle::new();
    lr_handle
        .decode(&data)
        .map_err(|e| anyhow::anyhow!("libraw: {e}"))?;
    let lr_cfa = lr_handle
        .raw_image()
        .map_err(|e| anyhow::anyhow!("libraw: {e}"))?
        .to_vec();

    let source = rawler::rawsource::RawSource::new_from_slice(&data);
    let rdecoder = rawler::get_decoder(&source)?;
    let raw_image = rdecoder.raw_image(
        &source,
        &rawler::decoders::RawDecodeParams::default(),
        false,
    )?;
    let rawler_cfa = match &raw_image.data {
        rawler::rawimage::RawImageData::Integer(v) => v.clone(),
        rawler::rawimage::RawImageData::Float(_) => anyhow::bail!("rawler returned float data"),
    };

    if lr_cfa.len() != rawler_cfa.len() {
        anyhow::bail!(
            "length mismatch: libraw {} vs rawler {}",
            lr_cfa.len(),
            rawler_cfa.len()
        );
    }

    let mut exact = 0u64;
    let mut max_abs_diff = 0i32;
    let mut sum_abs_diff = 0i64;
    let mut histogram: std::collections::BTreeMap<i32, u64> = Default::default();
    for (a, b) in lr_cfa.iter().zip(rawler_cfa.iter()) {
        let d = *a as i32 - *b as i32;
        if d == 0 {
            exact += 1;
        }
        max_abs_diff = max_abs_diff.max(d.abs());
        sum_abs_diff += d.abs() as i64;
        *histogram.entry(d).or_default() += 1;
    }
    let n = lr_cfa.len() as f64;
    println!("samples: {}", lr_cfa.len());
    println!("exact match: {exact} ({:.4}%)", 100.0 * exact as f64 / n);
    println!("max abs diff: {max_abs_diff}");
    println!("mean abs diff: {:.6}", sum_abs_diff as f64 / n);
    println!("diff histogram (delta -> count), only |delta| <= 3:");
    for (d, count) in &histogram {
        if d.abs() <= 3 {
            println!("  {d:+}: {count}");
        }
    }
    Ok(())
}

fn peek(path: &Path, decoder: Decoder, n: usize) -> anyhow::Result<()> {
    let data = fs::read(path)?;
    match decoder {
        Decoder::Libraw => {
            let mut handle = libraw_ffi::LibRawHandle::new();
            handle.decode(&data).map_err(|e| anyhow::anyhow!("{e}"))?;
            let meta = handle.metadata();
            let cfa = handle.raw_image().map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("{meta:?}");
            println!("first {n}: {:?}", &cfa[..n.min(cfa.len())]);
            println!(
                "row0 last 8: {:?}",
                &cfa[(meta.raw_width as usize - 8)..meta.raw_width as usize]
            );
            println!(
                "sum={} nonzero={}",
                cfa.iter().map(|&v| v as u64).sum::<u64>(),
                cfa.iter().filter(|&&v| v != 0).count()
            );
        }
        Decoder::Rawler => {
            let frame = rawler_backend::decode(&data).map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("{frame:?}");
            let source = rawler::rawsource::RawSource::new_from_slice(&data);
            let rdecoder = rawler::get_decoder(&source)?;
            let raw_image = rdecoder.raw_image(
                &source,
                &rawler::decoders::RawDecodeParams::default(),
                false,
            )?;
            if let rawler::rawimage::RawImageData::Integer(v) = &raw_image.data {
                println!("first {n}: {:?}", &v[..n.min(v.len())]);
                println!(
                    "row0 last 8: {:?}",
                    &v[(raw_image.width - 8)..raw_image.width]
                );
                println!(
                    "sum={} nonzero={}",
                    v.iter().map(|&x| x as u64).sum::<u64>(),
                    v.iter().filter(|&&x| x != 0).count()
                );
            }
        }
    }
    Ok(())
}

fn decode_one(decoder: Decoder, data: &[u8]) -> Result<frame::RawFrame, String> {
    match decoder {
        Decoder::Libraw => {
            let mut handle = libraw_ffi::LibRawHandle::new();
            handle.decode(data).map_err(|e| e.to_string())?;
            let meta = handle.metadata();
            let cfa = handle.raw_image().map_err(|e| e.to_string())?;
            Ok(frame::RawFrame::from_libraw(&meta, cfa))
        }
        Decoder::Rawler => rawler_backend::decode(data).map_err(|e| e.to_string()),
    }
}

fn sweep(
    root: &Path,
    manifest_path: &Path,
    decoder: Decoder,
    out: &Path,
    verify: bool,
) -> anyhow::Result<()> {
    let manifest = Manifest::load(manifest_path)?;

    if verify {
        let report = manifest.verify(root, Scope::All);
        report.clone().into_result()?;
        eprintln!("verified {} files clean against manifest", report.ok);
    }

    use rayon::prelude::*;
    let rows: Vec<SweepRow> = manifest
        .entries()
        .par_iter()
        .map(|entry| {
            let path = root.join(&entry.id);
            let start = Instant::now();
            let result = fs::read(&path)
                .map_err(|e| format!("read: {e}"))
                .and_then(|data| decode_one(decoder, &data));
            let ms = start.elapsed().as_secs_f64() * 1000.0;
            match result {
                Ok(frame) => SweepRow {
                    id: entry.id.clone(),
                    bucket: entry.bucket.clone(),
                    manifest_compression: entry.compression.clone(),
                    ok: true,
                    error: None,
                    ms,
                    frame: Some(frame),
                },
                Err(e) => SweepRow {
                    id: entry.id.clone(),
                    bucket: entry.bucket.clone(),
                    manifest_compression: entry.compression.clone(),
                    ok: false,
                    error: Some(e),
                    ms,
                    frame: None,
                },
            }
        })
        .collect();

    let mut writer = fs::File::create(out)?;
    use std::io::Write;
    let ok_count = rows.iter().filter(|r| r.ok).count();
    for row in &rows {
        writeln!(writer, "{}", serde_json::to_string(row)?)?;
    }
    eprintln!(
        "sweep [{decoder:?}]: {ok_count}/{} decoded ok -> {}",
        rows.len(),
        out.display()
    );
    Ok(())
}

fn compare(a_path: &Path, b_path: &Path) -> anyhow::Result<()> {
    use std::collections::BTreeMap;
    use std::io::BufRead;

    let load = |p: &Path| -> anyhow::Result<BTreeMap<String, SweepRow>> {
        let file = fs::File::open(p)?;
        let reader = std::io::BufReader::new(file);
        let mut map = BTreeMap::new();
        for line in reader.lines() {
            let row: SweepRow = serde_json::from_str(&line?)?;
            map.insert(row.id.clone(), row);
        }
        Ok(map)
    };

    let a = load(a_path)?;
    let b = load(b_path)?;

    // Columns: both decoded + hash-exact agreement | both decoded but CFA differs (a real
    // correctness concern -- worth a `diff` follow-up, see #37's write-up on the Lossless ±1 LSB
    // rounding finding) | both failed (agreement that this id is unsupported by both, not a
    // concern) | exactly one side decoded (informational -- expected for HE/HE* rows once one
    // decoder is rawler, since rawler is known to reject those; only worth flagging when it's
    // the *unexpected* side that failed) | present in a but missing from b's output entirely.
    #[derive(Default)]
    struct BucketStats {
        both_ok_match: usize,
        both_ok_diff: usize,
        both_failed: usize,
        one_sided: usize,
        missing_in_b: usize,
    }
    let mut per_bucket: BTreeMap<String, BucketStats> = BTreeMap::new();
    for (id, row_a) in &a {
        let stats = per_bucket.entry(row_a.bucket.clone()).or_default();
        match b.get(id) {
            None => stats.missing_in_b += 1,
            Some(row_b) => match (&row_a.frame, &row_b.frame) {
                (Some(fa), Some(fb)) if fa.cfa_hash == fb.cfa_hash => stats.both_ok_match += 1,
                (Some(_), Some(_)) => {
                    stats.both_ok_diff += 1;
                    println!("CFA DIFFERS {id}: both decoded ok but hashes disagree");
                }
                (None, None) => stats.both_failed += 1,
                _ => {
                    stats.one_sided += 1;
                    println!(
                        "ONE-SIDED {id}: a.ok={} ({:?}) b.ok={} ({:?})",
                        row_a.ok, row_a.error, row_b.ok, row_b.error
                    );
                }
            },
        }
    }

    println!("bucket | both-match | both-differ | both-failed | one-sided | missing-in-b");
    for (bucket, s) in per_bucket {
        println!(
            "{bucket} | {} | {} | {} | {} | {}",
            s.both_ok_match, s.both_ok_diff, s.both_failed, s.one_sided, s.missing_in_b
        );
    }
    Ok(())
}

fn bench(
    root: &Path,
    manifest_path: &Path,
    decoder: Decoder,
    sample_per_bucket: usize,
) -> anyhow::Result<()> {
    let manifest = Manifest::load(manifest_path)?;
    let protocol = Protocol::default();

    let mut buckets: std::collections::BTreeMap<String, Vec<&nicti_prowl::manifest::Entry>> =
        Default::default();
    for entry in manifest.entries() {
        buckets
            .entry(entry.compression.clone())
            .or_default()
            .push(entry);
    }

    for (label, entries) in buckets {
        let sample: Vec<_> = entries.into_iter().take(sample_per_bucket).collect();
        if sample.is_empty() {
            continue;
        }
        // Load bytes up front -- benchmark decode time, not disk I/O (sniff already measured
        // I/O separately for #28/#29; retina isn't re-measuring that here).
        let files: Vec<Vec<u8>> = sample
            .iter()
            .filter_map(|e| fs::read(root.join(&e.id)).ok())
            .collect();
        if files.is_empty() {
            eprintln!("bucket {label}: no readable files, skipping");
            continue;
        }

        // Caught by CodeRabbit: discarding decode_one's Result meant a bucket where every file
        // fails (e.g. rawler against an all-HE bucket, which rejects fast via a metadata check
        // rather than doing real decode work) would still print a latency number -- a fast,
        // meaningless one, since nothing was actually decoded. Track failures and skip reporting
        // for a bucket where none of the sampled files decoded.
        let mut idx = 0usize;
        let mut failures = 0usize;
        let stats = protocol.run(|| {
            let data = &files[idx % files.len()];
            idx += 1;
            if decode_one(decoder, data).is_err() {
                failures += 1;
            }
        });

        let total_runs = protocol.warmup + protocol.measured;
        if failures >= total_runs {
            eprintln!(
                "bucket {label} [{decoder:?}]: every sampled decode failed, skipping latency report (not a real measurement)"
            );
            continue;
        }
        if failures > 0 {
            eprintln!(
                "bucket {label} [{decoder:?}]: {failures}/{total_runs} sampled decodes failed -- latency below includes only the failed-fast calls too, treat with caution"
            );
        }

        println!(
            "{label} [{decoder:?}] n={} p50={:.2}ms p95={:.2}ms max={:.2}ms",
            files.len(),
            stats.p50_ms,
            stats.p95_ms,
            stats.max_ms
        );
    }
    Ok(())
}
