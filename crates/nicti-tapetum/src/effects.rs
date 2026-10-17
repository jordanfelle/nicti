//! Post-crop vignette and film grain (#380): the CPU reference `present_sample.wgsl` is proven
//! against, and the one place the formulas are documented.
//!
//! Both effects are functions of a pixel's position in the **crop**, not of its output pixel
//! coordinates: `uv` is `0..1` across the crop rect on each axis (see [`crop_norm`]). That is what
//! makes them tile-exact (an export tile samples the same continuous pattern as the whole frame),
//! resolution-independent (a screen-res preview and a full-size export show the same grain pattern
//! at the same place) and crop-following (a vignette is centred on the crop, not the sensor).
//!
//! The formulas are this repo's own approximation of LRC's Effects panel -- there is no LRC
//! available to match against pixel-for-pixel, only its parameter ranges and defaults.

use crate::coat::{EffectsParams, VignetteStyle};
use crate::geometry::Affine2D;

/// Rec. 709 luma weights, the same as the live suffix's local adjustments use.
const LUMA: [f32; 3] = [0.2126, 0.7152, 0.0722];

/// Grain cells along the crop's long edge at size 0 (fine) and size 1 (coarse).
pub const GRAIN_CELLS_FINE: f32 = 1500.0;
pub const GRAIN_CELLS_COARSE: f32 = 300.0;
/// The most a vignette changes exposure, in stops, at amount +-1.
pub const VIGNETTE_STOPS: f32 = 2.0;
/// The most grain moves a pixel's perceptual (cube-root) luma at amount 1.
pub const GRAIN_GAIN: f32 = 0.3;
/// The grain brightness ratio is clamped so one hostile value cannot blow a pixel out.
pub const GRAIN_RATIO_RANGE: (f32, f32) = (0.25, 4.0);
/// Preview-scale grain (#421): a pixel whose footprint is at most this many export pixels wide is
/// point sampled, exactly as an export does, so a full-size render (or an export tile) is
/// bit-identical to the pre-#421 pattern.
pub const GRAIN_MINIFY_EPS: f32 = 1.001;
/// Taps per axis for the footprint average (so at most `MAX_TAPS^2` lattice reads per octave).
pub const GRAIN_MAX_TAPS: u32 = 4;
/// `1 / integral(phi^2)` for the smoothstep value-noise basis `phi` (integral 0.7429): the ratio of
/// a wide box average's std (`1/w`) to one point sample's, used to keep the capped tap average from
/// staying noisier than the true filter.
pub const GRAIN_BOX_K: f32 = 1.346;

