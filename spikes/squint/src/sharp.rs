//! Sharpness/blur/misfocus candidate signals (#34/ADR-0034). Every candidate is a pure function
//! over a grayscale frame (plus, for the AF-aware candidates, an AF-region rectangle from
//! `af::AfArea`) returning a [`SharpnessScore`] -- comparable across candidates only in relative
//! terms (higher = sharper), never as an absolute cross-candidate unit.
//!
//! Global candidates score the frame's *sharpest* tile, not a whole-frame average, so a shallow-
//! depth-of-field portrait (in-focus subject, intentionally blurred background) isn't penalized
//! for its own bokeh -- the same reasoning `spikes/litter`'s SSIM/hash signals apply per-frame, now
//! applied per-tile instead.

use image::RgbImage;
use rustfft::{num_complex::Complex32, FftPlanner};

#[derive(Debug, Clone, Copy)]
pub struct Gray {
    pub width: u32,
    pub height: u32,
}

/// A grayscale (luma, ITU-R BT.601) frame as `f32` samples, row-major.
pub struct GrayFrame {
    pub dims: Gray,
    pub data: Vec<f32>,
}

impl GrayFrame {
    pub fn from_rgb(img: &RgbImage) -> Self {
        let (width, height) = img.dimensions();
        let data = img
            .pixels()
            .map(|p| 0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32)
            .collect();
        GrayFrame {
            dims: Gray { width, height },
            data,
        }
    }

    #[inline]
    fn at(&self, x: u32, y: u32) -> f32 {
        self.data[(y * self.dims.width + x) as usize]
    }

    /// Extracts a sub-rectangle as its own standalone `GrayFrame`, clamped to this frame's bounds.
    pub fn crop(&self, x: u32, y: u32, w: u32, h: u32) -> GrayFrame {
        let x0 = x.min(self.dims.width.saturating_sub(1));
        let y0 = y.min(self.dims.height.saturating_sub(1));
        let w = w.min(self.dims.width - x0).max(1);
        let h = h.min(self.dims.height - y0).max(1);
        let mut data = Vec::with_capacity((w * h) as usize);
        for yy in y0..y0 + h {
            for xx in x0..x0 + w {
                data.push(self.at(xx, yy));
            }
        }
        GrayFrame {
            dims: Gray {
                width: w,
                height: h,
            },
            data,
        }
    }
}

/// A candidate's score for one frame (or region): higher means sharper/more in-focus, in that
/// candidate's own arbitrary units. `reason` is a short human-readable explanation for the UI/
/// labelling tool, not part of the score itself.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SharpnessScore(pub f64);

/// Splits `frame` into non-overlapping `tile x tile` tiles (a ragged final row/column is kept,
/// smaller than `tile`) and calls `score_tile` on each, returning the maximum. Used by every
/// global candidate below so a small in-focus subject against a defocused background scores as
/// sharp, not blurred-on-average.
fn max_over_tiles<F: Fn(&GrayFrame) -> f64>(frame: &GrayFrame, tile: u32, score_tile: F) -> f64 {
    let mut best = 0.0f64;
    let mut y = 0;
    while y < frame.dims.height {
        let h = tile.min(frame.dims.height - y);
        let mut x = 0;
        while x < frame.dims.width {
            let w = tile.min(frame.dims.width - x);
            let sub = frame.crop(x, y, w, h);
            let score = score_tile(&sub);
            if score > best {
                best = score;
            }
            x += tile;
        }
        y += tile;
    }
    best
}

/// Discrete 3x3 Laplacian (`[[0,1,0],[1,-4,1],[0,1,0]]`), variance of the response over the whole
/// input -- the standard "variance of Laplacian" blur metric (Pech-Canul et al.). Requires at
/// least a 3x3 input; smaller regions return 0.0 (treated as "no signal," not "blurry" -- callers
/// should not feed regions this small).
pub fn laplacian_variance(frame: &GrayFrame) -> f64 {
    let (w, h) = (frame.dims.width, frame.dims.height);
    if w < 3 || h < 3 {
        return 0.0;
    }
    let mut responses = Vec::with_capacity(((w - 2) * (h - 2)) as usize);
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let center = frame.at(x, y);
            let lap =
                frame.at(x - 1, y) + frame.at(x + 1, y) + frame.at(x, y - 1) + frame.at(x, y + 1)
                    - 4.0 * center;
            responses.push(lap);
        }
    }
    variance(&responses)
}

/// Tile-wise max of [`laplacian_variance`] -- the actual candidate used for a full frame.
pub fn laplacian_variance_tiled(frame: &GrayFrame, tile: u32) -> SharpnessScore {
    SharpnessScore(max_over_tiles(frame, tile, laplacian_variance))
}

