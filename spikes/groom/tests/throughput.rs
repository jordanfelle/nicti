//! Throughput comparison of clone-stamp / spot-heal / auto-source-pick (CPU) and the
//! `poisson_jacobi` WGSL kernel (GPU), `#[ignore]`d since these are timing measurements, not
//! correctness checks (matches `spikes/glint/tests/throughput.rs`'s own `#[ignore]` convention
//! for perf tests).
//!
//! CPU numbers are legitimate anywhere. **GPU numbers require a real GPU-backed Vulkan/Dx12
//! adapter** (#97's reference-machine pass) -- `poisson_jacobi_gpu_throughput` skips cleanly,
//! printing a message, when `GpuContext::enumerate()` finds nothing or no adapter reports
//! `TIMESTAMP_QUERY` support (matching `tests/correctness.rs`'s skip pattern).
//!
//! Run explicitly: `cargo test -p groom --test throughput --release -- --ignored --nocapture`.

use std::time::Instant;

use groom::cpu_reference::{auto_source_pick, clone_stamp, spot_heal, Image};
use groom::gpu::{run_poisson_jacobi, GpuContext};

fn checkerboard(width: usize, height: usize) -> Image {
    let mut img = Image::new(width, height, [0.0, 0.0, 0.0, 1.0]);
    for y in 0..height {
        for x in 0..width {
            let v = if (x / 8 + y / 8) % 2 == 0 { 0.85 } else { 0.15 };
            img.set(x as i32, y as i32, [v, v, v, 1.0]);
        }
    }
    img
}

fn time_it<F: FnMut()>(mut f: F, runs: u32) -> f64 {
    // One discarded warm-up run, then average of the measured runs -- same shape as
    // docs/benchmarks.md's own warm/cold measurement rule, scaled down for a spike.
    f();
    let t0 = Instant::now();
    for _ in 0..runs {
        f();
    }
    t0.elapsed().as_secs_f64() * 1000.0 / runs as f64
}

#[test]
#[ignore = "timing measurement, not a correctness check -- run explicitly with --ignored"]
fn cpu_clone_heal_and_auto_pick_throughput() {
    let src = checkerboard(512, 512);
    let radius = 20.0;
    let feather = 4.0;
    let offset = (30, 30);
    let center = (256, 256);

    let clone_ms = time_it(
        || {
            let mut dst = src.clone();
            clone_stamp(&mut dst, &src, center, offset, radius, feather);
        },
        20,
    );

    let heal_ms = time_it(
        || {
            let mut dst = src.clone();
            spot_heal(&mut dst, &src, center, offset, radius, feather, 50);
        },
        20,
    );

    let pick_ms = time_it(
        || {
            auto_source_pick(&src, center, radius, 70.0, 24);
        },
        20,
    );

    eprintln!(
        "groom CPU throughput (512x512 checkerboard, radius={radius}, 50 Jacobi iterations for heal):\n\
         clone_stamp: {clone_ms:.4} ms/op\n\
         spot_heal:   {heal_ms:.4} ms/op\n\
         auto_source_pick: {pick_ms:.4} ms/op (24 candidates)\n\
         (GPU numbers for poisson_jacobi deferred to the reference-machine follow-up)"
    );

    // Loose sanity bounds, not perf assertions -- this test's purpose is to print real numbers
    // for docs/adr/0007-healing-and-removal.md, not to gate CI on a timing threshold.
    assert!(clone_ms > 0.0);
    assert!(heal_ms > 0.0);
    assert!(pick_ms > 0.0);
}

/// Nearest-rank p50/p95/max over `values_ms`, after discarding `warmup` samples from the front --
/// `docs/benchmarks.md`'s "1 warm-up run discarded, then N measured runs" protocol. Duplicated
/// (not imported) from `spikes/glint/src/stats.rs` since spikes don't depend on each other.
fn summarize_after_warmup(values_ms: &[f64], warmup: usize) -> Option<(f64, f64, f64)> {
    let samples = values_ms.get(warmup..)?;
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).expect("NaN in latency samples"));
    let percentile = |q: f64| -> f64 {
        let idx = (q * (sorted.len() as f64 - 1.0)).round() as usize;
        sorted[idx.min(sorted.len() - 1)]
    };
    Some((
        percentile(0.50),
        percentile(0.95),
        *sorted.last().expect("checked non-empty above"),
    ))
}

