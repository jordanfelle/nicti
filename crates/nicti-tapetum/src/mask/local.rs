//! Per-mask *pointwise* local adjustments (#49): the maths the fused live shader runs for each
//! pixel inside a mask, and its CPU twin.
//!
//! **Model.** Local corrections stack *additively* on the global values, exactly how LRC stacks
//! them: at a pixel, every slider's effective value is `global + sum_i(weight_i * amount_i *
//! delta_i)`. So the shader loops the active corrections once, accumulates seven sums and a tint
//! multiplier, and then runs the normal pipeline with the adjusted values -- a slider drag only
//! rewrites a uniform (zero recomposes, zero bakes).
//!
//! Where each one lands in the live pipeline (ADR-0038 order, locals marked *):
//! `camera->working matrix -> [DCP HueSatMap] -> exposure* -> [DCP LookTable] -> temp/tint* ->
//! tone (contrast/highlights/shadows/whites/blacks)* -> tone curve -> vibrance -> HSL ->
//! saturation/hue/colour overlay*`.
//!
//! The spatial adjustments need neighbouring pixels, so their *inputs* are precomputed and cached
//! per photo (`bases`): clarity/texture read two detail bands of the baked luminance, dehaze reads a
//! transmission map and an airlight colour. The shader only *applies* them, per pixel, scaled by
//! the same stacked mask weights -- a slider drag is still uniform-only. Sharpness and noise
//! reduction are per-pixel deltas into `detail_combine`.

use super::params::{LocalCorrection, MaskParams, TintColor};
use crate::coat::{HslParams, PresenceParams, ToneParams};
use crate::color::{self, Mat3};

/// `temp` of +1 scales red by `2^TEMP_STOPS` and blue by `2^-TEMP_STOPS` (warmer).
pub const TEMP_STOPS: f32 = 0.5;
/// `tint` of +1 scales green by `2^-TINT_STOPS` (towards magenta).
pub const TINT_STOPS: f32 = 0.25;
/// `hue` of +1 rotates colour around the grey axis by this many degrees (the HSL panel's scale).
pub const HUE_DEGREES: f32 = 30.0;
/// Strength of the clarity (mid-scale) band at a slider value of 1, in perceptual-luma units.
pub const CLARITY_GAIN: f32 = 1.5;
/// Strength of the texture (fine-scale) band at a slider value of 1.
pub const TEXTURE_GAIN: f32 = 1.5;
/// The luminance ratio a clarity/texture edit may apply is clamped to this range.
pub const RATIO_RANGE: (f32, f32) = (0.25, 4.0);
/// Fraction of the estimated haze the dark-channel prior removes at dehaze = 1 (He et al.'s omega).
pub const HAZE_OMEGA: f32 = 0.95;
/// Transmission floor, so dividing by it can't blow up a dense-haze pixel.
pub const HAZE_T0: f32 = 0.1;
/// A negative dehaze adds a *uniform* veil of up to this much, independent of the estimated haze.
pub const VEIL_MAX: f32 = 0.5;
/// A noise-reduction delta of 1 adds this much to the NR luminance amount (0..=1).
pub const NOISE_LOCAL_GAIN: f32 = 1.0;
/// A sharpness delta of 1 adds this much to the sharpen amount (0..=1.5). A stacked amount below
/// zero softens toward the sharpen-radius blur instead of sharpening.
pub const SHARPNESS_LOCAL_GAIN: f32 = 1.0;
/// Effective contrast is clamped to this range after stacking.
pub const CONTRAST_RANGE: (f32, f32) = (-1.0, 2.0);
/// Every other effective tone slider is clamped to `-TONE_LIMIT..=TONE_LIMIT` after stacking.
pub const TONE_LIMIT: f32 = 2.0;

const LUMA: [f32; 3] = [0.2126, 0.7152, 0.0722];

/// One correction as the shader reads it: five `vec4`s.
///
/// | field | x | y | z | w |
/// |---|---|---|---|---|
/// | `d0` | amount | exposure (stops) | contrast | highlights |
/// | `d1` | shadows | whites | blacks | temp |
/// | `d2` | tint | saturation | hue | noise |
/// | `d3` | tint multiplier r | g | b | sharpness |
/// | `d4` | clarity | texture | dehaze | 0 |
#[derive(Debug, Clone, Copy, PartialEq, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct LocalUniform {
    pub d0: [f32; 4],
    pub d1: [f32; 4],
    pub d2: [f32; 4],
    pub d3: [f32; 4],
    pub d4: [f32; 4],
}

/// HSV -> RGB with `s = 1, v = 1`, hue in degrees: the pure colour of a tint swatch.
fn swatch(hue_deg: f32) -> [f32; 3] {
    let h = (hue_deg.rem_euclid(360.0)) / 60.0;
    let x = 1.0 - ((h % 2.0) - 1.0).abs();
    match h as i32 {
        0 => [1.0, x, 0.0],
        1 => [x, 1.0, 0.0],
        2 => [0.0, 1.0, x],
        3 => [0.0, x, 1.0],
        4 => [x, 0.0, 1.0],
        _ => [1.0, 0.0, x],
    }
}

/// The per-channel multiplier offset a colour overlay applies: a luma-preserving tint, so the
/// overlay changes colour without changing brightness. `(tint / luma(tint) - 1) * saturation`.
fn tint_multiplier(c: &TintColor) -> [f32; 3] {
    let s = swatch(c.hue_deg);
    let luma = LUMA[0] * s[0] + LUMA[1] * s[1] + LUMA[2] * s[2];
    s.map(|v| (v / luma.max(1e-4) - 1.0) * c.saturation)
}

impl LocalUniform {
    /// Packs one (already sanitized) correction. `amount` stays separate from the deltas so an
    /// Amount drag is a pure uniform change.
    pub fn pack(c: &LocalCorrection) -> Self {
        let a = &c.adjust;
        let tint = a.color.map(|t| tint_multiplier(&t)).unwrap_or([0.0; 3]);
        Self {
            d0: [c.amount, a.exposure, a.contrast, a.highlights],
            d1: [a.shadows, a.whites, a.blacks, a.temp],
            d2: [a.tint, a.saturation, a.hue, a.noise],
            d3: [tint[0], tint[1], tint[2], a.sharpness],
            d4: [a.clarity, a.texture, a.dehaze, 0.0],
        }
    }
}

/// Uniforms for every *active* correction, in list order (the order the atlas channels use).
pub fn pack_active(params: &MaskParams) -> Vec<LocalUniform> {
    params.active().map(LocalUniform::pack).collect()
}

/// The accumulated local deltas at one pixel.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LocalSums {
    pub exposure: f32,
    pub contrast: f32,
    pub highlights: f32,
    pub shadows: f32,
    pub whites: f32,
    pub blacks: f32,
    pub temp: f32,
    pub tint: f32,
    pub saturation: f32,
    pub hue: f32,
    pub tint_mult: [f32; 3],
    pub noise: f32,
    pub sharpness: f32,
    pub clarity: f32,
    pub texture: f32,
    pub dehaze: f32,
}

impl LocalSums {
    /// Sums `weight_i * amount_i * delta_i` over the corrections; `weights[i]` is correction `i`'s
    /// mask weight at this pixel. Extra weights or uniforms are ignored.
    pub fn accumulate(uniforms: &[LocalUniform], weights: &[f32]) -> Self {
        let mut s = Self::default();
        for (u, &w) in uniforms.iter().zip(weights) {
            let f = w * u.d0[0];
            s.exposure += f * u.d0[1];
            s.contrast += f * u.d0[2];
            s.highlights += f * u.d0[3];
            s.shadows += f * u.d1[0];
            s.whites += f * u.d1[1];
            s.blacks += f * u.d1[2];
            s.temp += f * u.d1[3];
            s.tint += f * u.d2[0];
            s.saturation += f * u.d2[1];
            s.hue += f * u.d2[2];
            for c in 0..3 {
                s.tint_mult[c] += f * u.d3[c];
            }
            s.noise += f * u.d2[3];
            s.sharpness += f * u.d3[3];
            s.clarity += f * u.d4[0];
            s.texture += f * u.d4[1];
            s.dehaze += f * u.d4[2];
        }
        s
    }
}

/// Temperature/tint as per-channel gains in linear working space.
pub fn temp_tint_gains(temp: f32, tint: f32) -> [f32; 3] {
    [
        (TEMP_STOPS * temp).exp2(),
        (-TINT_STOPS * tint).exp2(),
        (-TEMP_STOPS * temp).exp2(),
    ]
}

/// Rotates `rgb` around the grey axis by `degrees` (Rodrigues' formula about `(1,1,1)/sqrt(3)`).
pub fn rotate_hue(rgb: [f32; 3], degrees: f32) -> [f32; 3] {
    let (s, c) = degrees.to_radians().sin_cos();
    let k = 1.0 / 3.0f32.sqrt();
    let dot = (rgb[0] + rgb[1] + rgb[2]) * k;
    let cross = [
        k * (rgb[2] - rgb[1]),
        k * (rgb[0] - rgb[2]),
        k * (rgb[1] - rgb[0]),
    ];
    std::array::from_fn(|i| c * rgb[i] + s * cross[i] + (1.0 - c) * dot * k)
}

