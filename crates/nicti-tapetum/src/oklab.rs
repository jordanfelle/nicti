//! OkLab/OkLCh colour operations (#432): Color Grading and Point Color.
//!
//! Both run on the live suffix's linear ProPhoto (D50) pixels: the pixel is taken to OkLab through
//! one precomputed 3x3 (ProPhoto -> XYZ D50 -> Bradford -> XYZ D65 -> LMS, then the usual cube
//! root and Ottosson's M2), edited, and taken back. `live_suffix.wgsl`'s `apply_oklab_ops` is the
//! GPU twin of [`OkLabOps::apply`]; both read the constants [`OkLabOps::new`] precomputes, so the
//! only per-pixel maths duplicated between them is the formulas below.
//!
//! The formulas (tonal weights, wheel offsets, the Point Color soft boxes) are adapted from
//! storytold/lightcraft@265248c `crates/pipeline/src/colorops.rs` (MIT OR Apache-2.0, Copyright (c)
//! 2026 ArtCraft Team and the LightCraft contributors), re-expressed for ProPhoto rather than
//! Rec.2020 and for this crate's normalized params. Like the rest of that file they are tuned by
//! eye, not fitted to LRC renders: see the follow-up parity issue before trusting them.

use crate::coat::{ColorGradeParams, GradeWheel, PointColorParams, MAX_POINT_COLORS};
use crate::color::{mat3_apply, mat3_invert, mat3_mul, Mat3, XYZ_D50_TO_PROPHOTO};

/// XYZ (D65) -> LMS, Ottosson's M1.
pub const M1: Mat3 = [
    [0.818_933, 0.361_866_74, -0.128_859_71],
    [0.032_984_544, 0.929_311_9, 0.036_145_64],
    [0.048_200_3, 0.264_366_27, 0.633_851_7],
];

/// LMS' (cube-rooted) -> Lab, Ottosson's M2.
pub const M2: Mat3 = [
    [0.210_454_26, 0.793_617_8, -0.004_072_047],
    [1.977_998_5, -2.428_592_2, 0.450_593_7],
    [0.025_904_037, 0.782_771_77, -0.808_675_77],
];

/// Bradford-adapted XYZ D50 -> XYZ D65.
const XYZ_D50_TO_D65: Mat3 = [
    [0.955_576_6, -0.023_039_3, 0.063_163_6],
    [-0.028_289_5, 1.009_941_6, 0.021_007_7],
    [0.012_298_2, -0.020_483_0, 1.329_909_8],
];

/// Linear sRGB -> XYZ (D65), for turning a wheel's painted hue into an OkLab direction.
const SRGB_TO_XYZ_D65: Mat3 = [
    [0.412_456_4, 0.357_576_1, 0.180_437_5],
    [0.212_672_9, 0.715_152_2, 0.072_175_0],
    [0.019_333_9, 0.119_192, 0.950_304_1],
];

/// Offsets a grade wheel applies in OkLab: lightness, a, b.
const WHEEL_CHROMA_SCALE: f32 = 0.09;
const WHEEL_LUM_SCALE: f32 = 0.12;