fn variance(xs: &[f32]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    let n = xs.len() as f64;
    let mean = xs.iter().map(|&v| v as f64).sum::<f64>() / n;
    xs.iter()
        .map(|&v| {
            let d = v as f64 - mean;
            d * d
        })
        .sum::<f64>()
        / n
}

/// Sobel gradient magnitude at every interior pixel, as `(gx, gy)` pairs -- shared by
/// [`tenengrad`] and [`structure_tensor_anisotropy`] so both read the exact same gradient field.
fn sobel_gradients(frame: &GrayFrame) -> Vec<(f32, f32)> {
    let (w, h) = (frame.dims.width, frame.dims.height);
    if w < 3 || h < 3 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(((w - 2) * (h - 2)) as usize);
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let tl = frame.at(x - 1, y - 1);
            let t = frame.at(x, y - 1);
            let tr = frame.at(x + 1, y - 1);
            let l = frame.at(x - 1, y);
            let r = frame.at(x + 1, y);
            let bl = frame.at(x - 1, y + 1);
            let b = frame.at(x, y + 1);
            let br = frame.at(x + 1, y + 1);
            let gx = (tr + 2.0 * r + br) - (tl + 2.0 * l + bl);
            let gy = (bl + 2.0 * b + br) - (tl + 2.0 * t + tr);
            out.push((gx, gy));
        }
    }
    out
}

/// Tenengrad: mean squared Sobel gradient magnitude (Krotkov 1988), a standard focus measure
/// distinct from Laplacian variance (first-derivative energy rather than second-derivative).
pub fn tenengrad(frame: &GrayFrame) -> f64 {
    let grads = sobel_gradients(frame);
    if grads.is_empty() {
        return 0.0;
    }
    let sum: f64 = grads
        .iter()
        .map(|&(gx, gy)| (gx * gx + gy * gy) as f64)
        .sum();
    sum / grads.len() as f64
}

pub fn tenengrad_tiled(frame: &GrayFrame, tile: u32) -> SharpnessScore {
    SharpnessScore(max_over_tiles(frame, tile, tenengrad))
}

/// High-frequency energy ratio: a real 2D FFT (via `rustfft`, row-then-column, not a Gaussian-
/// pyramid approximation) of the tile, split into a low-frequency disc (radius `cutoff_frac` of
/// the smaller dimension's half-size, in cycles/tile) and everything outside it, returning
/// `high_energy / total_energy`. `frame` is used as-is (not zero-padded to a power of two --
/// `rustfft`'s mixed-radix planner handles arbitrary lengths, just somewhat slower than a strict
/// power-of-two size; fine at the tile sizes this candidate runs at).
pub fn fft_high_freq_ratio(frame: &GrayFrame, cutoff_frac: f32) -> f64 {
    let (w, h) = (frame.dims.width as usize, frame.dims.height as usize);
    if w < 4 || h < 4 {
        return 0.0;
    }
    let mut planner = FftPlanner::<f32>::new();
    let fft_row = planner.plan_fft_forward(w);
    let fft_col = planner.plan_fft_forward(h);

    let mut buf: Vec<Complex32> = frame.data.iter().map(|&v| Complex32::new(v, 0.0)).collect();

    for row in buf.chunks_mut(w) {
        fft_row.process(row);
    }
    // Transpose, FFT again (now columns), transpose back -- the standard row-then-column 2D FFT
    // decomposition.
    let mut transposed = vec![Complex32::new(0.0, 0.0); w * h];
    for y in 0..h {
        for x in 0..w {
            transposed[x * h + y] = buf[y * w + x];
        }
    }
    for col in transposed.chunks_mut(h) {
        fft_col.process(col);
    }
    for x in 0..w {
        for y in 0..h {
            buf[y * w + x] = transposed[x * h + y];
        }
    }

    let cutoff_radius = cutoff_frac * (w.min(h) as f32 / 2.0);
    let mut low_energy = 0.0f64;
    let mut high_energy = 0.0f64;
    for y in 0..h {
        for x in 0..w {
            // FFT frequency-domain coordinates: index 0 is DC, indices wrap past the midpoint to
            // negative frequencies -- fold both axes back to a signed cycle count before measuring
            // radius from the origin.
            let fx = if x <= w / 2 {
                x as f32
            } else {
                x as f32 - w as f32
            };
            let fy = if y <= h / 2 {
                y as f32
            } else {
                y as f32 - h as f32
            };
            let radius = (fx * fx + fy * fy).sqrt();
            let e = buf[y * w + x].norm_sqr() as f64;
            if radius <= cutoff_radius {
                low_energy += e;
            } else {
                high_energy += e;
            }
        }
    }
    let total = low_energy + high_energy;
    if total <= 0.0 {
        0.0
    } else {
        high_energy / total
    }
}