/// Saturation as a luma-preserving chroma scale; the factor floors at 0 (fully grey).
pub fn saturate(rgb: [f32; 3], amount: f32) -> [f32; 3] {
    let luma = LUMA[0] * rgb[0] + LUMA[1] * rgb[1] + LUMA[2] * rgb[2];
    let k = (1.0 + amount).max(0.0);
    rgb.map(|c| luma + (c - luma) * k)
}

/// What the cached bases hold for one pixel (see `bases`): the two detail bands of the baked
/// perceptual luminance `g`, the dehaze transmission there, and the frame's airlight colour.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpatialPixel {
    /// Fine-scale band: `g - base_fine`.
    pub d_tex: f32,
    /// Mid-scale band: `base_fine - base_coarse`.
    pub d_clar: f32,
    /// Perceptual luminance of the *baked* pixel.
    pub g: f32,
    /// Estimated transmission, 0..=1 (1 = no haze).
    pub transmission: f32,
    /// Airlight, in the baked (camera-linear) colour space.
    pub airlight_cam: [f32; 3],
}

impl Default for SpatialPixel {
    fn default() -> Self {
        Self {
            d_tex: 0.0,
            d_clar: 0.0,
            g: 0.5,
            transmission: 1.0,
            airlight_cam: [0.0; 3],
        }
    }
}

/// Dehaze in scene-linear working space. Positive `amount` divides the airlight out along the
/// estimated transmission (`J = (I - A) / max(t', t0) + A`, `t' = 1 - amount * (1 - t)`); negative
/// adds a uniform veil toward the airlight (`J = I * v + A * (1 - v)`).
pub fn apply_dehaze(rgb: [f32; 3], transmission: f32, airlight: [f32; 3], amount: f32) -> [f32; 3] {
    if amount == 0.0 {
        return rgb;
    }
    if amount > 0.0 {
        let t = (1.0 - amount * (1.0 - transmission)).max(HAZE_T0);
        std::array::from_fn(|i| (rgb[i] - airlight[i]) / t + airlight[i])
    } else {
        let v = 1.0 - VEIL_MAX * (-amount).min(1.0);
        std::array::from_fn(|i| rgb[i] * v + airlight[i] * (1.0 - v))
    }
}

/// Clarity and texture: shifts the baked perceptual luma `g` by the mid/fine detail bands and
/// applies the resulting brightness *ratio* to `rgb` (so chroma is preserved). Clarity is weighted
/// toward midtones by the current pixel's own brightness; texture is not. The ratio is clamped so a
/// hostile band value can't blow a pixel out.
pub fn apply_bands(rgb: [f32; 3], sp: &SpatialPixel, clarity: f32, texture: f32) -> [f32; 3] {
    if clarity == 0.0 && texture == 0.0 {
        return rgb;
    }
    let luma = LUMA[0] * rgb[0] + LUMA[1] * rgb[1] + LUMA[2] * rgb[2];
    let g_now = luma.max(0.0).cbrt().clamp(0.0, 1.0);
    let mid = 4.0 * g_now * (1.0 - g_now);
    let g_base = sp.g.max(1e-3);
    let g_new =
        (g_base + clarity * CLARITY_GAIN * mid * sp.d_clar + texture * TEXTURE_GAIN * sp.d_tex)
            .max(0.0);
    let ratio = (g_new / g_base)
        .powf(2.2)
        .clamp(RATIO_RANGE.0, RATIO_RANGE.1);
    rgb.map(|c| c * ratio)
}

/// Luma after noise reduction and sharpening with the local deltas stacked on the global Detail
/// values -- the CPU twin of `detail_combine.wgsl`'s local path. Local noise adds to the NR
/// luminance amount; local sharpness adds to the sharpen amount, and a stacked amount below zero
/// softens toward `blurred_sharpen` instead.
pub fn local_detail_luma(
    original: f32,
    blurred_nr: f32,
    blurred_sharpen: f32,
    edge_weight: f32,
    nr: &crate::coat::NoiseReductionParams,
    sharpen: &crate::coat::SharpenParams,
    s: &LocalSums,
) -> f32 {
    let mut nr = *nr;
    nr.luminance = (nr.luminance + s.noise * NOISE_LOCAL_GAIN).clamp(0.0, 1.0);
    let effective = sharpen.amount + s.sharpness * SHARPNESS_LOCAL_GAIN;
    let mut sharpen = *sharpen;
    sharpen.amount = effective.max(0.0);
    let soften = (-effective).clamp(0.0, 1.0);
    let luma = crate::detail::apply_detail(
        original,
        blurred_nr,
        blurred_sharpen,
        edge_weight,
        &nr,
        &sharpen,
    );
    luma + (blurred_sharpen - luma) * soften
}

/// The global (non-local) inputs to one pixel, mirroring `live_suffix.wgsl`'s uniforms.
pub struct PixelParams<'a> {
    pub matrix: Mat3,
    /// Baseline-exposure x the user's global exposure multiplier.
    pub exposure_mult: f32,
    pub tone: ToneParams,
    pub lut: &'a [f32; 256],
    pub vibrance: f32,
    /// Global Presence (#380): summed with the local clarity/texture/dehaze/saturation deltas.
    pub presence: PresenceParams,
    pub hsl: &'a HslParams,
}

/// The tone values after stacking the local deltas, clamped to sane ranges.
pub fn effective_tone(global: &ToneParams, s: &LocalSums) -> ToneParams {
    let clamp = |v: f32| v.clamp(-TONE_LIMIT, TONE_LIMIT);
    ToneParams {
        contrast: (global.contrast + s.contrast).clamp(CONTRAST_RANGE.0, CONTRAST_RANGE.1),
        highlights: clamp(global.highlights + s.highlights),
        shadows: clamp(global.shadows + s.shadows),
        whites: clamp(global.whites + s.whites),
        blacks: clamp(global.blacks + s.blacks),
    }
}

/// The whole per-pixel live pipeline with local deltas applied -- what `live_suffix.wgsl` computes
/// for a pixel whose stacked local sums are `s`. (No DCP profile: the CPU twin covers the path the
/// masks add, and the profile path has its own parity test.)
pub fn live_pixel(
    cam_rgb: [f32; 3],
    p: &PixelParams,
    s: &LocalSums,
    spatial: Option<&SpatialPixel>,
) -> [f32; 3] {
    let mut rgb = color::mat3_apply(p.matrix, cam_rgb);
    let exposure = p.exposure_mult * s.exposure.exp2();
    rgb = rgb.map(|c| c * exposure);
    if let Some(sp) = spatial {
        // The airlight goes through the same matrix and exposure as the pixels it is subtracted from.
        let a = color::mat3_apply(p.matrix, sp.airlight_cam).map(|c| c * exposure);
        rgb = apply_dehaze(rgb, sp.transmission, a, s.dehaze + p.presence.dehaze);
    }
    let gains = temp_tint_gains(s.temp, s.tint);
    rgb = std::array::from_fn(|i| rgb[i] * gains[i]);
    rgb = color::apply_tone(rgb, &effective_tone(&p.tone, s));
    rgb = color::apply_tone_curve(rgb, p.lut);
    if let Some(sp) = spatial {
        rgb = apply_bands(
            rgb,
            sp,
            s.clarity + p.presence.clarity,
            s.texture + p.presence.texture,
        );
    }
    rgb = color::apply_vibrance(rgb, p.vibrance);
    rgb = color::apply_hsl(rgb, p.hsl);
    // Each is skipped when its stacked delta is exactly zero (mirroring the shader), so a mask that
    // selects nothing leaves the pixel bit-identical.
    let saturation = s.saturation + p.presence.saturation;
    if saturation != 0.0 {
        rgb = saturate(rgb, saturation);
    }
    if s.hue != 0.0 {
        rgb = rotate_hue(rgb, s.hue * HUE_DEGREES);
    }
    if s.tint_mult != [0.0; 3] {
        rgb = std::array::from_fn(|i| rgb[i] * (1.0 + s.tint_mult[i]).max(0.0));
    }
    rgb
}

#[cfg(test)]
mod tests {
    use super::super::params::{LocalAdjust, MaskComponent, MaskGroup, MaskSource};
    use super::*;
    use crate::coat::ToneCurveParams;

    fn correction(adjust: LocalAdjust, amount: f32) -> LocalCorrection {
        LocalCorrection {
            id: "c".into(),
            amount,
            mask: MaskGroup {
                components: vec![MaskComponent {
                    source: MaskSource::Brush {
                        strokes: Vec::new(),
                    },
                    ..MaskComponent::default()
                }],
            },
            adjust,
            ..LocalCorrection::default()
        }
    }

