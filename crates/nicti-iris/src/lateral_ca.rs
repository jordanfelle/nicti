//! Automatic lateral chromatic aberration estimation (#428): the no-profile fallback for the lens
//! stage.
//!
//! Lateral CA is a radial magnification difference between the colour planes. We find strong,
//! radially oriented edges in the green plane, locate the same edge in red and blue with
//! sub-pixel 1-D profile matching, and fit `displacement = α·r` by weighted, outlier-trimmed
//! least squares. `[α_R, α_B]` are dimensionless: a red edge that sits at radius `r` in green
//! sits at `r·(1 + α_R)` in red, so the lens stage resamples red at `(1 + α_R)·offset` to line
//! it back up.
//!
//! Adapted from storytold/lightcraft@265248c `crates/pipeline/src/optics.rs` (`estimate_lateral_ca`),
//! Copyright (c) 2026 ArtCraft Team and the LightCraft contributors, MIT OR Apache-2.0
//! (see `docs/licensing.md`).
//! Changes:
//!
//! - Works on a 2x2-box-decimated copy, so the fixed ±[`MAX_SHIFT`] px search covers ±2× that at
//!   full resolution. LightCraft searched ±3 px at full resolution, which caps the measurable α
//!   near `3 / r` and silently discards (biasing toward zero) everything larger.
//! - Takes the optical centre (a DNG profile's, when there is one) instead of assuming the middle.
//! - Normalises `u16` linear camera RGB by black/white level itself.
//! - No hidden global cache in this module: it is a pure function of its arguments. (The lens stage
//!   memoises the result per frame in `nicti-tapetum`'s `slit.rs`, where the frame identity lives.)
//!
//! The thresholds below are LightCraft's hand-tuned values, **not** validated on real photos yet;
//! `NICTI_TEST_REAL_NEF_DIR` drives `real_nef_estimates_are_small_and_stable` in the Tapetum lens
//! tests for that.

/// Edge candidates per grid cell, and the grid is `CELLS`×`CELLS` over the frame.
const CELLS: usize = 24;
const PER_CELL: usize = 12;
/// Minimum relative gradient (gradient / local mean) for an edge candidate.
const MIN_REL_GRADIENT: f32 = 0.2;
/// An edge must be at least this fraction of the centre-to-corner distance from the centre.
const MIN_RADIUS_FRACTION: f64 = 0.2;
/// |cos| between the gradient and the radial direction: edges must be near-radial.
const MIN_RADIAL_COS: f64 = 0.85;
/// Profile half-length in (decimated) pixels, sub-pixel steps per pixel, and max shift searched.
const HALF: i32 = 4;
const SUB: i32 = 20;
/// Max shift searched, in *decimated* pixels (twice that at full resolution).
pub const MAX_SHIFT: i32 = 3;
/// Normalised SSD per sample above which a match is rejected (`2(1 − corr)`).
const MAX_SSD_PER_SAMPLE: f32 = 0.3;
/// Fewer matches than this and the plane is reported as `0` (no estimate).
const MIN_MATCHES: usize = 12;
const MIN_MATCHES_AFTER_TRIM: usize = 8;
/// Smallest share of the displacement's (weighted) energy the radial-scale model must explain.
const MIN_R_SQUARED: f64 = 0.03;
/// Significance floor for the fitted slope (t >= 5); see `fit`.
const MIN_T_SQUARED: f64 = 25.0;
/// Outlier trim: drop residuals beyond `TRIM_MADS` × the median residual (floored).
const TRIM_MADS: f64 = 2.5;
const TRIM_FLOOR: f64 = 0.05;
/// Images smaller than this (decimated) have too little structure to fit.
const MIN_SIDE: usize = 32;

/// Interleaved R,G,B `u16` linear camera RGB, as `nicti_cornea::LinearFrame` carries it.
#[derive(Clone, Copy, Debug)]
pub struct RgbU16<'a> {
    pub width: usize,
    pub height: usize,
    /// `width * height * 3` samples.
    pub pixels: &'a [u16],
    /// Black and white level: samples are normalised to `(v - black) / (white - black)`.
    pub black: f32,
    pub white: f32,
}

