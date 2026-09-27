//! The one fixed color treatment #40 applies identically to every candidate (classic demosaic,
//! Path A/B AI denoise, and a from-scratch camera-RGB reconstruction of LRC's export) so quality
//! metrics measure demosaic/denoise differences, not a second, candidate-specific color pipeline.
//! Deliberately *not* calico's real DCP/HueSatMap/LookTable pipeline (ADR-0038) -- that's still
//! its own open research pass (pending a reference-machine ΔE run), and #40 shouldn't block on or
//! duplicate it. This is camera-XYZ(D50) -> linear sRGB -> sRGB OETF, the same fallback path
//! LibRaw's own built-in camera matrix takes when no DCP/ForwardMatrix profile is installed (see
//! calico's own `docs/decisions/color.md` stage-order note): correct enough to compare candidates
//! against each other and against LRC on equal footing, not a claim of accurate camera-specific
//! color rendering.

/// Bradford-adapted XYZ(D50) -> linear sRGB(D65), the standard matrix used throughout ICC/color
/// management tooling (e.g. littleCMS, ArgyllCMS) -- a published mathematical constant, not
/// project-specific data.
const XYZ_D50_TO_LINEAR_SRGB: [[f64; 3]; 3] = [
    [3.1338561, -1.6168667, -0.4906146],
    [-0.9787684, 1.9161415, 0.0334540],
    [0.0719453, -0.2289914, 1.4052427],
];

/// Camera RGB -> XYZ(D50), from `retina`'s `cam_xyz` sidecar field: row-major 4x3 (up to 4 camera
/// channels x XYZ, unused rows -- typically the 4th, for a 3-color Bayer sensor -- zero).
/// `XYZ_k = sum_c camRGB[c] * cam_xyz[c*3+k]`, matching LibRaw's own `cam_xyz` convention.
pub fn camera_rgb_to_xyz(cam_rgb: [f64; 3], cam_xyz: &[f32; 12]) -> [f64; 3] {
    std::array::from_fn(|k| (0..3).map(|c| cam_rgb[c] * cam_xyz[c * 3 + k] as f64).sum())
}

pub fn xyz_to_linear_srgb(xyz: [f64; 3]) -> [f64; 3] {
    std::array::from_fn(|j| (0..3).map(|k| XYZ_D50_TO_LINEAR_SRGB[j][k] * xyz[k]).sum())
}

/// The standard sRGB opto-electronic transfer function (linear -> gamma-encoded), IEC 61966-2-1.
pub fn srgb_oetf(linear: f64) -> f64 {
    if linear <= 0.0031308 {
        12.92 * linear
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    }
}

/// The full fixed treatment: black/white-scaled camera RGB (already `linearize_sample`d to
/// `0.0..=1.0`, see `linear_input.rs`) through to a display-encoded, clamped `0.0..=1.0` sRGB
/// triple every candidate shares.
pub fn to_display_srgb(cam_rgb: [f64; 3], cam_xyz: &[f32; 12]) -> [f32; 3] {
    let xyz = camera_rgb_to_xyz(cam_rgb, cam_xyz);
    let linear_srgb = xyz_to_linear_srgb(xyz);
    std::array::from_fn(|c| srgb_oetf(linear_srgb[c].clamp(0.0, 1.0)) as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// LibRaw's `cam_xyz` for a sensor whose camera-RGB primaries happen to equal CIE XYZ exactly
    /// (an identity-mapping stand-in camera) -- not a real sensor, just a matrix that makes the
    /// expected output computable by hand for the test.
    const IDENTITY_CAM_XYZ: [f32; 12] = [
        1.0, 0.0, 0.0, //
        0.0, 1.0, 0.0, //
        0.0, 0.0, 1.0, //
        0.0, 0.0, 0.0,
    ];

    #[test]
    fn camera_rgb_to_xyz_identity_matrix_passes_through() {
        let xyz = camera_rgb_to_xyz([0.2, 0.5, 0.8], &IDENTITY_CAM_XYZ);
        assert_eq!(xyz, [0.2, 0.5, 0.8]);
    }

    #[test]
    fn srgb_oetf_matches_known_values() {
        assert_eq!(srgb_oetf(0.0), 0.0);
        // Standard reference point: linear 1.0 -> encoded 1.0 exactly.
        assert!((srgb_oetf(1.0) - 1.0).abs() < 1e-9);
        // Below the linear-segment threshold, OETF is the plain 12.92x scale.
        assert!((srgb_oetf(0.001) - 0.001 * 12.92).abs() < 1e-9);
    }

    #[test]
    fn srgb_oetf_is_monotonic() {
        let mut prev = srgb_oetf(0.0);
        let mut v = 0.01;
        while v <= 1.0 {
            let cur = srgb_oetf(v);
            assert!(cur > prev, "not monotonic at {v}: {prev} -> {cur}");
            prev = cur;
            v += 0.01;
        }
    }

    #[test]
    fn to_display_srgb_clamps_out_of_gamut() {
        // A wildly out-of-range XYZ (e.g. from a noisy/clipped pixel) must clamp, not produce a
        // NaN or negative sample a downstream metric would choke on.
        let out = to_display_srgb([-5.0, 5.0, 0.5], &IDENTITY_CAM_XYZ);
        for c in out {
            assert!((0.0..=1.0).contains(&c), "expected clamped 0..1, got {c}");
        }
    }
}
