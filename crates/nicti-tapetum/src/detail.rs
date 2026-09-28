//! CPU reference for the Detail pass: classic (non-AI) manual noise reduction and sharpening
//! (#46). Both need neighboring pixels, unlike every other live stage in this crate (a plain
//! per-pixel map) -- see this crate's `stages.rs` doc comment on `LiveSuffixKernel` for how the
//! GPU side turns this into extra compute passes inside one `LiveExec::encode` call, keeping the
//! render graph's "one live dispatch" invariant.
//!
//! Both effects share one separable Gaussian blur of the luma plane (`gaussian_blur`), the same
//! way LRC's own Detail panel computes a single blurred reference and derives both sliders' output
//! from it, rather than running two independent blurs.

use crate::coat::{NoiseReductionParams, SharpenParams};

/// A row-major `width * height` plane, e.g. luma or one chroma channel.
#[derive(Debug, Clone, Copy)]
pub struct Extent2D {
    pub width: usize,
    pub height: usize,
}

/// The GPU blur shader (`detail_blur.wgsl`) uses a fixed-size uniform tap array (WGSL's
/// uniform-address-space array rules force `vec4`-grouped storage, so a resizable `Vec` can't
/// cross that boundary) -- `MAX_BLUR_RADIUS` bounds how far out a single blur pass reaches,
/// covering every sigma this crate's own NR/sharpen sliders can produce in practice
/// (`SharpenParams::radius_px`'s documented max is 3.0).
pub const MAX_BLUR_RADIUS: usize = 17;
const KERNEL_TAPS: usize = 2 * MAX_BLUR_RADIUS + 1;

/// A fixed-length (`KERNEL_TAPS`), zero-padded-beyond-its-own-radius Gaussian kernel -- `sigma`'s
/// natural radius (`ceil(sigma*3)`, capped at [`MAX_BLUR_RADIUS`]) is where the real weights live;
/// every tap beyond that is `0.0`. Fixed length (not just a fixed cap) so `stages.rs`'s GPU blur
/// kernel can upload these same values into its own fixed-size uniform array with no reshaping --
/// its own GPU-vs-CPU parity test proves the two stay bit-for-bit identical.
pub fn gaussian_kernel(sigma: f32) -> [f32; KERNEL_TAPS] {
    let mut kernel = [0.0f32; KERNEL_TAPS];
    if sigma <= 0.0 {
        kernel[MAX_BLUR_RADIUS] = 1.0;
        return kernel;
    }
    let radius = ((sigma * 3.0).ceil() as usize).clamp(1, MAX_BLUR_RADIUS);
    let mut sum = 0.0f32;
    for offset in -(radius as isize)..=(radius as isize) {
        let w = (-((offset * offset) as f32) / (2.0 * sigma * sigma)).exp();
        kernel[(offset + MAX_BLUR_RADIUS as isize) as usize] = w;
        sum += w;
    }
    for w in kernel.iter_mut() {
        *w /= sum;
    }
    kernel
}

/// A separable Gaussian blur with a clamp-to-edge border (a pixel just off one side samples the
/// edge pixel's own value, rather than wrapping or reading out of bounds) -- the conventional
/// choice for a photo-editing blur, where a wrap-around or a hard zero border would visibly smear
/// the image's actual edges.
pub fn gaussian_blur(plane: &[f32], extent: Extent2D, sigma: f32) -> Vec<f32> {
    let kernel = gaussian_kernel(sigma);
    let radius = MAX_BLUR_RADIUS as isize;

    let Extent2D { width, height } = extent;
    let sample = |plane: &[f32], x: isize, y: isize| -> f32 {
        let cx = x.clamp(0, width as isize - 1) as usize;
        let cy = y.clamp(0, height as isize - 1) as usize;
        plane[cy * width + cx]
    };

    let mut horizontal = vec![0.0f32; plane.len()];
    for y in 0..height as isize {
        for x in 0..width as isize {
            let mut acc = 0.0;
            for (i, k) in kernel.iter().enumerate() {
                acc += k * sample(plane, x + i as isize - radius, y);
            }
            horizontal[y as usize * width + x as usize] = acc;
        }
    }

    let mut out = vec![0.0f32; plane.len()];
    for y in 0..height as isize {
        for x in 0..width as isize {
            let mut acc = 0.0;
            for (i, k) in kernel.iter().enumerate() {
                acc += k * sample(&horizontal, x, y + i as isize - radius);
            }
            out[y as usize * width + x as usize] = acc;
        }
    }
    out
}