/// The affine mapping a *source* pixel coordinate (the unclamped, texel-centre-at-+0.5 position the
/// geometry pass samples) to crop-normalized `(u, v)`: the inverse of the crop transform, divided by
/// the crop rect's size. `None` for a degenerate (singular) transform or crop -- the caller then
/// leaves the effects off rather than divide by zero.
pub fn crop_norm(crop_transform: Affine2D, crop_w: f32, crop_h: f32) -> Option<Affine2D> {
    let det = crop_transform.a * crop_transform.d - crop_transform.b * crop_transform.c;
    if !det.is_finite() || det.abs() < 1e-9 || crop_w < 1.0 || crop_h < 1.0 {
        return None;
    }
    let (a, b, c, d) = (
        crop_transform.d / det,
        -crop_transform.b / det,
        -crop_transform.c / det,
        crop_transform.a / det,
    );
    let tx = -(a * crop_transform.tx + b * crop_transform.ty);
    let ty = -(c * crop_transform.tx + d * crop_transform.ty);
    Some(Affine2D {
        a: a / crop_w,
        b: b / crop_w,
        c: c / crop_h,
        d: d / crop_h,
        tx: tx / crop_w,
        ty: ty / crop_h,
    })
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn luma(rgb: [f32; 3]) -> f32 {
    LUMA[0] * rgb[0] + LUMA[1] * rgb[1] + LUMA[2] * rgb[2]
}

/// How far into the vignette `(u, v)` is, 0 (untouched) .. 1 (fully vignetted). `dims` is the crop
/// rect's `(width, height)`, for the aspect ratio only.
pub fn vignette_t(uv: (f32, f32), dims: (f32, f32), e: &EffectsParams) -> f32 {
    let p = ((uv.0 - 0.5) * 2.0, (uv.1 - 0.5) * 2.0);
    // Positive roundness blends the oval (which follows the crop's aspect) toward a true circle in
    // pixel space, whose half-extent is the crop's long edge.
    let ar = dims.0 / dims.1;
    let m = ar.max(1.0);
    let circle = (p.0 * ar / m, p.1 / m);
    let r = e.vignette_roundness.max(0.0);
    let q = (p.0 + (circle.0 - p.0) * r, p.1 + (circle.1 - p.1) * r);
    // Negative roundness raises the superellipse exponent from 2 (oval) toward 6 (squarish).
    let n = 2.0 + 4.0 * (-e.vignette_roundness).max(0.0);
    let dist = (q.0.abs().powf(n) + q.1.abs().powf(n)).powf(1.0 / n);
    // Normalised so the oval's corners sit at 1.
    let rn = dist / 2.0f32.powf(1.0 / n);
    let inner = e.vignette_midpoint * 0.85;
    let outer = inner + (0.05 + 0.95 * e.vignette_feather) * (1.1 - inner);
    smoothstep(inner, outer, rn)
}

/// Applies the vignette to one linear working-space pixel.
pub fn apply_vignette(
    rgb: [f32; 3],
    uv: (f32, f32),
    dims: (f32, f32),
    e: &EffectsParams,
) -> [f32; 3] {
    let t = vignette_t(uv, dims, e);
    if t <= 0.0 {
        return rgb;
    }
    let a = e.vignette_amount;
    match e.vignette_style {
        VignetteStyle::HighlightPriority => {
            let protection = e.vignette_highlights * smoothstep(0.25, 1.5, luma(rgb));
            let f = (a * VIGNETTE_STOPS * t * (1.0 - protection)).exp2();
            rgb.map(|c| c * f)
        }
        VignetteStyle::ColorPriority => {
            let f = (a * VIGNETTE_STOPS * t).exp2();
            let out = rgb.map(|c| c * f);
            let l = luma(out);
            let k = 1.0 + 0.5 * a.abs() * t;
            out.map(|c| l + (c - l) * k)
        }
        VignetteStyle::PaintOverlay => {
            let target = if a < 0.0 { 0.0 } else { 1.0 };
            let w = t * a.abs();
            rgb.map(|c| c + (target - c) * w)
        }
    }
}

/// PCG integer hash (O'Neill), identical in `present_sample.wgsl`.
pub fn pcg(v: u32) -> u32 {
    let state = v.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    let word = ((state >> ((state >> 28) + 4)) ^ state).wrapping_mul(277_803_737);
    (word >> 22) ^ word
}

/// A lattice value in `-1..1` for cell `(ix, iy)`. 24 bits of the hash, so the `f32` conversion is
/// exact and the GPU agrees to the last bit.
pub fn lattice(ix: i32, iy: i32, seed: u32) -> f32 {
    let h = pcg(pcg((ix as u32) ^ seed.wrapping_mul(0x9E37_79B9)).wrapping_add(iy as u32));
    ((h >> 8) as f32) / 8_388_608.0 - 1.0
}

/// Smooth value noise in about `-1..1` at lattice position `p`.
pub fn value_noise(p: (f32, f32), seed: u32) -> f32 {
    let (fx, fy) = (p.0.floor(), p.1.floor());
    let (ix, iy) = (fx as i32, fy as i32);
    let (tx, ty) = (p.0 - fx, p.1 - fy);
    let (wx, wy) = (tx * tx * (3.0 - 2.0 * tx), ty * ty * (3.0 - 2.0 * ty));
    let v00 = lattice(ix, iy, seed);
    // Wrapping, like the shader's i32: a hostile footprint can put a tap at +-i32::MAX.
    let (ix1, iy1) = (ix.wrapping_add(1), iy.wrapping_add(1));
    let v10 = lattice(ix1, iy, seed);
    let v01 = lattice(ix, iy1, seed);
    let v11 = lattice(ix1, iy1, seed);
    let top = v00 + (v10 - v00) * wx;
    let bottom = v01 + (v11 - v01) * wx;
    top + (bottom - top) * wy
}

/// How much lattice area one output pixel covers (#421), so a view smaller than the export (a
/// screen-size preview, a zoomed-out Develop canvas) shows the export's grain *downsampled* rather
/// than the same lattice point sampled far above its Nyquist rate (noisier at the same settings).
///
/// `cols` are the lattice-position derivatives of one output pixel step in x and y (octave 1);
/// `minify` is how many export pixels one output pixel spans (1.0 for an export or any full-size
/// render). At `minify <= GRAIN_MINIFY_EPS` the pattern is point sampled.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GrainFootprint {
    pub minify: f32,
    pub cols: [(f32, f32); 2],
}

