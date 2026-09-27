//! Sub-pixel translation alignment + robust per-channel gain fit between two same-scene images
//! that may differ in crop margin, exposure/WB scaling, or a small real shift (a different real
//! exposure of a tripod scene, or an independently-exported reference) -- not needed between
//! candidates decoded from the *same* NEF (those share pixel grids exactly), only when comparing
//! against LRC's own export or a RawNIND ground-truth frame.
//!
//! Sub-pixel shift is estimated via single-level Lucas-Kanade (image-gradient Gauss-Newton on a
//! global translation), not FFT phase correlation -- no FFT dependency exists in this workspace
//! yet, and LK converges in a handful of iterations for the small (sub-few-pixel) shifts expected
//! here (a real subject-motion/vibration shift between two tripod exposures, not an arbitrary
//! search). A shift search wide enough for gross misregistration is out of scope; callers should
//! already have roughly aligned crops (matching `iwidth`/`iheight`/margins from the same decoder
//! metadata) before this runs.

/// A single-channel plane plus its dimensions, the unit this module operates on.
#[derive(Debug, Clone)]
pub struct Plane {
    pub width: u32,
    pub height: u32,
    pub samples: Vec<f32>,
}

impl Plane {
    pub fn get(&self, x: i64, y: i64) -> Option<f32> {
        if x < 0 || y < 0 || x >= self.width as i64 || y >= self.height as i64 {
            return None;
        }
        Some(self.samples[(y as u32 * self.width + x as u32) as usize])
    }

    /// Bilinear sample at a possibly-fractional coordinate; `None` outside the plane's bounds
    /// (including the 1px border needed for the bilinear footprint itself).
    pub fn sample_bilinear(&self, x: f64, y: f64) -> Option<f32> {
        if x < 0.0 || y < 0.0 || x >= (self.width - 1) as f64 || y >= (self.height - 1) as f64 {
            return None;
        }
        let x0 = x.floor() as i64;
        let y0 = y.floor() as i64;
        let fx = (x - x0 as f64) as f32;
        let fy = (y - y0 as f64) as f32;
        let p00 = self.get(x0, y0)?;
        let p10 = self.get(x0 + 1, y0)?;
        let p01 = self.get(x0, y0 + 1)?;
        let p11 = self.get(x0 + 1, y0 + 1)?;
        Some(
            p00 * (1.0 - fx) * (1.0 - fy)
                + p10 * fx * (1.0 - fy)
                + p01 * (1.0 - fx) * fy
                + p11 * fx * fy,
        )
    }
}

/// Result of [`estimate_shift`]: `moving`'s position relative to `reference`, in `reference`
/// pixels -- `moving` sampled at `(x + dx, y + dy)` should match `reference` at `(x, y)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shift {
    pub dx: f64,
    pub dy: f64,
}

impl Shift {
    /// The plan's alignment gate: reject anything past a quarter-pixel shift as unregistered
    /// rather than silently scoring two misaligned images against each other.
    pub fn within_tolerance(&self, tolerance_px: f64) -> bool {
        self.dx.abs() <= tolerance_px && self.dy.abs() <= tolerance_px
    }
}

