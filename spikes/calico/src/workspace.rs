//! Linear working-space definitions calico's `pipeline.rs` measures against (ADR-0021's
//! Candidates). All matrices convert to/from XYZ relative to **D50** (the DNG profile connection
//! space calico's camera->XYZ matrices, from `cct.rs`, already use) -- each space's own native
//! white is Bradford-adapted to D50 once, at construction, rather than carried around separately.

use crate::matrix::{bradford_adapt, mat_invert, mat_mul, xyz_from_xy, Mat3};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum WorkingSpace {
    /// ROMM/ProPhoto RGB, native D50 white -- ACR's own internal working space.
    ProPhoto,
    /// Rec.2020 primaries, native D65 white.
    Rec2020,
    /// ACEScg (AP1 primaries), native D60 white.
    Acescg,
    /// sRGB primaries/white -- used for final display-referred output, not as a working-space
    /// candidate itself.
    Srgb,
}

pub struct Space {
    /// XYZ(D50) -> linear working-space RGB.
    pub from_xyz_d50: Mat3,
    /// Linear working-space RGB -> XYZ(D50).
    pub to_xyz_d50: Mat3,
}

fn primaries_to_xyz(primaries: [(f64, f64); 3], white_xy: (f64, f64)) -> Mat3 {
    let xr = xyz_from_xy(primaries[0].0, primaries[0].1);
    let xg = xyz_from_xy(primaries[1].0, primaries[1].1);
    let xb = xyz_from_xy(primaries[2].0, primaries[2].1);
    // Solve for S such that M * S = whitepoint_XYZ, where M's columns are xr/xg/xb.
    let m: Mat3 = [
        [xr[0], xg[0], xb[0]],
        [xr[1], xg[1], xb[1]],
        [xr[2], xg[2], xb[2]],
    ];
    let inv = mat_invert(&m);
    let white = xyz_from_xy(white_xy.0, white_xy.1);
    let s = crate::matrix::mat_vec_mul(&inv, white);
    [
        [xr[0] * s[0], xg[0] * s[1], xb[0] * s[2]],
        [xr[1] * s[0], xg[1] * s[1], xb[1] * s[2]],
        [xr[2] * s[0], xg[2] * s[1], xb[2] * s[2]],
    ]
}

const D50: (f64, f64) = (0.3457, 0.3585);
const D65: (f64, f64) = (0.3127, 0.3290);
const D60: (f64, f64) = (0.32168, 0.33767);

impl Space {
    pub fn get(space: WorkingSpace) -> Self {
        let (primaries, white) = match space {
            WorkingSpace::ProPhoto => (
                [
                    (0.734699, 0.265301),
                    (0.159597, 0.840403),
                    (0.036598, 0.000105),
                ],
                D50,
            ),
            WorkingSpace::Rec2020 => ([(0.708, 0.292), (0.170, 0.797), (0.131, 0.046)], D65),
            WorkingSpace::Acescg => ([(0.713, 0.293), (0.165, 0.830), (0.128, 0.044)], D60),
            WorkingSpace::Srgb => ([(0.640, 0.330), (0.300, 0.600), (0.150, 0.060)], D65),
        };
        let to_xyz_native = primaries_to_xyz(primaries, white);
        let adapt_to_d50 = bradford_adapt(xyz_from_xy(white.0, white.1), xyz_from_xy(D50.0, D50.1));
        let to_xyz_d50 = mat_mul(&adapt_to_d50, &to_xyz_native);
        let from_xyz_d50 = mat_invert(&to_xyz_d50);
        Space {
            from_xyz_d50,
            to_xyz_d50,
        }
    }
}

/// sRGB's OETF (IEC 61966-2-1), applied per-channel to a linear sRGB sample in [0, 1] (values
/// outside that range are clamped -- out-of-gamut handling belongs to #42's soft-proofing, not
/// this research pass).
pub fn srgb_oetf(linear: f64) -> f64 {
    let c = linear.clamp(0.0, 1.0);
    if c <= 0.0031308 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

/// Inverse of [`srgb_oetf`]: encoded sRGB-like signal -> linear. Used for
/// `ProfileHueSatMapEncoding`/`ProfileLookTableEncoding` value 1 (DNG spec 6.3.7's "sRGB"
/// representation for a HueSatMap/LookTable's HSV coordinates).
pub fn srgb_eotf(encoded: f64) -> f64 {
    let c = encoded.clamp(0.0, 1.0);
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matrix::mat_vec_mul;

    #[test]
    fn srgb_white_maps_near_d65() {
        let s = Space::get(WorkingSpace::Srgb);
        let xyz = mat_vec_mul(&s.to_xyz_d50, [1.0, 1.0, 1.0]);
        // Adapted to D50: should be close to the D50 whitepoint XYZ, not D65's.
        let d50 = xyz_from_xy(D50.0, D50.1);
        assert!((xyz[0] - d50[0]).abs() < 0.01, "got {xyz:?}");
        assert!((xyz[1] - d50[1]).abs() < 0.01, "got {xyz:?}");
        assert!((xyz[2] - d50[2]).abs() < 0.01, "got {xyz:?}");
    }

    #[test]
    fn to_and_from_xyz_round_trip() {
        for space in [
            WorkingSpace::ProPhoto,
            WorkingSpace::Rec2020,
            WorkingSpace::Acescg,
            WorkingSpace::Srgb,
        ] {
            let s = Space::get(space);
            let rgb = [0.2, 0.5, 0.8];
            let xyz = mat_vec_mul(&s.to_xyz_d50, rgb);
            let back = mat_vec_mul(&s.from_xyz_d50, xyz);
            for (i, (b, r)) in back.iter().zip(rgb.iter()).enumerate() {
                assert!(
                    (b - r).abs() < 1e-9,
                    "space {space:?} channel {i}: {back:?}"
                );
            }
        }
    }

    #[test]
    fn srgb_oetf_endpoints() {
        assert_eq!(srgb_oetf(0.0), 0.0);
        assert!((srgb_oetf(1.0) - 1.0).abs() < 1e-9);
    }
}
