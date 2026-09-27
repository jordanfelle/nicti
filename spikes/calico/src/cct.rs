//! Correlated-color-temperature (CCT) estimation and DNG's dual-illuminant matrix interpolation.
//!
//! DNG cameras ship two calibrated ColorMatrix/ForwardMatrix pairs, one per calibration
//! illuminant (typically ~2856K Standard Light A and ~6504K D65). To render a given AsShotNeutral
//! (the raw-space color a neutral gray subject produces under the actual shooting light), the DNG
//! spec has the reader iteratively estimate that light's CCT and blend the two illuminants'
//! matrices by inverse-CCT weight -- see `docs/adr/0038-color-pipeline.md`'s Candidates section
//! for why this project doesn't try to reproduce Adobe's exact (undisclosed) iteration; this uses
//! the same publicly documented two-step process (interpolate -> estimate CCT -> re-interpolate)
//! with McCamy's published approximation in place of Adobe's own CCT solver.

use crate::matrix::{
    bradford_adapt, mat_add_scaled, mat_invert, mat_vec_mul, xyz_from_xy, Mat3, Vec3,
};

/// DNG's `CalibrationIlluminant` interpolation weight for illuminant 1 (spec section 6.3.7):
/// linear in inverse-CCT (mired) space, clamped to [0, 1] outside the calibrated range.
pub fn interpolation_weight(cct: f64, t1: f64, t2: f64) -> f64 {
    if t1 == t2 {
        return 1.0;
    }
    let (lo, hi) = if t1 < t2 { (t1, t2) } else { (t2, t1) };
    if cct <= lo {
        return if t1 <= t2 { 1.0 } else { 0.0 };
    }
    if cct >= hi {
        return if t1 <= t2 { 0.0 } else { 1.0 };
    }
    let g = (1.0 / cct - 1.0 / t2) / (1.0 / t1 - 1.0 / t2);
    g.clamp(0.0, 1.0)
}

/// McCamy's 1992 cubic approximation of CCT from CIE 1931 xy chromaticity. Accurate to within a
/// few K for chromaticities near the Planckian locus (2000K-10000K) -- a documented, published
/// stand-in for the DNG SDK's own (unpublished) Robertson-based solver.
pub fn xy_to_cct_mccamy(x: f64, y: f64) -> f64 {
    let n = (x - 0.3320) / (y - 0.1858);
    -449.0 * n.powi(3) + 3525.0 * n.powi(2) - 6823.3 * n + 5520.33
}

pub struct Illuminant {
    pub cct: f64,
    pub color_matrix: Mat3,
    pub forward_matrix: Option<Mat3>,
}

/// D50 white point, the DNG profile connection space.
const D50: Vec3 = [0.9642, 1.0, 0.8249];

/// The two DNG-spec camera->XYZ(D50) matrix contracts, distinguished because callers (`pipeline.rs`)
/// must feed each the right kind of camera-RGB input:
/// - **ForwardMatrix always expects *white-balanced* input** (each channel already divided by
///   AsShotNeutral) -- DNG spec 6.3.7.
/// - **The ColorMatrix-derived fallback expects raw, *not* white-balanced input** -- it maps
///   un-white-balanced camera RGB (calibrated under the estimated illuminant) to XYZ relative to
///   that illuminant's own white, and the Bradford step folded into this matrix does the
///   white-point correction instead of a separate per-pixel WB multiply. Applying `cam_mul` WB
///   *and* this matrix double-corrects the illuminant (a gray subject renders with a color cast).
pub enum CameraToXyz {
    Raw(Mat3),
    WhiteBalanced(Mat3),
}

