//! CPU-side color math for the live suffix: as-shot white balance, the camera-RGB -> working
//! -space (linear ProPhoto RGB) matrix chain, a simple tone curve, and a vibrance formula. Shared
//! between building the live-suffix shader's uniform values and a pure-CPU reference used to
//! prove the GPU kernel matches it (this crate's own goldens, before real ones exist in #45's
//! tiling slice).
//!
//! **Deliberate simplification vs. the original design sketch**: no `HueSatMap`/`LookTable`
//! bindings are reserved in the shader (#42's DCP-profile color pipeline). Wiring in unused
//! texture bindings with no real content to sample would be exactly the kind of half-finished
//! scaffolding this repo's own conventions ask to avoid -- #42 can extend the shader (and this
//! module) when it lands, without needing this pipeline's *shape* pre-declared for it. Likewise,
//! the tone curve here is a simple exposure/contrast formula rather than a 1D LUT texture --
//! real, but intentionally the smallest thing that proves the pipeline end-to-end; swapping in a
//! LUT later doesn't change any other stage.

pub type Mat3 = [[f32; 3]; 3];

pub fn mat3_identity() -> Mat3 {
    [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]
}

/// Row-major 3x3 * 3x3 (standard matrix multiplication: `mat3_mul(a, b)` applies `b` first, then
/// `a`, matching function-composition order).
pub fn mat3_mul(a: Mat3, b: Mat3) -> Mat3 {
    let mut out = [[0.0f32; 3]; 3];
    for r in 0..3 {
        for c in 0..3 {
            out[r][c] = (0..3).map(|k| a[r][k] * b[k][c]).sum();
        }
    }
    out
}

