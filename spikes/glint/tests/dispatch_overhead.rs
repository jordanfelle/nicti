//! CPU-side cost of submitting a chain of dispatches per frame -- this is where wgpu's overhead
//! vs raw `ash` actually lives (per-call validation, resource tracking), not in the shader
//! execution itself. Stands in for a raw-`ash` comparison harness: the ADR states this
//! substitution explicitly rather than building a second, parallel `ash` implementation just to
//! measure the same number. `#[ignore]`d for the same reason as `throughput.rs`.
//!
//! Uses `LiveChainKernel`, built once outside the timed loop, specifically so this measures
//! per-dispatch submission cost and not pipeline compilation + buffer allocation + upload --
//! an earlier version of this test called `run_live_chain` (which rebuilds all of that on every
//! call) inside the timed loop and mislabeled the result as "per-dispatch CPU overhead," caught
//! in adversarial review before merge.

use glint::gpu::{GpuContext, LiveChainKernel, LiveChainParams};
use glint::stats::summarize_after_warmup;

const WARMUP: usize = 1;
const RUNS: usize = 5;
const CHAIN_LENGTH: usize = 20;

#[test]
#[ignore]
fn dispatch_chain_cpu_submit_cost() {
    let contexts = GpuContext::enumerate();
    if contexts.is_empty() {
        eprintln!("dispatch_overhead: no wgpu adapter available, skipping");
        return;
    }
    // Small buffer: this test measures per-dispatch CPU submission overhead, not GPU compute
    // time, so keep the workload itself negligible.
    let pixels: Vec<[f32; 4]> = (0..256).map(|_| [0.5, 0.5, 0.5, 1.0]).collect();
    let params = LiveChainParams {
        wb_gain: [1.0, 1.0, 1.0],
        exposure_stops: 0.0,
        vibrance: 0.0,
        _pad: [0.0; 3],
    };

    for ctx in &contexts {
        let kernel = LiveChainKernel::new(ctx, pixels.len());
        let mut samples_ms = Vec::with_capacity(WARMUP + RUNS);
        for _ in 0..(WARMUP + RUNS) {
            let start = std::time::Instant::now();
            for _ in 0..CHAIN_LENGTH {
                let _ = kernel.dispatch(ctx, &pixels, params);
            }
            samples_ms.push(start.elapsed().as_secs_f64() * 1e3 / CHAIN_LENGTH as f64);
        }
        let stats = summarize_after_warmup(&samples_ms, WARMUP).expect("non-empty after warmup");
        println!(
            "backend={:?} adapter={} per_dispatch_ms p50={:.4} p95={:.4} max={:.4}",
            ctx.backend, ctx.adapter_name, stats.p50, stats.p95, stats.max
        );
    }
}
