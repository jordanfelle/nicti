//! CLI for the `crouch` spike (#54/ADR-0054). Four subcommands:
//! - `bench-wgpu`: measures foreground `busy.wgsl` dispatch latency alone, then under a
//!   continuous background `busy.wgsl` load at a chosen chunk size (wgpu-vs-wgpu contention).
//! - `bench-ort`: same measurement, but the background load is real SCUNet-tile `ort` inference
//!   (cross-API contention) instead of a second wgpu kernel.
//! - `bench-tile`: isolated per-tile SCUNet inference timing at one or more tile sizes (#205) --
//!   no wgpu contention, just how long one chunk itself costs, plus the estimated whole-frame
//!   cost that chunk size implies.
//! - `sim`: runs the tile-granular hero-scenario bake-queue simulation.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use crouch::gpu_contend::{BackgroundLoad, BusyKernel, GpuContext};
use crouch::ort_contend::{tiles_for_frame, BackgroundOrtLoad, ExecutionProviderKind, TileLoad};
use crouch::sim::{simulate_hero_bake_chunked, simulate_hero_bake_two_lane, ChunkedBakeCost};
use nicti_prowl::perf::{write_report, HardwareIdentity, Protocol, RunReport, Stats};

/// #205's own decision rule (ADR-0054's "well under ~16ms" same-API contention budget): a chunk
/// clears with real headroom only below this, not merely under the raw 16.7ms slider-drag budget
/// itself.
const CLEARS_WITH_HEADROOM_MS: f64 = 12.0;
/// Above this, a chunk of this size doesn't fit the same-API contention budget at all.
const DOES_NOT_CLEAR_MS: f64 = 16.0;

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// wgpu-vs-wgpu contention: foreground busy-kernel latency alone, then under a background
    /// busy-kernel load at `bg_iterations` per chunk.
    BenchWgpu {
        #[arg(long, default_value_t = 4096)]
        elements: u32,
        #[arg(long, default_value_t = 8)]
        fg_iterations: u32,
        #[arg(long, default_value_t = 200)]
        bg_iterations: u32,
        #[arg(long, default_value_t = 500)]
        bg_warmup_ms: u64,
        /// Fire-and-forget, no backpressure -- a deliberate stress test (see
        /// `gpu_contend::BackgroundLoad`'s doc comment), NOT the realistic scheduler
        /// simulation. Confirmed to crash the GPU device (Windows TDR) at large enough chunk
        /// sizes on the reference RTX 5080 -- don't run this unattended.
        #[arg(long, default_value_t = false)]
        unthrottled: bool,
        #[arg(long, default_value = "bench-results")]
        out_dir: PathBuf,
    },
    /// wgpu-vs-ort/CUDA contention: foreground busy-kernel latency alone, then under a background
    /// SCUNet-tile inference load.
    BenchOrt {
        #[arg(long)]
        model_path: PathBuf,
        #[arg(long)]
        ort_dylib_path: PathBuf,
        #[arg(long, value_enum, default_value_t = ExecutionProviderKind::Cuda)]
        ep: ExecutionProviderKind,
        #[arg(long, default_value_t = 256)]
        tile_size: u32,
        #[arg(long, default_value_t = 4096)]
        elements: u32,
        #[arg(long, default_value_t = 8)]
        fg_iterations: u32,
        #[arg(long, default_value_t = 500)]
        bg_warmup_ms: u64,
        #[arg(long, default_value = "bench-results")]
        out_dir: PathBuf,
    },
    /// #205: isolated per-tile SCUNet inference timing at one or more tile sizes, no wgpu
    /// contention -- just the chunk cost itself against ADR-0054's same-API contention budget,
    /// plus the estimated whole-frame cost that chunk size implies.
    BenchTile {
        #[arg(long)]
        model_path: PathBuf,
        #[arg(long)]
        ort_dylib_path: PathBuf,
        #[arg(long, value_enum, default_value_t = ExecutionProviderKind::Cuda)]
        ep: ExecutionProviderKind,
        #[arg(long, default_values_t = [128u32, 256u32])]
        tile_size: Vec<u32>,
        #[arg(long, default_value_t = 32)]
        overlap: u32,
        #[arg(long, default_value_t = 5)]
        warmup: usize,
        #[arg(long, default_value_t = 50, value_parser = parse_nonzero_measured)]
        measured: usize,
        /// Frame dimensions used only for the estimated whole-frame cost -- ADR-0040's own real
        /// full-resolution frame by default.
        #[arg(long, default_value_t = 6064)]
        frame_width: u32,
        #[arg(long, default_value_t = 4040)]
        frame_height: u32,
        #[arg(long, default_value = "bench-results")]
        out_dir: PathBuf,
    },
    /// Run the tile-granular hero-scenario bake-queue simulation.
    Sim {
        #[arg(long, default_value_t = 50)]
        n_images: usize,
        #[arg(long, default_value_t = 0)]
        cursor_start: usize,
        #[arg(long, default_value_t = 100)]
        walk_pace_ms: u64,
        #[arg(long, default_value_t = 1700)]
        decode_ms: u64,
        #[arg(long, default_value_t = 50_900)]
        denoise_total_ms: u64,
        #[arg(long, default_value_t = 45)]
        denoise_chunk_ms: u64,
        #[arg(long, default_value_t = 1000)]
        mask_bake_ms: u64,
        /// How often (ms) a foreground request becomes due during the sync -- 0 disables
        /// foreground demand.
        #[arg(long, default_value_t = 0)]
        foreground_interval_ms: u64,
        #[arg(long, default_value_t = 0)]
        foreground_cost_ms: u64,
        /// `one`: ADR-0054's original model, every stage on one worker timeline. `two`: #206's
        /// model of #55's real runtime, decode on its own CPU lane concurrent with the GPU lane.
        #[arg(long, value_enum, default_value_t = Lanes::One)]
        lanes: Lanes,
        /// Parallel decode slots on the CPU lane (`--lanes two` only).
        #[arg(long, default_value_t = 1)]
        cpu_decode_threads: usize,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Lanes {
    One,
    Two,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::BenchWgpu {
            elements,
            fg_iterations,
            bg_iterations,
            bg_warmup_ms,
            unthrottled,
            out_dir,
        } => bench_wgpu(
            elements,
            fg_iterations,
            bg_iterations,
            bg_warmup_ms,
            unthrottled,
            out_dir,
        ),
        Command::BenchOrt {
            model_path,
            ort_dylib_path,
            ep,
            tile_size,
            elements,
            fg_iterations,
            bg_warmup_ms,
            out_dir,
        } => bench_ort(
            &model_path,
            &ort_dylib_path,
            ep,
            tile_size,
            elements,
            fg_iterations,
            bg_warmup_ms,
            out_dir,
        ),
        Command::BenchTile {
            model_path,
            ort_dylib_path,
            ep,
            tile_size,
            overlap,
            warmup,
            measured,
            frame_width,
            frame_height,
            out_dir,
        } => bench_tile(
            &model_path,
            &ort_dylib_path,
            ep,
            &tile_size,
            overlap,
            warmup,
            measured,
            frame_width,
            frame_height,
            out_dir,
        ),
        Command::Sim {
            n_images,
            cursor_start,
            walk_pace_ms,
            decode_ms,
            denoise_total_ms,
            denoise_chunk_ms,
            mask_bake_ms,
            foreground_interval_ms,
            foreground_cost_ms,
            lanes,
            cpu_decode_threads,
        } => run_sim(
            n_images,
            cursor_start,
            walk_pace_ms,
            decode_ms,
            denoise_total_ms,
            denoise_chunk_ms,
            mask_bake_ms,
            foreground_interval_ms,
            foreground_cost_ms,
            lanes,
            cpu_decode_threads,
        ),
    }
}

