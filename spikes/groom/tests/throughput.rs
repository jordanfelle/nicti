//! CPU-only throughput comparison of clone-stamp / spot-heal / auto-source-pick, `#[ignore]`d
//! since it's a timing measurement, not a correctness check (matches
//! `spikes/glint/tests/throughput.rs`'s own `#[ignore]` convention for perf tests).
//!
//! **GPU numbers are deferred to the reference-machine follow-up** (ADR-0007's Measured results
//! section) -- this sandbox has no GPU-backed Vulkan/Dx12 adapter, so `poisson_jacobi`'s GPU
//! throughput can only be measured on real hardware, the same gap ADR-0005/0006 already flag for
//! their own GPU numbers. What's measured here is legitimate CPU-only data, not a placeholder.
//!
//! Run explicitly: `cargo test -p groom --test throughput -- --ignored --nocapture`.

use std::time::Instant;

use groom::cpu_reference::{auto_source_pick, clone_stamp, spot_heal, Image};

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