    fn params<'a>(lut: &'a [f32; 256], hsl: &'a HslParams) -> PixelParams<'a> {
        PixelParams {
            matrix: color::mat3_identity(),
            exposure_mult: 1.0,
            tone: ToneParams::default(),
            lut,
            vibrance: 0.0,
            presence: PresenceParams::default(),
            hsl,
        }
    }

    fn identity_lut() -> [f32; 256] {
        color::build_tone_curve_lut(&ToneCurveParams::default())
    }

    fn close(a: [f32; 3], b: [f32; 3], tol: f32) -> bool {
        (0..3).all(|i| (a[i] - b[i]).abs() < tol)
    }

    #[test]
    fn a_zero_weight_changes_nothing_exactly() {
        let (lut, hsl) = (identity_lut(), HslParams::default());
        let p = params(&lut, &hsl);
        let u = pack_active(&MaskParams {
            corrections: vec![correction(
                LocalAdjust {
                    exposure: 2.0,
                    contrast: 0.5,
                    saturation: 0.7,
                    ..LocalAdjust::default()
                },
                1.0,
            )],
        });
        let with = live_pixel(
            [0.2, 0.3, 0.1],
            &p,
            &LocalSums::accumulate(&u, &[0.0]),
            None,
        );
        let without = live_pixel([0.2, 0.3, 0.1], &p, &LocalSums::default(), None);
        assert_eq!(with, without);
    }

    #[test]
    fn local_exposure_of_one_stop_doubles_linear_values_in_the_mask() {
        let (lut, hsl) = (identity_lut(), HslParams::default());
        let mut p = params(&lut, &hsl);
        // Bypass the tone stack so the exposure multiplier is directly observable: a neutral tone
        // curve and zero tone still cube-root/cube, which is the identity for non-negative input.
        p.tone = ToneParams::default();
        let u = pack_active(&MaskParams {
            corrections: vec![correction(
                LocalAdjust {
                    exposure: 1.0,
                    ..LocalAdjust::default()
                },
                1.0,
            )],
        });
        let base = live_pixel([0.1, 0.05, 0.02], &p, &LocalSums::default(), None);
        let inside = live_pixel(
            [0.1, 0.05, 0.02],
            &p,
            &LocalSums::accumulate(&u, &[1.0]),
            None,
        );
        let outside = live_pixel(
            [0.1, 0.05, 0.02],
            &p,
            &LocalSums::accumulate(&u, &[0.0]),
            None,
        );
        assert!(close(outside, base, 1e-7));
        for c in 0..3 {
            assert!(
                (inside[c] / base[c] - 2.0).abs() < 0.01,
                "channel {c}: {} vs {}",
                inside[c],
                base[c]
            );
        }
    }

    #[test]
    fn a_half_weight_is_half_the_exposure_in_stops_not_half_the_light() {
        let (lut, hsl) = (identity_lut(), HslParams::default());
        let p = params(&lut, &hsl);
        let u = pack_active(&MaskParams {
            corrections: vec![correction(
                LocalAdjust {
                    exposure: 2.0,
                    ..LocalAdjust::default()
                },
                1.0,
            )],
        });
        let base = live_pixel([0.1; 3], &p, &LocalSums::default(), None)[0];
        let half = live_pixel([0.1; 3], &p, &LocalSums::accumulate(&u, &[0.5]), None)[0];
        assert!(
            (half / base - 2.0).abs() < 0.01,
            "+1 stop at half weight of +2"
        );
    }

    #[test]
    fn amount_scales_the_delta_linearly() {
        let adjust = LocalAdjust {
            exposure: 1.0,
            ..LocalAdjust::default()
        };
        let full = pack_active(&MaskParams {
            corrections: vec![correction(adjust, 1.0)],
        });
        let half = pack_active(&MaskParams {
            corrections: vec![correction(adjust, 0.5)],
        });
        assert_eq!(LocalSums::accumulate(&full, &[1.0]).exposure, 1.0);
        assert_eq!(LocalSums::accumulate(&half, &[1.0]).exposure, 0.5);
    }

    #[test]
    fn stacked_corrections_add_and_a_correction_and_its_inverse_weights_sum_to_one_application() {
        let adjust = LocalAdjust {
            exposure: 1.0,
            contrast: 0.2,
            ..LocalAdjust::default()
        };
        let u = pack_active(&MaskParams {
            corrections: vec![correction(adjust, 1.0), correction(adjust, 1.0)],
        });
        // Two full-weight corrections stack to double the delta.
        let s = LocalSums::accumulate(&u, &[1.0, 1.0]);
        assert_eq!(s.exposure, 2.0);
        assert!((s.contrast - 0.4).abs() < 1e-6);
        // Subject (w) and background (1 - w) carrying the SAME adjustment sum to exactly one
        // global application, at every weight -- the partition property.
        for w in [0.0f32, 0.25, 0.5, 1.0] {
            let s = LocalSums::accumulate(&u, &[w, 1.0 - w]);
            assert!((s.exposure - 1.0).abs() < 1e-6, "w={w}");
            assert!((s.contrast - 0.2).abs() < 1e-6, "w={w}");
        }
    }

    #[test]
    fn effective_tone_is_clamped_after_stacking() {
        let s = LocalSums {
            contrast: -5.0,
            highlights: 9.0,
            blacks: -9.0,
            ..LocalSums::default()
        };
        let t = effective_tone(&ToneParams::default(), &s);
        assert_eq!(t.contrast, CONTRAST_RANGE.0);
        assert_eq!(t.highlights, TONE_LIMIT);
        assert_eq!(t.blacks, -TONE_LIMIT);
    }

    #[test]
    fn temp_warms_and_tint_shifts_green_and_zero_is_identity() {
        assert_eq!(temp_tint_gains(0.0, 0.0), [1.0, 1.0, 1.0]);
        let warm = temp_tint_gains(1.0, 0.0);
        assert!(warm[0] > 1.0 && warm[2] < 1.0 && warm[1] == 1.0);
        let magenta = temp_tint_gains(0.0, 1.0);
        assert!(magenta[1] < 1.0 && magenta[0] == 1.0 && magenta[2] == 1.0);
        // Opposite temps are exact reciprocals on each channel.
        let cool = temp_tint_gains(-1.0, 0.0);
        assert!((warm[0] * cool[0] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn saturation_zero_is_identity_and_minus_one_is_grey_and_preserves_luma() {
        let c = [0.4, 0.2, 0.1];
        assert!(close(saturate(c, 0.0), c, 1e-7));
        let grey = saturate(c, -1.0);
        assert!((grey[0] - grey[1]).abs() < 1e-6 && (grey[1] - grey[2]).abs() < 1e-6);
        let luma = |v: [f32; 3]| LUMA[0] * v[0] + LUMA[1] * v[1] + LUMA[2] * v[2];
        assert!((luma(saturate(c, 0.8)) - luma(c)).abs() < 1e-6);
        // Over-desaturating (< -1) floors at grey rather than inverting the colour.
        assert!(close(saturate(c, -3.0), grey, 1e-6));
    }

    #[test]
    fn hue_rotation_keeps_grey_and_is_periodic_and_preserves_the_grey_axis_component() {
        assert!(close(rotate_hue([0.3; 3], 77.0), [0.3; 3], 1e-6));
        let c = [0.6, 0.3, 0.1];
        assert!(close(rotate_hue(c, 0.0), c, 1e-6));
        assert!(close(rotate_hue(c, 360.0), c, 1e-5));
        let mean = |v: [f32; 3]| (v[0] + v[1] + v[2]) / 3.0;
        assert!((mean(rotate_hue(c, 40.0)) - mean(c)).abs() < 1e-6);
        // 120 degrees cycles the channels: r -> g -> b.
        assert!(close(
            rotate_hue([1.0, 0.0, 0.0], 120.0),
            [0.0, 1.0, 0.0],
            1e-5
        ));
    }

    #[test]
    fn a_tint_overlay_changes_colour_but_not_brightness() {
        let m = tint_multiplier(&TintColor {
            hue_deg: 210.0,
            saturation: 1.0,
        });
        let grey = [0.3f32; 3];
        let tinted: [f32; 3] = std::array::from_fn(|i| grey[i] * (1.0 + m[i]));
        let luma = |v: [f32; 3]| LUMA[0] * v[0] + LUMA[1] * v[1] + LUMA[2] * v[2];
        assert!((luma(tinted) - luma(grey)).abs() < 1e-4);
        assert!(
            tinted[2] > tinted[0],
            "a blue-ish tint pushes blue above red"
        );
        let none = tint_multiplier(&TintColor {
            hue_deg: 210.0,
            saturation: 0.0,
        });
        assert_eq!(none, [0.0; 3]);
    }

    #[test]
    fn pack_orders_the_fields_the_shader_reads() {
        let c = correction(
            LocalAdjust {
                exposure: 0.1,
                contrast: 0.2,
                highlights: 0.3,
                shadows: 0.4,
                whites: 0.5,
                blacks: 0.6,
                temp: 0.7,
                tint: 0.8,
                saturation: 0.9,
                hue: 0.11,
                ..LocalAdjust::default()
            },
            0.75,
        );
        let u = LocalUniform::pack(&c);
        assert_eq!(u.d0, [0.75, 0.1, 0.2, 0.3]);
        assert_eq!(u.d1, [0.4, 0.5, 0.6, 0.7]);
        assert_eq!(u.d2, [0.8, 0.9, 0.11, 0.0]);
        assert_eq!(std::mem::size_of::<LocalUniform>(), 80);
    }

    #[test]
    fn only_active_corrections_are_packed() {
        let mut off = correction(
            LocalAdjust {
                exposure: 1.0,
                ..LocalAdjust::default()
            },
            1.0,
        );
        off.enabled = false;
        let on = correction(
            LocalAdjust {
                exposure: 2.0,
                ..LocalAdjust::default()
            },
            1.0,
        );
        let u = pack_active(&MaskParams {
            corrections: vec![off, on],
        });
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].d0[1], 2.0);
    }
}

#[cfg(test)]
mod gpu_tests {
    use super::super::atlas::{Atlas, Bases, MaskFrame};
    use super::super::kernels::FieldTexture;
    use super::super::params::{LocalAdjust, MaskComponent, MaskGroup, MaskSource};
    use super::super::Field;
    use super::*;
    use crate::coat::{
        ExposureParams, HslParams, NoiseReductionParams, SharpenParams, ToneCurveParams,
        VibranceParams,
    };
    use crate::frame::{Extent, FrameTexture};
    use crate::gpu::GpuContext;
    use crate::renderer::LiveExec;
    use crate::stages::{LiveParams, LiveSuffixKernel};
    use crate::test_util::{shared_test_gpu, upload_frame};
    use std::sync::Arc;

