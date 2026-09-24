//! Each WGSL kernel checked against `cpu_reference` within f16-scale tolerance, across every
//! wgpu adapter this machine exposes. Skips cleanly (prints a message, doesn't fail) when no
//! adapter is available at all -- CI runners without a GPU or software Vulkan ICD hit this path.

use glint::cpu_reference::{live_chain_pixel, tile_blend_pixel};
use glint::gpu::{run_live_chain, run_tile_blend, GpuContext, LiveChainParams, TileBlendParams};

const TOLERANCE: f32 = 1e-2; // f16-scale: ~3 decimal digits of precision at worst.

fn require_contexts() -> Vec<GpuContext> {
    let contexts = GpuContext::enumerate();
    if contexts.is_empty() {
        eprintln!("correctness: no wgpu adapter available, skipping");
    }
    contexts
}

fn make_test_pixels(n: usize) -> Vec<[f32; 4]> {
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
fn live_chain_matches_cpu_reference() {
    for ctx in require_contexts() {
        let pixels = make_test_pixels(1024);
        let params = LiveChainParams {
            wb_gain: [1.05, 1.0, 0.92],
            exposure_stops: 0.6,
            vibrance: 0.4,
            _pad: [0.0; 3],
        };
        let (gpu_out, _) = run_live_chain(&ctx, &pixels, params);

        for (i, (px, gpu)) in pixels.iter().zip(gpu_out.iter()).enumerate() {
            let expected = live_chain_pixel(
                [px[0], px[1], px[2]],
                params.wb_gain,
                params.exposure_stops,
                params.vibrance,
            );
            for c in 0..3 {
                assert!(
                    (expected[c] - gpu[c]).abs() < TOLERANCE,
                    "backend {:?} pixel {i} channel {c}: cpu={} gpu={}",
                    ctx.backend,
                    expected[c],
                    gpu[c]
                );
            }
            assert!((gpu[3] - px[3]).abs() < TOLERANCE, "alpha not passed through on backend {:?}", ctx.backend);
        }
    }
}

#[test]
fn tile_blend_matches_cpu_reference() {
    for ctx in require_contexts() {
        let width = 32u32;
        let height = 32usize;
        let count = width as usize * height;
        let tile_a: Vec<[f32; 4]> = (0..count).map(|_| [1.0, 0.0, 0.0, 1.0]).collect();
        let tile_b: Vec<[f32; 4]> = (0..count).map(|_| [0.0, 1.0, 0.0, 1.0]).collect();
        let params = TileBlendParams { width, seam_start: 10, seam_width: 8, _pad: 0 };

        let (gpu_out, _) = run_tile_blend(&ctx, &tile_a, &tile_b, params);

        for i in 0..count {
            let col = i as u32 % width;
            let t = if params.seam_width > 0 {
                ((col as f32 - params.seam_start as f32) / params.seam_width as f32).clamp(0.0, 1.0)
            } else if col >= params.seam_start {
                1.0
            } else {
                0.0
            };
            let expected = tile_blend_pixel(
                [tile_a[i][0], tile_a[i][1], tile_a[i][2]],
                [tile_b[i][0], tile_b[i][1], tile_b[i][2]],
                t,
            );
            for c in 0..3 {
                assert!(
                    (expected[c] - gpu_out[i][c]).abs() < TOLERANCE,
                    "backend {:?} pixel {i} channel {c}: cpu={} gpu={}",
                    ctx.backend,
                    expected[c],
                    gpu_out[i][c]
                );
            }
        }
    }
}