/// Estimates the sub-pixel translation of `moving` relative to `reference` via Lucas-Kanade
/// (Gauss-Newton on a single global 2D translation, using `reference`'s own spatial gradients).
/// Both planes must be the same size. Margin excludes a border (in pixels) from the fit, so the
/// gradient/bilinear footprints near the edges never read out of bounds.
///
/// Converges in `max_iters` Gauss-Newton steps or once the update falls below `1e-4` px.
pub fn estimate_shift(reference: &Plane, moving: &Plane, margin: u32, max_iters: u32) -> Shift {
    assert_eq!(reference.width, moving.width);
    assert_eq!(reference.height, moving.height);

    let mut dx = 0.0f64;
    let mut dy = 0.0f64;

    for _ in 0..max_iters {
        // Gauss-Newton normal equations for a global translation: minimize
        // sum((I_ref(x,y) - I_mov(x+dx,y+dy))^2) over the interior region.
        let (mut a11, mut a12, mut a22) = (0.0f64, 0.0f64, 0.0f64);
        let (mut b1, mut b2) = (0.0f64, 0.0f64);
        let mut samples = 0usize;

        for y in margin..(reference.height - margin) {
            for x in margin..(reference.width - margin) {
                let ix = x as f64;
                let iy = y as f64;
                let Some(i_ref) = reference.get(x as i64, y as i64) else {
                    continue;
                };
                let Some(i_mov) = moving.sample_bilinear(ix + dx, iy + dy) else {
                    continue;
                };
                // Central-difference gradient of the moving image at the current warp, the
                // standard (inverse-additive) LK formulation.
                let Some(gx_hi) = moving.sample_bilinear(ix + dx + 1.0, iy + dy) else {
                    continue;
                };
                let Some(gx_lo) = moving.sample_bilinear(ix + dx - 1.0, iy + dy) else {
                    continue;
                };
                let Some(gy_hi) = moving.sample_bilinear(ix + dx, iy + dy + 1.0) else {
                    continue;
                };
                let Some(gy_lo) = moving.sample_bilinear(ix + dx, iy + dy - 1.0) else {
                    continue;
                };
                let gx = (gx_hi - gx_lo) as f64 / 2.0;
                let gy = (gy_hi - gy_lo) as f64 / 2.0;
                let err = (i_ref - i_mov) as f64;

                a11 += gx * gx;
                a12 += gx * gy;
                a22 += gy * gy;
                b1 += gx * err;
                b2 += gy * err;
                samples += 1;
            }
        }

        if samples == 0 {
            break;
        }

        let det = a11 * a22 - a12 * a12;
        if det.abs() < 1e-12 {
            // Degenerate (e.g. a flat/textureless region) -- can't solve for a shift here, and
            // continuing would divide by ~0. Stop where we are rather than diverge.
            break;
        }
        let step_dx = (a22 * b1 - a12 * b2) / det;
        let step_dy = (a11 * b2 - a12 * b1) / det;
        dx += step_dx;
        dy += step_dy;

        if step_dx.abs() < 1e-4 && step_dy.abs() < 1e-4 {
            break;
        }
    }

    Shift { dx, dy }
}

/// Per-channel least-squares gain: the scalar `g` minimizing `sum((g*moving - reference)^2)`
/// over unmasked samples, i.e. `g = sum(reference*moving) / sum(moving*moving)`. Absorbs a WB or
/// exposure-scaling difference between two otherwise-identical images so quality metrics measure
/// structural/noise differences, not a shared linear scale factor. Returns `1.0` (no correction)
/// if every sample is masked or `moving` is all-zero.
pub fn fit_gain(reference: &[f32], moving: &[f32], mask: &[bool]) -> f64 {
    assert_eq!(reference.len(), moving.len());
    assert_eq!(reference.len(), mask.len());

    let mut num = 0.0f64;
    let mut den = 0.0f64;
    for i in 0..reference.len() {
        if !mask[i] {
            continue;
        }
        let r = reference[i] as f64;
        let m = moving[i] as f64;
        num += r * m;
        den += m * m;
    }
    if den == 0.0 {
        1.0
    } else {
        num / den
    }
}