    const W: usize = 24;
    const H: usize = 16;

    fn correction(adjust: LocalAdjust, amount: f32) -> LocalCorrection {
        LocalCorrection {
            id: "c".into(),
            amount,
            mask: MaskGroup {
                components: vec![MaskComponent {
                    source: MaskSource::Brush {
                        strokes: Vec::new(),
                    },
                    ..MaskComponent::default()
                }],
            },
            adjust,
            ..LocalCorrection::default()
        }
    }

    fn frame_pixels() -> Vec<[f32; 4]> {
        (0..W * H)
            .map(|i| {
                let (x, y) = ((i % W) as f32 / W as f32, (i / W) as f32 / H as f32);
                [
                    0.05 + 0.6 * x,
                    0.04 + 0.5 * y,
                    0.03 + 0.4 * (1.0 - x) * (1.0 - y),
                    1.0,
                ]
            })
            .collect()
    }

    fn ramp(flip: bool) -> Field {
        Field {
            width: W,
            height: H,
            data: (0..W * H)
                .map(|i| {
                    let t = (i % W) as f32 / (W - 1) as f32;
                    if flip {
                        1.0 - t
                    } else {
                        t
                    }
                })
                .collect(),
        }
    }

    struct Setup {
        corrections: Vec<LocalCorrection>,
        fields: Vec<Field>,
        matrix: Mat3,
        exposure: f32,
        tone: ToneParams,
        vibrance: f32,
        presence: PresenceParams,
    }

    /// Renders `setup` through the real `LiveSuffixKernel` and returns GPU pixels.
    fn render(gpu: &Arc<GpuContext>, setup: &Setup, with_masks: bool) -> Vec<[f32; 4]> {
        let extent = Extent {
            width: W as u32,
            height: H as u32,
        };
        let input = upload_frame(gpu, extent, &frame_pixels());
        let output = FrameTexture::new(gpu, extent);
        let kernel = LiveSuffixKernel::new(gpu);
        kernel.set_params(
            gpu,
            &LiveParams {
                working_space_matrix: setup.matrix,
                exposure: ExposureParams {
                    stops: setup.exposure,
                },
                tone: setup.tone,
                tone_curve: ToneCurveParams::default(),
                vibrance: VibranceParams {
                    amount: setup.vibrance,
                },
                presence: setup.presence,
                defringe: Default::default(),
                hsl: HslParams::default(),
                sharpen: SharpenParams::default(),
                noise_reduction: NoiseReductionParams::default(),
                camera_profile: None,
                pixel_scale: 1.0,
            },
        );
        if with_masks {
            let params = MaskParams {
                corrections: setup.corrections.clone(),
            };
            let uniforms = pack_active(&params);
            let atlas = Atlas::new(gpu, W as u32, H as u32, uniforms.len());
            let kernels = super::super::kernels::tests::shared_kernels(gpu);
            let textures: Vec<FieldTexture> = setup
                .fields
                .iter()
                .map(|f| FieldTexture::upload(gpu, f))
                .collect();
            let mut enc = gpu
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
            for (layer, chunk) in textures.chunks(4).enumerate() {
                let slot = |i: usize| chunk.get(i);
                kernels.pack(
                    gpu,
                    &mut enc,
                    [slot(0), slot(1), slot(2), slot(3)],
                    &atlas.layer_views[layer],
                    W as u32,
                    H as u32,
                );
            }
            gpu.queue.submit(Some(enc.finish()));
            kernel.set_masks(
                gpu,
                Some(&MaskFrame {
                    atlas: Arc::new(atlas),
                    uniforms,
                    bases: Bases::default(),
                }),
            );
        }
        let mut enc = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        kernel.encode(gpu, &mut enc, &input, &output);
        gpu.queue.submit(Some(enc.finish()));
        crate::test_util::read_frame(gpu, &output)
    }

    fn cpu(setup: &Setup) -> Vec<[f32; 3]> {
        let lut = color::build_tone_curve_lut(&ToneCurveParams::default());
        let hsl = HslParams::default();
        let p = PixelParams {
            matrix: setup.matrix,
            exposure_mult: color::exposure_multiplier(setup.exposure),
            tone: setup.tone,
            lut: &lut,
            vibrance: setup.vibrance,
            presence: setup.presence,
            hsl: &hsl,
        };
        let uniforms = pack_active(&MaskParams {
            corrections: setup.corrections.clone(),
        });
        frame_pixels()
            .iter()
            .enumerate()
            .map(|(i, px)| {
                let weights: Vec<f32> = setup.fields.iter().map(|f| f.data[i]).collect();
                let sums = LocalSums::accumulate(&uniforms, &weights);
                live_pixel([px[0], px[1], px[2]], &p, &sums, None)
            })
            .collect()
    }

    fn setup(corrections: Vec<LocalCorrection>, fields: Vec<Field>) -> Setup {
        Setup {
            corrections,
            fields,
            matrix: [[1.4, -0.3, -0.1], [-0.2, 1.3, -0.1], [0.0, -0.2, 1.2]],
            exposure: 0.25,
            tone: ToneParams {
                contrast: 0.2,
                highlights: -0.1,
                shadows: 0.15,
                whites: 0.05,
                blacks: -0.05,
            },
            vibrance: 0.1,
            presence: PresenceParams::default(),
        }
    }

    fn assert_matches_cpu(gpu: &Arc<GpuContext>, s: &Setup, what: &str) {
        let got = render(gpu, s, true);
        let want = cpu(s);
        let mut worst = 0.0f32;
        for (g, w) in got.iter().zip(&want) {
            for c in 0..3 {
                worst = worst.max((g[c] - w[c]).abs());
            }
        }
        assert!(worst < 0.02, "{what}: GPU vs CPU differ by {worst}");
    }

    #[test]
    fn the_live_shader_with_every_pointwise_local_matches_the_cpu_twin() {
        let Some(gpu) = shared_test_gpu() else { return };
        let everything = LocalAdjust {
            exposure: 0.8,
            contrast: 0.3,
            highlights: -0.4,
            shadows: 0.5,
            whites: 0.2,
            blacks: -0.3,
            temp: 0.5,
            tint: -0.3,
            saturation: 0.4,
            hue: 0.25,
            color: Some(crate::mask::params::TintColor {
                hue_deg: 200.0,
                saturation: 0.3,
            }),
            ..LocalAdjust::default()
        };
        let s = setup(vec![correction(everything, 0.8)], vec![ramp(false)]);
        assert_matches_cpu(&gpu, &s, "one correction, every slider");
    }

    #[test]
    fn stacked_corrections_across_two_atlas_layers_match_the_cpu_twin() {
        let Some(gpu) = shared_test_gpu() else { return };
        // Six corrections -> two layers (4 + 2), each with a different weight field and delta.
        let deltas = [
            LocalAdjust {
                exposure: 0.6,
                ..LocalAdjust::default()
            },
            LocalAdjust {
                contrast: 0.4,
                saturation: -0.3,
                ..LocalAdjust::default()
            },
            LocalAdjust {
                highlights: -0.5,
                ..LocalAdjust::default()
            },
            LocalAdjust {
                temp: -0.6,
                tint: 0.4,
                ..LocalAdjust::default()
            },
            LocalAdjust {
                shadows: 0.7,
                hue: -0.4,
                ..LocalAdjust::default()
            },
            LocalAdjust {
                exposure: -0.5,
                blacks: 0.2,
                ..LocalAdjust::default()
            },
        ];
        let corrections: Vec<_> = deltas
            .iter()
            .enumerate()
            .map(|(i, d)| correction(*d, 1.0 - i as f32 * 0.1))
            .collect();
        let fields: Vec<_> = (0..6).map(|i| ramp(i % 2 == 1)).collect();
        assert_matches_cpu(&gpu, &setup(corrections, fields), "six corrections");
    }

