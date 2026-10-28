//! Output sharpening (#339): the LRC "Sharpen for screen / matte paper / glossy paper, low /
//! standard / high" step that runs on the *resized* image, because resampling softens detail and
//! the right amount depends on where the file ends up.
//!
//! An unsharp mask on luminance only: the difference between a pixel's perceptual luma and a
//! Gaussian blur of it is added back, scaled, with a small noise threshold and a hard cap on the
//! correction so a high-contrast edge cannot ring without limit. Hue is preserved (the pixel's
//! linear RGB is scaled by the luma ratio, never offset per channel), and pixels over 1.0 are left
//! alone. It runs on the output-space linear frame after the resize and before the watermark, so a
//! logo is never sharpened.
//!
//! **Untuned.** The radius and amount tables below are reasoned starting points, not matched to
//! Lightroom Classic's (unpublished) output; they need a visual comparison on the reference
//! machine and real prints. Paper radii scale with the export's DPI (more pixels per inch means a
//! wider correction for the same print-size halo); screen output ignores the DPI.

use serde::{Deserialize, Serialize};

/// Where the exported file is going.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SharpenMedia {
    #[default]
    Screen,
    Matte,
    Glossy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SharpenAmount {
    Low,
    #[default]
    Standard,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SharpenSpec {
    pub media: SharpenMedia,
    pub amount: SharpenAmount,
}

/// Blur radius (sigma, px) and strength for `spec` at `dpi`.
pub fn params(spec: SharpenSpec, dpi: u32) -> (f32, f32) {
    let dpi_scale = (dpi as f32 / 300.0).clamp(0.5, 2.0);
    let (radius, gains): (f32, [f32; 3]) = match spec.media {
        SharpenMedia::Screen => (0.6, [0.35, 0.6, 0.9]),
        SharpenMedia::Matte => (1.0 * dpi_scale, [0.5, 0.85, 1.3]),
        SharpenMedia::Glossy => (0.8 * dpi_scale, [0.4, 0.7, 1.1]),
    };
    let amount = match spec.amount {
        SharpenAmount::Low => gains[0],
        SharpenAmount::Standard => gains[1],
        SharpenAmount::High => gains[2],
    };
    (radius, amount)
}

/// Corrections smaller than this (in encoded luma) are noise, not detail.
const THRESHOLD: f32 = 0.0025;
/// The largest correction applied to one pixel (encoded luma), so an edge cannot ring wildly.
const MAX_DELTA: f32 = 0.25;
/// The most a pixel's linear RGB is scaled by.
const MAX_GAIN: f32 = 8.0;

fn oetf(l: f32) -> f32 {
    if l <= 0.003_130_8 {
        l * 12.92
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    }
}

fn eotf(e: f32) -> f32 {
    if e <= 0.040_45 {
        e / 12.92
    } else {
        ((e + 0.055) / 1.055).powf(2.4)
    }
}

/// Normalised 1-D Gaussian taps for `sigma`.
fn kernel(sigma: f32) -> Vec<f32> {
    let sigma = sigma.max(0.3);
    let half = (sigma * 3.0).ceil() as i32;
    let mut k: Vec<f32> = (-half..=half)
        .map(|i| (-(i * i) as f32 / (2.0 * sigma * sigma)).exp())
        .collect();
    let sum: f32 = k.iter().sum();
    k.iter_mut().for_each(|v| *v /= sum);
    k
}

/// Separable Gaussian blur of a `w` x `h` plane, edge pixels replicated.
fn blur(src: &[f32], w: usize, h: usize, sigma: f32) -> Vec<f32> {
    let k = kernel(sigma);
    let half = (k.len() / 2) as isize;
    let mut tmp = vec![0.0f32; src.len()];
    for y in 0..h {
        let row = &src[y * w..(y + 1) * w];
        for x in 0..w {
            let mut acc = 0.0;
            for (i, &kv) in k.iter().enumerate() {
                let sx = (x as isize + i as isize - half).clamp(0, w as isize - 1) as usize;
                acc += kv * row[sx];
            }
            tmp[y * w + x] = acc;
        }
    }
    let mut out = vec![0.0f32; src.len()];
    for y in 0..h {
        for x in 0..w {
            let mut acc = 0.0;
            for (i, &kv) in k.iter().enumerate() {
                let sy = (y as isize + i as isize - half).clamp(0, h as isize - 1) as usize;
                acc += kv * tmp[sy * w + x];
            }
            out[y * w + x] = acc;
        }
    }
    out
}

/// Sharpens interleaved linear output-space RGB (`w` x `h`) in place.
pub fn apply(pixels: &mut [f32], w: u32, h: u32, spec: SharpenSpec, dpi: u32) {
    let (w, h) = (w as usize, h as usize);
    if w == 0 || h == 0 || pixels.len() != w * h * 3 {
        return;
    }
    let (radius, amount) = params(spec, dpi);
    // Perceptual luma of every in-range pixel; out-of-range ones are marked NaN and skipped.
    let luma: Vec<f32> = pixels
        .as_chunks::<3>()
        .0
        .iter()
        .map(|p| {
            let y = 0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2];
            if y.is_finite() && (0.0..=1.0).contains(&y) {
                oetf(y)
            } else {
                f32::NAN
            }
        })
        .collect();
    // Blur needs a defined value everywhere: substitute neighbours-agnostic mid-grey for the
    // skipped pixels (they are rare, and are not themselves modified).
    let filled: Vec<f32> = luma
        .iter()
        .map(|&v| if v.is_nan() { 0.5 } else { v })
        .collect();
    let blurred = blur(&filled, w, h, radius);
    for (i, p) in pixels.as_chunks_mut::<3>().0.iter_mut().enumerate() {
        let e = luma[i];
        if e.is_nan() {
            continue;
        }
        let diff = (e - blurred[i]) * amount;
        let delta = diff.signum() * (diff.abs() - THRESHOLD).max(0.0);
        if delta == 0.0 {
            continue;
        }
        let e2 = (e + delta.clamp(-MAX_DELTA, MAX_DELTA)).clamp(0.0, 1.0);
        let (y_old, y_new) = (eotf(e), eotf(e2));
        if y_old > 1e-6 {
            let gain = (y_new / y_old).min(MAX_GAIN);
            p.iter_mut().for_each(|c| *c *= gain);
        } else {
            // Near-black: no hue to preserve, add the luma change to every channel.
            let add = y_new - y_old;
            p.iter_mut().for_each(|c| *c += add);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STD: SharpenSpec = SharpenSpec {
        media: SharpenMedia::Screen,
        amount: SharpenAmount::Standard,
    };

    /// A vertical step edge: left half `a`, right half `b` (grey, linear).
    fn step(w: u32, h: u32, a: f32, b: f32) -> Vec<f32> {
        (0..w * h)
            .flat_map(|i| {
                let v = if (i % w) < w / 2 { a } else { b };
                [v, v, v]
            })
            .collect()
    }

    fn at(p: &[f32], w: u32, x: u32, y: u32) -> f32 {
        p[((y * w + x) * 3) as usize]
    }

    #[test]
    fn a_flat_image_is_untouched() {
        let mut p = vec![0.3f32; 16 * 16 * 3];
        let orig = p.clone();
        apply(&mut p, 16, 16, STD, 300);
        assert_eq!(p, orig);
    }

    #[test]
    fn an_edge_gets_overshoot_on_the_bright_side_and_undershoot_on_the_dark_side() {
        let (w, h) = (32, 8);
        let mut p = step(w, h, 0.1, 0.5);
        apply(&mut p, w, h, STD, 300);
        let (dark_near, dark_far) = (at(&p, w, w / 2 - 1, 4), at(&p, w, 2, 4));
        let (bright_near, bright_far) = (at(&p, w, w / 2, 4), at(&p, w, w - 3, 4));
        assert!(dark_near < dark_far - 0.002, "{dark_near} vs {dark_far}");
        assert!(
            bright_near > bright_far + 0.002,
            "{bright_near} vs {bright_far}"
        );
        // Far from the edge nothing changes.
        assert!((dark_far - 0.1).abs() < 1e-3 && (bright_far - 0.5).abs() < 1e-3);
    }

    #[test]
    fn a_higher_amount_corrects_more_and_media_differ() {
        let (w, h) = (32, 8);
        let edge = |spec: SharpenSpec, dpi: u32| {
            let mut p = step(w, h, 0.1, 0.5);
            apply(&mut p, w, h, spec, dpi);
            at(&p, w, w / 2, 4) - 0.5
        };
        let mk = |media, amount| SharpenSpec { media, amount };
        let low = edge(mk(SharpenMedia::Screen, SharpenAmount::Low), 300);
        let std = edge(mk(SharpenMedia::Screen, SharpenAmount::Standard), 300);
        let high = edge(mk(SharpenMedia::Screen, SharpenAmount::High), 300);
        assert!(0.0 < low && low < std && std < high, "{low} {std} {high}");
        let matte = edge(mk(SharpenMedia::Matte, SharpenAmount::Standard), 300);
        assert!(matte > std, "matte {matte} vs screen {std}");
    }

    #[test]
    fn paper_radius_scales_with_dpi_but_screen_ignores_it() {
        let paper = SharpenSpec {
            media: SharpenMedia::Matte,
            amount: SharpenAmount::Standard,
        };
        assert!(params(paper, 600).0 > params(paper, 300).0);
        assert!(params(paper, 150).0 < params(paper, 300).0);
        assert_eq!(params(STD, 72), params(STD, 600));
        // Clamped: an absurd DPI cannot blow up the kernel.
        assert!(params(paper, 10_000).0 <= 2.0 * 1.0 + 1e-6);
    }

    #[test]
    fn hue_is_preserved_across_a_coloured_edge() {
        let (w, h) = (32u32, 8u32);
        let mut p: Vec<f32> = (0..w * h)
            .flat_map(|i| {
                if (i % w) < w / 2 {
                    [0.05, 0.1, 0.2]
                } else {
                    [0.4, 0.3, 0.15]
                }
            })
            .collect();
        let before = p.clone();
        apply(&mut p, w, h, STD, 300);
        for i in 0..(w * h) as usize {
            let (a, b) = (&before[i * 3..i * 3 + 3], &p[i * 3..i * 3 + 3]);
            // The linear channel ratios survive: only the overall level moves.
            let (ra, rb) = (a[0] / a[2], b[0] / b[2]);
            assert!(
                (ra - rb).abs() < 1e-3 * ra.max(1.0),
                "pixel {i}: {ra} vs {rb}"
            );
        }
    }

    #[test]
    fn noise_below_the_threshold_is_left_alone_and_the_correction_is_capped() {
        // Alternating +/-0.001 encoded-luma-sized ripple on mid grey stays put.
        let (w, h) = (16u32, 4u32);
        let mut p: Vec<f32> = (0..w * h)
            .flat_map(|i| {
                let v = 0.2 + if i % 2 == 0 { 0.0003 } else { -0.0003 };
                [v, v, v]
            })
            .collect();
        let before = p.clone();
        apply(&mut p, w, h, STD, 300);
        assert_eq!(p, before);
        // The hardest possible edge never moves a pixel by more than MAX_DELTA in encoded luma.
        let (w, h) = (32u32, 4u32);
        let mut p = step(w, h, 0.0, 1.0);
        apply(
            &mut p,
            w,
            h,
            SharpenSpec {
                media: SharpenMedia::Matte,
                amount: SharpenAmount::High,
            },
            300,
        );
        assert!(p
            .iter()
            .all(|v| v.is_finite() && *v >= -1e-6 && *v <= 1.0 + 1e-6));
    }

    #[test]
    fn the_correction_to_one_pixel_never_exceeds_the_cap() {
        let (w, h) = (32u32, 4u32);
        let before = step(w, h, 0.02, 0.95);
        let mut p = before.clone();
        let hard = SharpenSpec {
            media: SharpenMedia::Matte,
            amount: SharpenAmount::High,
        };
        apply(&mut p, w, h, hard, 300);
        let mut capped = 0;
        for i in 0..(w * h) as usize {
            let moved = (oetf(p[i * 3]) - oetf(before[i * 3])).abs();
            assert!(moved <= MAX_DELTA + 1e-4, "pixel {i} moved {moved}");
            capped += usize::from(moved > MAX_DELTA - 1e-3);
        }
        assert!(capped > 0, "the cap must actually bind in this fixture");
    }

    #[test]
    fn over_range_non_finite_and_tiny_inputs_are_safe() {
        let mut p = vec![
            1.5,
            1.5,
            1.5,
            0.2,
            0.2,
            0.2,
            f32::NAN,
            0.1,
            0.1,
            -0.3,
            0.1,
            0.1,
        ];
        apply(&mut p, 4, 1, STD, 300);
        assert_eq!(&p[..3], &[1.5, 1.5, 1.5], "over-range pixel is untouched");
        assert!(p[3].is_finite());
        for (w, h) in [(1u32, 1u32), (2, 2), (1, 9), (9, 1)] {
            let mut p = vec![0.4f32; (w * h * 3) as usize];
            apply(&mut p, w, h, STD, 300);
        }
        // Wrong length / zero size are no-ops, not panics.
        let mut p = vec![0.4f32; 5];
        apply(&mut p, 2, 2, STD, 300);
        apply(&mut p, 0, 0, STD, 300);
    }
}