struct Plane {
    w: usize,
    h: usize,
    data: Vec<f32>,
}

impl Plane {
    fn at(&self, x: usize, y: usize) -> f32 {
        self.data[y * self.w + x]
    }

    fn bilinear(&self, x: f64, y: f64) -> f32 {
        let fx = x.clamp(0.0, (self.w - 1) as f64);
        let fy = y.clamp(0.0, (self.h - 1) as f64);
        let (x0, y0) = (fx.floor() as usize, fy.floor() as usize);
        let (x1, y1) = ((x0 + 1).min(self.w - 1), (y0 + 1).min(self.h - 1));
        let (tx, ty) = ((fx - x0 as f64) as f32, (fy - y0 as f64) as f32);
        let a = self.at(x0, y0) + (self.at(x1, y0) - self.at(x0, y0)) * tx;
        let b = self.at(x0, y1) + (self.at(x1, y1) - self.at(x0, y1)) * tx;
        a + (b - a) * ty
    }
}

/// 2x2 box-decimate channel `c` of an interleaved frame into a normalised float plane.
fn decimated_plane(img: &RgbU16<'_>, c: usize) -> Plane {
    let (w, h) = (img.width / 2, img.height / 2);
    let span = (img.white - img.black).max(1.0);
    let norm = |v: u16| ((v as f32 - img.black) / span).max(0.0);
    let mut data = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            let mut s = 0.0;
            for (dy, dx) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                s += norm(img.pixels[((2 * y + dy) * img.width + 2 * x + dx) * 3 + c]);
            }
            data.push(s * 0.25);
        }
    }
    Plane { w, h, data }
}

fn znorm(v: &mut [f32]) -> bool {
    let n = v.len() as f32;
    let mean = v.iter().sum::<f32>() / n;
    let sd = (v.iter().map(|x| (x - mean) * (x - mean)).sum::<f32>() / n).sqrt();
    if sd < 1e-5 {
        return false;
    }
    v.iter_mut().for_each(|x| *x = (*x - mean) / sd);
    true
}

/// Weighted least squares for `d = α·r` through the origin, then two rounds of residual trimming.
fn fit(obs: &[(f64, f64, f64)]) -> f64 {
    if obs.len() < MIN_MATCHES {
        return 0.0;
    }
    let solve = |o: &[&(f64, f64, f64)]| {
        let (num, den) = o.iter().fold((0.0, 0.0), |(n, d), (r, dd, wt)| {
            (n + wt * dd * r, d + wt * r * r)
        });
        if den > 0.0 {
            num / den
        } else {
            0.0
        }
    };
    let all: Vec<&(f64, f64, f64)> = obs.iter().collect();
    let mut alpha = solve(&all);
    let mut kept = all;
    for _ in 0..2 {
        let mut res: Vec<f64> = obs.iter().map(|(r, d, _)| (d - alpha * r).abs()).collect();
        res.sort_by(f64::total_cmp);
        let mad = res[res.len() / 2].max(TRIM_FLOOR);
        let keep: Vec<&(f64, f64, f64)> = obs
            .iter()
            .filter(|(r, d, _)| (d - alpha * r).abs() <= TRIM_MADS * mad)
            .collect();
        if keep.len() < MIN_MATCHES_AFTER_TRIM {
            break;
        }
        alpha = solve(&keep);
        kept = keep;
    }
    // Is there a radial-scale relationship at all? Lateral CA makes the displacement proportional
    // to the radius. Noise, clipped highlights and texture produce matches too, but scattered ones
    // no radial scale explains; without a gate they yielded a confident alpha of ~1e-3 (about a
    // pixel of fringe at the corners) on frames with no CA. Over the matches the trim *kept* (the
    // junk it discarded must not veto a correct fit), require both:
    //  - **significance**: for a slope through the origin, t^2 = R^2 (n - 1) / (1 - R^2) is about
    //    chi-square(1) under "no relationship", so t >= 5 is a ~1e-6 false-positive rate however
    //    many matches there are (pure noise has R^2 ~ 1/n, a real fit 0.9+);
    //  - **effect size**: R^2 >= MIN_R_SQUARED, because with thousands of matches even a trivial
    //    correlation is "significant".
    // Measured: real CA on a clean grid R^2 0.94, noise <= 0.024 (n ~ 250-300), real CA with a
    // third of the frame cluttered 0.08 (n ~ 3800, where the fit is biased low but in the right
    // direction, which still beats none).
    let (mut ss_total, mut ss_resid) = (0.0, 0.0);
    for (r, d, wt) in &kept {
        ss_total += wt * d * d;
        ss_resid += wt * (d - alpha * r).powi(2);
    }
    if ss_total <= 0.0 {
        return 0.0;
    }
    let r2 = 1.0 - ss_resid / ss_total;
    let t_squared = r2 * (kept.len() as f64 - 1.0) / (1.0 - r2).max(1e-12);
    if r2 < MIN_R_SQUARED || t_squared < MIN_T_SQUARED {
        return 0.0;
    }
    alpha
}

