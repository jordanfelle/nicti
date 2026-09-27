//! Full-reference quality metrics for #40/ADR-0025's demosaic+denoise comparison: PSNR and a
//! public f32 SSIM, operating on the `f32` sample arrays `spikes/rods`'s alignment step produces
//! (display-encoded, 0..1 per channel) -- distinct from `golden.rs`'s SSIM, which is 8-bit-only
//! and private to that module's `GoldenStore` use case. No shared implementation: `golden.rs`'s
//! constants (`L=255.0`) are wrong for a 0..1 float signal, and duplicating a ~30-line function
//! is cheaper than parameterizing both call sites over dynamic range for one shared use.

/// Peak signal-to-noise ratio in dB between two equal-length `f32` sample buffers, values assumed
/// in `0.0..=1.0` (`peak = 1.0`). Returns `f64::INFINITY` for identical inputs (MSE == 0) rather
/// than panicking on the `log10(0)`/division-by-zero that would otherwise produce `inf` anyway --
/// making the identical-input case explicit here means a caller comparing against a threshold
/// doesn't need its own NaN/inf special-casing.
///
/// Panics if `a.len() != b.len()`.
pub fn psnr(a: &[f32], b: &[f32]) -> f64 {
    assert_eq!(a.len(), b.len(), "psnr requires equal-length inputs");
    if a.is_empty() {
        return f64::INFINITY;
    }
    let mse: f64 = a
        .iter()
        .zip(b)
        .map(|(&x, &y)| {
            let d = (x - y) as f64;
            d * d
        })
        .sum::<f64>()
        / a.len() as f64;
    if mse == 0.0 {
        return f64::INFINITY;
    }
    // PSNR = 10*log10(peak^2 / mse); peak = 1.0 for a 0..1-normalized signal, so peak^2 = 1.0.
    -10.0 * mse.log10()
}

/// Single-scale SSIM over 8x8 non-overlapping windows, Wang et al.'s standard formula, for a
/// single-channel `f32` plane in `0.0..=1.0` (`L = 1.0`, unlike `golden.rs`'s 8-bit `L = 255.0`).
/// Returns a score in roughly `[-1.0, 1.0]`; `1.0` is identical.
///
/// Panics if `a.len() != b.len()` or either isn't exactly `width * height` samples.
pub fn ssim(a: &[f32], b: &[f32], width: u32, height: u32) -> f64 {
    assert_eq!(a.len(), b.len(), "ssim requires equal-length inputs");
    assert_eq!(
        a.len(),
        width as usize * height as usize,
        "ssim requires exactly width*height samples"
    );

    const WINDOW: u32 = 8;
    const L: f64 = 1.0;
    const C1: f64 = (0.01 * L) * (0.01 * L);
    const C2: f64 = (0.03 * L) * (0.03 * L);

    let mut total = 0.0;
    let mut windows = 0usize;

    let mut y = 0;
    while y < height {
        let win_h = WINDOW.min(height - y);
        let mut x = 0;
        while x < width {
            let win_w = WINDOW.min(width - x);
            let n = (win_w * win_h) as f64;

            let (mut sum_a, mut sum_b) = (0.0, 0.0);
            for wy in y..y + win_h {
                for wx in x..x + win_w {
                    let idx = (wy * width + wx) as usize;
                    sum_a += a[idx] as f64;
                    sum_b += b[idx] as f64;
                }
            }
            let mean_a = sum_a / n;
            let mean_b = sum_b / n;

            let (mut var_a, mut var_b, mut covar) = (0.0, 0.0, 0.0);
            for wy in y..y + win_h {
                for wx in x..x + win_w {
                    let idx = (wy * width + wx) as usize;
                    let da = a[idx] as f64 - mean_a;
                    let db = b[idx] as f64 - mean_b;
                    var_a += da * da;
                    var_b += db * db;
                    covar += da * db;
                }
            }
            var_a /= n;
            var_b /= n;
            covar /= n;

            let numerator = (2.0 * mean_a * mean_b + C1) * (2.0 * covar + C2);
            let denominator = (mean_a * mean_a + mean_b * mean_b + C1) * (var_a + var_b + C2);
            total += numerator / denominator;
            windows += 1;

            x += WINDOW;
        }
        y += WINDOW;
    }

    if windows == 0 {
        1.0
    } else {
        total / windows as f64
    }
}