    #[test]
    fn a_zero_weight_mask_and_no_masks_render_identically_to_the_unmasked_pipeline() {
        let Some(gpu) = shared_test_gpu() else { return };
        let strong = LocalAdjust {
            exposure: 2.0,
            contrast: 0.5,
            saturation: 0.6,
            ..LocalAdjust::default()
        };
        let s = setup(vec![correction(strong, 1.0)], vec![Field::new(W, H, 0.0)]);
        let unmasked = render(&gpu, &s, false);
        let zero_weight = render(&gpu, &s, true);
        for (a, b) in unmasked.iter().zip(&zero_weight) {
            for c in 0..3 {
                assert!((a[c] - b[c]).abs() < 1e-3, "{a:?} vs {b:?}");
            }
        }
    }

    /// The outcome test the parity tests can't give: a real +1 EV radial-shaped mask brightens
    /// inside and leaves outside exactly as the unmasked render, through the real GPU path.
    #[test]
    fn plus_one_ev_inside_a_mask_doubles_light_inside_and_leaves_outside_alone() {
        let Some(gpu) = shared_test_gpu() else { return };
        let mut s = setup(
            vec![correction(
                LocalAdjust {
                    exposure: 1.0,
                    ..LocalAdjust::default()
                },
                1.0,
            )],
            // Left half fully selected, right half not.
            vec![Field {
                width: W,
                height: H,
                data: (0..W * H)
                    .map(|i| if i % W < W / 2 { 1.0 } else { 0.0 })
                    .collect(),
            }],
        );
        // Neutral tone so the exposure change is directly readable as a light ratio.
        s.tone = ToneParams::default();
        s.vibrance = 0.0;
        s.exposure = 0.0;
        s.matrix = color::mat3_identity();
        let base = render(&gpu, &s, false);
        let masked = render(&gpu, &s, true);
        for y in 0..H {
            for x in 0..W {
                let (b, m) = (base[y * W + x], masked[y * W + x]);
                if x < W / 2 - 1 {
                    for c in 0..3 {
                        // The tone-curve stage clamps at white, so a doubled value that would pass
                        // 1.0 is (correctly) clipped -- only assert where it stays in range.
                        if b[c] * 2.0 > 0.95 {
                            continue;
                        }
                        assert!(
                            (m[c] / b[c] - 2.0).abs() < 0.03,
                            "({x},{y}) c{c}: {} vs {}",
                            m[c],
                            b[c]
                        );
                    }
                } else if x > W / 2 {
                    for c in 0..3 {
                        assert!((m[c] - b[c]).abs() < 1e-3, "({x},{y}) outside changed");
                    }
                }
            }
        }
    }
}

/// End-to-end tests of the spatial adjustments through the real engine and live kernel.
#[cfg(test)]
mod spatial_tests {
    use super::super::atlas::MaskFrame;
    use super::super::bases;
    use super::super::engine::{MaskEngine, MaskInputs};
    use super::super::guided;
    use super::super::params::{LocalAdjust, MaskComponent, MaskGroup, MaskSource};
    use super::*;
    use crate::coat::{
        ExposureParams, HslParams, NoiseReductionParams, SharpenParams, ToneCurveParams,
        VibranceParams,
    };
    use crate::frame::{Extent, FrameTexture};
    use crate::gpu::GpuContext;
    use crate::renderer::LiveExec;
    use crate::stages::{LiveParams, LiveSuffixKernel};
    use crate::test_util::{read_frame, shared_test_gpu, upload_frame};
    use std::collections::HashMap;
    use std::sync::Arc;

    const W: usize = 384;
    const H: usize = 256;

    fn luma(p: &[f32; 4]) -> f32 {
        LUMA[0] * p[0] + LUMA[1] * p[1] + LUMA[2] * p[2]
    }

    /// A correction whose mask covers the whole frame.
    fn everywhere(adjust: LocalAdjust) -> LocalCorrection {
        LocalCorrection {
            id: "all".into(),
            mask: MaskGroup {
                components: vec![MaskComponent {
                    source: MaskSource::RadialGradient {
                        center: [0.5, 0.5],
                        radii: [4.0, 4.0],
                        angle_deg: 0.0,
                        feather: 0.5,
                    },
                    ..MaskComponent::default()
                }],
            },
            adjust,
            ..LocalCorrection::default()
        }
    }

    /// Renders `frame` through the neutral live pipeline with `corrections` (the frame itself is
    /// the guide, as it is in the real graph). Returns the pixels and the mask frame used.
    fn render(
        gpu: &Arc<GpuContext>,
        frame: &[[f32; 4]],
        corrections: Vec<LocalCorrection>,
    ) -> (Vec<[f32; 4]>, Option<MaskFrame>) {
        render_with(
            gpu,
            frame,
            corrections,
            SharpenParams::default(),
            NoiseReductionParams::default(),
        )
    }

    /// As [`render`], with explicit *global* Detail-panel values.
    fn render_with(
        gpu: &Arc<GpuContext>,
        frame: &[[f32; 4]],
        corrections: Vec<LocalCorrection>,
        sharpen: SharpenParams,
        noise_reduction: NoiseReductionParams,
    ) -> (Vec<[f32; 4]>, Option<MaskFrame>) {
        render_full(
            gpu,
            frame,
            corrections,
            sharpen,
            noise_reduction,
            PresenceParams::default(),
        )
    }

    /// As [`render`], with explicit *global* Presence values (#380).
    fn render_presence(
        gpu: &Arc<GpuContext>,
        frame: &[[f32; 4]],
        corrections: Vec<LocalCorrection>,
        presence: PresenceParams,
    ) -> (Vec<[f32; 4]>, Option<MaskFrame>) {
        render_full(
            gpu,
            frame,
            corrections,
            SharpenParams::default(),
            NoiseReductionParams::default(),
            presence,
        )
    }

    fn render_full(
        gpu: &Arc<GpuContext>,
        frame: &[[f32; 4]],
        corrections: Vec<LocalCorrection>,
        sharpen: SharpenParams,
        noise_reduction: NoiseReductionParams,
        presence: PresenceParams,
    ) -> (Vec<[f32; 4]>, Option<MaskFrame>) {
        let extent = Extent {
            width: W as u32,
            height: H as u32,
        };
        let input = upload_frame(gpu, extent, frame);
        let output = FrameTexture::new(gpu, extent);
        let kernel = LiveSuffixKernel::new(gpu);
        kernel.set_params(
            gpu,
            &LiveParams {
                working_space_matrix: color::mat3_identity(),
                exposure: ExposureParams::default(),
                tone: ToneParams::default(),
                tone_curve: ToneCurveParams::default(),
                vibrance: VibranceParams::default(),
                presence,
                defringe: Default::default(),
                hsl: HslParams::default(),
                sharpen,
                noise_reduction,
                camera_profile: None,
                pixel_scale: 1.0,
            },
        );
        let params = MaskParams { corrections };
        let mut engine = MaskEngine::with_kernels(
            super::super::kernels::tests::shared_kernels(gpu),
            super::super::guided::tests::shared_kernels(gpu),
            super::super::bases::tests::shared_kernels(gpu),
        );
        let alphas = HashMap::new();
        let mask_frame = engine.prepare(
            gpu,
            &MaskInputs {
                params: &params,
                ai_alphas: &alphas,
                neutral_key: blake3::hash(b"n"),
                guide: &input,
                guide_key: blake3::hash(b"g"),
                range_matrix: color::mat3_identity(),
                presence,
            },
        );
        kernel.set_masks(gpu, mask_frame.as_ref());
        let mut enc = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        kernel.encode(gpu, &mut enc, &input, &output);
        gpu.queue.submit(Some(enc.finish()));
        (read_frame(gpu, &output), mask_frame)
    }

    /// Standard deviation of the local high-pass (pixel minus its `r`-box mean) over a region.
    fn highpass_std(px: &[[f32; 4]], r: usize, x0: usize, x1: usize, y0: usize, y1: usize) -> f32 {
        let l: Vec<f32> = px.iter().map(luma).collect();
        let mut acc = 0.0f32;
        let mut n = 0.0f32;
        for y in y0..y1 {
            for x in x0..x1 {
                let mut sum = 0.0;
                let mut c = 0.0;
                for yy in y.saturating_sub(r)..=(y + r).min(H - 1) {
                    for xx in x.saturating_sub(r)..=(x + r).min(W - 1) {
                        sum += l[yy * W + xx];
                        c += 1.0;
                    }
                }
                let d = l[y * W + x] - sum / c;
                acc += d * d;
                n += 1.0;
            }
        }
        (acc / n).sqrt()
    }