/// The same synthetic patch shape as `tests/correctness.rs::make_patch`, at 512x512. **Not an
/// equal-work comparison against the CPU case above**: `cpu_reference::spot_heal` crops to a
/// ~41x41 bounding box around the destination circle before running its Jacobi solve, while this
/// WGSL kernel dispatches one thread per pixel across the *entire* width*height grid every
/// iteration regardless of `mask` (the mask only picks interior-vs-boundary handling per pixel,
/// it doesn't shrink the dispatch) -- so this measures ~156x more raw work per iteration than the
/// CPU number it's printed alongside. That makes it a strictly more conservative (pessimistic)
/// proxy for the interactive-heal budget, not a matched-workload comparison; see #97's adversarial
/// review for why this distinction matters for ADR-0007's Measured-results claims.
fn make_patch(width: usize, height: usize) -> (Vec<[f32; 4]>, Vec<[f32; 4]>, Vec<u32>) {
    let mut guidance = Vec::with_capacity(width * height);
    let mut initial = Vec::with_capacity(width * height);
    let mut mask = Vec::with_capacity(width * height);
    let cx = width as f32 / 2.0;
    let cy = height as f32 / 2.0;
    let radius = 20.0f32;

    for y in 0..height {
        for x in 0..width {
            let fx = x as f32 / width as f32;
            let fy = y as f32 / height as f32;
            guidance.push([fx, fy, (fx + fy) * 0.5, 1.0]);
            let v = if (x / 8 + y / 8) % 2 == 0 { 0.85 } else { 0.15 };
            initial.push([v, v, v, 1.0]);
            let dist = (((x as f32 - cx).powi(2)) + ((y as f32 - cy).powi(2))).sqrt();
            mask.push((dist < radius) as u32);
        }
    }
    (guidance, initial, mask)
}

const WARMUP: usize = 1;
const RUNS: usize = 5;

#[test]
#[ignore = "timing measurement, requires a real GPU-backed adapter -- run explicitly with --ignored"]
fn poisson_jacobi_gpu_throughput() {
    let contexts = GpuContext::enumerate();
    if contexts.is_empty() {
        eprintln!("throughput: no wgpu adapter available, skipping");
        return;
    }

    let width = 512;
    let height = 512;
    let iterations = 50u32; // same iteration count as the CPU spot_heal case above (see
                            // make_patch's doc comment for why the per-iteration workload isn't
                            // actually equal)
    let (guidance, initial, mask) = make_patch(width, height);

    for ctx in &contexts {
        if ctx.device_type == wgpu::DeviceType::Cpu {
            eprintln!(
                "backend {:?} adapter={}: software adapter, skipping (this test requires real GPU hardware)",
                ctx.backend, ctx.adapter_name
            );
            continue;
        }
        if !ctx.supports_timestamps() {
            eprintln!(
                "backend {:?}: no TIMESTAMP_QUERY, wall-clock numbers would be unreliable, skipping",
                ctx.backend
            );
            continue;
        }
        let mut samples_ms = Vec::with_capacity(WARMUP + RUNS);
        for _ in 0..(WARMUP + RUNS) {
            let (_, elapsed_ns) = run_poisson_jacobi(
                ctx,
                &guidance,
                &initial,
                &mask,
                width as u32,
                height as u32,
                iterations,
            );
            samples_ms.push(elapsed_ns.expect("timestamps enabled above") / 1e6);
        }
        let (p50, p95, max) =
            summarize_after_warmup(&samples_ms, WARMUP).expect("non-empty after warmup");
        eprintln!(
            "groom GPU throughput (512x512, radius=20, {iterations} Jacobi iterations) \
             backend={:?} adapter={}: p50_ms={p50:.4} p95_ms={p95:.4} max_ms={max:.4} \
             (target: <16ms/update interactive-heal budget, ADR-0007)",
            ctx.backend, ctx.adapter_name
        );
        assert!(p50 > 0.0);
    }
}
