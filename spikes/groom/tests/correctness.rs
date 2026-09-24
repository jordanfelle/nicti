//! Checks the WGSL `poisson_jacobi` kernel against `cpu_reference::poisson_jacobi_cpu` on a small
//! synthetic patch, generated procedurally here (no external fixture). Skips cleanly (prints a
//! message, doesn't fail) when no wgpu adapter is available at all -- mirrors
//! `spikes/glint/tests/correctness.rs`'s exact pattern for CI runners without a GPU or software
//! Vulkan ICD.

use groom::cpu_reference::poisson_jacobi_cpu;
use groom::gpu::{run_poisson_jacobi, GpuContext};

const TOLERANCE: f32 = 1e-3;

fn require_contexts() -> Vec<GpuContext> {
    let contexts = GpuContext::enumerate();
    if contexts.is_empty() {
        eprintln!("correctness: no wgpu adapter available, skipping");
    }
    contexts
}

/// A small synthetic patch: a smooth gradient `guidance` field and a checkerboard-ish `initial`
/// field, with a circular interior mask -- enough structure that a broken Jacobi update (wrong
/// neighbor sum, wrong boundary handling, off-by-one indexing) would visibly diverge from the CPU
/// reference, without needing a real image fixture.
type Patch = (Vec<[f32; 4]>, Vec<[f32; 4]>, Vec<bool>, Vec<u32>);

fn make_patch(width: usize, height: usize) -> Patch {
    let mut guidance = Vec::with_capacity(width * height);
    let mut initial = Vec::with_capacity(width * height);
    let mut mask_bool = Vec::with_capacity(width * height);
    let cx = width as f32 / 2.0;
    let cy = height as f32 / 2.0;
    let radius = (width.min(height) as f32) * 0.3;

    for y in 0..height {
        for x in 0..width {
            let fx = x as f32 / width as f32;
            let fy = y as f32 / height as f32;
            guidance.push([fx, fy, (fx + fy) * 0.5, 1.0]);

            let v = if (x / 3 + y / 3) % 2 == 0 { 0.8 } else { 0.2 };
            initial.push([v, v, v, 1.0]);

            let dist = (((x as f32 - cx).powi(2)) + ((y as f32 - cy).powi(2))).sqrt();
            mask_bool.push(dist < radius);
        }
    }
    let mask_u32: Vec<u32> = mask_bool.iter().map(|&b| b as u32).collect();
    (guidance, initial, mask_bool, mask_u32)
}

#[test]
fn poisson_jacobi_gpu_matches_cpu_reference() {
    let width = 16;
    let height = 16;
    let iterations = 12;
    let (guidance, initial, mask_bool, mask_u32) = make_patch(width, height);

    let cpu_out = poisson_jacobi_cpu(&guidance, &initial, &mask_bool, width, height, iterations);

    for ctx in require_contexts() {
        let gpu_out = run_poisson_jacobi(
            &ctx,
            &guidance,
            &initial,
            &mask_u32,
            width as u32,
            height as u32,
            iterations,
        );
        assert_eq!(gpu_out.len(), cpu_out.len());
        for (i, (cpu_px, gpu_px)) in cpu_out.iter().zip(gpu_out.iter()).enumerate() {
            for c in 0..3 {
                assert!(
                    (cpu_px[c] - gpu_px[c]).abs() < TOLERANCE,
                    "backend {:?} pixel {i} channel {c}: cpu={} gpu={}",
                    ctx.backend,
                    cpu_px[c],
                    gpu_px[c]
                );
            }
        }
    }
}

#[test]
fn poisson_jacobi_gpu_matches_cpu_reference_at_zero_iterations() {
    // A degenerate but real edge case: zero iterations should just return `initial` unchanged on
    // both backends, proving the ping-pong buffer-selection logic (`iterations % 2`) handles the
    // zero case correctly rather than only ever being exercised at odd/even iteration counts.
    let width = 8;
    let height = 8;
    let (guidance, initial, mask_bool, mask_u32) = make_patch(width, height);
    let cpu_out = poisson_jacobi_cpu(&guidance, &initial, &mask_bool, width, height, 0);
    assert_eq!(cpu_out, initial);

    for ctx in require_contexts() {
        let gpu_out = run_poisson_jacobi(
            &ctx,
            &guidance,
            &initial,
            &mask_u32,
            width as u32,
            height as u32,
            0,
        );
        for (a, b) in initial.iter().zip(gpu_out.iter()) {
            for c in 0..4 {
                assert!((a[c] - b[c]).abs() < TOLERANCE);
            }
        }
    }
}