pub fn mat3_apply(m: Mat3, v: [f32; 3]) -> [f32; 3] {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

fn mat3_diag(d: [f32; 3]) -> Mat3 {
    [[d[0], 0.0, 0.0], [0.0, d[1], 0.0], [0.0, 0.0, d[2]]]
}

/// Cramer's-rule 3x3 inverse. Panics on a singular matrix -- `cam_xyz` (the only matrix this
/// module inverts) is always invertible in practice: LibRaw derives it from a real sensor's
/// spectral-sensitivity calibration, never a degenerate/rank-deficient one.
fn mat3_invert(m: &Mat3) -> Mat3 {
    let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
    assert!(det.abs() > 1e-12, "cannot invert a singular matrix");
    let inv_det = 1.0 / det;
    [
        [
            (m[1][1] * m[2][2] - m[1][2] * m[2][1]) * inv_det,
            (m[0][2] * m[2][1] - m[0][1] * m[2][2]) * inv_det,
            (m[0][1] * m[1][2] - m[0][2] * m[1][1]) * inv_det,
        ],
        [
            (m[1][2] * m[2][0] - m[1][0] * m[2][2]) * inv_det,
            (m[0][0] * m[2][2] - m[0][2] * m[2][0]) * inv_det,
            (m[0][2] * m[1][0] - m[0][0] * m[1][2]) * inv_det,
        ],
        [
            (m[1][0] * m[2][1] - m[1][1] * m[2][0]) * inv_det,
            (m[0][1] * m[2][0] - m[0][0] * m[2][1]) * inv_det,
            (m[0][0] * m[1][1] - m[0][1] * m[1][0]) * inv_det,
        ],
    ]
}

/// The first three rows/cols of `nicti_cornea::LinearFrame::cam_xyz` (row-major 4x3) as a 3x3 --
/// the 4th row/col (LibRaw's G2 channel) is never populated by `LinearFrame`, which already drops
/// G2 during decode.
///
/// **Direction**: per LibRaw's own `cam_xyz_coeff` (`utils_dcraw.cpp`, confirmed against the
/// vendored source, not just its header comment), `cam_xyz` maps **XYZ -> camera** RGB, the
/// opposite of what its name suggests read as "camera to XYZ" -- `cam_rgb[i][j] = cam_xyz[i][k] *
/// xyz_rgb[k][j]`, i.e. `cam_xyz` composes with an XYZ input, never a camera one. Callers that
/// need camera -> XYZ (every caller in this crate) must invert the 3x3 this function returns; see
/// [`camera_to_working_space_matrix`].
pub fn cam_xyz_to_mat3(cam_xyz: &[f32; 12]) -> Mat3 {
    [
        [cam_xyz[0], cam_xyz[1], cam_xyz[2]],
        [cam_xyz[3], cam_xyz[4], cam_xyz[5]],
        [cam_xyz[6], cam_xyz[7], cam_xyz[8]],
    ]
}

/// As-shot white-balance gains from `cam_mul` (R/G/B/G2), normalized so the green gain is 1.0 --
/// the conventional "ratio over as-shot" framing (a gain of 1.0 on every channel is a no-op WB).
pub fn wb_gains(cam_mul: [f32; 4]) -> [f32; 3] {
    let g = if cam_mul[1] != 0.0 { cam_mul[1] } else { 1.0 };
    [cam_mul[0] / g, 1.0, cam_mul[2] / g]
}

/// Bradford-free XYZ(D50) -> linear ProPhoto RGB, the standard published matrix (ProPhoto RGB's
/// own native white point is D50, so no chromatic-adaptation step is needed here -- a real
/// illuminant-dependent adaptation, e.g. for a strongly non-D50 as-shot white balance, is #42's
/// DCP-profile scope, not this pipeline-proving slice's).
pub const XYZ_D50_TO_PROPHOTO: Mat3 = [
    [1.3459433, -0.2556075, -0.0511118],
    [-0.5445989, 1.5081673, 0.0205351],
    [0.0000000, 0.0000000, 1.2118128],
];

/// Linear ProPhoto RGB -> linear sRGB (via XYZ(D50), then a Bradford D50->D65 adaptation, then
/// XYZ(D65) -> linear sRGB) -- used only for readback/golden-image comparison
/// ([`crate::geometry::output_encode`]), never in the live suffix itself, which stays in the
/// working space (linear ProPhoto) end to end.
pub const PROPHOTO_TO_XYZ_D50: Mat3 = [
    [0.7976749, 0.1351917, 0.0313534],
    [0.2880402, 0.7118741, 0.0000857],
    [0.0, 0.0, 0.82521],
];

/// Bradford D50 -> D65 chromatic adaptation.
const BRADFORD_D50_TO_D65: Mat3 = [
    [0.9555766, -0.0230393, 0.0631636],
    [-0.0282895, 1.0099416, 0.0210077],
    [0.0122982, -0.0204830, 1.3299098],
];

const XYZ_D65_TO_SRGB: Mat3 = [
    [3.2404542, -1.5371385, -0.4985314],
    [-0.969266, 1.8760108, 0.041556],
    [0.0556434, -0.2040259, 1.0572252],
];

pub fn prophoto_to_srgb_linear_matrix() -> Mat3 {
    mat3_mul(
        XYZ_D65_TO_SRGB,
        mat3_mul(BRADFORD_D50_TO_D65, PROPHOTO_TO_XYZ_D50),
    )
}

/// The full camera-RGB -> working-space (linear ProPhoto) matrix, folding as-shot white balance
/// (a diagonal gain matrix) and the camera -> XYZ(D50) -> ProPhoto chain into one 3x3 -- linear
/// operations compose, so this is exactly equivalent to applying WB, then cam->XYZ, then
/// XYZ->ProPhoto as three separate steps, computed once per render on the CPU rather than on
/// every pixel on the GPU.
pub fn camera_to_working_space_matrix(cam_mul: [f32; 4], cam_xyz: &[f32; 12]) -> Mat3 {
    let wb = mat3_diag(wb_gains(cam_mul));
    // cam_xyz_to_mat3 returns XYZ->camera (see its own doc comment); invert to get camera->XYZ.
    let cam_to_xyz = mat3_invert(&cam_xyz_to_mat3(cam_xyz));
    mat3_mul(XYZ_D50_TO_PROPHOTO, mat3_mul(cam_to_xyz, wb))
}

pub fn exposure_multiplier(stops: f32) -> f32 {
    2f32.powf(stops)
}

/// A simple contrast curve applied per channel in a rough perceptual (cube-root) space, pivoting
/// around mid-grey -- `contrast` of 0.0 is a no-op; positive steepens the curve, negative
/// flattens it. Operates on non-negative linear values; a negative result never occurs since the
/// curve only ever scales a non-negative cube-root value.
pub fn apply_tone(rgb: [f32; 3], contrast: f32) -> [f32; 3] {
    rgb.map(|c| {
        let c = c.max(0.0);
        let perceptual = c.cbrt();
        let adjusted = (perceptual - 0.5) * (1.0 + contrast) + 0.5;
        adjusted.max(0.0).powi(3)
    })
}

/// Luma-preserving saturation boost, weighted more heavily on already-low-saturation pixels (the
/// conventional definition of "vibrance" vs. a flat "saturation" boost). `vibrance` of 0.0 is a
/// no-op. Luma uses Rec.709 weights as a working approximation in ProPhoto space, not a
/// colorimetrically exact ProPhoto luminance -- adequate for a saturation-boost weighting, not
/// claimed as radiometrically precise.
pub fn apply_vibrance(rgb: [f32; 3], vibrance: f32) -> [f32; 3] {
    let max = rgb[0].max(rgb[1]).max(rgb[2]);
    let min = rgb[0].min(rgb[1]).min(rgb[2]);
    let sat = if max > 0.0 { (max - min) / max } else { 0.0 };
    let boost = vibrance * (1.0 - sat);
    let luma = 0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2];
    rgb.map(|c| luma + (c - luma) * (1.0 + boost))
}

