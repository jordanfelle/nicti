//! Host<->device staging cost for a full 45MP RGBA16F-equivalent frame, quantifying (not just
//! asserting) that Tapetum's bake-time interop is cheap relative to the 100ms warm image-switch
//! budget -- it happens once per bake, off the per-frame hot path, but "off the hot path" is a
//! claim this test backs with a number. `#[ignore]`d, same reasoning as `throughput.rs`.

use glint::gpu::{run_live_chain, GpuContext, LiveChainParams};
use glint::stats::summarize_after_warmup;

const WARMUP: usize = 1;
const RUNS: usize = 5;
const HERO_PIXEL_COUNT: usize = 45_000_000;
const WARM_SWITCH_BUDGET_MS: f64 = 100.0;

#[test]
#[ignore]
fn full_frame_upload_readback_cost() {
    let contexts = GpuContext::enumerate();
    if contexts.is_empty() {
        eprintln!("interop_roundtrip: no wgpu adapter available, skipping");
        return;
    }
    let pixels: Vec<[f32; 4]> = (0..HERO_PIXEL_COUNT)
        .map(|_| [0.5, 0.5, 0.5, 1.0])
        .collect();
    let params = LiveChainParams {
        wb_gain: [1.0, 1.0, 1.0],
        exposure_stops: 0.0,
        vibrance: 0.0,
        _pad: [0.0; 3],
    };

    for ctx in &contexts {
        let mut samples_ms = Vec::with_capacity(WARMUP + RUNS);
        for _ in 0..(WARMUP + RUNS) {
            let start = std::time::Instant::now();
            let _ = run_live_chain(ctx, &pixels, params);
            samples_ms.push(start.elapsed().as_secs_f64() * 1e3);
        }
        let stats = summarize_after_warmup(&samples_ms, WARMUP).expect("non-empty after warmup");
        println!(
            "backend={:?} adapter={} full_45mp_roundtrip_ms p50={:.2} p95={:.2} max={:.2} (warm-switch budget: {WARM_SWITCH_BUDGET_MS}ms)",
            ctx.backend, ctx.adapter_name, stats.p50, stats.p95, stats.max
        );
    }
}
