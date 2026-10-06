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
    let v10 = lattice(ix + 1, iy, seed);
    let v01 = lattice(ix, iy + 1, seed);
    let v11 = lattice(ix + 1, iy + 1, seed);
    let top = v00 + (v10 - v00) * wx;
    let bottom = v01 + (v11 - v01) * wx;
    top + (bottom - top) * wy
}

/// The grain pattern at crop position `uv`: a smooth octave at the chosen cell size mixed with a
/// finer one by roughness. A function of position and seed only.
pub fn grain_noise(uv: (f32, f32), dims: (f32, f32), e: &EffectsParams) -> f32 {
    let long = dims.0.max(dims.1);
    let cells = GRAIN_CELLS_FINE + (GRAIN_CELLS_COARSE - GRAIN_CELLS_FINE) * e.grain_size;
    let p = (uv.0 * dims.0 / long * cells, uv.1 * dims.1 / long * cells);
    let coarse = value_noise(p, e.grain_seed);
    let fine = value_noise(
        (p.0 * 2.1 + 17.3, p.1 * 2.1 + 17.3),
        e.grain_seed ^ 0x68E3_1DA4,
    );
    coarse + (fine - coarse) * e.grain_roughness
}

/// Applies grain to one linear working-space pixel: perceptual-luma noise weighted toward
/// midtones, applied as a brightness ratio so chroma is preserved.
pub fn apply_grain(rgb: [f32; 3], uv: (f32, f32), dims: (f32, f32), e: &EffectsParams) -> [f32; 3] {
    let g_now = luma(rgb).max(0.0).cbrt().clamp(0.0, 1.0);
    let weight = 4.0 * g_now * (1.0 - g_now) + 0.15;
    let amp = e.grain_amount * GRAIN_GAIN * weight;
    let g_base = g_now.max(1e-3);
    let g_new = (g_base + amp * grain_noise(uv, dims, e)).max(0.0);
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
) -> [f32; 3] {
    let mut out = rgb;
    if e.vignette_active() {
        out = apply_vignette(out, uv, dims, e);
    }
    if e.grain_active() {
        out = apply_grain(out, uv, dims, e);
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
        assert_eq!(apply_effects(px, (0.01, 0.99), (300.0, 200.0), &e), px);
    }

    #[test]
    fn the_centre_is_untouched_and_the_corners_darken_for_a_negative_amount() {
        let e = vignette(-0.8);
        let px = [0.3, 0.3, 0.3];
        let centre = apply_effects(px, (0.5, 0.5), (300.0, 200.0), &e);
        assert_eq!(centre, px);
        let corner = apply_effects(px, (0.0, 0.0), (300.0, 200.0), &e);
        assert!(corner[0] < px[0] * 0.5, "{corner:?}");
        // Positive lightens.
        let lighter = apply_effects(px, (0.0, 0.0), (300.0, 200.0), &vignette(0.8));
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
        let a = apply_effects(bright, corner, dims, &plain)[0] / bright[0];
        let b = apply_effects(bright, corner, dims, &guarded)[0] / bright[0];
        assert!(b > a * 1.5, "highlights resist the darkening: {b} vs {a}");
        // Shadows are not protected.
        let dark = [0.01, 0.01, 0.01];
        let da = apply_effects(dark, corner, dims, &plain)[0] / dark[0];
        let db = apply_effects(dark, corner, dims, &guarded)[0] / dark[0];
        assert!((da - db).abs() < 1e-3);

        let colour = [0.4, 0.2, 0.1];
        let sat = |p: [f32; 3]| (p[0] - p[2]) / luma(p);
        let hp = apply_effects(colour, corner, dims, &plain);
        let cp = apply_effects(
            colour,
            corner,
            dims,
            &EffectsParams {
                vignette_style: VignetteStyle::ColorPriority,
                ..plain
            },
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
        let dark = apply_effects(px, (0.0, 0.0), (300.0, 200.0), &overlay(-1.0));
        let light = apply_effects(px, (0.0, 0.0), (300.0, 200.0), &overlay(1.0));
        assert!(dark[0] < 0.05 && light[0] > 0.95, "{dark:?} {light:?}");
    }

    #[test]
    fn grain_is_deterministic_seeded_and_zero_mean() {
        let e = EffectsParams {
            grain_amount: 1.0,
            ..EffectsParams::default()
        };
        let dims = (600.0, 400.0);
        let at = |u: f32, v: f32, e: &EffectsParams| grain_noise((u, v), dims, e);
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
                    (grain_noise((u + 0.0005, 0.5), (600.0, 400.0), &e)
                        - grain_noise((u, 0.5), (600.0, 400.0), &e))
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
                    (apply_grain(px, (u, v), (600.0, 400.0), &e)[0] / px[0] - 1.0).abs()
                })
                .sum::<f32>()
        };
        assert!(spread([0.18, 0.18, 0.18]) > spread([0.9, 0.9, 0.9]));
        let tinted = apply_grain([0.4, 0.2, 0.1], (0.3, 0.3), (600.0, 400.0), &e);
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
}