impl GrainFootprint {
    /// One lattice sample per pixel: what an export does.
    pub const fn point() -> Self {
        Self {
            minify: 1.0,
            cols: [(0.0, 0.0); 2],
        }
    }

    /// The footprint of an output pixel. `out_to_src` is the geometry pass's output -> source
    /// transform for this render (scaled for a preview, offset for a tile), `norm` the untiled
    /// `crop_norm`, and `view_scale` how many output pixels the display squeezes into one screen
    /// pixel (`>= 1`; 1.0 when the output is shown 1:1 or larger).
    pub fn new(
        out_to_src: Affine2D,
        norm: Affine2D,
        dims: (f32, f32),
        grain_size: f32,
        view_scale: f32,
    ) -> Self {
        let m = if view_scale.is_finite() {
            view_scale.max(1.0)
        } else {
            1.0
        };
        let minify = (out_to_src.a.hypot(out_to_src.c)).max(out_to_src.b.hypot(out_to_src.d)) * m;
        if !minify.is_finite() {
            return Self::point();
        }
        let long = dims.0.max(dims.1);
        let cells = GRAIN_CELLS_FINE + (GRAIN_CELLS_COARSE - GRAIN_CELLS_FINE) * grain_size;
        let (sx, sy) = (dims.0 / long * cells * m, dims.1 / long * cells * m);
        // d(u, v) / d(output x, y) = norm_linear * out_to_src_linear.
        let duv = [
            (
                norm.a * out_to_src.a + norm.b * out_to_src.c,
                norm.c * out_to_src.a + norm.d * out_to_src.c,
            ),
            (
                norm.a * out_to_src.b + norm.b * out_to_src.d,
                norm.c * out_to_src.b + norm.d * out_to_src.d,
            ),
        ];
        Self {
            minify,
            cols: [
                (duv[0].0 * sx, duv[0].1 * sy),
                (duv[1].0 * sx, duv[1].1 * sy),
            ],
        }
    }

    /// Taps per axis and the residual gain for an octave whose lattice runs `scale` times denser
    /// than octave 1.
    fn taps(&self, scale: f32) -> (u32, f32) {
        if self.minify <= GRAIN_MINIFY_EPS {
            return (1, 1.0);
        }
        let w = scale
            * self.cols[0]
                .0
                .hypot(self.cols[0].1)
                .max(self.cols[1].0.hypot(self.cols[1].1));
        if !w.is_finite() || w <= 0.5 {
            return (1, 1.0);
        }
        let n = ((2.0 * w).ceil() as u32).clamp(1, GRAIN_MAX_TAPS);
        (n, (GRAIN_BOX_K * n as f32 / w).min(1.0))
    }
}

/// The largest display minification the grain filter distinguishes (#421); beyond it the grain is
/// already averaged to near nothing.
pub const MAX_VIEW_SCALE: f32 = 64.0;

/// Rounds a display minification to the nearest 0.25 step in `1..=MAX_VIEW_SCALE` (anything else,
/// or a magnification, is `1.0`; nearest, not up, so the footprint is never overstated) so a window-resize drag re-runs the geometry pass a few times, not on
/// every pixel of size change.
pub fn quantize_view_scale(m: f32) -> f32 {
    if !m.is_finite() || m <= 1.0 {
        return 1.0;
    }
    ((m * 4.0).round() / 4.0).clamp(1.0, MAX_VIEW_SCALE)
}

/// Stamps the display minification into the *render's* copy of the document (never the stored one)
/// so it enters the `nicti.effects` node's cache key: a canvas resize re-runs only the geometry
/// pass, and the same photo at two sizes can't be served each other's grain. A no-op when there is
/// no active grain or nothing to filter (the key then stays exactly the stored params').
pub fn stamp_view_scale(doc: &mut nicti_pawprint::EditDocument, view_scale: f32) {
    let view_scale = quantize_view_scale(view_scale);
    if view_scale <= 1.0 {
        return;
    }
    let Some(entry) = doc.stages.get_mut(crate::stages::EFFECTS) else {
        return;
    };
    let grain: EffectsParams = crate::coat::parse(&entry.params);
    if !grain.sanitized().grain_active() {
        return;
    }
    if let serde_json::Value::Object(map) = &mut entry.params {
        map.insert("view_scale".to_owned(), serde_json::json!(view_scale));
    }
}