pub fn fft_high_freq_ratio_tiled(frame: &GrayFrame, tile: u32, cutoff_frac: f32) -> SharpnessScore {
    SharpnessScore(max_over_tiles(frame, tile, |t| {
        fft_high_freq_ratio(t, cutoff_frac)
    }))
}

/// Motion-vs-defocus discriminator: eigenvalue anisotropy of the averaged structure tensor
/// (`[[Ixx, Ixy], [Ixy, Iyy]]`, from the same Sobel field [`tenengrad`] uses) over the sharpest
/// tile a caller has already located. Directional energy loss (one eigenvalue much larger than the
/// other) indicates motion blur along that axis; isotropic loss (eigenvalues close together)
/// indicates defocus, which blurs uniformly in every direction. Returns a ratio in `[0.0, 1.0]`:
/// 0.0 is perfectly isotropic (defocus-like), approaching 1.0 is strongly directional (motion-
/// like).
pub fn structure_tensor_anisotropy(frame: &GrayFrame) -> f64 {
    let grads = sobel_gradients(frame);
    if grads.is_empty() {
        return 0.0;
    }
    let n = grads.len() as f64;
    let (mut ixx, mut iyy, mut ixy) = (0.0f64, 0.0f64, 0.0f64);
    for &(gx, gy) in &grads {
        ixx += (gx * gx) as f64;
        iyy += (gy * gy) as f64;
        ixy += (gx * gy) as f64;
    }
    ixx /= n;
    iyy /= n;
    ixy /= n;

    // Eigenvalues of a symmetric 2x2 matrix via the closed-form trace/determinant formula.
    let trace = ixx + iyy;
    let det = ixx * iyy - ixy * ixy;
    let disc = ((trace * trace) / 4.0 - det).max(0.0).sqrt();
    let lambda1 = trace / 2.0 + disc;
    let lambda2 = trace / 2.0 - disc;
    if lambda1 <= 0.0 {
        return 0.0;
    }
    (lambda1 - lambda2.max(0.0)) / lambda1
}