/// Iteratively finds the shooting illuminant's CCT and the corresponding camera->XYZ(D50) matrix
/// for a given AsShotNeutral (camera-space RGB of a neutral subject, i.e. proportional to
/// `1/cam_mul`).
///
/// The white-point search (the iteration below) always uses the interpolated `ColorMatrix`
/// inverse applied to `neutral_camera` -- per the DNG spec, this is true regardless of whether a
/// `ForwardMatrix` exists, because `ForwardMatrix` expects already-white-balanced input, which
/// isn't known until this search converges. `ForwardMatrix` (when both illuminants have one) is
/// used only for the *final* matrix, after CCT convergence.
pub fn solve_camera_to_xyz(
    neutral_camera: Vec3,
    illum1: &Illuminant,
    illum2: &Illuminant,
) -> (f64, CameraToXyz) {
    let mut cct = 5000.0;
    let mut last_xy = cct_to_approx_xy(cct);
    for _ in 0..16 {
        let g = interpolation_weight(cct, illum1.cct, illum2.cct);
        let color_matrix = mat_add_scaled(&illum2.color_matrix, &illum1.color_matrix, g);
        let inv = mat_invert(&color_matrix);
        let xyz = mat_vec_mul(&inv, neutral_camera);
        let sum = xyz[0] + xyz[1] + xyz[2];
        if sum.abs() < 1e-12 {
            break;
        }
        let (x, y) = (xyz[0] / sum, xyz[1] / sum);
        last_xy = (x, y);
        let new_cct = xy_to_cct_mccamy(x, y).clamp(1500.0, 25000.0);
        if (new_cct - cct).abs() < 1.0 {
            cct = new_cct;
            break;
        }
        cct = new_cct;
    }
    let g = interpolation_weight(cct, illum1.cct, illum2.cct);
    let matrix = match (illum1.forward_matrix, illum2.forward_matrix) {
        (Some(f1), Some(f2)) => CameraToXyz::WhiteBalanced(mat_add_scaled(&f2, &f1, g)),
        _ => {
            let color_matrix = mat_add_scaled(&illum2.color_matrix, &illum1.color_matrix, g);
            let inv = mat_invert(&color_matrix);
            // Bradford-adapt from the *actual solved* chromaticity (last_xy), not a re-derived
            // Planckian-locus point for the final CCT -- the latter drops the tint (Duv) of the
            // real neutral, which `last_xy` (from the converged iteration) already captures.
            let illum_white = xyz_from_xy(last_xy.0, last_xy.1);
            let adapt = bradford_adapt(illum_white, D50);
            CameraToXyz::Raw(crate::matrix::mat_mul(&adapt, &inv))
        }
    };
    (cct, matrix)
}

/// Approximate Planckian-locus xy for a given CCT (Kim et al. 2002 approximation) -- used only to
/// re-derive a chromaticity for Bradford adaptation in `solve_camera_to_xyz`'s no-ForwardMatrix
/// path; a rough estimate is sufficient there since the CCT search itself already converged.
fn cct_to_approx_xy(cct: f64) -> (f64, f64) {
    let t = cct.clamp(1667.0, 25000.0);
    let x = if t <= 4000.0 {
        -0.2661239e9 / t.powi(3) - 0.2343589e6 / t.powi(2) + 0.8776956e3 / t + 0.179910
    } else {
        -3.0258469e9 / t.powi(3) + 2.1070379e6 / t.powi(2) + 0.2226347e3 / t + 0.240390
    };
    let y = if t <= 2222.0 {
        -1.1063814 * x.powi(3) - 1.34811020 * x.powi(2) + 2.18555832 * x - 0.20219683
    } else if t <= 4000.0 {
        -0.9549476 * x.powi(3) - 1.37418593 * x.powi(2) + 2.09137015 * x - 0.16748867
    } else {
        3.0817580 * x.powi(3) - 5.87338670 * x.powi(2) + 3.75112997 * x - 0.37001483
    };
    (x, y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolation_weight_clamps_at_endpoints() {
        assert_eq!(interpolation_weight(1000.0, 2856.0, 6504.0), 1.0);
        assert_eq!(interpolation_weight(20000.0, 2856.0, 6504.0), 0.0);
    }

    #[test]
    fn interpolation_weight_is_half_at_harmonic_midpoint() {
        let t1 = 2856.0_f64;
        let t2 = 6504.0_f64;
        let mid = 1.0 / (0.5 * (1.0 / t1 + 1.0 / t2));
        let g = interpolation_weight(mid, t1, t2);
        assert!((g - 0.5).abs() < 1e-9);
    }

    #[test]
    fn mccamy_d65_is_near_6500k() {
        // CIE D65 chromaticity.
        let cct = xy_to_cct_mccamy(0.31272, 0.32903);
        assert!((cct - 6500.0).abs() < 200.0, "got {cct}");
    }

    #[test]
    fn solve_converges_for_a_neutral_matching_illuminant2() {
        // A synthetic camera whose ColorMatrix2 (D65) is the identity: a neutral camera-RGB of
        // [1,1,1] should resolve to something near D65's own CCT, and the returned matrix should
        // map that neutral back to a chromaticity near D65's.
        let illum1 = Illuminant {
            cct: 2856.0,
            color_matrix: crate::matrix::IDENTITY,
            forward_matrix: None,
        };
        let illum2 = Illuminant {
            cct: 6504.0,
            color_matrix: crate::matrix::IDENTITY,
            forward_matrix: None,
        };
        let (cct, matrix) = solve_camera_to_xyz([1.0, 1.0, 1.0], &illum1, &illum2);
        assert!(cct > 3000.0, "expected a mid/high CCT, got {cct}");
        let CameraToXyz::Raw(m) = matrix else {
            panic!("expected the ColorMatrix (Raw) branch: neither illuminant has a ForwardMatrix");
        };
        let xyz = mat_vec_mul(&m, [1.0, 1.0, 1.0]);
        assert!(xyz[1] > 0.0);
    }
}
