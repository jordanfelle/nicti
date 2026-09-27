//! CLI for the `loaf` spike (#44/ADR-0044). Three subcommands:
//! - `graph`: builds the proposed stage graph and prints its topological order plus the
//!   invalidation-set proof (decision rule #1) as a sanity check outside the test suite.
//! - `bench`: runs the GPU kernels (live suffix, present/sample, box filter) via
//!   `nicti_prowl::perf::Protocol` and writes a `RunReport` to `bench-results/` -- this is what
//!   fills in ADR-0044's Measured results table once run on the reference machine.
//! - `sim`: runs the hero-scenario bake-queue simulation and prints/writes its result.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};
use loaf::{cost_model, geometry, gpu, graph, prefetch, sim};
use nicti_prowl::perf::{HardwareIdentity, Protocol, RunReport};

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the proposed stage graph's topological order and invalidation sets.
    Graph,
    /// Run GPU kernel benchmarks (live_suffix, present_sample, box_filter) and write a report.
    Bench {
        #[arg(long, default_value = "bench-results")]
        out_dir: PathBuf,
        /// Long edge in pixels for the synthetic frame used to benchmark live_suffix/present_sample.
        #[arg(long, default_value_t = 3840)]
        long_edge: u32,
    },
    /// Run the hero-scenario bake-queue simulation.
    Sim {
        #[arg(long, default_value_t = 50)]
        n_images: usize,
        #[arg(long, default_value_t = 0)]
        cursor_start: usize,
        #[arg(long, default_value_t = 100)]
        walk_pace_ms: u64,
        #[arg(long, default_value = "bench-results")]
        out_dir: PathBuf,
    },
}

fn build_hero_graph() -> graph::RenderGraph {
    let mut g = graph::RenderGraph::new();
    let baked = ["decode", "demosaic", "denoise", "lens", "heal"];
    let mut prev: Option<&str> = None;
    for id in baked {
        g.add_node(graph::StageNode {
            id: id.to_string(),
            kind: graph::StageKind::Baked,
            upstream: prev.map(|p| vec![p.to_string()]).unwrap_or_default(),
            own_hash: blake3::hash(id.as_bytes()),
        })
        .unwrap();
        prev = Some(id);
    }
    // The neutral-tone branch the AI mask bake reads from -- decoupled from live sliders per
    // ADR-0024's "AI model input is decoupled from tone sliders" decision.
    g.add_node(graph::StageNode {
        id: "neutral_render".to_string(),
        kind: graph::StageKind::Baked,
        upstream: vec![prev.unwrap().to_string()],
        own_hash: blake3::hash(b"neutral_render"),
    })
    .unwrap();
    g.add_node(graph::StageNode {
        id: "mask_bake".to_string(),
        kind: graph::StageKind::Baked,
        upstream: vec!["neutral_render".to_string()],
        own_hash: blake3::hash(b"mask_bake"),
    })
    .unwrap();
    let live = [
        "wb",
        "huesat",
        "exposure",
        "tone",
        "vibrance",
        "mask_compose",
    ];
    for id in live {
        let mut upstream = vec![prev.unwrap().to_string()];
        if id == "mask_compose" {
            upstream.push("mask_bake".to_string());
        }
        g.add_node(graph::StageNode {
            id: id.to_string(),
            kind: graph::StageKind::Live,
            upstream,
            own_hash: blake3::hash(id.as_bytes()),
        })
        .unwrap();
        prev = Some(id);
    }
    g.add_node(graph::StageNode {
        id: "crop".to_string(),
        kind: graph::StageKind::Geometry,
        upstream: vec![prev.unwrap().to_string()],
        own_hash: blake3::hash(b"crop"),
    })
    .unwrap();
    g
}

fn run_graph() {
    let g = build_hero_graph();
    let order = g.topological_order().expect("acyclic by construction");
    println!("Topological order: {}", order.join(" -> "));

    for changed in ["wb", "demosaic", "mask_bake", "crop"] {
        let bakes = g.invalidated_bakes(changed).unwrap();
        let mut bakes: Vec<_> = bakes.into_iter().collect();
        bakes.sort();
        println!("invalidated_bakes({changed:?}) = {bakes:?}");
    }
}