/// Misfocus signal: sharpness inside `af_rect` (already rescaled into `frame`'s coordinate space,
/// e.g. via `af::AfArea::rescale_to`) compared against the frame's own sharpest tile. A ratio near
/// 1.0 means the AF area is (at least) as sharp as anywhere else in the frame -- properly focused.
/// A ratio well below 1.0 means something else in the frame is sharper than where the camera
/// actually focused -- back- or front-focus. `score_fn` is one of the tile-scoring candidates
/// above (e.g. `|f| laplacian_variance_tiled(f, 32).0`), so this composes with any of them rather
/// than hardcoding one.
pub fn af_region_misfocus_ratio<F: Fn(&GrayFrame) -> f64>(
    frame: &GrayFrame,
    af_rect: (u32, u32, u32, u32),
    tile: u32,
    score_fn: F,
) -> f64 {
    let (x, y, w, h) = af_rect;
    let af_region = frame.crop(x, y, w, h);
    let af_score = score_fn(&af_region);
    let frame_best = max_over_tiles(frame, tile, score_fn);
    if frame_best <= 0.0 {
        // No measurable sharpness anywhere -- can't judge misfocus (e.g. a flat/blank test image).
        return 1.0;
    }
    (af_score / frame_best).min(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    fn checkerboard(width: u32, height: u32, period: u32) -> RgbImage {
        RgbImage::from_fn(width, height, |x, y| {
            if (x / period + y / period).is_multiple_of(2) {
                Rgb([230, 230, 230])
            } else {
                Rgb([20, 20, 20])
            }
        })
    }

    fn box_blur(img: &RgbImage, radius: u32) -> RgbImage {
        let (w, h) = img.dimensions();
        RgbImage::from_fn(w, h, |x, y| {
            let mut sum = [0u32; 3];
            let mut n = 0u32;
            let x0 = x.saturating_sub(radius);
            let x1 = (x + radius).min(w - 1);
            let y0 = y.saturating_sub(radius);
            let y1 = (y + radius).min(h - 1);
            for yy in y0..=y1 {
                for xx in x0..=x1 {
                    let p = img.get_pixel(xx, yy);
                    sum[0] += p[0] as u32;
                    sum[1] += p[1] as u32;
                    sum[2] += p[2] as u32;
                    n += 1;
                }
            }
            Rgb([(sum[0] / n) as u8, (sum[1] / n) as u8, (sum[2] / n) as u8])
        })
    }

    #[test]
    fn laplacian_variance_ranks_sharp_above_blurred() {
        let sharp = GrayFrame::from_rgb(&checkerboard(64, 64, 4));
        let blurred_img = box_blur(&checkerboard(64, 64, 4), 3);
        let blurred = GrayFrame::from_rgb(&blurred_img);

        let sharp_score = laplacian_variance_tiled(&sharp, 64).0;
        let blurred_score = laplacian_variance_tiled(&blurred, 64).0;
        assert!(
            sharp_score > blurred_score * 2.0,
            "sharp={sharp_score}, blurred={blurred_score}"
        );
    }

    #[test]
    fn tenengrad_ranks_sharp_above_blurred() {
        let sharp = GrayFrame::from_rgb(&checkerboard(64, 64, 4));
        let blurred_img = box_blur(&checkerboard(64, 64, 4), 3);
        let blurred = GrayFrame::from_rgb(&blurred_img);

        let sharp_score = tenengrad_tiled(&sharp, 64).0;
        let blurred_score = tenengrad_tiled(&blurred, 64).0;
        assert!(
            sharp_score > blurred_score * 2.0,
            "sharp={sharp_score}, blurred={blurred_score}"
        );
    }

    #[test]
    fn fft_high_freq_ratio_ranks_sharp_above_blurred() {
        let sharp = GrayFrame::from_rgb(&checkerboard(64, 64, 4));
        let blurred_img = box_blur(&checkerboard(64, 64, 4), 3);
        let blurred = GrayFrame::from_rgb(&blurred_img);

        let sharp_score = fft_high_freq_ratio(&sharp, 0.25);
        let blurred_score = fft_high_freq_ratio(&blurred, 0.25);
        assert!(
            sharp_score > blurred_score,
            "sharp={sharp_score}, blurred={blurred_score}"
        );
    }

    #[test]
    fn structure_tensor_isotropic_on_checkerboard_high_on_horizontal_lines() {
        // A checkerboard has energy in both axes (isotropic-ish); a set of pure horizontal
        // lines has gradient energy almost entirely along the vertical axis (anisotropic).
        let checker = GrayFrame::from_rgb(&checkerboard(64, 64, 4));
        let lines = RgbImage::from_fn(64, 64, |_, y| {
            if y % 4 < 2 {
                Rgb([230, 230, 230])
            } else {
                Rgb([20, 20, 20])
            }
        });
        let lines = GrayFrame::from_rgb(&lines);

        let checker_aniso = structure_tensor_anisotropy(&checker);
        let lines_aniso = structure_tensor_anisotropy(&lines);
        assert!(
            lines_aniso > checker_aniso,
            "lines={lines_aniso}, checker={checker_aniso}"
        );
    }

    #[test]
    fn af_region_misfocus_ratio_is_near_one_when_af_region_is_sharpest() {
        // A frame where the AF region (top-left) is sharp and the rest is blurred.
        let mut base = checkerboard(64, 64, 4);
        let blurred_rest = box_blur(&base, 4);
        for y in 0..64 {
            for x in 0..64 {
                if x >= 16 || y >= 16 {
                    base.put_pixel(x, y, *blurred_rest.get_pixel(x, y));
                }
            }
        }
        let frame = GrayFrame::from_rgb(&base);
        let ratio = af_region_misfocus_ratio(&frame, (0, 0, 16, 16), 16, |f| {
            laplacian_variance_tiled(f, 16).0
        });
        assert!(ratio > 0.9, "expected near-1.0 (in focus), got {ratio}");
    }

    #[test]
    fn af_region_misfocus_ratio_is_low_when_af_region_is_soft() {
        // Inverse of the above: AF region (top-left) is the blurred one, sharp detail is
        // elsewhere -- a real back/front-focus shape.
        let base = checkerboard(64, 64, 4);
        let blurred = box_blur(&base, 4);
        let mut composed = base.clone();
        for y in 0..16 {
            for x in 0..16 {
                composed.put_pixel(x, y, *blurred.get_pixel(x, y));
            }
        }
        let frame = GrayFrame::from_rgb(&composed);
        let ratio = af_region_misfocus_ratio(&frame, (0, 0, 16, 16), 16, |f| {
            laplacian_variance_tiled(f, 16).0
        });
        assert!(ratio < 0.5, "expected a low ratio (misfocus), got {ratio}");
    }
}
