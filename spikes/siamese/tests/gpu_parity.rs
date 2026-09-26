//! CPU vs GPU parity for every kernel in `src/gpu.rs`: gradient rasterization, brush rasterization,
//! compose steps, and masked-adjust apply. Skips cleanly when no wgpu adapter is available
//! (matches groom/calico/glint's own parity-test convention).

use siamese::geometry::{Dab, Geometry, LinearGradient, RadialGradient, Stroke};
use siamese::gpu::{
    dabs_from_strokes, run_compose_step, run_masked_adjust_apply, run_rasterize_brush,
    run_rasterize_linear_gradient, run_rasterize_radial_gradient, GpuContext, GpuPixel,
};

const TOLERANCE: f32 = 1e-4;

fn require_contexts() -> Vec<GpuContext> {
    let contexts = GpuContext::enumerate();
    if contexts.is_empty() {
        eprintln!("gpu_parity: no wgpu adapter available, skipping");
    }
    contexts
}

fn assert_fields_close(cpu: &[f32], gpu: &[f32], label: &str) {
    assert_eq!(cpu.len(), gpu.len());
    let mut max_diff = 0.0f32;
    for (i, (&c, &g)) in cpu.iter().zip(gpu.iter()).enumerate() {
        let diff = (c - g).abs();
        if diff > max_diff {
            max_diff = diff;
        }
        assert!(
            diff < TOLERANCE,
            "{label}: pixel {i} cpu={c} gpu={g} diff={diff}"
        );
    }
    eprintln!("{label}: max diff {max_diff}");
}

#[test]
fn linear_gradient_gpu_matches_cpu() {
    for ctx in require_contexts() {
        let g = LinearGradient {
            p0: (5.0, 40.0),
            p1: (90.0, 10.0),
            invert: false,
        };
        let (width, height) = (100, 50);
        let cpu = Geometry::LinearGradient(g).rasterize(width, height);
        let gpu = run_rasterize_linear_gradient(&ctx, width, height, g.p0, g.p1, g.invert);
        assert_fields_close(&cpu.data, &gpu, "linear_gradient");
    }
}

#[test]
fn linear_gradient_invert_gpu_matches_cpu() {
    for ctx in require_contexts() {
        let g = LinearGradient {
            p0: (0.0, 0.0),
            p1: (20.0, 0.0),
            invert: true,
        };
        let (width, height) = (20, 5);
        let cpu = Geometry::LinearGradient(g).rasterize(width, height);
        let gpu = run_rasterize_linear_gradient(&ctx, width, height, g.p0, g.p1, g.invert);
        assert_fields_close(&cpu.data, &gpu, "linear_gradient_invert");
    }
}

#[test]
fn radial_gradient_gpu_matches_cpu() {
    for ctx in require_contexts() {
        let g = RadialGradient {
            center: (50.0, 30.0),
            radii: (25.0, 15.0),
            angle: 0.6,
            feather: 6.0,
            invert: false,
        };
        let (width, height) = (100, 60);
        let cpu = Geometry::RadialGradient(g).rasterize(width, height);
        let gpu = run_rasterize_radial_gradient(
            &ctx, width, height, g.center, g.radii, g.angle, g.feather, g.invert,
        );
        assert_fields_close(&cpu.data, &gpu, "radial_gradient");
    }
}

#[test]
fn brush_gpu_matches_cpu_with_erase_stroke() {
    for ctx in require_contexts() {
        let strokes = vec![
            Stroke {
                dabs: vec![
                    Dab {
                        center: (10.0, 10.0),
                        radius: 8.0,
                        feather: 2.0,
                        flow: 1.0,
                    },
                    Dab {
                        center: (14.0, 10.0),
                        radius: 8.0,
                        feather: 2.0,
                        flow: 0.7,
                    },
                ],
                erase: false,
            },
            Stroke {
                dabs: vec![Dab {
                    center: (12.0, 10.0),
                    radius: 4.0,
                    feather: 1.0,
                    flow: 1.0,
                }],
                erase: true,
            },
        ];
        let (width, height) = (24, 20);
        let cpu = Geometry::Brush(strokes.clone()).rasterize(width, height);
        let dabs = dabs_from_strokes(&strokes);
        let gpu = run_rasterize_brush(&ctx, width, height, &dabs);
        assert_fields_close(&cpu.data, &gpu, "brush");
    }
}