/// One pixel's combined noise-reduction + sharpen result, given its original value, the same
/// pixel's NR-radius blur and sharpen-radius blur, and an edge weight in `0.0..=1.0` (1.0 = a flat
/// region, 0.0 = a strong edge) that both `detail` sliders use to protect real detail from being
/// smoothed away or ringing from being amplified.
///
/// Order: NR first (denoise before judging what counts as a real edge to sharpen), then
/// sharpening's unsharp mask on the *denoised* value -- sharpening a still-noisy image would
/// amplify the noise right back.
pub fn apply_detail(
    original: f32,
    blurred_nr: f32,
    blurred_sharpen: f32,
    edge_weight: f32,
    nr: &NoiseReductionParams,
    sharpen: &SharpenParams,
) -> f32 {
    // `detail` protects edges from being smoothed: at detail=1.0, an edge (edge_weight near 0)
    // gets almost no blur-blend regardless of `luminance`'s amount.
    let nr_blend = nr.luminance * (1.0 - nr.detail * (1.0 - edge_weight));
    let denoised = original + (blurred_nr - original) * nr_blend.clamp(0.0, 1.0);

    // `detail` here damps sharpening in already-flat regions (there's no edge to sharpen), the
    // opposite protection direction from NR's own `detail` above.
    let sharpen_gate = sharpen.detail * (1.0 - edge_weight) + (1.0 - sharpen.detail);
    let unsharp = denoised - blurred_sharpen;
    denoised + sharpen.amount * unsharp * sharpen_gate.clamp(0.0, 1.0)
}

/// Rec.709 luma weights -- the same constants `color.rs`'s own tone/vibrance/HSL functions use,
/// kept consistent across every stage that needs a luma estimate.
const LUMA_WEIGHTS: [f32; 3] = [0.2126, 0.7152, 0.0722];

fn luma(rgb: [f32; 3]) -> f32 {
    rgb[0] * LUMA_WEIGHTS[0] + rgb[1] * LUMA_WEIGHTS[1] + rgb[2] * LUMA_WEIGHTS[2]
}

/// Fixed local-contrast scale [`edge_weight`] uses to decide "edge" vs "noise" -- a v1 constant,
/// not user-adjustable; a future pass could derive this from the image's own noise floor instead.
pub const EDGE_SCALE: f32 = 0.08;

/// Combines [`apply_detail`] and [`edge_weight`] into a full RGB pixel: Noise Reduction and
/// Sharpening both operate on **luma only** ([`apply_detail`], reusing `nr.luminance` and the
/// whole `sharpen` unsharp-mask step -- sharpening a color channel independently would fringe
/// edges with color), while `nr.color` is a separate, ungated lerp of the **chroma** (the
/// original minus its own luma, a 3-vector) toward its own blurred-nr chroma -- LRC's own
/// Luminance/Color Noise Reduction split, collapsed here into "one shared blur, two different
/// blend targets" rather than two independent blurs.
pub fn apply_detail_rgb(
    original: [f32; 3],
    blurred_nr: [f32; 3],
    blurred_sharpen: [f32; 3],
    nr: &NoiseReductionParams,
    sharpen: &SharpenParams,
) -> [f32; 3] {
    let orig_luma = luma(original);
    let nr_luma = luma(blurred_nr);
    let sharpen_luma = luma(blurred_sharpen);
    let weight = edge_weight(orig_luma, nr_luma, EDGE_SCALE);
    let new_luma = apply_detail(orig_luma, nr_luma, sharpen_luma, weight, nr, sharpen);

    let color_amount = nr.color.clamp(0.0, 1.0);
    std::array::from_fn(|i| {
        let chroma_orig = original[i] - orig_luma;
        let chroma_blurred = blurred_nr[i] - nr_luma;
        let new_chroma = chroma_orig + (chroma_blurred - chroma_orig) * color_amount;
        new_luma + new_chroma
    })
}