/// [`ssim`] averaged over R/G/B planes given as one interleaved `f32` buffer (`rgb.len() ==
/// width*height*3`) -- see `golden.rs`'s own doc comment on why per-channel averaging (not luma)
/// matters for this project: a color-only shift can hold luma constant while a luma-only SSIM
/// would score it a perfect match.
pub fn ssim_rgb(a_rgb: &[f32], b_rgb: &[f32], width: u32, height: u32) -> f64 {
    let pixels = width as usize * height as usize;
    assert_eq!(a_rgb.len(), pixels * 3, "a_rgb must be width*height*3");
    assert_eq!(b_rgb.len(), pixels * 3, "b_rgb must be width*height*3");

    let deinterleave = |rgb: &[f32], channel: usize| -> Vec<f32> {
        rgb.as_chunks::<3>()
            .0
            .iter()
            .map(|px| px[channel])
            .collect()
    };

    let scores: [f64; 3] = std::array::from_fn(|channel| {
        let a = deinterleave(a_rgb, channel);
        let b = deinterleave(b_rgb, channel);
        ssim(&a, &b, width, height)
    });
    scores.iter().sum::<f64>() / 3.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn psnr_identical_is_infinite() {
        let a = vec![0.1f32, 0.5, 0.9, 0.2];
        assert_eq!(psnr(&a, &a), f64::INFINITY);
    }

    #[test]
    fn psnr_known_gaussian_matches_formula() {
        // MSE = 0.01 exactly -> PSNR = 10*log10(1/0.01) = 20 dB.
        let a = vec![0.5f32; 100];
        let b = vec![0.4f32; 100]; // diff 0.1, squared = 0.01
        let score = psnr(&a, &b);
        assert!((score - 20.0).abs() < 1e-6, "expected 20.0 dB, got {score}");
    }

    #[test]
    fn psnr_empty_is_infinite() {
        assert_eq!(psnr(&[], &[]), f64::INFINITY);
    }

    fn checkerboard(width: u32, height: u32, lo: f32, hi: f32) -> Vec<f32> {
        (0..height)
            .flat_map(|y| (0..width).map(move |x| if (x / 4 + y / 4) % 2 == 0 { hi } else { lo }))
            .collect()
    }

    #[test]
    fn ssim_identical_scores_near_one() {
        let img = checkerboard(32, 32, 0.1, 0.9);
        let score = ssim(&img, &img, 32, 32);
        assert!(score > 0.999, "expected near-1.0, got {score}");
    }

    #[test]
    fn ssim_detects_heavy_perturbation() {
        // A tonal inversion keeps the same edges/structure a naive metric might latch onto, but
        // flips light<->dark, driving the covariance term negative -- a uniform brightness/gain
        // shift (tried first) doesn't stress SSIM enough, since its luminance term partially
        // compensates for a shared mean shift while structure stays perfectly correlated.
        let a = checkerboard(32, 32, 0.1, 0.9);
        let b: Vec<f32> = a.iter().map(|&v| 1.0 - v).collect();
        let score = ssim(&a, &b, 32, 32);
        assert!(score < 0.5, "expected a low score, got {score}");
    }

    #[test]
    #[should_panic(expected = "equal-length")]
    fn psnr_rejects_length_mismatch() {
        psnr(&[0.1, 0.2], &[0.1]);
    }

    #[test]
    #[should_panic(expected = "width*height")]
    fn ssim_rejects_dimension_mismatch() {
        ssim(&[0.1, 0.2, 0.3, 0.4], &[0.1, 0.2, 0.3, 0.4], 3, 3);
    }

    #[test]
    fn ssim_rgb_detects_color_only_shift_luma_constant() {
        // Same construction as golden.rs's regression test: red vs green, luma-equal to within
        // rounding, obviously not the same color.
        let pixels = 16 * 16;
        let red: Vec<f32> = std::iter::repeat_n([1.0f32, 0.0, 0.0], pixels)
            .flatten()
            .collect();
        let green: Vec<f32> = std::iter::repeat_n([0.0f32, 130.0 / 255.0, 0.0], pixels)
            .flatten()
            .collect();
        let score = ssim_rgb(&red, &green, 16, 16);
        assert!(score < 0.5, "expected a low score, got {score}");
    }
}