fn synthetic_frame(pixel_count: usize) -> Vec<[f32; 4]> {
    (0..pixel_count)
        .map(|i| {
            let v = (i % 256) as f32 / 255.0;
            [v, v, v, 1.0]
        })
        .collect()
}

/// Builds a `Stats` from real GPU-timestamp durations (nanoseconds, one per dispatch) rather than
/// wall-clock time -- see `gpu::LiveSuffixKernel`'s doc comment for why wall-clock-over-a-closure
/// (via a fresh `run_live_suffix` call, which rebuilds the pipeline every time) isn't the number
/// decision rule #2/#3 actually ask for. `nicti_prowl::perf::Stats`'s fields are all `pub`, so this
/// builds one directly rather than needing a crate-private constructor.
fn stats_from_ns_samples(mut samples_ns: Vec<f64>) -> nicti_prowl::perf::Stats {
    samples_ns.sort_by(|a, b| a.total_cmp(b));
    let ms: Vec<f64> = samples_ns.iter().map(|ns| ns / 1_000_000.0).collect();
    let percentile = |p: f64| -> f64 {
        if ms.is_empty() {
            return 0.0;
        }
        let rank = ((p * ms.len() as f64).ceil() as usize).clamp(1, ms.len());
        ms[rank - 1]
    };
    nicti_prowl::perf::Stats {
        p50_ms: percentile(0.50),
        p95_ms: percentile(0.95),
        max_ms: ms.last().copied().unwrap_or(0.0),
        samples_ms: ms,
    }
}

fn run_bench(out_dir: PathBuf, long_edge: u32) -> anyhow::Result<()> {
    let contexts = gpu::GpuContext::enumerate();
    let Some(ctx) = contexts.into_iter().next() else {
        eprintln!("no wgpu adapter available -- skipping bench (correctness tests still run under `cargo test`)");
        return Ok(());
    };
    println!("Using adapter: {} ({:?})", ctx.adapter_name, ctx.backend);

    let aspect = 3.0 / 2.0;
    let height = (long_edge as f32 / aspect).round() as u32;
    let pixel_count = (long_edge * height) as usize;
    let pixels = synthetic_frame(pixel_count);

    let protocol = Protocol::default();

    // Every kernel is built once (persistent pipeline + buffers) and re-dispatched across the
    // 1-warmup+5-measured loop -- rebuilding the pipeline (and re-uploading the workload) inside
    // the timed loop measures pipeline/shader setup cost, not steady per-frame dispatch cost. This
    // was a real bug caught on the first real-hardware run of this bench (~400ms p50 for
    // live_suffix, ~1000x ADR-0005's own comparable figure) before this fix.
    let live_kernel = gpu::LiveSuffixKernel::new(&ctx, pixel_count);
    let live_params = gpu::LiveSuffixParams {
        wb_r: 1.05,
        wb_g: 1.0,
        wb_b: 0.92,
        exposure_stops: 0.3,
        vibrance: 0.25,
        _pad0: 0.0,
        _pad1: 0.0,
        _pad2: 0.0,
    };
    let mut live_ns = Vec::new();
    protocol.run(|| {
        let (_, ns) = live_kernel.dispatch(&ctx, &pixels, live_params);
        if let Some(ns) = ns {
            live_ns.push(ns);
        }
    });
    write_and_print(
        &out_dir,
        "loaf-live-suffix",
        &ctx,
        stats_from_ns_samples(live_ns),
    )?;

    let (present_out_w, present_out_h) = (1920u32, 1280u32);
    let present_kernel =
        gpu::PresentSampleKernel::new(&ctx, long_edge, height, present_out_w, present_out_h);
    let transform =
        geometry::Affine2D::crop(0.0, 0.0, long_edge as f32, height as f32, 1920.0, 1280.0);
    let mut present_ns = Vec::new();
    protocol.run(|| {
        let (_, ns) = present_kernel.dispatch(&ctx, &pixels, &transform);
        if let Some(ns) = ns {
            present_ns.push(ns);
        }
    });
    write_and_print(
        &out_dir,
        "loaf-present-sample",
        &ctx,
        stats_from_ns_samples(present_ns),
    )?;

    let field: Vec<f32> = (0..(512 * 512)).map(|i| (i % 100) as f32 / 100.0).collect();
    let box_kernel = gpu::BoxFilterKernel::new(&ctx, 512, 512);
    let mut box_ns = Vec::new();
    protocol.run(|| {
        let (_, ns) = box_kernel.dispatch(&ctx, &field, 2);
        if let Some(ns) = ns {
            box_ns.push(ns);
        }
    });
    write_and_print(
        &out_dir,
        "loaf-box-filter",
        &ctx,
        stats_from_ns_samples(box_ns),
    )?;

    Ok(())
}