/// A simple, stable edge-weight estimate: how close the fine-radius blur is to the original --
/// far apart (a real edge) gives a weight near 0, close together (a flat region) gives a weight
/// near 1. `scale` controls how much local contrast counts as "an edge" versus "noise" -- a larger
/// scale calls more of the image flat.
pub fn edge_weight(original: f32, fine_blur: f32, scale: f32) -> f32 {
    let diff = (original - fine_blur).abs();
    (1.0 - diff / scale.max(1e-4)).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gaussian_blur_is_a_no_op_at_zero_sigma() {
        let plane = vec![0.1, 0.9, 0.2, 0.8];
        let out = gaussian_blur(
            &plane,
            Extent2D {
                width: 2,
                height: 2,
            },
            0.0,
        );
        assert_eq!(out, plane);
    }

    #[test]
    fn gaussian_blur_leaves_a_constant_plane_unchanged() {
        let plane = vec![0.5f32; 16];
        let out = gaussian_blur(
            &plane,
            Extent2D {
                width: 4,
                height: 4,
            },
            1.5,
        );
        for (a, b) in plane.iter().zip(out.iter()) {
            assert!((a - b).abs() < 1e-4, "{a} vs {b}");
        }
    }

    #[test]
    fn gaussian_blur_smooths_a_sharp_edge() {
        // A 1x8 row, half 0.0 half 1.0 -- the pixel right at the boundary should move toward the
        // average of its neighborhood after blurring, not stay pinned at the edge value.
        let plane = vec![0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0];
        let out = gaussian_blur(
            &plane,
            Extent2D {
                width: 8,
                height: 1,
            },
            1.5,
        );
        assert!(out[3] > 0.0 && out[3] < 0.5, "out[3]={}", out[3]);
        assert!(out[4] > 0.5 && out[4] < 1.0, "out[4]={}", out[4]);
    }

    #[test]
    fn gaussian_blur_clamps_at_the_border_instead_of_wrapping() {
        let plane = vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let out = gaussian_blur(
            &plane,
            Extent2D {
                width: 8,
                height: 1,
            },
            1.0,
        );
        // If the border wrapped, out[7] (the far end) would pick up some of the spike at out[0].
        assert!(
            out[7] < 1e-3,
            "wrap-around leaked into the far border: {}",
            out[7]
        );
    }

    #[test]
    fn apply_detail_with_zero_params_is_identity() {
        let out = apply_detail(
            0.5,
            0.3,
            0.7,
            0.8,
            &NoiseReductionParams::default(),
            &SharpenParams::default(),
        );
        assert!((out - 0.5).abs() < 1e-6);
    }

    #[test]
    fn apply_detail_nr_blends_toward_the_blur_in_flat_regions() {
        let nr = NoiseReductionParams {
            luminance: 1.0,
            detail: 0.0,
            ..Default::default()
        };
        let out = apply_detail(0.5, 0.3, 0.5, 1.0, &nr, &SharpenParams::default());
        assert!((out - 0.3).abs() < 1e-4, "out={out}");
    }

    #[test]
    fn apply_detail_nr_protects_edges_when_detail_is_high() {
        let nr = NoiseReductionParams {
            luminance: 1.0,
            detail: 1.0,
            ..Default::default()
        };
        // edge_weight=0.0 means "this is a real edge" -- high detail should mostly preserve it.
        let out = apply_detail(0.5, 0.0, 0.5, 0.0, &nr, &SharpenParams::default());
        assert!(
            (out - 0.5).abs() < 1e-3,
            "a protected edge should barely move toward the blur: {out}"
        );
    }

    #[test]
    fn apply_detail_sharpen_increases_local_contrast() {
        let sharpen = SharpenParams {
            amount: 1.0,
            radius_px: 1.0,
            detail: 1.0,
        };
        // original brighter than its own blur -> sharpening should push it brighter still.
        // edge_weight=0.0 ("this is a real edge") so `detail`=1.0 doesn't gate the sharpen away.
        let out = apply_detail(
            0.6,
            0.6,
            0.4,
            0.0,
            &NoiseReductionParams::default(),
            &sharpen,
        );
        assert!(out > 0.6, "sharpening should amplify the edge: {out}");
    }

    #[test]
    fn apply_detail_rgb_with_zero_params_is_identity() {
        let rgb = [0.3, 0.6, 0.1];
        let out = apply_detail_rgb(
            rgb,
            [0.35, 0.5, 0.15],
            [0.25, 0.55, 0.2],
            &NoiseReductionParams::default(),
            &SharpenParams::default(),
        );
        for (a, b) in rgb.iter().zip(out.iter()) {
            assert!((a - b).abs() < 1e-5, "{a} vs {b}");
        }
    }

    #[test]
    fn apply_detail_rgb_color_amount_desaturates_toward_the_blur() {
        let original = [0.8, 0.2, 0.2]; // saturated red
        let blurred_nr = [0.4, 0.4, 0.4]; // neutral gray blur (same luma-ish neighborhood)
        let nr = NoiseReductionParams {
            color: 1.0,
            ..Default::default()
        };
        let out = apply_detail_rgb(
            original,
            blurred_nr,
            original,
            &nr,
            &SharpenParams::default(),
        );
        let out_delta = out[0].max(out[1]).max(out[2]) - out[0].min(out[1]).min(out[2]);
        let in_delta = original[0].max(original[1]).max(original[2])
            - original[0].min(original[1]).min(original[2]);
        assert!(
            out_delta < in_delta,
            "full color NR should desaturate toward the neutral blur: {out_delta} vs {in_delta}"
        );
    }

    #[test]
    fn apply_detail_rgb_preserves_luma_when_only_color_nr_is_active() {
        // Color NR reshuffles chroma but the reconstruction re-adds new_luma unchanged from
        // original (luminance NR/sharpen both off) -- the pixel's own luma should barely move.
        let original = [0.8, 0.2, 0.2];
        let nr = NoiseReductionParams {
            color: 1.0,
            ..Default::default()
        };
        let out = apply_detail_rgb(
            original,
            [0.4, 0.4, 0.4],
            original,
            &nr,
            &SharpenParams::default(),
        );
        let orig_luma = luma(original);
        let out_luma = luma(out);
        assert!(
            (orig_luma - out_luma).abs() < 1e-3,
            "{orig_luma} vs {out_luma}"
        );
    }

    #[test]
    fn edge_weight_is_one_on_a_flat_region_and_near_zero_on_a_strong_edge() {
        assert!((edge_weight(0.5, 0.5, 0.1) - 1.0).abs() < 1e-6);
        assert!(edge_weight(0.9, 0.1, 0.1) < 0.1);
    }
}