/// `--measured 0` would otherwise pass clap's parsing cleanly, then produce an all-zero `Stats`
/// (`Stats::from_samples` on an empty sample vec) -- a false report, including a false "CLEARS
/// with headroom" verdict, rather than a rejected argument.
fn parse_nonzero_measured(s: &str) -> Result<usize, String> {
    let n: usize = s.parse().map_err(|e| format!("{e}"))?;
    if n == 0 {
        return Err(
            "--measured must be at least 1 (0 measured samples always reports a false \
                     0.0ms/0.0ms/0.0ms Stats, not a real measurement)"
                .to_string(),
        );
    }
    Ok(n)
}

fn first_gpu_context() -> anyhow::Result<GpuContext> {
    GpuContext::enumerate()
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no wgpu adapter found"))
}

fn write_and_print(
    out_dir: &PathBuf,
    name: &str,
    protocol: Protocol,
    stats: Stats,
) -> anyhow::Result<()> {
    let report = RunReport {
        name: name.to_string(),
        hardware: HardwareIdentity::capture(),
        protocol,
        stats: stats.clone(),
        unix_time: nicti_prowl::perf::now_unix(),
    };
    let path = write_report(out_dir, &report)?;
    println!(
        "{name}: p50={:.3}ms p95={:.3}ms max={:.3}ms -> {}",
        stats.p50_ms,
        stats.p95_ms,
        stats.max_ms,
        path.display()
    );
    Ok(())
}

