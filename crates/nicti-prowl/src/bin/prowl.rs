//! `prowl` -- the CLI front-end for `nicti-prowl`'s manifest verification and reference-set
//! selection, so PowerShell scripts on any benchmark machine can call one binary instead of
//! re-implementing SHA-256 checking themselves (see `bench/run-hero.ps1`, which points at
//! `prowl verify --ids` in place of its own inline `Get-FileHash` loop).

#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use nicti_prowl::manifest::{Manifest, Scope};
use nicti_prowl::materialize::{self, Mode};
use nicti_prowl::refset;

#[derive(Parser)]
#[command(
    name = "prowl",
    about = "ref-10k manifest verification and reference-set selection"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Verify local ref-10k files against docs/ref-10k-manifest.csv's sha256 column.
    Verify(VerifyArgs),
    /// Pick `n` reference files from one bucket.
    Select(SelectArgs),
    /// Rebuild a ref-10k copy from a live photo library by matching files on sha256 (#136).
    Materialize(MaterializeArgs),
    /// Compare benchmark CPU time against history.jsonl and fail on a regression (#538).
    BenchGate(BenchGateArgs),
}

#[derive(Args)]
struct BenchGateArgs {
    /// The JSON-lines history of earlier runs (created on first `--append`).
    #[arg(long, default_value = "bench-results/history.jsonl")]
    history: PathBuf,
    /// A `RunReport` JSON written by `perf::write_report` (repeatable).
    #[arg(long = "report", required = true)]
    reports: Vec<PathBuf>,
    /// Regression threshold as a fraction (0.20 = more than 20 % over the baseline).
    #[arg(long, default_value_t = 0.20)]
    threshold: f64,
    /// How many of the latest history entries make the baseline (median).
    #[arg(long, default_value_t = 5)]
    window: usize,
    /// Benchmarks with fewer history entries than this are recorded but not gated.
    #[arg(long, default_value_t = 3)]
    min_history: usize,
    /// Baselines under this many ms are too small to gate.
    #[arg(long, default_value_t = 100.0)]
    noise_floor_ms: f64,
    /// Commit id to record with the new entries.
    #[arg(long)]
    commit: Option<String>,
    /// Append the new entries to the history, but only when nothing regressed.
    #[arg(long)]
    append: bool,
}

#[derive(Args)]
struct MaterializeArgs {
    #[arg(long, default_value = "docs/ref-10k-manifest.csv")]
    manifest: PathBuf,
    /// The photo library to search (read-only; symlinks are not followed).
    #[arg(long)]
    source: PathBuf,
    /// Where to put `ref-NNNNN.*`. Falls back to the NICTI_REF10K env var.
    #[arg(long)]
    dest: Option<PathBuf>,
    /// Only this manifest bucket (e.g. "z8").
    #[arg(long)]
    bucket: Option<String>,
    /// Only the first N matching manifest entries.
    #[arg(long)]
    limit: Option<usize>,
    /// Hard-link instead of copying (same volume only). The result aliases the library file: if the
    /// library app later rewrites that file in place (metadata embed, re-save), the ref-10k copy
    /// changes with it and `prowl verify` will fail. Use the default copy for a stable baseline.
    #[arg(long)]
    hardlink: bool,
    /// Report what would be placed without writing anything.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Args)]
struct VerifyArgs {
    /// Path to the manifest CSV.
    #[arg(long, default_value = "docs/ref-10k-manifest.csv")]
    manifest: PathBuf,
    /// Root directory holding the ref-10k files. Falls back to the NICTI_REF10K env var.
    #[arg(long)]
    root: Option<PathBuf>,
    /// Verify every entry in the manifest (default if no other scope flag is given).
    #[arg(long, conflicts_with_all = ["ids", "sample"])]
    all: bool,
    /// Verify exactly these ids (repeatable, or comma-separated).
    #[arg(long, value_delimiter = ',', conflicts_with = "sample")]
    ids: Vec<String>,
    /// Verify a random sample of this many entries instead of the whole set.
    #[arg(long)]
    sample: Option<usize>,
    /// Seed for --sample's selection.
    #[arg(long, default_value_t = 43)]
    seed: u64,
}

#[derive(Args)]
struct SelectArgs {
    #[arg(long, default_value = "docs/ref-10k-manifest.csv")]
    manifest: PathBuf,
    /// Manifest bucket to select from (e.g. "z8").
    #[arg(long)]
    bucket: String,
    /// Number of files to select.
    #[arg(short = 'n', long)]
    count: usize,
    #[arg(long, default_value_t = 43)]
    seed: u64,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Verify(args) => run_verify(args),
        Command::Select(args) => run_select(args),
        Command::Materialize(args) => run_materialize(args),
        // Exit 1 is reserved for "regressed"; anything that stops the gate from judging at all
        // (corrupt history, report without CPU time, unreadable file) exits 2, so a CI script can
        // tell a slow build from a broken gate.
        Command::BenchGate(args) => {
            if let Err(e) = run_bench_gate(args) {
                eprintln!("bench-gate: {e:#}");
                std::process::exit(2);
            }
            Ok(())
        }
    }
}