/// One octave of value noise averaged over the pixel footprint: `n x n` taps on a regular grid
/// across it (`scale` multiplies the footprint for the denser fine octave).
fn filtered_noise(p: (f32, f32), seed: u32, fp: &GrainFootprint, scale: f32) -> f32 {
    let (n, gain) = fp.taps(scale);
    if n == 1 {
        return value_noise(p, seed);
    }
    let (c0, c1) = (fp.cols[0], fp.cols[1]);
    let mut sum = 0.0;
    for j in 0..n {
        let oy = (j as f32 + 0.5) / n as f32 - 0.5;
        for i in 0..n {
            let ox = (i as f32 + 0.5) / n as f32 - 0.5;
            let q = (
                p.0 + scale * (c0.0 * ox + c1.0 * oy),
                p.1 + scale * (c0.1 * ox + c1.1 * oy),
            );
            sum += value_noise(q, seed);
        }
    }
    sum / (n * n) as f32 * gain
}

/// The grain pattern at crop position `uv`: a smooth octave at the chosen cell size mixed with a
/// finer one by roughness, averaged over `fp` (#421). A function of position, seed and footprint.
pub fn grain_noise(
    uv: (f32, f32),
    dims: (f32, f32),
    e: &EffectsParams,
    fp: &GrainFootprint,
) -> f32 {
    let long = dims.0.max(dims.1);
    let cells = GRAIN_CELLS_FINE + (GRAIN_CELLS_COARSE - GRAIN_CELLS_FINE) * e.grain_size;
    let p = (uv.0 * dims.0 / long * cells, uv.1 * dims.1 / long * cells);
    let coarse = filtered_noise(p, e.grain_seed, fp, 1.0);
    let fine = filtered_noise(
        (p.0 * 2.1 + 17.3, p.1 * 2.1 + 17.3),
        e.grain_seed ^ 0x68E3_1DA4,
        fp,
        2.1,
    );
    coarse + (fine - coarse) * e.grain_roughness
}

/// Applies grain to one linear working-space pixel: perceptual-luma noise weighted toward
/// midtones, applied as a brightness ratio so chroma is preserved.
pub fn apply_grain(
    rgb: [f32; 3],
    uv: (f32, f32),
    dims: (f32, f32),
    e: &EffectsParams,
    fp: &GrainFootprint,
) -> [f32; 3] {
    let g_now = luma(rgb).max(0.0).cbrt().clamp(0.0, 1.0);
    let weight = 4.0 * g_now * (1.0 - g_now) + 0.15;
    let amp = e.grain_amount * GRAIN_GAIN * weight;
    let g_base = g_now.max(1e-3);
    let g_new = (g_base + amp * grain_noise(uv, dims, e, fp)).max(0.0);
    let ratio = (g_new / g_base)
        .powf(3.0)
        .clamp(GRAIN_RATIO_RANGE.0, GRAIN_RATIO_RANGE.1);
    rgb.map(|c| c * ratio)
}

