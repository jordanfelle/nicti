//! CPU reference pipeline: linear camera RGB (from `retina dump-linear`) -> a display-referred
//! image in a chosen working space, following ADR-0021's stage order.
//!
//! HueSatMap/LookTable application (DNG spec 6.3.7) is defined over a nonlinear ("gamma-encoded")
//! ProPhoto RGB representation regardless of the pipeline's own working-space choice -- this uses
//! a standard 1/1.8 power curve as a documented approximation of ACR's own encoding (undisclosed
//! exactly), consistent with several independent open-source DCP implementations' public
//! write-ups. See ADR-0021's Candidates/Deferred sections; this is exactly the kind of fidelity
//! gap the user's reference-machine ΔE pass is meant to catch.

use image::{ImageBuffer, Rgb, RgbImage};

use crate::cct::{solve_camera_to_xyz, Illuminant};
use crate::dcp::DcpProfile;
use crate::huesatmap::{hsv_to_rgb, rgb_to_hsv, HueSatMap};
use crate::linear_input::{linearize_sample, LinearInput};
use crate::matrix::{mat_vec_mul, Vec3};
use crate::tonecurve::ToneCurve;
use crate::workspace::{srgb_oetf, Space, WorkingSpace};

const PROPHOTO_GAMMA: f64 = 1.8;

fn prophoto_encode(c: f64) -> f64 {
    c.max(0.0).powf(1.0 / PROPHOTO_GAMMA)
}
fn prophoto_decode(c: f64) -> f64 {
    c.max(0.0).powf(PROPHOTO_GAMMA)
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

fn apply_hue_sat(
    rgb_linear_prophoto: Vec3,
    map1: &HueSatMap,
    map2: Option<&HueSatMap>,
    weight: f64,
) -> Vec3 {
    let encoded = [
        prophoto_encode(rgb_linear_prophoto[0]),
        prophoto_encode(rgb_linear_prophoto[1]),
        prophoto_encode(rgb_linear_prophoto[2]),
    ];
    let hsv = rgb_to_hsv(encoded);
    let adj = sample_blended(map1, map2, hsv[0], hsv[1], hsv[2], weight);
    let new_hsv = [
        hsv[0] + adj[0],
        (hsv[1] * adj[1]).clamp(0.0, 1.0),
        hsv[2] * adj[2],
    ];
    let out_encoded = hsv_to_rgb(new_hsv);
    [
        prophoto_decode(out_encoded[0]),
        prophoto_decode(out_encoded[1]),
        prophoto_decode(out_encoded[2]),
    ]
}

pub struct RenderOptions<'a> {
    pub profile: &'a DcpProfile,
    pub look: Option<&'a HueSatMap>,
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
    let (cct, camera_to_xyz_d50) = solve_camera_to_xyz(neutral, &illum1, &illum2);
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
            let g = meta.cam_mul[1] as f64;
            let wb: Vec3 = [
                cam_rgb[0] * (meta.cam_mul[0] as f64 / g),
                cam_rgb[1],
                cam_rgb[2] * (meta.cam_mul[2] as f64 / g),
            ];

            let xyz_d50 = mat_vec_mul(&camera_to_xyz_d50, wb);

            let prophoto_rgb = mat_vec_mul(&prophoto.from_xyz_d50, xyz_d50);
            let hue_sat_applied = match (&profile.hue_sat_map1, &profile.hue_sat_map2) {
                (Some(map1), map2) => {
                    apply_hue_sat(prophoto_rgb, map1, map2.as_ref(), hue_sat_weight)
                }
                (None, _) => prophoto_rgb,
            };

            let exposed: Vec3 = [
                hue_sat_applied[0] * exposure_scale,
                hue_sat_applied[1] * exposure_scale,
                hue_sat_applied[2] * exposure_scale,
            ];

            let looked = match opts.look {
                Some(look) => apply_hue_sat(exposed, look, None, 1.0),
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
