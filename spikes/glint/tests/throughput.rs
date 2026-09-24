//! Live-chain throughput at two resolutions, across every backend + CUDA (if available).
//! `#[ignore]`d: correctness tests run everywhere including CI's software-Vulkan fallback, but
//! throughput numbers are only meaningful on real hardware -- run explicitly on the reference
//! machine with `cargo test -p glint --release --test throughput -- --ignored --nocapture`.
//! GPU timestamps, not wall-clock, per `docs/benchmarks.md`'s methodology.

use glint::cuda::{CudaLiveChain, CudaLiveChainParams};
use glint::gpu::{run_live_chain, GpuContext, LiveChainParams};
use glint::stats::summarize_after_warmup;

const WARMUP: usize = 1;
const RUNS: usize = 5;

fn hero_params() -> LiveChainParams {
    LiveChainParams {
        wb_gain: [1.05, 1.0, 0.92],
        exposure_stops: 0.6,
        vibrance: 0.4,
        _pad: [0.0; 3],
    }
}

fn make_pixels(n: usize) -> Vec<[f32; 4]> {
    (0..n)
        .map(|i| {
            let t = i as f32 / n.max(1) as f32;
            [
                (t * 1.3).fract(),
                (t * 0.7 + 0.2).fract(),
                (t * 2.1 + 0.5).fract(),
                1.0,
            ]
        })
        .collect()
}

#[test]
#[ignore]
fn live_chain_throughput_wgpu_backends() {
    let contexts = GpuContext::enumerate();
    if contexts.is_empty() {
        eprintln!("throughput: no wgpu adapter available, skipping");
        return;
    }
    for ctx in &contexts {
        if !ctx.supports_timestamps() {
            eprintln!("backend {:?}: no TIMESTAMP_QUERY, wall-clock numbers would be unreliable, skipping", ctx.backend);
            continue;
        }
        for &(label, w, h) in &[
            ("3840x2160", 3840usize, 2160usize),
            ("8256x5504", 8256usize, 5504usize),
        ] {
            let pixels = make_pixels(w * h);
            let params = hero_params();
            let mut samples_ms = Vec::with_capacity(WARMUP + RUNS);
            for _ in 0..(WARMUP + RUNS) {
                let (_, elapsed_ns) = run_live_chain(ctx, &pixels, params);
                samples_ms.push(elapsed_ns.expect("timestamps enabled above") / 1e6);
            }
            let stats =
                summarize_after_warmup(&samples_ms, WARMUP).expect("non-empty after warmup");
            println!(
                "wgpu backend={:?} adapter={} res={label} p50_ms={:.3} p95_ms={:.3} max_ms={:.3}",
                ctx.backend, ctx.adapter_name, stats.p50, stats.p95, stats.max
            );
        }
    }
}

#[test]
#[ignore]
fn live_chain_throughput_cuda() {
    let Some(cuda) = CudaLiveChain::new() else {
        eprintln!("throughput: no CUDA driver/NVRTC available, skipping");
        return;
    };
    let params = CudaLiveChainParams {
        wb_gain: [1.05, 1.0, 0.92],
        exposure_stops: 0.6,
        vibrance: 0.4,
    };
    for &(label, w, h) in &[
        ("3840x2160", 3840usize, 2160usize),
        ("8256x5504", 8256usize, 5504usize),
    ] {
        let pixels = make_pixels(w * h);
        let mut samples_ms = Vec::with_capacity(WARMUP + RUNS);
        for _ in 0..(WARMUP + RUNS) {
            let (_, elapsed_ns) = cuda.run(&pixels, params);
            samples_ms.push(elapsed_ns / 1e6);
        }
        let stats = summarize_after_warmup(&samples_ms, WARMUP).expect("non-empty after warmup");
        println!(
            "cuda device={} res={label} p50_ms={:.3} p95_ms={:.3} max_ms={:.3}",
            cuda.device_name(),
            stats.p50,
            stats.p95,
            stats.max
        );
    }
}