/// Vignette then grain, the order LRC's Effects panel applies them in. `uv` is crop-normalized.
pub fn apply_effects(
    rgb: [f32; 3],
    uv: (f32, f32),
    dims: (f32, f32),
    e: &EffectsParams,
    fp: &GrainFootprint,
) -> [f32; 3] {
    let mut out = rgb;
    if e.vignette_active() {
        out = apply_vignette(out, uv, dims, e);
    }
    if e.grain_active() {
        out = apply_grain(out, uv, dims, e, fp);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{affine_for_crop, CropRect};

    fn vignette(amount: f32) -> EffectsParams {
        EffectsParams {
            vignette_amount: amount,
            ..EffectsParams::default()
        }
    }

    #[test]
    fn a_noop_changes_nothing_exactly() {
        let e = EffectsParams::default();
        let px = [0.2, 0.4, 0.1];
        assert_eq!(
            apply_effects(
                px,
                (0.01, 0.99),
                (300.0, 200.0),
                &e,
                &GrainFootprint::point()
            ),
            px
        );
    }

    #[test]
    fn the_centre_is_untouched_and_the_corners_darken_for_a_negative_amount() {
        let e = vignette(-0.8);
        let px = [0.3, 0.3, 0.3];
        let centre = apply_effects(px, (0.5, 0.5), (300.0, 200.0), &e, &GrainFootprint::point());
        assert_eq!(centre, px);
        let corner = apply_effects(px, (0.0, 0.0), (300.0, 200.0), &e, &GrainFootprint::point());
        assert!(corner[0] < px[0] * 0.5, "{corner:?}");
        // Positive lightens.
        let lighter = apply_effects(
            px,
            (0.0, 0.0),
            (300.0, 200.0),
            &vignette(0.8),
            &GrainFootprint::point(),
        );
        assert!(lighter[0] > px[0] * 2.0, "{lighter:?}");
    }

    #[test]
    fn the_falloff_grows_monotonically_toward_the_edge() {
        let e = vignette(-1.0);
        let mut last = -1.0;
        for i in 0..=10 {
            let t = vignette_t((0.5 + 0.05 * i as f32, 0.5), (300.0, 200.0), &e);
            assert!(t >= last, "t must not decrease: {t} < {last} at {i}");
            last = t;
        }
        assert!(last > 0.5);
    }

    #[test]
    fn midpoint_confines_the_vignette_and_feather_softens_it() {
        let at = |mid: f32, feather: f32, x: f32| {
            vignette_t(
                (x, 0.5),
                (300.0, 200.0),
                &EffectsParams {
                    vignette_amount: -1.0,
                    vignette_midpoint: mid,
                    vignette_feather: feather,
                    ..EffectsParams::default()
                },
            )
        };
        assert!(at(0.9, 0.5, 0.85) < at(0.1, 0.5, 0.85));
        // A crisp feather reaches full strength sooner than a soft one.
        assert!(at(0.5, 0.0, 0.95) > at(0.5, 1.0, 0.95));
    }

    #[test]
    fn roundness_turns_an_oval_into_a_circle_in_pixel_space() {
        // A wide crop: on the long axis edge (u=1, v=.5) vs the short axis edge (u=.5, v=1). An oval
        // follows the crop so both sit at the same normalized distance; a circle reaches the short
        // edge at the same pixel radius as the long one only far past it, so it vignettes the short
        // edge *less*.
        let dims = (400.0, 100.0);
        let oval = EffectsParams {
            vignette_amount: -1.0,
            ..EffectsParams::default()
        };
        let circle = EffectsParams {
            vignette_roundness: 1.0,
            ..oval
        };
        let long_o = vignette_t((1.0, 0.5), dims, &oval);
        let short_o = vignette_t((0.5, 1.0), dims, &oval);
        assert!((long_o - short_o).abs() < 1e-5);
        let short_c = vignette_t((0.5, 1.0), dims, &circle);
        assert!(short_c < short_o - 0.05, "{short_c} vs {short_o}");
    }

    #[test]
    fn highlight_priority_holds_bright_pixels_back_and_color_priority_keeps_saturation() {
        let corner = (0.0, 0.0);
        let dims = (300.0, 200.0);
        let plain = vignette(-1.0);
        let guarded = EffectsParams {
            vignette_highlights: 1.0,
            ..plain
        };
        let bright = [2.0, 2.0, 2.0];
        let a =
            apply_effects(bright, corner, dims, &plain, &GrainFootprint::point())[0] / bright[0];
        let b =
            apply_effects(bright, corner, dims, &guarded, &GrainFootprint::point())[0] / bright[0];
        assert!(b > a * 1.5, "highlights resist the darkening: {b} vs {a}");
        // Shadows are not protected.
        let dark = [0.01, 0.01, 0.01];
        let da = apply_effects(dark, corner, dims, &plain, &GrainFootprint::point())[0] / dark[0];
        let db = apply_effects(dark, corner, dims, &guarded, &GrainFootprint::point())[0] / dark[0];
        assert!((da - db).abs() < 1e-3);

        let colour = [0.4, 0.2, 0.1];
        let sat = |p: [f32; 3]| (p[0] - p[2]) / luma(p);
        let hp = apply_effects(colour, corner, dims, &plain, &GrainFootprint::point());
        let cp = apply_effects(
            colour,
            corner,
            dims,
            &EffectsParams {
                vignette_style: VignetteStyle::ColorPriority,
                ..plain
            },
            &GrainFootprint::point(),
        );
        assert!(sat(cp) > sat(hp) * 1.2, "{} vs {}", sat(cp), sat(hp));
    }

    #[test]
    fn paint_overlay_blends_toward_black_or_white() {
        let overlay = |amount: f32| EffectsParams {
            vignette_amount: amount,
            vignette_style: VignetteStyle::PaintOverlay,
            ..EffectsParams::default()
        };
        let px = [0.4, 0.4, 0.4];
        let dark = apply_effects(
            px,
            (0.0, 0.0),
            (300.0, 200.0),
            &overlay(-1.0),
            &GrainFootprint::point(),
        );
        let light = apply_effects(
            px,
            (0.0, 0.0),
            (300.0, 200.0),
            &overlay(1.0),
            &GrainFootprint::point(),
        );
        assert!(dark[0] < 0.05 && light[0] > 0.95, "{dark:?} {light:?}");
    }

    #[test]
    fn grain_is_deterministic_seeded_and_zero_mean() {
        let e = EffectsParams {
            grain_amount: 1.0,
            ..EffectsParams::default()
        };
        let dims = (600.0, 400.0);
        let at = |u: f32, v: f32, e: &EffectsParams| {
            grain_noise((u, v), dims, e, &GrainFootprint::point())
        };
        assert_eq!(at(0.3, 0.7, &e), at(0.3, 0.7, &e));
        let reseeded = EffectsParams {
            grain_seed: 99,
            ..e
        };
        assert_ne!(at(0.3, 0.7, &e), at(0.3, 0.7, &reseeded));
        let mut sum = 0.0f64;
        let mut max = 0.0f32;
        let n = 100;
        for i in 0..n {
            for j in 0..n {
                let v = at(i as f32 / n as f32, j as f32 / n as f32, &e);
                sum += f64::from(v);
                max = max.max(v.abs());
            }
        }
        assert!((sum / f64::from(n * n)).abs() < 0.05, "mean {}", sum);
        assert!(max <= 1.0 + 1e-4, "value noise stays within -1..1: {max}");
    }

    #[test]
    fn a_larger_grain_size_is_smoother() {
        let e = EffectsParams {
            grain_amount: 1.0,
            ..EffectsParams::default()
        };
        // Neighbouring points differ less at a coarser cell size.
        let step = |size: f32| {
            let e = EffectsParams {
                grain_size: size,
                grain_roughness: 0.0,
                ..e
            };
            (0..200)
                .map(|i| {
                    let u = i as f32 / 2000.0;
                    (grain_noise(
                        (u + 0.0005, 0.5),
                        (600.0, 400.0),
                        &e,
                        &GrainFootprint::point(),
                    ) - grain_noise((u, 0.5), (600.0, 400.0), &e, &GrainFootprint::point()))
                    .abs()
                })
                .sum::<f32>()
        };
        assert!(step(1.0) < step(0.0));
    }

    #[test]
    fn grain_weights_midtones_and_preserves_chroma() {
        let e = EffectsParams {
            grain_amount: 1.0,
            ..EffectsParams::default()
        };
        let spread = |px: [f32; 3]| {
            (0..400)
                .map(|i| {
                    let u = (i % 20) as f32 / 20.0;
                    let v = (i / 20) as f32 / 20.0;
                    (apply_grain(px, (u, v), (600.0, 400.0), &e, &GrainFootprint::point())[0]
                        / px[0]
                        - 1.0)
                        .abs()
                })
                .sum::<f32>()
        };
        assert!(spread([0.18, 0.18, 0.18]) > spread([0.9, 0.9, 0.9]));
        let tinted = apply_grain(
            [0.4, 0.2, 0.1],
            (0.3, 0.3),
            (600.0, 400.0),
            &e,
            &GrainFootprint::point(),
        );
        let ratios = [tinted[0] / 0.4, tinted[1] / 0.2, tinted[2] / 0.1];
        assert!((ratios[0] - ratios[1]).abs() < 1e-4 && (ratios[1] - ratios[2]).abs() < 1e-4);
    }

    #[test]
    fn hash_values_are_reference_pinned_so_a_shader_edit_cannot_drift_silently() {
        // Reference vectors computed independently (Python, 32-bit wrapping): if these change,
        // present_sample.wgsl's copy of the hash must change with them.
        assert_eq!(pcg(0), 0x07BB_2FE2);
        assert_eq!(pcg(1), 0xA8BE_EA3C);
        assert_eq!(pcg(12_345), 0xF45E_AD0E);
        assert_eq!(lattice(5, -3, 7), 0.641_302_6);
    }

    #[test]
    fn crop_norm_maps_the_crop_rect_onto_the_unit_square_through_a_rotation() {
        let rect = CropRect {
            x: 100.0,
            y: 50.0,
            width: 400.0,
            height: 200.0,
        };
        for rotation in [0.0f32, 7.5, -20.0] {
            let tf = affine_for_crop(rect, rotation);
            let norm = crop_norm(tf, rect.width, rect.height).unwrap();
            // The output pixel (ox, oy) (texel centre) samples source `tf(o)`, and that must map
            // back to (ox, oy) / (w, h).
            for (ox, oy) in [(0.5, 0.5), (200.0, 100.0), (399.5, 199.5), (10.0, 190.0)] {
                let s = tf.apply((ox, oy));
                let (u, v) = norm.apply(s);
                assert!((u - ox / 400.0).abs() < 1e-4, "{rotation}: u {u}");
                assert!((v - oy / 200.0).abs() < 1e-4, "{rotation}: v {v}");
            }
        }
    }

    #[test]
    fn crop_norm_refuses_a_degenerate_transform() {
        let singular = Affine2D {
            a: 0.0,
            b: 0.0,
            c: 0.0,
            d: 0.0,
            tx: 0.0,
            ty: 0.0,
        };
        assert!(crop_norm(singular, 100.0, 100.0).is_none());
        assert!(crop_norm(Affine2D::IDENTITY, 0.0, 100.0).is_none());
    }

    /// The grain of a `scale`-times-smaller render (output -> source transform scaled by `scale`)
    /// over a `n x n` window of output pixels, filtered by `GrainFootprint`.
    fn scaled_grain(e: &EffectsParams, scale: f32, n: usize, filtered: bool) -> Vec<f32> {
        let (w, h) = (4096.0f32, 2730.0f32);
        let a = Affine2D {
            a: scale,
            d: scale,
            ..Affine2D::IDENTITY
        };
        let norm = crop_norm(Affine2D::IDENTITY, w, h).unwrap();
        let fp = if filtered {
            GrainFootprint::new(a, norm, (w, h), e.grain_size, 1.0)
        } else {
            GrainFootprint::point()
        };
        let mut out = Vec::with_capacity(n * n);
        for oy in 0..n {
            for ox in 0..n {
                let s = a.apply((ox as f32 + 0.5, oy as f32 + 0.5));
                out.push(grain_noise(norm.apply(s), (w, h), e, &fp));
            }
        }
        out
    }

    fn std_dev(v: &[f32]) -> f32 {
        let mean = v.iter().sum::<f32>() / v.len() as f32;
        (v.iter().map(|x| (x - mean) * (x - mean)).sum::<f32>() / v.len() as f32).sqrt()
    }

    /// #421: a render `scale` times smaller than the export must show what the export looks like
    /// shrunk by `scale` -- the std of the export's grain box-averaged `scale x scale`.
    #[test]
    fn a_scaled_render_matches_the_downsampled_export() {
        for (size, roughness) in [(0.0f32, 0.0f32), (0.0, 1.0), (0.5, 0.5), (1.0, 0.5)] {
            let e = EffectsParams {
                grain_amount: 1.0,
                grain_size: size,
                grain_roughness: roughness,
                ..EffectsParams::default()
            };
            for scale in [2usize, 4, 8, 16] {
                let n = 48;
                let full = scaled_grain(&e, 1.0, n * scale, false);
                let mut down = Vec::with_capacity(n * n);
                for by in 0..n {
                    for bx in 0..n {
                        let mut sum = 0.0;
                        for y in 0..scale {
                            for x in 0..scale {
                                sum += full[(by * scale + y) * n * scale + bx * scale + x];
                            }
                        }
                        down.push(sum / (scale * scale) as f32);
                    }
                }
                let want = std_dev(&down);
                let got = std_dev(&scaled_grain(&e, scale as f32, n, true));
                let point = std_dev(&scaled_grain(&e, scale as f32, n, false));
                let ratio = got / want;
                eprintln!(
                    "size {size} rough {roughness} x{scale}: want {want:.4} got {got:.4} ({ratio:.2}) point {point:.4}"
                );
                assert!(
                    (0.88..=1.12).contains(&ratio),
                    "size {size} rough {roughness} x{scale}: filtered std {got} vs downsampled export {want}"
                );
            }
        }
    }

    /// #421: at export scale (and for any full-size tile) the footprint changes nothing -- the
    /// result is bit-identical to a point sample.
    #[test]
    fn export_scale_grain_is_a_point_sample() {
        let e = EffectsParams {
            grain_amount: 1.0,
            grain_roughness: 0.6,
            ..EffectsParams::default()
        };
        let dims = (300.0, 200.0);
        // Dense lattice per pixel (a tiny crop), but 1:1 with the export: still unfiltered.
        let norm = crop_norm(Affine2D::IDENTITY, dims.0, dims.1).unwrap();
        let fp = GrainFootprint::new(Affine2D::IDENTITY, norm, dims, 0.0, 1.0);
        for (u, v) in [(0.1, 0.2), (0.5, 0.5), (0.93, 0.31)] {
            assert_eq!(
                grain_noise((u, v), dims, &e, &fp),
                grain_noise((u, v), dims, &e, &GrainFootprint::point())
            );
        }
        // A view scale below 1 is not a magnification of the grain.
        let zoomed = GrainFootprint::new(Affine2D::IDENTITY, norm, dims, 0.0, 0.25);
        assert_eq!(zoomed, fp);
    }

    #[test]
    fn a_hostile_footprint_cannot_blow_up_the_tap_count() {
        let norm = crop_norm(Affine2D::IDENTITY, 100.0, 100.0).unwrap();
        let wild = Affine2D {
            a: 1e30,
            d: 1e30,
            ..Affine2D::IDENTITY
        };
        let fp = GrainFootprint::new(wild, norm, (100.0, 100.0), 0.0, 1.0);
        let e = EffectsParams {
            grain_amount: 1.0,
            ..EffectsParams::default()
        };
        assert!(grain_noise((0.5, 0.5), (100.0, 100.0), &e, &fp).is_finite());
        let nan = GrainFootprint::new(Affine2D::IDENTITY, norm, (100.0, 100.0), 0.0, f32::NAN);
        assert_eq!(nan.minify, 1.0);
    }

    #[test]
    fn the_view_scale_is_quantized_and_enters_the_stamped_document_only_for_active_grain() {
        assert_eq!(quantize_view_scale(0.5), 1.0);
        assert_eq!(quantize_view_scale(f32::NAN), 1.0);
        assert_eq!(quantize_view_scale(1.01), 1.0);
        assert_eq!(quantize_view_scale(1.13), 1.25);
        assert_eq!(quantize_view_scale(3.1), 3.0);
        assert_eq!(quantize_view_scale(1e9), MAX_VIEW_SCALE);

        let mut doc = nicti_pawprint::EditDocument::default();
        stamp_view_scale(&mut doc, 3.0);
        assert!(!doc.stages.contains_key(crate::stages::EFFECTS));
        let entry = |grain: f32| nicti_pawprint::StageEntry {
            schema_version: 1,
            params: serde_json::json!({ "grain_amount": grain }),
        };
        doc.stages
            .insert(crate::stages::EFFECTS.to_owned(), entry(0.0));
        stamp_view_scale(&mut doc, 3.0);
        assert!(doc.stages[crate::stages::EFFECTS]
            .params
            .get("view_scale")
            .is_none());
        doc.stages
            .insert(crate::stages::EFFECTS.to_owned(), entry(0.5));
        stamp_view_scale(&mut doc, 1.0);
        assert!(doc.stages[crate::stages::EFFECTS]
            .params
            .get("view_scale")
            .is_none());
        stamp_view_scale(&mut doc, 3.2);
        assert_eq!(
            doc.stages[crate::stages::EFFECTS].params["view_scale"],
            serde_json::json!(3.25)
        );
        // Unknown to the typed params: it changes the cache key, not the parsed effects.
        let parsed: EffectsParams = crate::coat::parse(&doc.stages[crate::stages::EFFECTS].params);
        assert_eq!(parsed.grain_amount, 0.5);
    }

    /// Speculative review finding on #421: a rotated crop's transform columns are unit length only
    /// up to f32 rounding, so the point-sample gate must sit above that noise.
    #[test]
    fn a_rotated_export_transform_stays_under_the_point_sample_gate() {
        use crate::geometry::{affine_for_crop, CropRect};
        let rect = CropRect {
            x: 100.0,
            y: 50.0,
            width: 6000.0,
            height: 4000.0,
        };
        for rotation in [0.0f32, 0.3, 7.5, -20.0, 45.0, 89.9, 133.0] {
            let tf = affine_for_crop(rect, rotation);
            let norm = crop_norm(tf, rect.width, rect.height).unwrap();
            let fp = GrainFootprint::new(tf, norm, (rect.width, rect.height), 0.0, 1.0);
            assert!(fp.minify <= GRAIN_MINIFY_EPS, "{rotation}: {}", fp.minify);
        }
    }
}