pub fn smoothstep(lo: f32, hi: f32, x: f32) -> f32 {
    if hi <= lo {
        return if x < lo { 0.0 } else { 1.0 };
    }
    let t = ((x - lo) / (hi - lo)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// ProPhoto (linear, D50) -> LMS, the matrix every pixel conversion starts with.
pub fn prophoto_to_lms() -> Mat3 {
    mat3_mul(
        M1,
        mat3_mul(XYZ_D50_TO_D65, mat3_invert(&XYZ_D50_TO_PROPHOTO)),
    )
}

/// OkLab (L, a, b) for a linear ProPhoto pixel. Negative LMS (out-of-gamut) clamps to 0 before the
/// cube root, matching the shader.
pub fn lab_from_prophoto(rgb: [f32; 3]) -> [f32; 3] {
    let lms = mat3_apply(prophoto_to_lms(), rgb).map(|v| v.max(0.0).cbrt());
    mat3_apply(M2, lms)
}

/// An OkLCh colour (`l` 0..1, `chroma`, `hue_degrees`) as an 8-bit sRGB triple, for swatches.
/// Out-of-gamut colours are clamped per channel.
pub fn srgb8_from_oklch(l: f32, chroma: f32, hue_degrees: f32) -> [u8; 3] {
    let h = hue_degrees.to_radians();
    let lab = [l, chroma * h.cos(), chroma * h.sin()];
    let lms = mat3_apply(mat3_invert(&M2), lab).map(|v| v * v * v);
    let xyz = mat3_apply(mat3_invert(&M1), lms);
    let lin = mat3_apply(mat3_invert(&SRGB_TO_XYZ_D65), xyz);
    lin.map(|c| {
        let c = c.clamp(0.0, 1.0);
        let e = if c <= 0.003_130_8 {
            12.92 * c
        } else {
            1.055 * c.powf(1.0 / 2.4) - 0.055
        };
        (e * 255.0).round() as u8
    })
}

/// A wheel's painted hue (HSV degrees, full saturation/value sRGB) as an OkLab chroma direction
/// `(cos h, sin h)`, so the picker's colours and the grade it applies agree.
pub fn wheel_direction(hue_degrees: f32) -> [f32; 2] {
    let h = hue_degrees.rem_euclid(360.0) / 60.0;
    let x = 1.0 - (h % 2.0 - 1.0).abs();
    let (r, g, b) = match h as u32 {
        0 => (1.0, x, 0.0),
        1 => (x, 1.0, 0.0),
        2 => (0.0, 1.0, x),
        3 => (0.0, x, 1.0),
        4 => (x, 0.0, 1.0),
        _ => (1.0, 0.0, x),
    };
    // Linearise the pure sRGB colour.
    let lin = |c: f32| {
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let xyz = mat3_apply(SRGB_TO_XYZ_D65, [lin(r), lin(g), lin(b)]);
    let lms = mat3_apply(M1, xyz).map(|v| v.max(0.0).cbrt());
    let lab = mat3_apply(M2, lms);
    let c = lab[1].hypot(lab[2]).max(1e-6);
    [lab[1] / c, lab[2] / c]
}

/// A wheel's effect as OkLab offsets `(dL, da, db)`.
fn wheel_offset(w: &GradeWheel) -> [f32; 3] {
    let d = wheel_direction(w.hue);
    let s = w.sat * WHEEL_CHROMA_SCALE;
    [w.lum * WHEEL_LUM_SCALE, s * d[0], s * d[1]]
}

/// Color Grading, precomputed: the four wheels' offsets and the tonal-split constants.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GradeK {
    pub shadows: [f32; 3],
    pub midtones: [f32; 3],
    pub highlights: [f32; 3],
    pub global: [f32; 3],
    /// Midpoint of the shadow/highlight split in OkLab L.
    pub split: f32,
    /// Half-overlap of the tonal ranges.
    pub width: f32,
}

impl GradeK {
    pub fn new(p: &ColorGradeParams) -> Self {
        let p = p.sanitized();
        Self {
            shadows: wheel_offset(&p.shadows),
            midtones: wheel_offset(&p.midtones),
            highlights: wheel_offset(&p.highlights),
            global: wheel_offset(&p.global),
            split: 0.5 - p.balance * 0.25,
            width: 0.15 + p.blending * 0.5,
        }
    }

    /// `(shadow, midtone, highlight)` weights for a pixel of OkLab lightness `l`.
    pub fn weights(&self, l: f32) -> [f32; 3] {
        let ws = 1.0 - smoothstep(self.split - self.width, self.split + 0.25 * self.width, l);
        let wh = smoothstep(self.split - 0.25 * self.width, self.split + self.width, l);
        [ws, (1.0 - ws - wh).max(0.0), wh]
    }

    fn apply(&self, lab: [f32; 3]) -> [f32; 3] {
        let [ws, wm, wh] = self.weights(lab[0]);
        std::array::from_fn(|i| {
            lab[i]
                + ws * self.shadows[i]
                + wm * self.midtones[i]
                + wh * self.highlights[i]
                + self.global[i]
        })
    }
}

/// One Point Color sample, precomputed into the units the shader works in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PointK {
    /// The sampled colour in OkLCh: L, chroma, hue (radians).
    pub l: f32,
    pub c: f32,
    pub h: f32,
    /// Hue only means something when the sample itself is chromatic.
    pub has_hue: bool,
    /// Soft-box half widths: hue (radians), chroma, lightness.
    pub hue_half: f32,
    pub chroma_half: f32,
    pub light_half: f32,
    /// Adjustments: hue shift (radians), saturation scale, lightness shift, variance.
    pub dh: f32,
    pub sat: f32,
    pub dl: f32,
    pub var: f32,
}

impl PointK {
    pub fn new(s: &crate::coat::PointColorSample) -> Self {
        let s = s.sanitized();
        let k = (s.range / 0.5).max(0.05);
        Self {
            l: s.lum,
            c: s.chroma,
            h: s.hue.to_radians(),
            has_hue: s.chroma >= 0.02,
            hue_half: (0.1 + 0.6 * s.hue_range) * k,
            chroma_half: (0.02 + 0.12 * s.sat_range) * k,
            light_half: (0.05 + 0.4 * s.lum_range) * k,
            dh: s.hue_shift * 0.5,
            sat: s.sat_shift,
            dl: s.lum_shift * 0.2,
            var: s.variance * 0.8,
        }
    }

    /// Soft-box weight of a pixel (OkLCh `l`, `c`, hue `h` radians) around this sample.
    pub fn weight(&self, l: f32, c: f32, h: f32) -> f32 {
        let soft = |half: f32, d: f32| 1.0 - smoothstep(0.5 * half, half, d.abs());
        let dh = wrap_pi(h - self.h);
        let hue_w = if self.has_hue {
            soft(self.hue_half, dh) * smoothstep(0.005, 0.025, c)
        } else {
            1.0
        };
        hue_w * soft(self.chroma_half, c - self.c) * soft(self.light_half, l - self.l)
    }
}

fn wrap_pi(a: f32) -> f32 {
    let tau = std::f32::consts::TAU;
    let r = (a + std::f32::consts::PI).rem_euclid(tau);
    r - std::f32::consts::PI
}

/// Color Grading plus Point Color with every constant precomputed. [`Self::apply`] is the CPU
/// reference for `live_suffix.wgsl`'s `apply_oklab_ops`.
#[derive(Debug, Clone, PartialEq)]
pub struct OkLabOps {
    /// ProPhoto -> LMS and its inverse.
    pub to_lms: Mat3,
    pub from_lms: Mat3,
    pub grade: Option<GradeK>,
    pub points: [Option<PointK>; MAX_POINT_COLORS],
}

impl OkLabOps {
    pub fn new(grade: &ColorGradeParams, points: &PointColorParams) -> Self {
        // Judge no-op-ness on the cleaned values, so a hand-edited out-of-range document that
        // sanitizes to nothing does not pay for (and perturb pixels with) the OkLab round trip.
        let grade = grade.sanitized();
        let to_lms = prophoto_to_lms();
        let mut slots = [None; MAX_POINT_COLORS];
        let sanitized = points.sanitized();
        for (slot, s) in slots.iter_mut().zip(sanitized.live()) {
            if !s.is_noop() {
                *slot = Some(PointK::new(s));
            }
        }
        Self {
            to_lms,
            from_lms: mat3_invert(&to_lms),
            grade: (!grade.is_noop()).then(|| GradeK::new(&grade)),
            points: slots,
        }
    }

    /// True when there is nothing to do, so the shader skips the OkLab round trip entirely.
    pub fn is_noop(&self) -> bool {
        self.grade.is_none() && self.points.iter().all(Option::is_none)
    }

    pub fn apply(&self, rgb: [f32; 3]) -> [f32; 3] {
        if self.is_noop() {
            return rgb;
        }
        let lms = mat3_apply(self.to_lms, rgb).map(|v| v.max(0.0).cbrt());
        let mut lab = mat3_apply(M2, lms);
        if let Some(g) = &self.grade {
            lab = g.apply(lab);
        }
        if self.points.iter().any(Option::is_some) {
            let mut l = lab[0];
            let mut c = lab[1].hypot(lab[2]);
            let mut h = lab[2].atan2(lab[1]);
            let (l0, c0, h0) = (l, c, h);
            for p in self.points.iter().flatten() {
                let w = p.weight(l0, c0, h0);
                if w <= 0.0 {
                    continue;
                }
                let dh = wrap_pi(h0 - p.h);
                h += w * (p.var * dh + p.dh);
                c += w * p.var * (c0 - p.c);
                c *= 1.0 + w * p.sat;
                l += w * (p.var * (l0 - p.l) + p.dl);
            }
            let c = c.max(0.0);
            lab = [l, c * h.cos(), c * h.sin()];
        }
        let m2_inv = mat3_invert(&M2);
        let lms = mat3_apply(m2_inv, lab).map(|v| v * v * v);
        mat3_apply(self.from_lms, lms).map(|v| v.max(0.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coat::PointColorSample;

    fn close(a: [f32; 3], b: [f32; 3], eps: f32) -> bool {
        a.iter().zip(&b).all(|(x, y)| (x - y).abs() < eps)
    }

    #[test]
    fn default_params_are_an_exact_noop() {
        let ops = OkLabOps::new(&ColorGradeParams::default(), &PointColorParams::default());
        assert!(ops.is_noop());
        assert_eq!(ops.apply([0.3, 0.2, 0.7]), [0.3, 0.2, 0.7]);
    }

    #[test]
    fn out_of_range_values_that_sanitize_to_nothing_are_still_a_noop() {
        let grade = ColorGradeParams {
            global: GradeWheel {
                hue: 10.0,
                sat: -0.5,
                lum: f32::NAN,
            },
            ..Default::default()
        };
        assert!(OkLabOps::new(&grade, &PointColorParams::default()).is_noop());
    }

    #[test]
    fn the_oklab_round_trip_is_lossless_for_in_gamut_pixels() {
        // A grade that is active but zero-strength still takes the round trip.
        let grade = ColorGradeParams {
            global: GradeWheel {
                hue: 10.0,
                sat: 1e-9,
                lum: 0.0,
            },
            ..Default::default()
        };
        let ops = OkLabOps::new(&grade, &PointColorParams::default());
        assert!(!ops.is_noop());
        for px in [
            [0.2, 0.3, 0.1],
            [0.5, 0.05, 0.4],
            [0.9, 0.9, 0.9],
            [0.01, 0.02, 0.2],
        ] {
            assert!(
                close(ops.apply(px), px, 2e-3),
                "{px:?} -> {:?}",
                ops.apply(px)
            );
        }
    }

    #[test]
    fn tonal_weights_partition_unity_and_follow_lightness() {
        let k = GradeK::new(&ColorGradeParams::default());
        for i in 0..=20 {
            let l = i as f32 / 20.0;
            let w = k.weights(l);
            assert!((w.iter().sum::<f32>() - 1.0).abs() < 1e-5, "L={l}: {w:?}");
        }
        assert!(k.weights(0.02)[0] > 0.95 && k.weights(0.98)[2] > 0.95);
        // Negative balance grows the shadow range (a higher split), positive the highlight range.
        let toward_shadows = GradeK::new(&ColorGradeParams {
            balance: -1.0,
            ..Default::default()
        });
        assert!(toward_shadows.split > k.split);
        assert!(toward_shadows.weights(0.6)[0] > k.weights(0.6)[0]);
    }

    #[test]
    fn a_shadow_wheel_tints_shadows_far_more_than_highlights() {
        let grade = ColorGradeParams {
            shadows: GradeWheel {
                hue: 240.0,
                sat: 1.0,
                lum: 0.0,
            },
            ..Default::default()
        };
        let ops = OkLabOps::new(&grade, &PointColorParams::default());
        let spread = |c: [f32; 3]| {
            c.iter().cloned().fold(f32::MIN, f32::max) - c.iter().cloned().fold(f32::MAX, f32::min)
        };
        let dark = ops.apply([0.02, 0.02, 0.02]);
        let bright = ops.apply([0.8, 0.8, 0.8]);
        assert!(spread(dark) / 0.02 > 10.0 * (spread(bright) / 0.8).max(1e-6));
        // The wheel's hue is blue, so the shadows should lean blue.
        assert!(dark[2] > dark[0]);
    }

    #[test]
    fn point_color_only_moves_colours_near_its_sample() {
        let near = [0.4, 0.1, 0.1];
        let far = [0.1, 0.1, 0.5];
        let lab = lab_from_prophoto(near);
        let sample = PointColorSample {
            lum: lab[0],
            chroma: lab[1].hypot(lab[2]),
            hue: lab[2].atan2(lab[1]).to_degrees().rem_euclid(360.0),
            lum_shift: 0.5,
            ..Default::default()
        };
        let mut points = PointColorParams {
            count: 1,
            ..Default::default()
        };
        points.samples[0] = sample;
        let ops = OkLabOps::new(&ColorGradeParams::default(), &points);
        let moved = ops.apply(near);
        assert!(
            moved[0] > near[0] + 0.01,
            "near colour should brighten: {moved:?}"
        );
        assert!(close(ops.apply(far), far, 2e-3), "far colour must not move");
    }

    #[test]
    fn swatch_conversion_round_trips_a_known_colour() {
        // sRGB (200, 80, 40) -> OkLCh via the forward path used elsewhere, then back to sRGB.
        let lin = |c: f32| {
            let c = c / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        let xyz = mat3_apply(SRGB_TO_XYZ_D65, [lin(200.0), lin(80.0), lin(40.0)]);
        let lab = mat3_apply(M2, mat3_apply(M1, xyz).map(f32::cbrt));
        let rgb = srgb8_from_oklch(
            lab[0],
            lab[1].hypot(lab[2]),
            lab[2].atan2(lab[1]).to_degrees(),
        );
        assert!(
            rgb.iter()
                .zip([200u8, 80, 40])
                .all(|(a, b)| a.abs_diff(b) <= 1),
            "{rgb:?}"
        );
    }

    #[test]
    fn wheel_hue_maps_to_the_matching_oklab_direction() {
        let red = wheel_direction(0.0);
        let blue = wheel_direction(240.0);
        assert!(red[0] > 0.5, "red leans +a: {red:?}");
        assert!(blue[1] < -0.5, "blue leans -b: {blue:?}");
    }
}
