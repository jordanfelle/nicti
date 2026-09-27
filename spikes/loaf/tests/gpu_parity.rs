//! GPU-vs-CPU parity, checked against this sandbox's lavapipe/llvmpipe software adapter (real
//! correctness evidence; not real hardware numbers -- see `.claude/rules/gpu-gui-and-healing/
//! REFERENCE.md`'s cross-compile-to-Windows note for those, and `src/bin/loaf.rs`'s `bench`
//! subcommand for what actually collects them). Skips (with a message, not a failure) if no wgpu
//! adapter is available at all.

use loaf::{geometry, gpu, refine};

fn with_ctx(f: impl FnOnce(&gpu::GpuContext)) {
    let contexts = gpu::GpuContext::enumerate();
    match contexts.into_iter().next() {
        Some(ctx) => f(&ctx),
        None => eprintln!("no wgpu adapter available in this environment -- skipping"),
    }
}

/// Runs `f` (expected to panic) with the default panic hook suppressed, so an intentional
/// `catch_unwind`-based "this must panic" assertion doesn't also spam a backtrace to stderr on an
/// otherwise-clean test run.
fn expect_panic(f: impl FnOnce() + std::panic::UnwindSafe) -> bool {
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let result = std::panic::catch_unwind(f);
    std::panic::set_hook(prev_hook);
    result.is_err()
}

#[test]
fn live_suffix_gpu_matches_a_hand_computed_reference_pixel() {
    with_ctx(|ctx| {
        // A single pixel, chosen so tone_curve's first segment (x <= 0.25) and vibrance's
        // saturation path are both exercised, matching the WGSL kernel's own branches.
        let pixels = vec![[0.4, 0.2, 0.1, 1.0]];
        let params = gpu::LiveSuffixParams {
            wb_r: 1.1,
            wb_g: 1.0,
            wb_b: 0.9,
            exposure_stops: 0.5,
            vibrance: 0.3,
            _pad0: 0.0,
            _pad1: 0.0,
            _pad2: 0.0,
        };
        let (gpu_out, _) = gpu::run_live_suffix(ctx, &pixels, params);

        let cpu_out = cpu_reference_live_suffix(pixels[0], params);
        for c in 0..3 {
            assert!(
                (gpu_out[0][c] - cpu_out[c]).abs() < 1e-4,
                "channel {c}: gpu={} cpu={}",
                gpu_out[0][c],
                cpu_out[c]
            );
        }
    });
}

fn tone_curve(x_in: f32) -> f32 {
    let xs = [0.0, 0.25, 0.5, 0.75, 1.0];
    let ys = [0.02, 0.22, 0.5, 0.80, 0.98];
    let x = x_in.clamp(0.0, 1.0);
    for i in 0..4 {
        if x <= xs[i + 1] || i == 3 {
            let t = (x - xs[i]) / (xs[i + 1] - xs[i]);
            return ys[i] + t * (ys[i + 1] - ys[i]);
        }
    }
    ys[4]
}

fn apply_vibrance(rgb: [f32; 3], vibrance: f32) -> [f32; 3] {
    let mx = rgb[0].max(rgb[1]).max(rgb[2]);
    let mn = rgb[0].min(rgb[1]).min(rgb[2]);
    let sat = if mx > 0.0 { (mx - mn) / mx } else { 0.0 };
    let weight = vibrance * (1.0 - sat);
    let avg = (rgb[0] + rgb[1] + rgb[2]) / 3.0;
    let mut out = [0.0f32; 3];
    for c in 0..3 {
        out[c] = (avg + (rgb[c] - avg) * (1.0 + weight)).clamp(0.0, 1.0);
    }
    out
}

fn cpu_reference_live_suffix(px: [f32; 4], params: gpu::LiveSuffixParams) -> [f32; 3] {
    let exposure_mul = params.exposure_stops.exp2();
    let wb = [params.wb_r, params.wb_g, params.wb_b];
    let mut c = [
        px[0] * wb[0] * exposure_mul,
        px[1] * wb[1] * exposure_mul,
        px[2] * wb[2] * exposure_mul,
    ];
    c = [tone_curve(c[0]), tone_curve(c[1]), tone_curve(c[2])];
    apply_vibrance(c, params.vibrance)
}

