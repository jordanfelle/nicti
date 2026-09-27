//! CPU vs GPU parity for the 3D-texture HueSatMap kernel (ADR-0038's GPU-feasibility question).
//! Skips cleanly when no wgpu adapter is available (matches glint's own correctness tests).

use calico::gpu::{run_apply_hue_sat_map, GpuContext, GpuPixel};
use calico::huesatmap::{hsv_to_rgb, rgb_to_hsv, HueSatMap};

// Confirmed via a separate throwaway diagnostic (texel-by-texel `textureLoad` nearest-fetch
// readback, not committed): the texture upload and the texel-center coordinate remap in
// `shaders/color.wgsl` are both exactly correct in exact arithmetic -- worked by hand for this
// test's own data against the specific pixel that first failed with a tighter bound.
//
// An earlier version of this test passed with a much looser 1e-1 tolerance and a ~0.05-0.06 max
// deviation, attributed (wrongly) to lavapipe's own lower-precision fixed-point trilinear
// filtering. The real cause was a CPU-only bug in `huesatmap.rs`'s `sample`/`sample_gpu_style`:
// the saturation-axis and value-axis interpolation fractions were swapped, which this test's
// smoothly-varying synthetic data mostly masked (the two fractions were often close enough that
// swapping them barely changed the result) -- caught for real by `pipeline.rs`'s
// `encoding_choice_does_affect_value_scaling` test, which used a sat_divisions=1 table where the
// swap's effect couldn't hide. With that fixed, the real CPU/GPU deviation (the hardware sampler
// doing shortest-path-unaware linear filtering, plus genuine floating-point precision) is two
// orders of magnitude smaller.
const TOLERANCE: f32 = 5e-3;

fn require_contexts() -> Vec<GpuContext> {
    let contexts = GpuContext::enumerate();
    if contexts.is_empty() {
        eprintln!("gpu_parity: no wgpu adapter available, skipping");
    }
    contexts
}

fn make_test_map() -> HueSatMap {
    // 12 hue steps (30 deg), 4 sat steps, 3 val steps -- a small but non-trivial synthetic map,
    // with a smooth (non-wraparound-discontinuous) hue-shift pattern so this test doesn't hit the
    // one documented CPU/GPU divergence (see gpu.rs's module doc).
    let (hue_div, sat_div, val_div) = (12, 4, 3);
    let mut data = Vec::with_capacity(hue_div * sat_div * val_div);
    // DNG SDK storage order: value outermost, hue middle, saturation innermost (huesatmap.rs's
    // module doc / `index()`) -- must match or this test would validate the wrong convention.
    for v in 0..val_div {
        for h in 0..hue_div {
            for s in 0..sat_div {
                let hue_shift = 5.0 * (h as f32 / hue_div as f32 * std::f32::consts::TAU).sin();
                let sat_scale = 0.8 + 0.2 * (s as f32 / (sat_div - 1) as f32);
                let val_scale = 0.9 + 0.1 * (v as f32 / (val_div - 1) as f32);
                data.push([hue_shift, sat_scale, val_scale]);
            }
        }
    }
    HueSatMap {
        hue_divisions: hue_div,
        sat_divisions: sat_div,
        val_divisions: val_div,
        data,
    }
}

fn make_test_pixels(n: usize) -> Vec<[f32; 3]> {
    (0..n)
        .map(|i| {
            let t = i as f32 / n.max(1) as f32;
            [
                (t * 1.3).fract(),
                (t * 0.7 + 0.2).fract(),
                (t * 2.1 + 0.5).fract(),
            ]
        })
        .collect()
}

#[test]
fn hue_sat_map_gpu_matches_cpu_reference() {
    for ctx in require_contexts() {
        let map = make_test_map();
        let pixels = make_test_pixels(2048);
        let gpu_input: Vec<GpuPixel> = pixels
            .iter()
            .map(|p| GpuPixel { rgb: *p, _pad: 0.0 })
            .collect();
        let gpu_output = run_apply_hue_sat_map(&ctx, &gpu_input, &map);

        let mut max_diff = 0.0f32;
        let mut worst: Option<(usize, usize)> = None;
        for (i, (px, gpu)) in pixels.iter().zip(gpu_output.iter()).enumerate() {
            let hsv = rgb_to_hsv([px[0] as f64, px[1] as f64, px[2] as f64]);
            let adj = map.sample_gpu_style(hsv[0], hsv[1], hsv[2]);
            let new_hsv = [
                hsv[0] + adj[0],
                (hsv[1] * adj[1]).clamp(0.0, 1.0),
                hsv[2] * adj[2],
            ];
            let expected = hsv_to_rgb(new_hsv);

            for (c, (&exp, &got)) in expected.iter().zip(gpu.rgb.iter()).enumerate() {
                let diff = (exp as f32 - got).abs();
                if diff > max_diff {
                    max_diff = diff;
                    worst = Some((i, c));
                }
            }
        }
        eprintln!(
            "backend {:?}: max diff {max_diff} at {worst:?}",
            ctx.backend
        );
        assert!(
            max_diff < TOLERANCE,
            "backend {:?}: max diff {max_diff} exceeds tolerance at {worst:?}",
            ctx.backend
        );
    }
}