fn write_and_print(
    out_dir: &PathBuf,
    name: &str,
    ctx: &gpu::GpuContext,
    stats: nicti_prowl::perf::Stats,
) -> anyhow::Result<()> {
    if stats.samples_ms.is_empty() {
        eprintln!(
            "{name}: adapter reported no TIMESTAMP_QUERY support -- writing an all-zero report, \
             not a real measurement"
        );
    }
    println!(
        "{name} ({}): p50={:.3}ms p95={:.3}ms max={:.3}ms (GPU-timestamp dispatch time only, \
         excludes host<->device transfer)",
        ctx.adapter_name, stats.p50_ms, stats.p95_ms, stats.max_ms
    );
    let report = RunReport {
        name: name.to_string(),
        hardware: HardwareIdentity::capture(),
        protocol: Protocol::default(),
        stats,
        unix_time: nicti_prowl::perf::now_unix(),
    };
    let path = nicti_prowl::perf::write_report(out_dir, &report)?;
    println!("  wrote {}", path.display());
    Ok(())
}

fn run_sim(
    n_images: usize,
    cursor_start: usize,
    walk_pace_ms: u64,
    out_dir: PathBuf,
) -> anyhow::Result<()> {
    let cost = sim::BakeCost::default();
    let result = sim::simulate_hero_bake(
        n_images,
        cursor_start,
        Duration::from_millis(walk_pace_ms),
        cost,
    );
    println!(
        "hero sim: n_images={n_images} cursor_start={cursor_start} walk_pace={walk_pace_ms}ms"
    );
    println!(
        "  per-image bake cost: decode={:?} denoise={:?} mask_bake={:?} total={:?}",
        cost.decode,
        cost.denoise,
        cost.mask_bake,
        cost.total()
    );
    println!("  first_image_ready = {:?}", result.first_image_ready);
    println!(
        "  total_wall_time (sync-to-done) = {:?}",
        result.total_wall_time
    );
    println!(
        "  stale_at_arrival = {} / {} images (cursor reaches the image before its bake finishes)",
        result.stale_at_arrival,
        n_images - cursor_start
    );

    let order = prefetch::priority_order(&(0..n_images).collect::<BTreeSet<_>>(), cursor_start);
    let json = serde_json::json!({
        "n_images": n_images,
        "cursor_start": cursor_start,
        "walk_pace_ms": walk_pace_ms,
        "cost_model": {
            "decode_ms": cost.decode.as_secs_f64() * 1000.0,
            "denoise_ms": cost.denoise.as_secs_f64() * 1000.0,
            "mask_bake_ms_hypothesis": cost.mask_bake.as_secs_f64() * 1000.0,
        },
        "bake_order": order,
        "first_image_ready_ms": result.first_image_ready.as_secs_f64() * 1000.0,
        "total_wall_time_ms": result.total_wall_time.as_secs_f64() * 1000.0,
        "stale_at_arrival": result.stale_at_arrival,
    });
    std::fs::create_dir_all(&out_dir)?;
    let path = out_dir.join(format!(
        "loaf-hero-sim-{}.json",
        nicti_prowl::perf::now_unix()
    ));
    std::fs::write(&path, serde_json::to_string_pretty(&json)?)?;
    println!("  wrote {}", path.display());

    let _ = cost_model::full_res_rgba16f_bytes(1, 1); // keep cost_model import used in all build configs
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Graph => {
            run_graph();
            Ok(())
        }
        Command::Bench { out_dir, long_edge } => run_bench(out_dir, long_edge),
        Command::Sim {
            n_images,
            cursor_start,
            walk_pace_ms,
            out_dir,
        } => run_sim(n_images, cursor_start, walk_pace_ms, out_dir),
    }
}