#[test]
fn present_sample_gpu_matches_cpu_reference_for_a_crop() {
    with_ctx(|ctx| {
        let (src_w, src_h) = (8u32, 8u32);
        let mut source = geometry::Plane::new(src_w as usize, src_h as usize, [0.0, 0.0, 1.0, 1.0]);
        for y in 0..4 {
            for x in 0..4 {
                source.data[y * 8 + x] = [1.0, 0.0, 0.0, 1.0];
            }
        }
        let transform = geometry::Affine2D::crop(0.0, 0.0, 4.0, 4.0, 4.0, 4.0);
        let (out_w, out_h) = (4u32, 4u32);

        let (gpu_out, _) =
            gpu::run_present_sample(ctx, &source.data, src_w, src_h, &transform, out_w, out_h);
        let cpu_out = geometry::sample(&source, &transform, out_w as usize, out_h as usize);

        for (i, (gpu_px, cpu_px)) in gpu_out.iter().zip(&cpu_out.data).enumerate() {
            for (c, (gpu_c, cpu_c)) in gpu_px.iter().zip(cpu_px).enumerate() {
                assert!(
                    (gpu_c - cpu_c).abs() < 1e-4,
                    "pixel {i} channel {c}: gpu={gpu_c} cpu={cpu_c}"
                );
            }
        }
    });
}

#[test]
fn box_filter_gpu_matches_cpu_reference() {
    with_ctx(|ctx| {
        let width = 16usize;
        let height = 12usize;
        let field_data: Vec<f32> = (0..(width * height))
            .map(|i| (i % 37) as f32 / 37.0)
            .collect();
        let mut cpu_field = refine::Field::new(width, height, 0.0);
        cpu_field.data = field_data.clone();

        let radius = 2u32;
        let (gpu_out, _) =
            gpu::run_box_filter(ctx, &field_data, width as u32, height as u32, radius);
        let cpu_out = refine::box_filter(&cpu_field, radius as usize);

        for (i, (gpu_v, cpu_v)) in gpu_out.iter().zip(&cpu_out.data).enumerate() {
            assert!(
                (gpu_v - cpu_v).abs() < 1e-4,
                "index {i}: gpu={gpu_v} cpu={cpu_v}"
            );
        }
    });
}

/// Regression test for an adversarial-review finding: `PresentSampleKernel::dispatch` used to take
/// `source_width`/`source_height` as separate runtime parameters alongside a flat `source` slice --
/// a caller could pass a `source_width * source_height` product that matched the buffer's flat
/// length while describing a completely different, wrong shape (e.g. a buffer built for a real
/// 10x10 layout, dispatched as 20x5 -- same element count, wrong row stride), and the WGSL
/// kernel's row-major indexing would silently read the wrong positions instead of erroring. The
/// fix removes the possibility structurally: `source_width`/`source_height` are now fixed at
/// `new()` and no longer accepted at `dispatch()` at all, so only a genuinely wrong-*length*
/// slice remains possible, which the existing length assert already catches cleanly.
#[test]
fn present_sample_kernel_panics_on_a_wrong_length_source_slice() {
    with_ctx(|ctx| {
        let kernel = gpu::PresentSampleKernel::new(ctx, 10, 10, 4, 4);
        let wrong_length_source: Vec<[f32; 4]> = vec![[0.0, 0.0, 0.0, 1.0]; 42];
        let transform = geometry::Affine2D::identity();
        let panicked = expect_panic(std::panic::AssertUnwindSafe(|| {
            kernel.dispatch(ctx, &wrong_length_source, &transform);
        }));
        assert!(
            panicked,
            "dispatch should panic when given a source slice whose length doesn't match what the \
             kernel was constructed for, not silently proceed"
        );
    });
}

/// Same regression class as above, for `BoxFilterKernel::dispatch`.
#[test]
fn box_filter_kernel_panics_on_a_wrong_length_field_slice() {
    with_ctx(|ctx| {
        let kernel = gpu::BoxFilterKernel::new(ctx, 10, 10);
        let wrong_length_field: Vec<f32> = vec![0.0; 42];
        let panicked = expect_panic(std::panic::AssertUnwindSafe(|| {
            kernel.dispatch(ctx, &wrong_length_field, 1);
        }));
        assert!(
            panicked,
            "dispatch should panic when given a field slice whose length doesn't match what the \
             kernel was constructed for, not silently proceed"
        );
    });
}
