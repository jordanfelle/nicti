//! End-to-end sanity check on a synthetic scene: a known camera-RGB value through a synthetic
//! identity-ish DCP profile (no HueSatMap/LookTable, identity tone curve), checked against a
//! hand-computed expected sRGB output. Exercises the full `pipeline.rs::render` path without any
//! real NEF or DCP.

use calico::dcp::DcpProfile;
use calico::matrix::{diag, mat_vec_mul, xyz_from_xy, IDENTITY};
use calico::pipeline::{render, RenderOptions};
use calico::tonecurve::ToneCurve;
use calico::workspace::{srgb_oetf, Space, WorkingSpace};
use image::{ImageBuffer, Rgb};

/// A camera->XYZ(D50) `ForwardMatrix` of `diag(D50_XYZ)` sends any equal-valued camera RGB
/// `[k,k,k]` to `k * D50_XYZ` -- i.e. genuinely neutral (same chromaticity as D50 white, just
/// scaled luminance), unlike a plain identity matrix (which would send `[k,k,k]` to XYZ
/// `[k,k,k]`, equal-energy-illuminant chromaticity, NOT neutral relative to D50 -- an earlier
/// version of this test used identity and got a visibly tinted "neutral" input as a result).
fn synthetic_profile() -> DcpProfile {
    let d50 = xyz_from_xy(0.3457, 0.3585);
    let forward = diag(d50);
    DcpProfile {
        name: "synthetic-neutral-preserving".to_string(),
        illuminant1_cct: 2856.0,
        illuminant2_cct: 6504.0,
        color_matrix1: IDENTITY,
        color_matrix2: IDENTITY,
        forward_matrix1: Some(forward),
        forward_matrix2: Some(forward),
        hue_sat_map1: None,
        hue_sat_map2: None,
        look_table: None,
        tone_curve_points: None,
        baseline_exposure_offset: 0.0,
    }
}

#[test]
fn neutral_gray_renders_to_neutral_srgb() {
    let width = 2;
    let height = 1;
    // Raw samples: black=0, maximum=1000, a mid-gray neutral value of 500 on every channel, no
    // per-channel black offset, and a 1:1 as-shot WB (cam_mul all equal) so no correction is
    // applied beyond linearization.
    let mut image: ImageBuffer<Rgb<u16>, Vec<u16>> = ImageBuffer::new(width, height);
    for x in 0..width {
        image.put_pixel(x, 0, Rgb([500, 500, 500]));
    }
    let meta = calico::linear_input::LinearMeta {
        make: "Test".into(),
        model: "Synthetic".into(),
        width,
        height,
        black: 0,
        maximum: 1000,
        cam_mul: [1.0, 1.0, 1.0, 1.0],
        pre_mul: [1.0, 1.0, 1.0, 1.0],
        cam_xyz: [0.0; 12],
        cblack: [0, 0, 0, 0],
    };
    let input = calico::linear_input::LinearInput { meta, image };

    let profile = synthetic_profile();
    let tone_curve = ToneCurve::identity();
    let opts = RenderOptions {
        profile: &profile,
        look: None,
        working_space: WorkingSpace::ProPhoto,
        tone_curve: &tone_curve,
    };
    let out = render(&input, &opts);

    let px = out.get_pixel(0, 0);
    // A neutral input (equal R=G=B, camera space == XYZ == D50) should render as a neutral gray
    // in sRGB too -- all three output channels equal.
    assert!(
        (px[0] as i32 - px[1] as i32).abs() <= 1,
        "expected neutral, got {px:?}"
    );
    assert!(
        (px[1] as i32 - px[2] as i32).abs() <= 1,
        "expected neutral, got {px:?}"
    );

    // Hand-computed expected value: linear 0.5 through D50->D65 Bradford adaptation (baked into
    // the ProPhoto->sRGB matrix chain) stays very close to 0.5 for a neutral color (adaptation
    // only reweights chromaticity, not luminance), then sRGB-encoded.
    let expected_linear = 0.5;
    let expected_u8 = (srgb_oetf(expected_linear) * 255.0).round() as i32;
    assert!(
        (px[0] as i32 - expected_u8).abs() <= 3,
        "expected ~{expected_u8}, got {}",
        px[0]
    );
}

#[test]
fn xyz_round_trip_through_working_spaces_is_stable() {
    // Direct sanity check on the matrix chain pipeline.rs relies on: ProPhoto -> XYZ(D50) ->
    // ProPhoto should be the identity for any color, independent of the render path above.
    let prophoto = Space::get(WorkingSpace::ProPhoto);
    let rgb = [0.3, 0.6, 0.1];
    let xyz = mat_vec_mul(&prophoto.to_xyz_d50, rgb);
    let back = mat_vec_mul(&prophoto.from_xyz_d50, xyz);
    for (b, r) in back.iter().zip(rgb.iter()) {
        assert!((b - r).abs() < 1e-9);
    }
}
