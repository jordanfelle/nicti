//! CLI for the `crouch` spike (#54/ADR-0054). Three subcommands:
//! - `bench-wgpu`: measures foreground `busy.wgsl` dispatch latency alone, then under a
//!   continuous background `busy.wgsl` load at a chosen chunk size (wgpu-vs-wgpu contention).
//! - `bench-ort`: same measurement, but the background load is real SCUNet-tile `ort` inference
//!   (cross-API contention) instead of a second wgpu kernel.
//! - `sim`: runs the tile-granular hero-scenario bake-queue simulation.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use crouch::gpu_contend::{BackgroundLoad, BusyKernel, GpuContext};
use crouch::ort_contend::{BackgroundOrtLoad, ExecutionProviderKind, TileLoad};
use crouch::sim::{simulate_hero_bake_chunked, ChunkedBakeCost};
use nicti_prowl::perf::{write_report, HardwareIdentity, Protocol, RunReport, Stats};

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
    },
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
        ),
    }
}

fn first_gpu_context() -> anyhow::Result<GpuContext> {
    GpuContext::enumerate()
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no wgpu adapter found"))
}

fn write_and_print(out_dir: &PathBuf, name: &str, stats: Stats) -> anyhow::Result<()> {
    let report = RunReport {
        name: name.to_string(),
        hardware: HardwareIdentity::capture(),
        protocol: Protocol::default(),
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
    write_and_print(&out_dir, "crouch-wgpu-foreground-alone", alone)?;

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
    write_and_print(&out_dir, name, under_contention)
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
    write_and_print(&out_dir, "crouch-ort-foreground-alone", alone)?;

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
        under_contention,
    )
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
) -> anyhow::Result<()> {
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
    let result = simulate_hero_bake_chunked(
        n_images,
        cursor_start,
        Duration::from_millis(walk_pace_ms),
        cost,
        Duration::from_millis(foreground_interval_ms),
        Duration::from_millis(foreground_cost_ms),
    );
    println!(
        "first_image_ready={:?} total_wall_time={:?} stale_at_arrival={}/{}",
        result.first_image_ready, result.total_wall_time, result.stale_at_arrival, n_images
    );
    println!(
        "worst_case_atomic_unit={:?} foreground_worst_latency={:?} (over {} serviced requests)",
        cost.worst_case_atomic_unit(),
        result.foreground_worst_latency(),
        result.foreground_latencies.len()
    );
    Ok(())
}