pub fn srgb_oetf(linear: f32) -> f32 {
    let c = linear.clamp(0.0, 1.0);
    if c <= 0.0031308 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mat3_invert_of_identity_is_identity() {
        assert_eq!(mat3_invert(&mat3_identity()), mat3_identity());
    }

    #[test]
    fn mat3_invert_round_trips() {
        let m = XYZ_D50_TO_PROPHOTO;
        let round_tripped = mat3_mul(mat3_invert(&m), m);
        for (r, row) in round_tripped.iter().enumerate() {
            for (c, &actual) in row.iter().enumerate() {
                let expected = if r == c { 1.0 } else { 0.0 };
                assert!(
                    (actual - expected).abs() < 1e-4,
                    "[{r}][{c}]: {actual} vs {expected}"
                );
            }
        }
    }

    #[test]
    fn camera_to_working_space_matrix_inverts_cam_xyz_direction() {
        // cam_xyz is XYZ->camera (see cam_xyz_to_mat3's doc comment); a non-identity, invertible
        // cam_xyz must be inverted, not used as-is, to reach camera->XYZ. This is a regression
        // test for a real bug: using cam_xyz un-inverted produced badly wrong colors (a strong
        // green cast) against a real NEF, caught in #45 PR4's real-hardware verification pass.
        let cam_mul = [1.0, 1.0, 1.0, 1.0];
        let cam_xyz = [
            0.5, 0.1, 0.0, // XYZ->camera R row
            0.0, 0.6, 0.1, // XYZ->camera G row
            0.1, 0.0, 0.7, // XYZ->camera B row
            0.0, 0.0, 0.0, // unused G2 row
        ];
        let m = camera_to_working_space_matrix(cam_mul, &cam_xyz);
        let expected = mat3_mul(XYZ_D50_TO_PROPHOTO, mat3_invert(&cam_xyz_to_mat3(&cam_xyz)));
        for r in 0..3 {
            for c in 0..3 {
                assert!((m[r][c] - expected[r][c]).abs() < 1e-6, "[{r}][{c}]");
            }
        }
    }

    #[test]
    fn mat3_mul_is_associative_with_identity() {
        let m = XYZ_D50_TO_PROPHOTO;
        assert_eq!(mat3_mul(m, mat3_identity()), m);
        assert_eq!(mat3_mul(mat3_identity(), m), m);
    }

    #[test]
    fn wb_gains_normalizes_green_to_one() {
        let gains = wb_gains([2.0, 1.0, 1.5, 1.0]);
        assert_eq!(gains, [2.0, 1.0, 1.5]);
    }

    #[test]
    fn wb_gains_handles_a_zero_green_multiplier_without_dividing_by_zero() {
        let gains = wb_gains([2.0, 0.0, 1.5, 0.0]);
        assert!(gains.iter().all(|g| g.is_finite()));
    }

    #[test]
    fn camera_to_working_space_matrix_is_identity_when_wb_and_cam_xyz_are_both_identity() {
        // An identity cam_xyz inverts to itself, so this doesn't exercise the inversion direction
        // (see camera_to_working_space_matrix_inverts_cam_xyz_direction for that) -- it only
        // pins the outer XYZ_D50_TO_PROPHOTO composition when WB and cam_xyz are both no-ops.
        let cam_mul = [1.0, 1.0, 1.0, 1.0];
        let identity_cam_xyz = [
            1.0, 0.0, 0.0, // R row
            0.0, 1.0, 0.0, // G row
            0.0, 0.0, 1.0, // B row
            0.0, 0.0, 0.0, // unused G2 row
        ];
        let m = camera_to_working_space_matrix(cam_mul, &identity_cam_xyz);
        assert_eq!(m, XYZ_D50_TO_PROPHOTO);
    }

    #[test]
    fn apply_tone_with_zero_contrast_is_a_near_identity() {
        let rgb = [0.2, 0.5, 0.8];
        let out = apply_tone(rgb, 0.0);
        for (a, b) in rgb.iter().zip(out.iter()) {
            assert!((a - b).abs() < 1e-4, "{a} vs {b}");
        }
    }

    #[test]
    fn apply_tone_positive_contrast_increases_spread_from_pivot() {
        let low = apply_tone([0.1, 0.1, 0.1], 0.5)[0];
        let high = apply_tone([0.9, 0.9, 0.9], 0.5)[0];
        assert!(low < 0.1, "low value should get darker: {low}");
        assert!(high > 0.9, "high value should get brighter: {high}");
    }

    #[test]
    fn apply_vibrance_with_zero_vibrance_is_identity() {
        let rgb = [0.3, 0.6, 0.1];
        let out = apply_vibrance(rgb, 0.0);
        for (a, b) in rgb.iter().zip(out.iter()) {
            assert!((a - b).abs() < 1e-5, "{a} vs {b}");
        }
    }

    #[test]
    fn apply_vibrance_boosts_a_low_saturation_pixel_more_than_a_high_saturation_one() {
        let low_sat = [0.5, 0.52, 0.48]; // near-gray
        let high_sat = [0.9, 0.1, 0.1]; // already very saturated
        let low_out = apply_vibrance(low_sat, 0.5);
        let high_out = apply_vibrance(high_sat, 0.5);
        let low_spread =
            low_out[0].max(low_out[1]).max(low_out[2]) - low_out[0].min(low_out[1]).min(low_out[2]);
        let low_spread_before = 0.52 - 0.48;
        let high_spread = high_out[0].max(high_out[1]).max(high_out[2])
            - high_out[0].min(high_out[1]).min(high_out[2]);
        let high_spread_before = 0.9 - 0.1;
        let low_growth = low_spread / low_spread_before;
        let high_growth = high_spread / high_spread_before;
        assert!(
            low_growth > high_growth,
            "low-saturation pixel should gain relatively more spread: {low_growth} vs {high_growth}"
        );
    }

    #[test]
    fn srgb_oetf_matches_known_reference_points() {
        assert!((srgb_oetf(0.0) - 0.0).abs() < 1e-6);
        assert!((srgb_oetf(1.0) - 1.0).abs() < 1e-6);
        // 18% mid-gray linear -> ~0.4614 in sRGB gamma space (1.055*0.18^(1/2.4) - 0.055).
        assert!((srgb_oetf(0.18) - 0.4614).abs() < 0.001);
    }
}