/// Resamples an interleaved RGB `f32` image (`width*height*3` samples) so that `moving`'s content
/// lands on `reference`'s pixel grid, undoing the shift `estimate_shift` found (`moving` sampled
/// at `(x+dx, y+dy)` matches `reference` at `(x,y)`, so this builds `out(x,y) =
/// moving.sample_bilinear(x+dx, y+dy)` for every pixel and channel). Samples falling outside
/// `moving`'s bounds (a real possibility at the image edges after a shift) are written as `0.0`,
/// and the returned `Vec<bool>` (one entry per pixel, not per channel) marks those positions
/// `false` -- **not enough on its own to exclude them from scoring** if a caller runs an AI
/// denoise stage afterward: the model's own receptive field can smear a real, non-black value
/// into a filled border pixel, at which point a naive "still near-black" check
/// (`clip_mask`'s original justification) no longer holds. Callers that denoise after resampling
/// should crop to a rectangle known to be entirely valid (every pixel's shifted source coordinate
/// in-bounds) using the shift itself, not just rely on this per-pixel mask post-denoise -- see
/// `rods.rs`'s own use of this return value's second element only for the (still valid) pre-
/// denoise gain fit, and its separate shift-derived rectangular crop before PSNR/SSIM.
pub fn resample_rgb(
    rgb_hwc: &[f32],
    width: u32,
    height: u32,
    shift: Shift,
) -> (Vec<f32>, Vec<bool>) {
    assert_eq!(rgb_hwc.len(), width as usize * height as usize * 3);

    let channel = |c: usize| -> Plane {
        Plane {
            width,
            height,
            samples: rgb_hwc.as_chunks::<3>().0.iter().map(|px| px[c]).collect(),
        }
    };
    let planes: [Plane; 3] = std::array::from_fn(channel);

    let mut out = vec![0.0f32; rgb_hwc.len()];
    let mut valid = vec![false; width as usize * height as usize];
    for y in 0..height {
        for x in 0..width {
            let sx = x as f64 + shift.dx;
            let sy = y as f64 + shift.dy;
            let idx = (y * width + x) as usize;
            let mut any_missing = false;
            for (c, plane) in planes.iter().enumerate() {
                match plane.sample_bilinear(sx, sy) {
                    Some(v) => out[idx * 3 + c] = v,
                    None => {
                        out[idx * 3 + c] = 0.0;
                        any_missing = true;
                    }
                }
            }
            valid[idx] = !any_missing;
        }
    }
    (out, valid)
}

/// The largest rectangle, inset symmetrically from every edge by `shift`'s own magnitude (rounded
/// up, plus 1 for `sample_bilinear`'s own footprint), that's guaranteed to be entirely valid after
/// [`resample_rgb`] -- every pixel inside it had an in-bounds source coordinate to sample from, so
/// none of it came from a `0.0` fill a denoise stage could have smeared into a false-valid value.
/// Simpler and more robust against that smearing than trying to track per-pixel validity through
/// an arbitrary downstream filter: a rectangle is directly usable by `metrics::ssim`'s windowed
/// scoring, which needs a contiguous 2D region, not a scattered pixel mask.
///
/// Returns `Some((x0, y0, valid_width, valid_height))`, or `None` if the shift is large enough
/// relative to the image that no non-empty rectangle can be guaranteed valid (the required inset
/// would meet or exceed the image's own center) -- this is a real "no valid data" case, not a
/// degenerate one to paper over: silently shrinking the *required* inset to fit a smaller image
/// would return a rectangle that's *not* actually guaranteed valid (some of its pixels would still
/// have an out-of-bounds source coordinate), which defeats the whole point of this function.
/// Callers must handle `None` explicitly (e.g. fall back to scoring the full frame with a loud
/// warning) rather than assuming a rectangle always exists.
pub fn valid_rect(width: u32, height: u32, shift: Shift) -> Option<(u32, u32, u32, u32)> {
    let inset = shift.dx.abs().max(shift.dy.abs()).ceil() as u32 + 1;
    if 2 * inset >= width || 2 * inset >= height {
        return None;
    }
    let valid_width = width - 2 * inset;
    let valid_height = height - 2 * inset;
    Some((inset, inset, valid_width, valid_height))
}

/// Extracts the interleaved-RGB sub-rectangle `(x0, y0, rect_width, rect_height)` out of a full
/// `width`x`height` image, for cropping down to a [`valid_rect`] before scoring.
pub fn crop_rgb(rgb_hwc: &[f32], width: u32, rect: (u32, u32, u32, u32)) -> Vec<f32> {
    let (x0, y0, rect_width, rect_height) = rect;
    let mut out = Vec::with_capacity(rect_width as usize * rect_height as usize * 3);
    for y in y0..y0 + rect_height {
        let row_start = ((y * width + x0) * 3) as usize;
        let row_end = row_start + rect_width as usize * 3;
        out.extend_from_slice(&rgb_hwc[row_start..row_end]);
    }
    out
}

