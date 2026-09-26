//! CPU reference pipeline: linear camera RGB (from `retina dump-linear`) -> a display-referred
//! image in a chosen working space, following ADR-0021's stage order.
//!
//! HueSatMap/LookTable application (DNG spec 6.3.7) is defined over the representation each
//! table's own `ProfileHueSatMapEncoding`/`ProfileLookTableEncoding` tag specifies (`Linear` when
//! the tag is absent, per spec -- there is no "gamma 1.8" encoding in the spec itself; an earlier
//! draft of this pipeline assumed one unconditionally, see dcp.rs's `TableEncoding`).

use image::{ImageBuffer, Rgb, RgbImage};

use crate::cct::{solve_camera_to_xyz, CameraToXyz, Illuminant};
use crate::dcp::{DcpProfile, TableEncoding};
use crate::huesatmap::{hsv_to_rgb, rgb_to_hsv, HueSatMap};
use crate::linear_input::{linearize_sample, LinearInput};
use crate::matrix::{mat_vec_mul, Vec3};
use crate::tonecurve::ToneCurve;
use crate::workspace::{srgb_eotf, srgb_oetf, srgb_oetf_unclamped, Space, WorkingSpace};

/// Deliberately unclamped in the `Srgb` case (via `srgb_oetf_unclamped`/`srgb_eotf`, not the
/// clamped `srgb_oetf`): ProPhoto-space channel values routinely exceed 1.0 for saturated colors
/// at this stage (before exposure/tone-curve), and clamping per-channel here would crush
/// highlight detail and skew hue (an R channel clipping while G/B don't changes the R:G:B ratio
/// `rgb_to_hsv` derives hue from).
fn table_encode(encoding: TableEncoding, c: f64) -> f64 {
    match encoding {
        TableEncoding::Linear => c.max(0.0),
        TableEncoding::Srgb => srgb_oetf_unclamped(c.max(0.0)),
    }
}
fn table_decode(encoding: TableEncoding, c: f64) -> f64 {
    match encoding {
        TableEncoding::Linear => c,
        TableEncoding::Srgb => srgb_eotf(c),
    }
}

/// Blends two hue-sat maps' *sampled output* at the same (hue, sat, val) query point by `weight`
/// (matching how `cct.rs` blends the two calibration illuminants' matrices) -- not a blend of the
/// tables' raw grid data, which would need identical dimensions between the two.
fn sample_blended(
    map1: &HueSatMap,
    map2: Option<&HueSatMap>,
    hue: f64,
    sat: f64,
    val: f64,
    weight: f64,
) -> [f64; 3] {
    let a = map1.sample(hue, sat, val);
    let Some(map2) = map2 else { return a };
    let b = map2.sample(hue, sat, val);
    [
        a[0] * weight + b[0] * (1.0 - weight),
        a[1] * weight + b[1] * (1.0 - weight),
        a[2] * weight + b[2] * (1.0 - weight),
    ]
}

/// Per Adobe's own reference implementation (`dng_reference.cpp`'s `RefBaselineHueSatMap`, the
/// DNG SDK's `DNG_RGBtoHSV`/encode/lookup/decode/`DNG_HSVtoRGB` sequence): hue and saturation are
/// computed from **unencoded, linear** RGB and used as-is for the table's hue/sat axes.
/// `ProfileHueSatMapEncoding`/`ProfileLookTableEncoding` only ever apply to the **value**
/// coordinate -- both for the table's value axis at lookup time, and for the scale the lookup
/// returns (`vEncoded = vEncoded * valScale`, decoded back afterward). An earlier version of this
/// function encoded all three R/G/B channels before computing HSV, which also distorts hue/sat
/// (a per-channel nonlinear curve changes the R:G:B ratios those are derived from) -- caught by a
/// CodeRabbit review citing the real DNG SDK source, verified against that source directly before
/// applying this fix.
fn apply_hue_sat(
    rgb_linear_prophoto: Vec3,
    map1: &HueSatMap,
    map2: Option<&HueSatMap>,
    weight: f64,
    encoding: TableEncoding,
) -> Vec3 {
    let hsv = rgb_to_hsv(rgb_linear_prophoto);
    let val_encoded = table_encode(encoding, hsv[2]);
    let adj = sample_blended(map1, map2, hsv[0], hsv[1], val_encoded, weight);
    let new_val_encoded = val_encoded * adj[2];
    let new_hsv = [
        hsv[0] + adj[0],
        (hsv[1] * adj[1]).clamp(0.0, 1.0),
        table_decode(encoding, new_val_encoded),
    ];
    hsv_to_rgb(new_hsv)
}