    fn mean_luma(px: &[[f32; 4]]) -> f32 {
        px.iter().map(luma).sum::<f32>() / px.len() as f32
    }

    /// Mid-scale blobs (wavelength ~24 px) on a broad gradient, with a hard step edge down the
    /// right-hand side.
    fn blob_scene() -> Vec<[f32; 4]> {
        (0..W * H)
            .map(|i| {
                let (x, y) = ((i % W) as f32, (i / W) as f32);
                let base = 0.10 + 0.08 * x / W as f32;
                let blobs = 0.020 * (x * 0.26).sin() * (y * 0.26).cos();
                let step = if x > 300.0 { 0.16 } else { 0.0 };
                let v = base + blobs + step;
                [v, v * 0.9, v * 0.8, 1.0]
            })
            .collect()
    }

    #[test]
    fn clarity_adds_local_contrast_keeps_the_mean_and_does_not_halo_a_hard_edge() {
        let Some(gpu) = shared_test_gpu() else { return };
        let scene = blob_scene();
        let (base, none) = render(&gpu, &scene, vec![]);
        assert!(none.is_none());
        let clarity = |amount: f32| {
            render(
                &gpu,
                &scene,
                vec![everywhere(LocalAdjust {
                    clarity: amount,
                    ..LocalAdjust::default()
                })],
            )
            .0
        };
        let boosted = clarity(1.0);
        let softened = clarity(-1.0);

        // Blob region (well away from the step): local contrast rises / falls.
        let s = |px: &[[f32; 4]]| highpass_std(px, 12, 40, 250, 40, 216);
        let (s0, s_up, s_down) = (s(&base), s(&boosted), s(&softened));
        assert!(
            s_up > s0 * 1.2,
            "clarity +1 must raise local contrast: {s0} -> {s_up}"
        );
        assert!(
            s_down < s0 * 0.9,
            "clarity -1 must lower it: {s0} -> {s_down}"
        );
        // Overall brightness is not shifted.
        assert!(
            (mean_luma(&boosted) / mean_luma(&base) - 1.0).abs() < 0.04,
            "the mean moved: {} -> {}",
            mean_luma(&base),
            mean_luma(&boosted)
        );
    }

    /// Two flat plateaus and a hard step: the cleanest place to look for a halo, because nothing
    /// else in the image is *supposed* to change.
    #[test]
    fn clarity_does_not_halo_a_hard_edge() {
        let Some(gpu) = shared_test_gpu() else { return };
        let scene: Vec<[f32; 4]> = (0..W * H)
            .map(|i| {
                let v = if i % W < 300 { 0.13 } else { 0.29 };
                [v, v, v, 1.0]
            })
            .collect();
        let (base, _) = render(&gpu, &scene, vec![]);
        let (boosted, _) = render(
            &gpu,
            &scene,
            vec![everywhere(LocalAdjust {
                clarity: 1.0,
                ..LocalAdjust::default()
            })],
        );
        // Local contrast legitimately steepens the edge itself (that is what clarity is), but the
        // effect stays local: past ~3x the coarse radius from the step the plateau is untouched,
        // and near it the change is bounded (a Gaussian-based clarity rings visibly for tens of
        // pixels here, and shifts the whole plateau).
        let reach = 3 * bases::coarse_radius(W, H);
        for y in [60usize, 128, 200] {
            for x in (0..W).filter(|&x| (x as i32 - 300).unsigned_abs() as usize > reach) {
                let (b, o) = (luma(&base[y * W + x]), luma(&boosted[y * W + x]));
                assert!(
                    (o / b - 1.0).abs() < 0.02,
                    "halo reaches x={x} (>{reach}px from the edge) at y={y}: {b} -> {o}"
                );
            }
            for x in (300 - reach)..(300 + reach) {
                let (b, o) = (luma(&base[y * W + x]), luma(&boosted[y * W + x]));
                assert!(
                    (o / b - 1.0).abs() < 0.30,
                    "overshoot at ({x},{y}): {b} -> {o}"
                );
            }
        }
    }

    #[test]
    fn texture_boosts_fine_detail_and_leaves_broad_shading_alone() {
        let Some(gpu) = shared_test_gpu() else { return };
        // A smooth ramp plus fine checker speckle.
        let scene: Vec<[f32; 4]> = (0..W * H)
            .map(|i| {
                let (x, y) = (i % W, i / W);
                let v =
                    0.15 + 0.15 * x as f32 / W as f32 + if (x + y) % 2 == 0 { 0.02 } else { -0.02 };
                [v, v, v, 1.0]
            })
            .collect();
        let (base, _) = render(&gpu, &scene, vec![]);
        let (boosted, _) = render(
            &gpu,
            &scene,
            vec![everywhere(LocalAdjust {
                texture: 1.0,
                ..LocalAdjust::default()
            })],
        );
        let fine = |px: &[[f32; 4]]| highpass_std(px, 1, 20, W - 20, 20, H - 20);
        assert!(
            fine(&boosted) > fine(&base) * 1.4,
            "texture +1 must raise fine detail: {} -> {}",
            fine(&base),
            fine(&boosted)
        );
        // Broad shading is untouched: compare the two images after blurring the speckle away.
        let blurred = |px: &[[f32; 4]]| -> Vec<f32> {
            let l: Vec<f32> = px.iter().map(luma).collect();
            let r = 5usize;
            (0..W * H)
                .map(|i| {
                    let (x, y) = (i % W, i / W);
                    let mut sum = 0.0;
                    let mut c = 0.0;
                    for yy in y.saturating_sub(r)..=(y + r).min(H - 1) {
                        for xx in x.saturating_sub(r)..=(x + r).min(W - 1) {
                            sum += l[yy * W + xx];
                            c += 1.0;
                        }
                    }
                    sum / c
                })
                .collect()
        };
        let (b0, b1) = (blurred(&base), blurred(&boosted));
        let drift =
            b0.iter().zip(&b1).map(|(a, b)| (a - b).abs()).sum::<f32>() / b0.iter().sum::<f32>();
        assert!(
            drift < 0.03,
            "broad shading drifted by {:.1} %",
            drift * 100.0
        );
    }

    /// Dark-channel dehaze on `I = J t + A (1 - t)`: with the prior's own assumption satisfied
    /// (dark pixels in every window) the recovered image must be much closer to the haze-free `J`.
    #[test]
    fn dehaze_recovers_a_synthetic_hazy_scene() {
        let Some(gpu) = shared_test_gpu() else { return };
        let a = [0.85f32, 0.86, 0.90];
        let t = 0.55f32;
        // The prior needs something to anchor the airlight on: a hazy sky band across the top,
        // where the pixel *is* the airlight (as in any real hazy landscape).
        let scene_j: Vec<[f32; 4]> = (0..W * H)
            .map(|i| {
                let (x, y) = (i % W, i / W);
                if y < 24 {
                    [a[0], a[1], a[2], 1.0]
                } else if (x * 5 + y * 11) % 8 == 0 {
                    [0.015, 0.015, 0.015, 1.0]
                } else {
                    let f = 0.25 + 0.35 * (x as f32 / W as f32);
                    [f, 0.3 + 0.2 * (y as f32 / H as f32), 0.15 + 0.2 * f, 1.0]
                }
            })
            .collect();
        let hazy: Vec<[f32; 4]> = scene_j
            .iter()
            .map(|p| {
                [
                    p[0] * t + a[0] * (1.0 - t),
                    p[1] * t + a[1] * (1.0 - t),
                    p[2] * t + a[2] * (1.0 - t),
                    1.0,
                ]
            })
            .collect();
        let (truth, _) = render(&gpu, &scene_j, vec![]);
        let (unfixed, _) = render(&gpu, &hazy, vec![]);
        let (fixed, frame) = render(
            &gpu,
            &hazy,
            vec![everywhere(LocalAdjust {
                dehaze: 1.0,
                ..LocalAdjust::default()
            })],
        );
        let est = frame.unwrap().bases.airlight;
        for c in 0..3 {
            assert!(
                (est[c] - a[c]).abs() < 0.1,
                "airlight channel {c}: estimated {} vs {}",
                est[c],
                a[c]
            );
        }
        let error = |px: &[[f32; 4]]| {
            px.iter()
                .zip(&truth)
                .map(|(p, q)| (0..3).map(|c| (p[c] - q[c]).abs()).sum::<f32>() / 3.0)
                .sum::<f32>()
                / px.len() as f32
        };
        let (before, after) = (error(&unfixed), error(&fixed));
        assert!(
            after < before * 0.5,
            "dehaze must recover most of the scene: error {before} -> {after}"
        );
    }