fn bench_wgpu(
    elements: u32,
    fg_iterations: u32,
    bg_iterations: u32,
    bg_warmup_ms: u64,
    unthrottled: bool,
    out_dir: PathBuf,
) -> anyhow::Result<()> {
    let ctx = first_gpu_context()?;
    println!("adapter: {} ({:?})", ctx.adapter_name, ctx.backend);
    if unthrottled {
        println!(
            "WARNING: --unthrottled is a deliberate stress test with no backpressure -- this has \
             crashed the GPU device (Windows TDR) on the reference RTX 5080 at large chunk sizes."
        );
    }
    let ctx = Arc::new(ctx);
    let kernel = Arc::new(BusyKernel::new(&ctx, elements));
    let protocol = Protocol::default();

    let alone = protocol.run(|| {
        kernel.dispatch_and_wait(&ctx, fg_iterations);
    });
    write_and_print(&out_dir, "crouch-wgpu-foreground-alone", protocol, alone)?;

    let load = if unthrottled {
        BackgroundLoad::start_unthrottled(ctx.clone(), kernel.clone(), bg_iterations)
    } else {
        BackgroundLoad::start(ctx.clone(), kernel.clone(), bg_iterations)
    };
    std::thread::sleep(Duration::from_millis(bg_warmup_ms));
    let under_contention = protocol.run(|| {
        kernel.dispatch_and_wait(&ctx, fg_iterations);
    });
    let chunks = load.stop();
    println!("background chunks submitted during measurement window: {chunks}");
    let name = if unthrottled {
        "crouch-wgpu-foreground-under-wgpu-background-unthrottled"
    } else {
        "crouch-wgpu-foreground-under-wgpu-background"
    };
    write_and_print(&out_dir, name, protocol, under_contention)
}

#[allow(clippy::too_many_arguments)]
fn bench_ort(
    model_path: &std::path::Path,
    ort_dylib_path: &std::path::Path,
    ep: ExecutionProviderKind,
    tile_size: u32,
    elements: u32,
    fg_iterations: u32,
    bg_warmup_ms: u64,
    out_dir: PathBuf,
) -> anyhow::Result<()> {
    let ctx = first_gpu_context()?;
    println!("adapter: {} ({:?})", ctx.adapter_name, ctx.backend);
    let kernel = BusyKernel::new(&ctx, elements);
    let protocol = Protocol::default();

    let alone = protocol.run(|| {
        kernel.dispatch_and_wait(&ctx, fg_iterations);
    });
    write_and_print(&out_dir, "crouch-ort-foreground-alone", protocol, alone)?;

    let tile_load = TileLoad::load(model_path, ort_dylib_path, ep, tile_size)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let load = BackgroundOrtLoad::start(tile_load);
    std::thread::sleep(Duration::from_millis(bg_warmup_ms));
    let under_contention = protocol.run(|| {
        kernel.dispatch_and_wait(&ctx, fg_iterations);
    });
    let chunks = load.stop().map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("background ort tiles run during measurement window: {chunks}");
    write_and_print(
        &out_dir,
        &format!("crouch-ort-foreground-under-{ep:?}-background"),
        protocol,
        under_contention,
    )
}

/// A chunk that would keep foreground responsiveness under ADR-0054's same-API contention rule
/// ("well under ~16ms"), reported against #205's own tightened decision rule.
fn verdict(p95_ms: f64) -> &'static str {
    if p95_ms <= CLEARS_WITH_HEADROOM_MS {
        "CLEARS with headroom"
    } else if p95_ms <= DOES_NOT_CLEAR_MS {
        "MARGINAL"
    } else {
        "DOES NOT CLEAR"
    }
}