pub struct RenderOptions<'a> {
    pub profile: &'a DcpProfile,
    pub look: Option<(&'a HueSatMap, TableEncoding)>,
    pub working_space: WorkingSpace,
    pub tone_curve: &'a ToneCurve,
}

/// Renders `input` to an 8-bit sRGB image (for `calico compare`'s side of the ΔE measurement --
/// LRC's own exports are sRGB TIFFs) via the pipeline stage order from ADR-0021: linearize -> WB
/// -> camera->XYZ(D50) -> working space -> HueSatMap -> baseline exposure -> LookTable -> tone
/// curve -> sRGB.
pub fn render(input: &LinearInput, opts: &RenderOptions) -> RgbImage {
    let meta = &input.meta;
    let profile = opts.profile;

    // AsShotNeutral, proportional to 1/cam_mul (a neutral surface reflects each channel in
    // inverse proportion to how much that channel's raw signal needed boosting to look neutral).
    let neutral: Vec3 = [
        1.0 / meta.cam_mul[0] as f64,
        1.0 / meta.cam_mul[1] as f64,
        1.0 / meta.cam_mul[2] as f64,
    ];
    let illum1 = Illuminant {
        cct: profile.illuminant1_cct,
        color_matrix: profile.color_matrix1,
        forward_matrix: profile.forward_matrix1,
    };
    let illum2 = Illuminant {
        cct: profile.illuminant2_cct,
        color_matrix: profile.color_matrix2,
        forward_matrix: profile.forward_matrix2,
    };
    let (cct, camera_to_xyz) = solve_camera_to_xyz(neutral, &illum1, &illum2);
    let hue_sat_weight =
        crate::cct::interpolation_weight(cct, profile.illuminant1_cct, profile.illuminant2_cct);

    let prophoto = Space::get(WorkingSpace::ProPhoto);
    let working = Space::get(opts.working_space);
    let srgb = Space::get(WorkingSpace::Srgb);

    let exposure_scale = 2f64.powf(profile.baseline_exposure_offset);

    let (width, height) = (meta.width, meta.height);
    let mut out: ImageBuffer<Rgb<u8>, Vec<u8>> = ImageBuffer::new(width, height);

    for y in 0..height {
        for x in 0..width {
            let px = input.image.get_pixel(x, y);
            let cam_rgb: Vec3 = [
                linearize_sample(px[0], 0, meta),
                linearize_sample(px[1], 1, meta),
                linearize_sample(px[2], 2, meta),
            ];
            // WB via as-shot multipliers, normalized so green is unity (the conventional DNG
            // AsShotNeutral scaling -- absolute scale doesn't matter here, only channel ratios).
            // Only feed this to the matrix when it's the ForwardMatrix (WhiteBalanced) branch --
            // the ColorMatrix (Raw) branch's Bradford adaptation already corrects the illuminant,
            // so white-balancing *and* applying that matrix would double-correct it.
            let g = meta.cam_mul[1] as f64;
            let wb: Vec3 = [
                cam_rgb[0] * (meta.cam_mul[0] as f64 / g),
                cam_rgb[1],
                cam_rgb[2] * (meta.cam_mul[2] as f64 / g),
            ];

            let xyz_d50 = match &camera_to_xyz {
                CameraToXyz::WhiteBalanced(m) => mat_vec_mul(m, wb),
                CameraToXyz::Raw(m) => mat_vec_mul(m, cam_rgb),
            };

            let prophoto_rgb = mat_vec_mul(&prophoto.from_xyz_d50, xyz_d50);
            let hue_sat_applied = match (&profile.hue_sat_map1, &profile.hue_sat_map2) {
                (Some(map1), map2) => apply_hue_sat(
                    prophoto_rgb,
                    map1,
                    map2.as_ref(),
                    hue_sat_weight,
                    profile.hue_sat_map_encoding,
                ),
                (None, _) => prophoto_rgb,
            };

            let exposed: Vec3 = [
                hue_sat_applied[0] * exposure_scale,
                hue_sat_applied[1] * exposure_scale,
                hue_sat_applied[2] * exposure_scale,
            ];

            let looked = match opts.look {
                Some((look, encoding)) => apply_hue_sat(exposed, look, None, 1.0, encoding),
                None => exposed,
            };

            let working_rgb = mat_vec_mul(
                &working.from_xyz_d50,
                mat_vec_mul(&prophoto.to_xyz_d50, looked),
            );

            let toned: Vec3 = [
                opts.tone_curve.eval(working_rgb[0].clamp(0.0, 1.0)),
                opts.tone_curve.eval(working_rgb[1].clamp(0.0, 1.0)),
                opts.tone_curve.eval(working_rgb[2].clamp(0.0, 1.0)),
            ];

            let xyz_final = mat_vec_mul(&working.to_xyz_d50, toned);
            let srgb_lin = mat_vec_mul(&srgb.from_xyz_d50, xyz_final);
            let encoded = [
                srgb_oetf(srgb_lin[0]),
                srgb_oetf(srgb_lin[1]),
                srgb_oetf(srgb_lin[2]),
            ];

            out.put_pixel(
                x,
                y,
                Rgb([
                    (encoded[0] * 255.0).round().clamp(0.0, 255.0) as u8,
                    (encoded[1] * 255.0).round().clamp(0.0, 255.0) as u8,
                    (encoded[2] * 255.0).round().clamp(0.0, 255.0) as u8,
                ]),
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A HueSatMap whose entries vary only by hue/sat bin, not by value (every value-axis entry
    /// is identical) -- so, per the DNG reference implementation, its output must be identical
    /// regardless of `TableEncoding`, since encoding only ever touches the value coordinate. The
    /// bug this guards against: an earlier version of `apply_hue_sat` encoded all of R/G/B before
    /// computing HSV, which changes the *hue and saturation* HSV coordinates too (not just V) --
    /// that version would have picked a different hue/sat table bin under `Srgb` than under
    /// `Linear` for the same input, and this test would fail on it.
    fn hue_only_map() -> HueSatMap {
        // 4 hue divisions (0/90/180/270 deg), 1 sat division, 1 val division (so val can't be
        // the source of any difference either) -- bin 1 (90 deg) gets a distinctive +15 deg
        // shift, every other bin is a no-op.
        let mut data = vec![[0.0f32, 1.0, 1.0]; 4];
        data[1] = [15.0, 1.0, 1.0];
        HueSatMap {
            hue_divisions: 4,
            sat_divisions: 1,
            val_divisions: 1,
            data,
        }
    }

    #[test]
    fn encoding_choice_does_not_affect_hue_or_saturation() {
        let map = hue_only_map();
        // An RGB with real, non-trivial ratios (not a neutral gray, where hue is undefined and
        // this test couldn't distinguish anything) landing near the map's shifted 90 deg bin.
        let rgb: Vec3 = [0.3, 0.6, 0.1];

        let out_linear = apply_hue_sat(rgb, &map, None, 1.0, TableEncoding::Linear);
        let out_srgb = apply_hue_sat(rgb, &map, None, 1.0, TableEncoding::Srgb);

        for (a, b) in out_linear.iter().zip(out_srgb.iter()) {
            assert!(
                (a - b).abs() < 1e-9,
                "encoding leaked into hue/sat: linear={out_linear:?} srgb={out_srgb:?}"
            );
        }
        // And confirm the map's hue shift actually did something (i.e. this isn't vacuously
        // passing because both paths happened to no-op).
        assert!(
            (out_linear[0] - rgb[0]).abs() > 1e-6 || (out_linear[1] - rgb[1]).abs() > 1e-6,
            "expected the hue shift to visibly change the output, got {out_linear:?}"
        );
    }

    #[test]
    fn encoding_choice_does_affect_value_scaling() {
        // A map whose only nontrivial entries differ by *value* bin -- this is the one axis
        // `TableEncoding` should actually influence, per the DNG reference implementation.
        let map = HueSatMap {
            hue_divisions: 1,
            sat_divisions: 1,
            val_divisions: 2,
            data: vec![[0.0, 1.0, 1.0], [0.0, 1.0, 2.0]],
        };
        let rgb: Vec3 = [0.5, 0.5, 0.5];

        let out_linear = apply_hue_sat(rgb, &map, None, 1.0, TableEncoding::Linear);
        let out_srgb = apply_hue_sat(rgb, &map, None, 1.0, TableEncoding::Srgb);

        assert!(
            (out_linear[0] - out_srgb[0]).abs() > 1e-6,
            "expected TableEncoding to change the value-axis lookup: linear={out_linear:?} srgb={out_srgb:?}"
        );
    }
}