    #[test]
    fn negative_dehaze_adds_a_veil_and_zero_changes_nothing() {
        let Some(gpu) = shared_test_gpu() else { return };
        let scene = blob_scene();
        let (base, _) = render(&gpu, &scene, vec![]);
        let (zero, _) = render(
            &gpu,
            &scene,
            vec![everywhere(LocalAdjust {
                dehaze: 0.0,
                exposure: 0.0001,
                ..LocalAdjust::default()
            })],
        );
        for (a, b) in base.iter().zip(&zero) {
            assert!((luma(a) - luma(b)).abs() < 1e-3);
        }
        let (veiled, _) = render(
            &gpu,
            &scene,
            vec![everywhere(LocalAdjust {
                dehaze: -1.0,
                ..LocalAdjust::default()
            })],
        );
        // A veil toward the (bright) airlight lowers contrast and lifts the shadows.
        assert!(mean_luma(&veiled) > mean_luma(&base));
        assert!(
            highpass_std(&veiled, 12, 40, 250, 40, 216) < highpass_std(&base, 12, 40, 250, 40, 216)
        );
    }

    /// A hazy scene with a speckled texture: every spatial base (bands, transmission, airlight) has
    /// something to chew on.
    fn hazy_textured_scene() -> Vec<[f32; 4]> {
        let a = [0.8f32, 0.82, 0.88];
        blob_scene()
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let dark = (i % W * 3 + i / W * 7).is_multiple_of(10);
                let k = if dark { 0.1 } else { 1.0 };
                [
                    p[0] * k * 0.6 + a[0] * 0.4,
                    p[1] * k * 0.6 + a[1] * 0.4,
                    p[2] * k * 0.6 + a[2] * 0.4,
                    1.0,
                ]
            })
            .collect()
    }

    /// The application step on the GPU must equal the CPU twin given the same bases: this pins the
    /// shader's band/dehaze maths (indexing, sampling, order in the pipeline) independently of the
    /// outcome tests above.
    #[test]
    fn the_gpu_spatial_application_matches_the_cpu_twin() {
        let Some(gpu) = shared_test_gpu() else { return };
        let scene = hazy_textured_scene();
        let adjust = LocalAdjust {
            clarity: 0.7,
            texture: 0.5,
            dehaze: 0.6,
            ..LocalAdjust::default()
        };
        let (got, frame) = render(&gpu, &scene, vec![everywhere(adjust)]);
        let frame = frame.unwrap();

        // The same bases on the CPU, at the (identical) extent.
        let g = guided::guide_from_frame(&scene, W, H, W, H);
        let (fine, mid) = bases::bands_cpu(&g);
        let airlight = frame.bases.airlight;
        let trans = bases::transmission_cpu(&scene, W, H, airlight, &g);
        let lut = color::build_tone_curve_lut(&ToneCurveParams::default());
        let hsl = HslParams::default();
        let p = PixelParams {
            matrix: color::mat3_identity(),
            exposure_mult: 1.0,
            tone: ToneParams::default(),
            lut: &lut,
            vibrance: 0.0,
            presence: PresenceParams::default(),
            hsl: &hsl,
        };
        let uniforms = pack_active(&MaskParams {
            corrections: vec![everywhere(adjust)],
        });
        let mut worst = 0.0f32;
        for (i, px) in scene.iter().enumerate() {
            let sums = LocalSums::accumulate(&uniforms, &[1.0]);
            let sp = SpatialPixel {
                d_tex: fine.data[i],
                d_clar: mid.data[i],
                g: g.data[i],
                transmission: trans.data[i],
                airlight_cam: airlight,
            };
            let want = live_pixel([px[0], px[1], px[2]], &p, &sums, Some(&sp));
            for c in 0..3 {
                worst = worst.max((got[i][c] - want[c]).abs());
            }
        }
        assert!(
            worst < 0.05,
            "GPU vs CPU spatial application differ by {worst}"
        );
    }

    /// #380: a global clarity/texture/dehaze/saturation with *no mask at all* still gets its bases
    /// built and matches the CPU twin.
    #[test]
    fn global_presence_without_any_mask_matches_the_cpu_twin() {
        let Some(gpu) = shared_test_gpu() else { return };
        let scene = hazy_textured_scene();
        let presence = PresenceParams {
            texture: 0.5,
            clarity: 0.7,
            dehaze: 0.6,
            saturation: 0.3,
        };
        let (got, frame) = render_presence(&gpu, &scene, vec![], presence);
        let frame = frame.expect("a global spatial adjustment builds the bases with no mask");
        assert!(frame.uniforms.is_empty());
        assert!(frame.bases.bands.is_some() && frame.bases.haze.is_some());

        let g = guided::guide_from_frame(&scene, W, H, W, H);
        let (fine, mid) = bases::bands_cpu(&g);
        let airlight = frame.bases.airlight;
        let trans = bases::transmission_cpu(&scene, W, H, airlight, &g);
        let lut = color::build_tone_curve_lut(&ToneCurveParams::default());
        let hsl = HslParams::default();
        let p = PixelParams {
            matrix: color::mat3_identity(),
            exposure_mult: 1.0,
            tone: ToneParams::default(),
            lut: &lut,
            vibrance: 0.0,
            presence,
            hsl: &hsl,
        };
        let mut worst = 0.0f32;
        for (i, px) in scene.iter().enumerate() {
            let sp = SpatialPixel {
                d_tex: fine.data[i],
                d_clar: mid.data[i],
                g: g.data[i],
                transmission: trans.data[i],
                airlight_cam: airlight,
            };
            let want = live_pixel([px[0], px[1], px[2]], &p, &LocalSums::default(), Some(&sp));
            for c in 0..3 {
                worst = worst.max((got[i][c] - want[c]).abs());
            }
        }
        assert!(
            worst < 0.05,
            "global presence: GPU vs CPU differ by {worst}"
        );
    }

    /// #380: global and local deltas are *summed*, so +0.3 global with +0.4 local equals +0.7 local.
    #[test]
    fn a_global_presence_delta_sums_with_a_local_one() {
        let Some(gpu) = shared_test_gpu() else { return };
        let scene = hazy_textured_scene();
        let local = |clarity: f32, texture: f32, dehaze: f32, saturation: f32| {
            vec![everywhere(LocalAdjust {
                clarity,
                texture,
                dehaze,
                saturation,
                ..LocalAdjust::default()
            })]
        };
        let (summed, _) = render_presence(
            &gpu,
            &scene,
            local(0.4, 0.2, 0.3, 0.1),
            PresenceParams {
                clarity: 0.3,
                texture: 0.2,
                dehaze: 0.3,
                saturation: 0.2,
            },
        );
        let (local_only, _) = render_presence(
            &gpu,
            &scene,
            local(0.7, 0.4, 0.6, 0.3),
            PresenceParams::default(),
        );
        let worst = summed
            .iter()
            .zip(&local_only)
            .flat_map(|(a, b)| (0..3).map(move |c| (a[c] - b[c]).abs()))
            .fold(0.0f32, f32::max);
        assert!(
            worst < 2e-3,
            "global + local must equal the summed local: {worst}"
        );
    }

    /// #380: a global saturation is per-pixel, so it builds no bases and no mask frame at all.
    #[test]
    fn global_saturation_alone_needs_no_mask_frame() {
        let Some(gpu) = shared_test_gpu() else { return };
        let scene = blob_scene();
        let presence = PresenceParams {
            saturation: -0.6,
            ..Default::default()
        };
        let (got, frame) = render_presence(&gpu, &scene, vec![], presence);
        assert!(frame.is_none());
        let lut = color::build_tone_curve_lut(&ToneCurveParams::default());
        let hsl = HslParams::default();
        let p = PixelParams {
            matrix: color::mat3_identity(),
            exposure_mult: 1.0,
            tone: ToneParams::default(),
            lut: &lut,
            vibrance: 0.0,
            presence,
            hsl: &hsl,
        };
        for (g, px) in got.iter().zip(&scene) {
            let want = live_pixel([px[0], px[1], px[2]], &p, &LocalSums::default(), None);
            assert!(
                (0..3).all(|c| (g[c] - want[c]).abs() < 0.02),
                "{g:?} vs {want:?}"
            );
        }
        // And it really desaturates (not a vacuous pass).
        let (base, _) = render(&gpu, &scene, vec![]);
        let chroma = |px: &[[f32; 4]]| -> f32 {
            px.iter()
                .map(|p| p[0].max(p[1]).max(p[2]) - p[0].min(p[1]).min(p[2]))
                .sum::<f32>()
        };
        assert!(chroma(&got) < chroma(&base) * 0.6);
    }

    #[test]
    fn bases_are_built_only_when_a_correction_needs_them() {
        let Some(gpu) = shared_test_gpu() else { return };
        let scene = blob_scene();
        let (_, only_exposure) = render(
            &gpu,
            &scene,
            vec![everywhere(LocalAdjust {
                exposure: 1.0,
                ..LocalAdjust::default()
            })],
        );
        let f = only_exposure.unwrap();
        assert!(f.bases.bands.is_none() && f.bases.haze.is_none());
        let (_, clarity) = render(
            &gpu,
            &scene,
            vec![everywhere(LocalAdjust {
                clarity: 0.3,
                ..LocalAdjust::default()
            })],
        );
        let f = clarity.unwrap();
        assert!(f.bases.bands.is_some() && f.bases.haze.is_none());
    }

    /// A soft-edged pattern plus deterministic noise, for the local sharpness / noise tests.
    fn noisy_scene() -> Vec<[f32; 4]> {
        (0..W * H)
            .map(|i| {
                let (x, y) = ((i % W) as f32, (i / W) as f32);
                // Soft vertical bars (edges to sharpen) ...
                let bars = 0.18
                    + 0.10 * (x * 0.09).sin().signum() * (1.0 - (0.5 * (x * 0.09).sin()).abs());
                // ... plus hash noise (something to reduce).
                let h = (i as u32).wrapping_mul(2654435761) ^ (i as u32 >> 7).wrapping_mul(40503);
                let n = ((h % 1000) as f32 / 1000.0 - 0.5) * 0.04;
                let v = (bars + n + 0.0001 * y).max(0.001);
                [v, v, v, 1.0]
            })
            .collect()
    }

    /// A mask over the left-middle third of the frame only.
    fn left_third(adjust: LocalAdjust) -> LocalCorrection {
        LocalCorrection {
            id: "left".into(),
            mask: MaskGroup {
                components: vec![MaskComponent {
                    source: MaskSource::RadialGradient {
                        center: [0.2, 0.5],
                        radii: [0.15, 4.0],
                        angle_deg: 0.0,
                        feather: 0.01,
                    },
                    ..MaskComponent::default()
                }],
            },
            adjust,
            ..LocalCorrection::default()
        }
    }

    const INSIDE: (usize, usize) = (40, 110);
    const OUTSIDE: (usize, usize) = (200, 360);

    #[test]
    fn local_sharpness_sharpens_inside_the_mask_only_even_with_the_global_detail_panel_untouched() {
        let Some(gpu) = shared_test_gpu() else { return };
        let scene = noisy_scene();
        let (base, _) = render(&gpu, &scene, vec![]);
        let (sharp, _) = render(
            &gpu,
            &scene,
            vec![left_third(LocalAdjust {
                sharpness: 1.0,
                ..LocalAdjust::default()
            })],
        );
        let hp =
            |px: &[[f32; 4]], (x0, x1): (usize, usize)| highpass_std(px, 2, x0, x1, 20, H - 20);
        assert!(
            hp(&sharp, INSIDE) > hp(&base, INSIDE) * 1.3,
            "inside: {} -> {}",
            hp(&base, INSIDE),
            hp(&sharp, INSIDE)
        );
        let (a, b) = (hp(&base, OUTSIDE), hp(&sharp, OUTSIDE));
        assert!((b / a - 1.0).abs() < 0.02, "outside changed: {a} -> {b}");
    }

    #[test]
    fn negative_local_sharpness_softens_inside_the_mask() {
        let Some(gpu) = shared_test_gpu() else { return };
        let scene = noisy_scene();
        let (base, _) = render(&gpu, &scene, vec![]);
        let (soft, _) = render(
            &gpu,
            &scene,
            vec![left_third(LocalAdjust {
                sharpness: -1.0,
                ..LocalAdjust::default()
            })],
        );
        let hp =
            |px: &[[f32; 4]], (x0, x1): (usize, usize)| highpass_std(px, 2, x0, x1, 20, H - 20);
        assert!(hp(&soft, INSIDE) < hp(&base, INSIDE) * 0.85);
        assert!((hp(&soft, OUTSIDE) / hp(&base, OUTSIDE) - 1.0).abs() < 0.02);
    }

    #[test]
    fn local_noise_reduction_smooths_noise_inside_the_mask_only() {
        let Some(gpu) = shared_test_gpu() else { return };
        // Pure noise on a flat field: nothing but noise to remove.
        let scene: Vec<[f32; 4]> = (0..W * H)
            .map(|i| {
                let h = (i as u32).wrapping_mul(2654435761) ^ (i as u32 >> 5).wrapping_mul(40503);
                let v = 0.2 + ((h % 1000) as f32 / 1000.0 - 0.5) * 0.05;
                [v, v, v, 1.0]
            })
            .collect();
        let (base, _) = render(&gpu, &scene, vec![]);
        let (denoised, _) = render(
            &gpu,
            &scene,
            vec![left_third(LocalAdjust {
                noise: 1.0,
                ..LocalAdjust::default()
            })],
        );
        let hp =
            |px: &[[f32; 4]], (x0, x1): (usize, usize)| highpass_std(px, 3, x0, x1, 20, H - 20);
        assert!(
            hp(&denoised, INSIDE) < hp(&base, INSIDE) * 0.6,
            "inside: {} -> {}",
            hp(&base, INSIDE),
            hp(&denoised, INSIDE)
        );
        assert!((hp(&denoised, OUTSIDE) / hp(&base, OUTSIDE) - 1.0).abs() < 0.02);
    }

    /// GPU == CPU twin with the global Detail panel active *and* local sharpness/noise on a
    /// gradient mask (so every weight in 0..1 is exercised).
    #[test]
    fn the_gpu_local_detail_matches_the_cpu_twin() {
        use crate::detail::{edge_weight, gaussian_blur, Extent2D, EDGE_SCALE};
        let Some(gpu) = shared_test_gpu() else { return };
        let scene = noisy_scene();
        let sharpen = SharpenParams {
            amount: 0.3,
            radius_px: 1.5,
            detail: 0.25,
        };
        let nr = NoiseReductionParams {
            luminance: 0.2,
            color: 0.1,
            detail: 0.5,
        };
        let correction = LocalCorrection {
            id: "grad".into(),
            amount: 0.9,
            mask: MaskGroup {
                components: vec![MaskComponent {
                    source: MaskSource::LinearGradient {
                        p0: [0.0, 0.5],
                        p1: [1.0, 0.5],
                    },
                    ..MaskComponent::default()
                }],
            },
            adjust: LocalAdjust {
                sharpness: 0.8,
                noise: 0.6,
                ..LocalAdjust::default()
            },
            ..LocalCorrection::default()
        };
        let (got, _) = render_with(&gpu, &scene, vec![correction.clone()], sharpen, nr);

        // CPU: the pointwise result (locals add nothing pointwise here), its two blurs, then the
        // per-pixel local combine.
        let extent = Extent2D {
            width: W,
            height: H,
        };
        let planes: [Vec<f32>; 3] = std::array::from_fn(|c| scene.iter().map(|p| p[c]).collect());
        let blur = |sigma: f32| planes.clone().map(|p| gaussian_blur(&p, extent, sigma));
        let (nr_b, sh_b) = (blur(2.0), blur(sharpen.radius_px));
        let weights =
            super::super::raster::rasterize_source(&correction.mask.components[0].source, W, H)
                .unwrap();
        let uniforms = pack_active(&MaskParams {
            corrections: vec![correction],
        });
        let lut = color::build_tone_curve_lut(&ToneCurveParams::default());
        let hsl = HslParams::default();
        let p = PixelParams {
            matrix: color::mat3_identity(),
            exposure_mult: 1.0,
            tone: ToneParams::default(),
            lut: &lut,
            vibrance: 0.0,
            presence: PresenceParams::default(),
            hsl: &hsl,
        };
        let mut worst = 0.0f32;
        for (i, px) in scene.iter().enumerate() {
            let sums = LocalSums::accumulate(&uniforms, &[weights.data[i]]);
            let base = live_pixel([px[0], px[1], px[2]], &p, &sums, None);
            // (Blurs are of the *input*, which equals the pointwise output for this neutral
            // pipeline up to the tone round trip.)
            let orig_l = LUMA[0] * base[0] + LUMA[1] * base[1] + LUMA[2] * base[2];
            let nr_px: [f32; 3] = std::array::from_fn(|c| nr_b[c][i]);
            let sh_px: [f32; 3] = std::array::from_fn(|c| sh_b[c][i]);
            let nr_l = LUMA[0] * nr_px[0] + LUMA[1] * nr_px[1] + LUMA[2] * nr_px[2];
            let sh_l = LUMA[0] * sh_px[0] + LUMA[1] * sh_px[1] + LUMA[2] * sh_px[2];
            let w = edge_weight(orig_l, nr_l, EDGE_SCALE);
            let new_l = local_detail_luma(orig_l, nr_l, sh_l, w, &nr, &sharpen, &sums);
            let color_amount = nr.color.clamp(0.0, 1.0);
            for c in 0..3 {
                let chroma = base[c] - orig_l;
                let chroma_b = nr_px[c] - nr_l;
                let want = new_l + chroma + (chroma_b - chroma) * color_amount;
                worst = worst.max((got[i][c] - want).abs());
            }
        }
        assert!(worst < 0.03, "GPU vs CPU local detail differ by {worst}");
    }
}