#[test]
fn brush_gpu_matches_cpu_with_no_strokes() {
    for ctx in require_contexts() {
        let (width, height) = (8, 8);
        let cpu = Geometry::Brush(vec![]).rasterize(width, height);
        let gpu = run_rasterize_brush(&ctx, width, height, &[]);
        assert_fields_close(&cpu.data, &gpu, "brush_empty");
    }
}

#[test]
fn compose_step_add_gpu_matches_cpu() {
    for ctx in require_contexts() {
        let running = vec![0.2, 0.5, 0.9, 0.0];
        let weight = vec![0.3, 0.6, 0.2, 1.0];
        let opacity = 0.8;

        let cpu: Vec<f32> = running
            .iter()
            .zip(&weight)
            .map(|(&r, &w): (&f32, &f32)| (r + w * opacity).min(1.0))
            .collect();
        let gpu = run_compose_step(&ctx, 2, 2, &running, &weight, false, 0, opacity);
        assert_fields_close(&cpu, &gpu, "compose_add");
    }
}

#[test]
fn compose_step_subtract_with_invert_gpu_matches_cpu() {
    for ctx in require_contexts() {
        let running = vec![0.8, 0.5, 0.3, 1.0];
        let weight = vec![0.3, 0.6, 0.2, 0.9];
        let opacity = 1.0;

        let cpu: Vec<f32> = running
            .iter()
            .zip(&weight)
            .map(|(&r, &w): (&f32, &f32)| (r - (1.0 - w) * opacity).max(0.0))
            .collect();
        let gpu = run_compose_step(&ctx, 2, 2, &running, &weight, true, 1, opacity);
        assert_fields_close(&cpu, &gpu, "compose_subtract_invert");
    }
}

#[test]
fn compose_step_intersect_gpu_matches_cpu() {
    for ctx in require_contexts() {
        let running = vec![0.5, 1.0, 0.0, 0.4];
        let weight = vec![0.5, 0.25, 0.9, 0.6];
        let opacity = 1.0;

        let cpu: Vec<f32> = running.iter().zip(&weight).map(|(&r, &w)| r * w).collect();
        let gpu = run_compose_step(&ctx, 2, 2, &running, &weight, false, 2, opacity);
        assert_fields_close(&cpu, &gpu, "compose_intersect");
    }
}

#[test]
fn masked_adjust_apply_gpu_matches_cpu() {
    for ctx in require_contexts() {
        let pixels = vec![
            GpuPixel {
                rgb: [0.5, 0.4, 0.3],
                _pad: 0.0,
            },
            GpuPixel {
                rgb: [0.1, 0.2, 0.3],
                _pad: 0.0,
            },
            GpuPixel {
                rgb: [1.0, 1.0, 1.0],
                _pad: 0.0,
            },
            GpuPixel {
                rgb: [0.0, 0.0, 0.0],
                _pad: 0.0,
            },
        ];
        let mask = vec![1.0, 0.5, 0.0, 1.0];
        let exposure_ev = 1.0;

        let cpu: Vec<[f32; 3]> = pixels
            .iter()
            .zip(&mask)
            .map(|(px, &m)| {
                let scale = 2f32.powf(exposure_ev * m);
                [px.rgb[0] * scale, px.rgb[1] * scale, px.rgb[2] * scale]
            })
            .collect();
        let gpu = run_masked_adjust_apply(&ctx, 2, 2, &pixels, &mask, exposure_ev);

        for (i, (c, g)) in cpu.iter().zip(gpu.iter()).enumerate() {
            for (ch, (&c_ch, &g_ch)) in c.iter().zip(g.rgb.iter()).enumerate() {
                let diff = (c_ch - g_ch).abs();
                assert!(
                    diff < TOLERANCE,
                    "masked_adjust_apply: pixel {i} channel {ch} cpu={c_ch} gpu={g_ch} diff={diff}"
                );
            }
        }
    }
}
