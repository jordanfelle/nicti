mod bench;
mod cache;
mod codec;
mod decode;
mod ifd;
mod inventory;
mod jpeg_meta;
mod source;
mod tier_bench;

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
        /// Whole-file read (the original pessimistic-bound path) vs. ranged/positioned reads
        /// (#29's seek-and-read implementation). See `bench::IoMode`.
        #[arg(long, value_enum, default_value = "whole")]
        io: bench::IoMode,
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
    /// #29's per-tier cache-format + payload-format comparison. `--tier t0-grid` reads
    /// `nikon_preview_ifd` verbatim (`--codec`/`--quality` ignored). `--tier t2-screen` extracts
    /// `JpgFromRaw`, resizes to `decode::SCREEN_TIER_LONG_EDGE`, and re-encodes with `--codec` at
    /// `--quality`. Either way, populates `--cache-format` then reads back in random order.
    /// Prints one JSON `TierBenchResult`.
    TierBench {
        /// One or more roots to draw the sample from (each is walked non-recursively). Accepts
        /// multiple values so #143's stratified-subset folders can be passed without making the
        /// walk itself recursive: `--root a --root b`.
        #[arg(long = "root", required = true)]
        roots: Vec<PathBuf>,
        #[arg(long, value_enum, default_value = "t2-screen")]
        tier: tier_bench::Tier,
        #[arg(long, value_enum, default_value = "jpeg")]
        codec: codec::Codec,
        #[arg(long, default_value_t = 80)]
        quality: u8,
        /// AVIF-only: ravif's 1 (fastest/largest) - 10 (slowest/smallest) encode-effort dial.
        /// Ignored by JPEG/WebP. #143's sweep axis -- see `codec.rs`'s module doc.
        #[arg(long, default_value_t = 6)]
        avif_speed: u8,
        #[arg(long, value_enum, default_value = "sqlite")]
        cache_format: cache::Format,
        #[arg(long)]
        sample_limit: Option<usize>,
        /// Also decode each encoded payload right after its (untimed) encode and score it
        /// against the pre-encode resized source with `nicti_prowl::golden::ssim`, so the sweep
        /// can tell "smaller/faster" apart from "smaller/faster because it also looks worse".
        #[arg(long, default_value_t = false)]
        ssim: bool,
        #[arg(long, default_value = "bench-results/sniff-tier-bench")]
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
            io,
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
            io,
            threads,
            order,
            cold,
            warmups,
            runs,
            sample_limit,
            &out_dir,
        ),
        Command::TierBench {
            roots,
            tier,
            codec,
            quality,
            avif_speed,
            cache_format,
            sample_limit,
            ssim,
            out_dir,
        } => {
            let result = tier_bench::run(
                &roots,
                tier,
                codec,
                quality,
                avif_speed,
                cache_format,
                sample_limit,
                ssim,
                &out_dir,
            )
            .map_err(|e| std::io::Error::other(e.to_string()))?;
            serde_json::to_writer_pretty(std::io::stdout(), &result)
                .map_err(std::io::Error::other)?;
            println!();
            Ok(())
        }
    }
}