/// Replaces every pixel `valid` marks `false` with the color of its nearest neighbor inside
/// `rect` (clamping its own coordinate into `rect`'s bounds) -- the same clamp-to-edge idea
/// `ai::build_padded_tile` already uses for tile-boundary padding. Meant to run on a
/// [`resample_rgb`] output **before** it reaches an AI denoiser: a denoiser's receptive field can
/// reach past `valid_rect`'s own margin, so a sharp, unnatural `0.0` edge at the resample-invalid
/// border risks propagating a visible artifact inward into the region `valid_rect` otherwise
/// guarantees is safe to score. A smooth clamp-to-edge extension is a much weaker signal for a
/// convolutional receptive field to pick up on than a hard black edge, though (like the tile
/// padding it mirrors) it's a mitigation, not a proof the denoiser's receptive field can never
/// reach past it -- `rect` should still be sized generously enough to outgrow any receptive field
/// worth worrying about for the models this project actually uses.
pub fn fill_invalid_border(
    rgb_hwc: &mut [f32],
    width: u32,
    height: u32,
    valid: &[bool],
    rect: (u32, u32, u32, u32),
) {
    let (x0, y0, rect_width, rect_height) = rect;
    let x_max = x0 + rect_width - 1;
    let y_max = y0 + rect_height - 1;
    for y in 0..height {
        for x in 0..width {
            let idx = (y * width + x) as usize;
            if valid[idx] {
                continue;
            }
            let cx = x.clamp(x0, x_max);
            let cy = y.clamp(y0, y_max);
            let src = (cy * width + cx) as usize;
            for c in 0..3 {
                rgb_hwc[idx * 3 + c] = rgb_hwc[src * 3 + c];
            }
        }
    }
}