/// Rejects every tile size up front, before loading any `ort` session -- a bad size only
/// surfaces otherwise once `tiles_for_frame` runs (an assertion panic) or once `TileLoad` feeds a
/// zero-dimension tensor into `ort` (an inference error `try_run` would otherwise only catch
/// after already loading/warming up a session for it).
fn validate_tile_sizes(tile_sizes: &[u32], overlap: u32) -> anyhow::Result<()> {
    for &tile_size in tile_sizes {
        anyhow::ensure!(
            tile_size > 0,
            "--tile-size must be nonzero, got {tile_size}"
        );
        anyhow::ensure!(
            overlap < tile_size,
            "--overlap ({overlap}) must be strictly less than --tile-size ({tile_size})"
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn bench_tile(
    model_path: &std::path::Path,
    ort_dylib_path: &std::path::Path,
    ep: ExecutionProviderKind,
    tile_sizes: &[u32],
    overlap: u32,
    warmup: usize,
    measured: usize,
    frame_width: u32,
    frame_height: u32,
    out_dir: PathBuf,
) -> anyhow::Result<()> {
    validate_tile_sizes(tile_sizes, overlap)?;

    let protocol = Protocol { warmup, measured };

    for &tile_size in tile_sizes {
        let mut tile_load = TileLoad::load(model_path, ort_dylib_path, ep, tile_size)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let stats = protocol
            .try_run(|| tile_load.run_one())
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let tile_count = tiles_for_frame(frame_width, frame_height, tile_size, overlap);
        let estimated_frame_ms = stats.p50_ms * tile_count as f64;

        write_and_print(
            &out_dir,
            &format!("crouch-tile-{tile_size}px-{ep:?}"),
            protocol,
            stats.clone(),
        )?;
        println!(
            "  tile={tile_size}px overlap={overlap} ep={ep:?}: {tile_count} tiles/frame \
             ({frame_width}x{frame_height}) -> estimated {estimated_frame_ms:.0}ms/frame -- {}",
            verdict(stats.p95_ms)
        );
    }

    println!(
        "(no CPU-EP reference in this run -- rerun with --ep cpu on one tile size and compare \
         by hand via ort_contend::suspiciously_close_to_cpu_speed's ~2x-speedup rule, matching \
         ADR-0040/0054's own silent-fallback check)"
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_sim(
    n_images: usize,
    cursor_start: usize,
    walk_pace_ms: u64,
    decode_ms: u64,
    denoise_total_ms: u64,
    denoise_chunk_ms: u64,
    mask_bake_ms: u64,
    foreground_interval_ms: u64,
    foreground_cost_ms: u64,
    lanes: Lanes,
    cpu_decode_threads: usize,
) -> anyhow::Result<()> {
    if matches!(lanes, Lanes::One) && cpu_decode_threads != 1 {
        anyhow::bail!("--cpu-decode-threads only applies to --lanes two");
    }
    if matches!(lanes, Lanes::Two) && cpu_decode_threads == 0 {
        anyhow::bail!("--cpu-decode-threads must be at least 1, or no decode ever finishes");
    }
    if foreground_interval_ms != 0 && foreground_cost_ms >= foreground_interval_ms {
        anyhow::bail!(
            "--foreground-cost-ms ({foreground_cost_ms}) must be strictly less than \
             --foreground-interval-ms ({foreground_interval_ms}), or the sim's due-time backlog \
             never clears and it never terminates"
        );
    }
    let cost = ChunkedBakeCost {
        decode: Duration::from_millis(decode_ms),
        denoise_total: Duration::from_millis(denoise_total_ms),
        denoise_chunk: Duration::from_millis(denoise_chunk_ms),
        mask_bake: Duration::from_millis(mask_bake_ms),
    };
    let walk_pace = Duration::from_millis(walk_pace_ms);
    let foreground_interval = Duration::from_millis(foreground_interval_ms);
    let foreground_cost = Duration::from_millis(foreground_cost_ms);
    let (result, bound_label, bound) = match lanes {
        Lanes::One => (
            simulate_hero_bake_chunked(
                n_images,
                cursor_start,
                walk_pace,
                cost,
                foreground_interval,
                foreground_cost,
            ),
            "worst_case_atomic_unit",
            cost.worst_case_atomic_unit(),
        ),
        Lanes::Two => (
            simulate_hero_bake_two_lane(
                n_images,
                cursor_start,
                walk_pace,
                cost,
                cpu_decode_threads,
                foreground_interval,
                foreground_cost,
            ),
            "worst_case_gpu_atomic_unit",
            cost.worst_case_gpu_atomic_unit(),
        ),
    };
    println!(
        "first_image_ready={:?} total_wall_time={:?} stale_at_arrival={}/{}",
        result.first_image_ready, result.total_wall_time, result.stale_at_arrival, n_images
    );
    println!(
        "{bound_label}={bound:?} foreground_worst_latency={:?} (over {} serviced requests)",
        result.foreground_worst_latency(),
        result.foreground_latencies.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{parse_nonzero_measured, validate_tile_sizes};

    #[test]
    fn zero_measured_is_rejected() {
        assert!(parse_nonzero_measured("0").is_err());
    }

    #[test]
    fn positive_measured_is_accepted() {
        assert_eq!(parse_nonzero_measured("50"), Ok(50));
    }

    #[test]
    fn non_numeric_measured_is_rejected() {
        assert!(parse_nonzero_measured("not-a-number").is_err());
    }

    #[test]
    fn zero_tile_size_is_rejected() {
        assert!(validate_tile_sizes(&[128, 0], 32).is_err());
    }

    #[test]
    fn overlap_equal_to_tile_size_is_rejected() {
        assert!(validate_tile_sizes(&[64], 64).is_err());
    }

    #[test]
    fn overlap_greater_than_tile_size_is_rejected() {
        assert!(validate_tile_sizes(&[64], 128).is_err());
    }

    #[test]
    fn valid_tile_sizes_are_accepted() {
        assert!(validate_tile_sizes(&[128, 256], 32).is_ok());
    }
}
