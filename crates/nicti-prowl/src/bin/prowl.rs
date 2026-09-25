//! `prowl` -- the CLI front-end for `nicti-prowl`'s manifest verification and reference-set
//! selection, so PowerShell scripts on the reference machine (and anyone else) can call one
//! binary instead of re-implementing SHA-256 checking themselves (see `bench/run-hero.ps1`,
//! which this PR points at `prowl verify --ids` in place of its own inline `Get-FileHash` loop).

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use nicti_prowl::manifest::{Manifest, Scope};
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
    #[arg(long)]
    all: bool,
    /// Verify exactly these ids (repeatable, or comma-separated).
    #[arg(long, value_delimiter = ',')]
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