fn run_verify(args: VerifyArgs) -> anyhow::Result<()> {
    let manifest = Manifest::load(&args.manifest)?;
    let root = refset::resolve_root(args.root.as_deref())?;

    // `--ids ""` (e.g. a caller joining an empty id list, as bench/run-hero.ps1's `-join ","`
    // would on a zero-entry hero set) splits to `[""]`, not `[]` -- clap's `value_delimiter`
    // never produces zero elements from a non-empty string. `ids_given` is decided from the raw
    // arg (before filtering), so an explicit-but-empty `--ids` still selects Scope::Ids(vec![])
    // (trivially clean, verifies nothing) rather than silently falling through to Scope::All and
    // verifying the entire ~9k-file manifest instead of the zero files actually requested.
    let ids_given = !args.ids.is_empty();
    let ids: Vec<String> = args
        .ids
        .into_iter()
        .filter(|id| !id.trim().is_empty())
        .collect();

    let scope = if let Some(n) = args.sample {
        Scope::Sample { n, seed: args.seed }
    } else if ids_given {
        Scope::Ids(ids)
    } else {
        Scope::All
    };

    let report = manifest.verify(&root, scope);
    println!(
        "checked {} ok={} missing={} mismatched={} unknown={}",
        report.checked(),
        report.ok,
        report.missing.len(),
        report.mismatched.len(),
        report.unknown.len(),
    );
    for id in &report.missing {
        eprintln!("MISSING {id}");
    }
    for m in &report.mismatched {
        eprintln!(
            "MISMATCH {} expected={} actual={}",
            m.id, m.expected, m.actual
        );
    }
    for id in &report.unknown {
        eprintln!("UNKNOWN {id} (not in manifest)");
    }

    report.into_result().map(|_| ())
}

fn run_select(args: SelectArgs) -> anyhow::Result<()> {
    let manifest = Manifest::load(&args.manifest)?;
    let selected = refset::select(&manifest, &args.bucket, args.count, args.seed)?;
    for entry in selected {
        println!("{}", entry.id);
    }
    Ok(())
}

fn run_materialize(args: MaterializeArgs) -> anyhow::Result<()> {
    let manifest = Manifest::load(&args.manifest)?;
    let dest = refset::resolve_root(args.dest.as_deref())?;
    let report = materialize::materialize(
        &manifest,
        &materialize::Options {
            source: &args.source,
            dest: &dest,
            mode: if args.hardlink {
                Mode::Hardlink
            } else {
                Mode::Copy
            },
            bucket: args.bucket.as_deref(),
            limit: args.limit,
            dry_run: args.dry_run,
        },
    )?;
    println!(
        "{} placed={} already_present={} not_found={} failed={} unreadable={}",
        if args.dry_run {
            "dry-run"
        } else {
            "materialized"
        },
        report.placed.len(),
        report.already_present.len(),
        report.not_found.len(),
        report.failed.len(),
        report.unreadable.len(),
    );
    for id in &report.not_found {
        eprintln!("NOT FOUND {id}");
    }
    for (id, why) in &report.failed {
        eprintln!("FAILED {id}: {why}");
    }
    for path in &report.unreadable {
        eprintln!("UNREADABLE {}", path.display());
    }
    if !report.unreadable.is_empty() && !report.not_found.is_empty() {
        eprintln!(
            "note: {} source path(s) could not be read, so NOT FOUND may mean unreadable, not absent",
            report.unreadable.len()
        );
    }
    if report.is_complete() {
        Ok(())
    } else {
        anyhow::bail!("ref-10k is incomplete: the source library is missing some manifest files")
    }
}

fn run_bench_gate(args: BenchGateArgs) -> anyhow::Result<()> {
    use nicti_prowl::gate::{self, Policy};
    anyhow::ensure!(
        args.threshold.is_finite() && args.threshold >= 0.0,
        "--threshold must be a non-negative number"
    );
    let history = gate::read_history(&args.history)?;
    let mut current: Vec<gate::Entry> = Vec::new();
    for path in &args.reports {
        let json = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
        let entry = gate::entry_from_report(&json, args.commit.as_deref())
            .map_err(|e| anyhow::anyhow!("{}: {e:#}", path.display()))?;
        // Two reports of one benchmark would be judged against the same history and both
        // appended, double-weighting one run in every later median.
        anyhow::ensure!(
            !current.iter().any(|c| c.bench == entry.bench),
            "{}: a second report for benchmark `{}`",
            path.display(),
            entry.bench
        );
        current.push(entry);
    }
    let policy = Policy {
        threshold: args.threshold,
        window: args.window.max(1),
        min_history: args.min_history,
        noise_floor_ms: args.noise_floor_ms,
    };
    let verdicts = gate::evaluate(&history, &current, &policy);
    for v in &verdicts {
        println!("{}", v.line());
    }
    let regressed = verdicts.iter().filter(|v| v.is_regression()).count();
    if regressed > 0 {
        eprintln!(
            "{regressed} benchmark(s) regressed more than {:.0} % in CPU time",
            policy.threshold * 100.0
        );
        // History is left alone: a regression must not become the next baseline.
        std::process::exit(1);
    }
    if args.append {
        gate::append_history(&args.history, &current)?;
        println!(
            "appended {} entr(ies) to {}",
            current.len(),
            args.history.display()
        );
    }
    Ok(())
}