/// A mask excluding samples near the clipped extremes of either image (highlights or deep
/// blacks) -- these bias both the shift estimate's gradients and the gain fit, since a clipped
/// region carries no real signal past the clip point in either image.
pub fn clip_mask(reference: &[f32], moving: &[f32], low: f32, high: f32) -> Vec<bool> {
    assert_eq!(reference.len(), moving.len());
    reference
        .iter()
        .zip(moving)
        .map(|(&r, &m)| r > low && r < high && m > low && m < high)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_scene(width: u32, height: u32) -> Plane {
        // A smooth-but-textured field (not a flat gradient, which would make x/y shift
        // ambiguous along the flat axis) so LK has real gradient information everywhere.
        let samples = (0..height)
            .flat_map(|y| {
                (0..width).map(move |x| {
                    let fx = x as f32 / width as f32;
                    let fy = y as f32 / height as f32;
                    (0.5 + 0.3 * (fx * 12.0).sin() + 0.2 * (fy * 9.0).cos()).clamp(0.0, 1.0)
                })
            })
            .collect();
        Plane {
            width,
            height,
            samples,
        }
    }

    fn shift_plane(p: &Plane, dx: f64, dy: f64) -> Plane {
        let mut samples = vec![0.0f32; (p.width * p.height) as usize];
        for y in 0..p.height {
            for x in 0..p.width {
                // `shifted` sampled at (x,y) should equal `p` at (x - dx, y - dy), i.e. `shifted`
                // is `p` moved by (dx, dy) -- matching `estimate_shift`'s contract that `moving`
                // at `(x+dx, y+dy)` matches `reference` at `(x,y)`.
                let v = p
                    .sample_bilinear(x as f64 - dx, y as f64 - dy)
                    .unwrap_or(0.5);
                samples[(y * p.width + x) as usize] = v;
            }
        }
        Plane {
            width: p.width,
            height: p.height,
            samples,
        }
    }

    #[test]
    fn estimate_shift_zero_for_identical_images() {
        let a = synthetic_scene(64, 64);
        let shift = estimate_shift(&a, &a, 8, 20);
        assert!(
            shift.within_tolerance(0.05),
            "expected ~0 shift, got {shift:?}"
        );
    }

    #[test]
    fn estimate_shift_recovers_known_subpixel_shift() {
        let reference = synthetic_scene(64, 64);
        let moving = shift_plane(&reference, 1.3, -0.7);
        let shift = estimate_shift(&reference, &moving, 8, 30);
        assert!(
            (shift.dx - 1.3).abs() < 0.05 && (shift.dy - (-0.7)).abs() < 0.05,
            "expected ~(1.3, -0.7), got {shift:?}"
        );
    }

    #[test]
    fn within_tolerance_rejects_large_shift() {
        let shift = Shift { dx: 2.0, dy: 0.0 };
        assert!(!shift.within_tolerance(0.25));
    }

    #[test]
    fn resample_rgb_undoes_a_known_shift() {
        // Build a reference scene, shift it to make "moving", then resample moving by the same
        // shift -- the result should match reference again (within bilinear-resample tolerance,
        // tighter in the interior than at the edges where samples fall outside moving's bounds).
        let reference = synthetic_scene(64, 64);
        let shift = Shift { dx: 1.3, dy: -0.7 };
        let moving_luma = shift_plane(&reference, shift.dx, shift.dy);

        let mut moving_rgb = vec![0.0f32; 64 * 64 * 3];
        for (i, &v) in moving_luma.samples.iter().enumerate() {
            moving_rgb[i * 3] = v;
            moving_rgb[i * 3 + 1] = v;
            moving_rgb[i * 3 + 2] = v;
        }

        let (resampled, valid) = resample_rgb(&moving_rgb, 64, 64, shift);

        // Compare over the interior only (margin 4px), away from the edges a 1.3/-0.7px shift
        // pushes out of `moving`'s bounds.
        let mut max_abs_diff = 0.0f32;
        for y in 4..60u32 {
            for x in 4..60u32 {
                let idx = (y * 64 + x) as usize;
                let expected = reference.get(x as i64, y as i64).unwrap();
                let actual = resampled[idx * 3];
                max_abs_diff = max_abs_diff.max((actual - expected).abs());
                assert!(
                    valid[idx],
                    "expected interior pixel ({x},{y}) to be marked valid"
                );
            }
        }
        assert!(
            max_abs_diff < 0.01,
            "expected resampled to match reference in the interior, max abs diff {max_abs_diff}"
        );

        // The top-left corner (0,0) samples at (1.3,-0.7): y is out of moving's bounds, so it
        // must be marked invalid, matching the 0.0 fill resample_rgb writes for it.
        assert!(
            !valid[0],
            "expected (0,0) to be marked invalid after a shift that pushes it out of bounds"
        );
    }

    #[test]
    fn fill_invalid_border_clamps_to_nearest_rect_edge() {
        // A tiny 4x4 image, rect = (1,1,2,2) valid (the center 2x2), everything outside invalid.
        // Every invalid pixel should end up with the color of its nearest rect-edge neighbor, not
        // a smoothed average or anything more clever -- a hard clamp-to-edge, mirroring
        // ai::build_padded_tile's own tile-boundary padding.
        let width = 4u32;
        let height = 4u32;
        let rect = (1u32, 1u32, 2u32, 2u32);
        let mut rgb = vec![0.0f32; (width * height * 3) as usize];
        // Distinct colors per rect cell so we can tell which one got copied.
        let set = |rgb: &mut [f32], x: u32, y: u32, v: f32| {
            let idx = ((y * width + x) * 3) as usize;
            rgb[idx] = v;
            rgb[idx + 1] = v;
            rgb[idx + 2] = v;
        };
        set(&mut rgb, 1, 1, 10.0);
        set(&mut rgb, 2, 1, 20.0);
        set(&mut rgb, 1, 2, 30.0);
        set(&mut rgb, 2, 2, 40.0);

        let mut valid = vec![false; (width * height) as usize];
        for y in 1..3u32 {
            for x in 1..3u32 {
                valid[(y * width + x) as usize] = true;
            }
        }

        fill_invalid_border(&mut rgb, width, height, &valid, rect);

        let px = |rgb: &[f32], x: u32, y: u32| rgb[((y * width + x) * 3) as usize];
        // Top-left corner (0,0) clamps to rect's (1,1) = 10.0.
        assert_eq!(px(&rgb, 0, 0), 10.0);
        // Top-right corner (3,0) clamps to rect's (2,1) = 20.0.
        assert_eq!(px(&rgb, 3, 0), 20.0);
        // Bottom-left corner (0,3) clamps to rect's (1,2) = 30.0.
        assert_eq!(px(&rgb, 0, 3), 30.0);
        // Bottom-right corner (3,3) clamps to rect's (2,2) = 40.0.
        assert_eq!(px(&rgb, 3, 3), 40.0);
        // The rect's own interior must be untouched.
        assert_eq!(px(&rgb, 1, 1), 10.0);
        assert_eq!(px(&rgb, 2, 2), 40.0);
    }

    #[test]
    fn valid_rect_insets_by_shift_magnitude_plus_one() {
        let (x0, y0, w, h) =
            valid_rect(64, 64, Shift { dx: 1.3, dy: -0.7 }).expect("rect should exist");
        assert_eq!((x0, y0), (3, 3));
        assert_eq!((w, h), (58, 58));
    }

    #[test]
    fn valid_rect_every_pixel_is_actually_valid_after_resampling() {
        // Regression test for a real bug an adversarial review found: an earlier version of
        // valid_rect clamped its inset down to fit small images, silently returning a rectangle
        // that violated its own "guaranteed valid" contract. This exercises the full pipeline --
        // resample a synthetic image at a real shift, then check every pixel inside the returned
        // rect is actually marked valid by resample_rgb itself, not just that the coordinates
        // match the inset arithmetic.
        let reference = synthetic_scene(64, 64);
        let shift = Shift { dx: 1.3, dy: -0.7 };
        let moving_luma = shift_plane(&reference, shift.dx, shift.dy);
        let mut moving_rgb = vec![0.0f32; 64 * 64 * 3];
        for (i, &v) in moving_luma.samples.iter().enumerate() {
            moving_rgb[i * 3] = v;
            moving_rgb[i * 3 + 1] = v;
            moving_rgb[i * 3 + 2] = v;
        }
        let (_, valid) = resample_rgb(&moving_rgb, 64, 64, shift);
        let (x0, y0, w, h) = valid_rect(64, 64, shift).expect("rect should exist");
        for y in y0..y0 + h {
            for x in x0..x0 + w {
                let idx = (y * 64 + x) as usize;
                assert!(
                    valid[idx],
                    "expected ({x},{y}) inside valid_rect to be marked valid"
                );
            }
        }
    }

    #[test]
    fn valid_rect_returns_none_for_a_shift_larger_than_the_image() {
        // A shift this large relative to an 8x8 image means no non-empty rectangle can be
        // guaranteed valid -- must report that honestly (None) rather than silently returning a
        // rectangle whose coordinates fall outside the image or whose pixels aren't really valid.
        assert_eq!(valid_rect(8, 8, Shift { dx: 100.0, dy: 0.0 }), None);
    }

    #[test]
    fn fit_gain_recovers_known_scale() {
        let moving: Vec<f32> = (0..100).map(|i| i as f32 / 100.0).collect();
        let reference: Vec<f32> = moving.iter().map(|&v| v * 2.0).collect();
        let mask = vec![true; 100];
        let gain = fit_gain(&reference, &moving, &mask);
        assert!((gain - 2.0).abs() < 1e-6, "expected gain 2.0, got {gain}");
    }

    #[test]
    fn fit_gain_ignores_masked_samples() {
        let moving = vec![1.0f32, 1.0, 1.0];
        // Without masking, this outlier would corrupt the fit toward a gain of ~3.33.
        let reference = vec![2.0f32, 2.0, 100.0];
        let mask = vec![true, true, false];
        let gain = fit_gain(&reference, &moving, &mask);
        assert!((gain - 2.0).abs() < 1e-6, "expected gain 2.0, got {gain}");
    }

    #[test]
    fn clip_mask_excludes_extremes() {
        let reference = vec![0.0f32, 0.5, 1.0, 0.5];
        let moving = vec![0.5f32, 0.5, 0.5, 1.0];
        let mask = clip_mask(&reference, &moving, 0.01, 0.99);
        assert_eq!(mask, vec![false, true, false, false]);
    }
}
