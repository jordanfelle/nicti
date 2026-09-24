mod bench;
mod decode;
mod ifd;
mod inventory;
mod jpeg_meta;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "sniff",
    about = "Throwaway spike for #28: embedded-JPEG inventory and read/decode benchmarking across Nikon bodies (Z8/D7500/D3400) in the ref-10k reference set."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Inventory every embedded JPEG in `root`, cross-checked against `manifest.csv`, written to
    /// `out.csv`.
    Inventory {
        root: PathBuf,
        manifest: PathBuf,
        #[arg(long, default_value = "sniff-inventory.csv")]
        out: PathBuf,
        /// On a slow (HDD-like) root, only re-hash every Nth file against the manifest instead
        /// of hashing the whole set. 1 = hash every file.
        #[arg(long, default_value_t = 1)]
        hdd_sample_every: u32,
    },
    /// Benchmark locate/read/decode latency for embedded JPEGs under `root`.
    Bench {
        root: PathBuf,
        #[arg(long, value_enum)]
        mode: bench::Mode,
        #[arg(long, default_value_t = 1)]
        threads: usize,
        #[arg(long, value_enum, default_value = "manifest")]
        order: bench::Order,
        /// Request sector-aligned, cache-bypassing reads (Windows only; see bench.rs).
        #[arg(long, default_value_t = false)]
        cold: bool,
        #[arg(long, default_value_t = 1)]
        warmups: u32,
        #[arg(long, default_value_t = 5)]
        runs: u32,
        /// Cap the file set (e.g. for the full-read HDD baseline sample).
        #[arg(long)]
        sample_limit: Option<usize>,
        #[arg(long, default_value = "bench-results/sniff")]
        out_dir: PathBuf,
    },
}

fn main() -> std::io::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Inventory {
            root,
            manifest,
            out,
            hdd_sample_every,
        } => inventory::run(&root, &manifest, &out, hdd_sample_every),
        Command::Bench {
            root,
            mode,
            threads,
            order,
            cold,
            warmups,
            runs,
            sample_limit,
            out_dir,
        } => bench::run(
            &root,
            mode,
            threads,
            order,
            cold,
            warmups,
            runs,
            sample_limit,
            &out_dir,
        ),
    }
}