/// Estimate `[α_R, α_B]`. `center` is the optical centre as a fraction of the image (`None` =
/// the middle). Returns `[0.0, 0.0]` for an image that is too small, flat, or without enough
/// clean radial edges; a plane with too few matches is reported as exactly `0`.
pub fn estimate(img: &RgbU16<'_>, center: Option<[f64; 2]>) -> [f64; 2] {
    if img.width < 2 * MIN_SIDE
        || img.height < 2 * MIN_SIDE
        || img.pixels.len() < img.width * img.height * 3
    {
        return [0.0; 2];
    }
    let planes = [
        decimated_plane(img, 0),
        decimated_plane(img, 1),
        decimated_plane(img, 2),
    ];
    let g = &planes[1];
    let (w, h) = (g.w, g.h);
    let [fx, fy] = center.unwrap_or([0.5, 0.5]);
    let (cx, cy) = (fx * w as f64, fy * h as f64);
    let hd = crate::farthest_corner(w as f64, h as f64, cx, cy);

    // Candidate edge points, strongest per grid cell so they spread over the frame.
    let mut cells: Vec<Vec<(f32, usize, usize)>> = vec![Vec::new(); CELLS * CELLS];
    for y in (6..h - 6).step_by(2) {
        for x in (6..w - 6).step_by(2) {
            let gx = g.at(x + 1, y) - g.at(x - 1, y);
            let gy = g.at(x, y + 1) - g.at(x, y - 1);
            let mag = gx.hypot(gy);
            let mean =
                (g.at(x + 1, y) + g.at(x - 1, y) + g.at(x, y + 1) + g.at(x, y - 1)) * 0.25 + 0.02;
            let rel = mag / mean;
            if rel < MIN_REL_GRADIENT {
                continue;
            }
            let (vx, vy) = (x as f64 - cx, y as f64 - cy);
            let rr = vx.hypot(vy);
            if rr < MIN_RADIUS_FRACTION * hd {
                continue;
            }
            let cos = ((gx as f64 * vx + gy as f64 * vy) / (mag as f64 * rr)).abs();
            if cos < MIN_RADIAL_COS {
                continue;
            }
            let cell = (y * CELLS / h) * CELLS + x * CELLS / w;
            let v = &mut cells[cell];
            if v.len() < PER_CELL {
                v.push((rel, x, y));
            } else if let Some(min) = v.iter_mut().min_by(|a, b| a.0.total_cmp(&b.0)) {
                if min.0 < rel {
                    *min = (rel, x, y);
                }
            }
        }
    }

    // Sub-pixel displacement of R and B against G along the radial direction.
    let mut obs: [Vec<(f64, f64, f64)>; 2] = [Vec::new(), Vec::new()];
    let reach = HALF + MAX_SHIFT;
    for &(wt, x, y) in cells.iter().flatten() {
        let (vx, vy) = (x as f64 - cx, y as f64 - cy);
        let rr = vx.hypot(vy);
        let (ux, uy) = (vx / rr, vy / rr);
        let (px, py) = (x as f64, y as f64);
        let mut gp: Vec<f32> = (-HALF..=HALF)
            .map(|t| g.bilinear(px + ux * t as f64, py + uy * t as f64))
            .collect();
        if !znorm(&mut gp) {
            continue;
        }
        for (k, ch) in [0usize, 2].into_iter().enumerate() {
            let fine: Vec<f32> = (-reach * SUB..=reach * SUB)
                .map(|i| {
                    let t = i as f64 / SUB as f64;
                    planes[ch].bilinear(px + ux * t, py + uy * t)
                })
                .collect();
            let mut best = (f32::MAX, 0i32);
            for d in -MAX_SHIFT * SUB..=MAX_SHIFT * SUB {
                let mut prof: Vec<f32> = (-HALF..=HALF)
                    .map(|t| fine[((t + reach) * SUB + d) as usize])
                    .collect();
                if !znorm(&mut prof) {
                    continue;
                }
                let ssd: f32 = prof.iter().zip(&gp).map(|(a, b)| (a - b) * (a - b)).sum();
                if ssd < best.0 {
                    best = (ssd, d);
                }
            }
            let n = (2 * HALF + 1) as f32;
            // A match pinned at the search limit is a clipped one: discard, don't bias.
            if best.0 < MAX_SSD_PER_SAMPLE * n && best.1.abs() < MAX_SHIFT * SUB {
                obs[k].push((rr, best.1 as f64 / SUB as f64, wt as f64));
            }
        }
    }
    [fit(&obs[0]), fit(&obs[1])]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smooth-edged discs on a grid (edges in every direction) with the red and blue planes
    /// magnified by `(1 + ar)` / `(1 + ab)` about `(cx, cy)` relative to green.
    fn ca_grid(w: usize, h: usize, ar: f64, ab: f64, centre: [f64; 2]) -> Vec<u16> {
        let (cx, cy) = (centre[0] * w as f64, centre[1] * h as f64);
        let cell = w as f64 / 10.0;
        let val = |x: f64, y: f64| -> f64 {
            let (fx, fy) = ((x / cell).fract() - 0.5, (y / cell).fract() - 0.5);
            let d = fx.hypot(fy) * cell - cell * 0.28;
            0.05 + 0.85 * (d / 1.2).clamp(-0.5, 0.5) + 0.425
        };
        let mut out = Vec::with_capacity(w * h * 3);
        for y in 0..h {
            for x in 0..w {
                let (x, y) = (x as f64 + 0.5, y as f64 + 0.5);
                let at = |a: f64| val(cx + (x - cx) / (1.0 + a), cy + (y - cy) / (1.0 + a));
                for v in [at(ar), at(0.0), at(ab)] {
                    out.push((v * 65535.0).round().clamp(0.0, 65535.0) as u16);
                }
            }
        }
        out
    }

    fn frame(w: usize, h: usize, px: &[u16]) -> RgbU16<'_> {
        RgbU16 {
            width: w,
            height: h,
            pixels: px,
            black: 0.0,
            white: 65535.0,
        }
    }

    #[test]
    fn recovers_planted_scales_and_a_clean_image_reads_zero() {
        let px = ca_grid(600, 400, 0.004, -0.003, [0.5, 0.5]);
        let [r, b] = estimate(&frame(600, 400, &px), None);
        assert!((r - 0.004).abs() < 0.0008, "red {r}");
        assert!((b + 0.003).abs() < 0.0008, "blue {b}");
        let clean = ca_grid(600, 400, 0.0, 0.0, [0.5, 0.5]);
        let [r, b] = estimate(&frame(600, 400, &clean), None);
        assert!(r.abs() < 0.0005 && b.abs() < 0.0005, "{r} {b}");
    }

    #[test]
    fn measures_a_scale_beyond_the_old_3px_full_resolution_limit() {
        // α = 0.01 at r ≈ 720 px is a ~7 px shift at the corners: more than LightCraft's native
        // ±3 px search could hold, inside our decimated ±6 px.
        let px = ca_grid(1200, 800, 0.01, -0.008, [0.5, 0.5]);
        let [r, b] = estimate(&frame(1200, 800, &px), None);
        assert!((r - 0.01).abs() < 0.002, "red {r}");
        assert!((b + 0.008).abs() < 0.002, "blue {b}");
    }

    #[test]
    fn fits_about_the_supplied_optical_centre() {
        let centre = [0.55, 0.45];
        let px = ca_grid(600, 400, 0.004, -0.003, centre);
        let [r, b] = estimate(&frame(600, 400, &px), Some(centre));
        assert!((r - 0.004).abs() < 0.0008, "red {r}");
        assert!((b + 0.003).abs() < 0.0008, "blue {b}");
    }

    #[test]
    fn black_level_offset_does_not_change_the_answer() {
        let px = ca_grid(600, 400, 0.004, -0.003, [0.5, 0.5]);
        let lifted: Vec<u16> = px.iter().map(|v| (*v as u32 / 2 + 1000) as u16).collect();
        let img = RgbU16 {
            width: 600,
            height: 400,
            pixels: &lifted,
            black: 1000.0,
            white: 1000.0 + 32767.5,
        };
        let [r, b] = estimate(&img, None);
        assert!((r - 0.004).abs() < 0.001, "red {r}");
        assert!((b + 0.003).abs() < 0.001, "blue {b}");
    }

    /// Small, real CA must survive an ordinary photo's clutter and noise: the gate exists to reject
    /// scatter, not to throw away a correct fit because some matches are junk.
    fn degraded_ca_grid(
        w: usize,
        h: usize,
        ar: f64,
        ab: f64,
        clutter: bool,
        noise: f64,
    ) -> Vec<u16> {
        let mut px = ca_grid(w, h, ar, ab, [0.5, 0.5]);
        let mut state = 0x1234_5678_9ABC_DEF1u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 11) as f64 / (1u64 << 53) as f64
        };
        for y in 0..h {
            for x in 0..w {
                for c in 0..3 {
                    let i = (y * w + x) * 3 + c;
                    let mut v = px[i] as f64 / 65535.0;
                    if clutter && x < w / 3 {
                        // Foliage-like texture, uncorrelated between channels.
                        v = 0.3 + 0.25 * ((x as f64 * 0.9 + c as f64 * 2.1).sin() * next());
                    }
                    v += noise * (next() - 0.5);
                    px[i] = (v.clamp(0.0, 1.0) * 65535.0) as u16;
                }
            }
        }
        px
    }

    #[test]
    fn small_real_ca_survives_clutter_and_moderate_noise() {
        // A 2400x1600 frame: alpha 0.0015 is ~2 px of fringe at the corners, the size of CA a user
        // would actually want removed (at 1200 px, 0.001 is a 0.35 px fringe, below the matcher's own
        // scatter, and declining to estimate there is the right call). Junk matches pull the
        // least-squares slope toward zero, so the bar is the right sign and a useful share of the
        // magnitude (a partial correction in the right direction beats none), not exactness.
        for (clutter, noise) in [(true, 0.0), (false, 0.1), (true, 0.1)] {
            let px = degraded_ca_grid(2400, 1600, 0.0015, -0.0015, clutter, noise);
            let [r, b] = estimate(&frame(2400, 1600, &px), None);
            assert!(
                (0.0006..0.0022).contains(&r) && (-0.0022..-0.0006).contains(&b),
                "clutter {clutter} noise {noise}: alpha ({r}, {b}) for a planted (0.0015, -0.0015)"
            );
        }
    }

    /// Independent white noise per channel: there is no CA to find, so any confident non-zero
    /// answer would paint fringes onto a clean high-ISO frame.
    fn noise_frame(w: usize, h: usize, seed: u64, level: f64) -> Vec<u16> {
        let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 11) as f64 / (1u64 << 53) as f64
        };
        (0..w * h * 3)
            .map(|_| ((0.3 + level * (next() - 0.5)) * 65535.0).clamp(0.0, 65535.0) as u16)
            .collect()
    }

    #[test]
    fn pure_noise_reports_no_ca() {
        for level in [0.1, 0.3, 0.6] {
            for seed in 1..=3u64 {
                let px = noise_frame(1200, 800, seed, level);
                let [r, b] = estimate(&frame(1200, 800, &px), None);
                assert!(
                    r.abs() < 2.0e-4 && b.abs() < 2.0e-4,
                    "noise level {level} seed {seed}: alpha ({r}, {b}) from a frame with no CA"
                );
            }
        }
    }

    /// Blown highlights clip each channel at its own boundary; that is not lateral CA either.
    #[test]
    fn clipped_highlights_report_no_ca() {
        let (w, h) = (1200usize, 800usize);
        let cell = w as f64 / 10.0;
        let mut px = Vec::with_capacity(w * h * 3);
        for y in 0..h {
            for x in 0..w {
                let (fx, fy) = (
                    (x as f64 / cell).fract() - 0.5,
                    (y as f64 / cell).fract() - 0.5,
                );
                let d = fx.hypot(fy) * cell - cell * 0.28;
                let base = 1.4 - 1.6 * (d / 1.2).clamp(-0.5, 0.5);
                // Different gain per channel, all clipping at 1.0: boundaries land at different
                // places per channel, with no radial scale relationship.
                for gain in [1.25, 1.0, 0.8] {
                    px.push(((base * gain).clamp(0.0, 1.0) * 65535.0) as u16);
                }
            }
        }
        let [r, b] = estimate(&frame(w, h, &px), None);
        assert!(r.abs() < 2.0e-4 && b.abs() < 2.0e-4, "alpha ({r}, {b})");
    }

    /// Cost at a Z8-sized frame (`cargo test -p nicti-iris --release lateral_ca::tests::throughput
    /// -- --ignored --nocapture`). The estimate only runs on a baked-cache miss with Remove CA on.
    #[test]
    #[ignore = "benchmark: run explicitly in release"]
    fn throughput_at_45mp() {
        let (w, h) = (8256usize, 5504usize);
        let px = ca_grid(w, h, 0.002, -0.0015, [0.5, 0.5]);
        let img = frame(w, h, &px);
        let started = std::time::Instant::now();
        let [r, b] = estimate(&img, None);
        eprintln!(
            "estimate at {w}x{h} ({} MP): {:?} -> alpha ({r:.5}, {b:.5})",
            w * h / 1_000_000,
            started.elapsed()
        );
        assert!((r - 0.002).abs() < 0.001 && (b + 0.0015).abs() < 0.001);
    }

    #[test]
    fn degenerate_inputs_return_zero_without_panicking() {
        assert_eq!(estimate(&frame(10, 10, &[0; 300]), None), [0.0; 2]);
        // Flat image: no edges.
        let flat = vec![30000u16; 600 * 400 * 3];
        assert_eq!(estimate(&frame(600, 400, &flat), None), [0.0; 2]);
        // Pixel buffer shorter than the claimed size.
        assert_eq!(estimate(&frame(600, 400, &flat[..100]), None), [0.0; 2]);
        // Odd dimensions.
        let px = ca_grid(601, 401, 0.0, 0.0, [0.5, 0.5]);
        let _ = estimate(&frame(601, 401, &px), None);
    }
}
