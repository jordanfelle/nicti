//! Output color spaces (sRGB, Display P3, Adobe RGB) and their matrices/transfer functions,
//! relative to the pipeline's linear ProPhoto (D50) working space (ADR-0038, ADR-0042).
//!
//! Every matrix is built from published primaries with the space's own white Bradford-adapted
//! to D50 once, at construction — the same convention as `spikes/calico`'s `workspace.rs`.

use crate::math::{bradford_adapt, mat_invert, mat_mul, primaries_to_xyz, xyz_from_xy, Mat3};

const D50: (f64, f64) = (0.3457, 0.3585);
const D65: (f64, f64) = (0.3127, 0.3290);
const PROPHOTO: [(f64, f64); 3] = [
    (0.734699, 0.265301),
    (0.159597, 0.840403),
    (0.036598, 0.000105),
];

/// AdobeRGB (1998)'s pure power-law transfer exponent: 563/256.
pub const ADOBE_RGB_GAMMA: f64 = 563.0 / 256.0;

/// ProPhoto's own transfer exponent. The LUT shaper in [`crate::transform::DisplayTransform`]
/// uses it so the shaped working space matches `moxcms`'s `new_pro_photo_rgb()` source profile.
pub const PROPHOTO_GAMMA: f64 = 1.8;

/// Linear ProPhoto (D50) -> XYZ(D50), for building matrices from a foreign profile's colorants.
pub(crate) fn working_to_xyz_d50() -> Mat3 {
    primaries_to_xyz(PROPHOTO, D50)
}

/// XYZ(D50) -> linear ProPhoto (D50): the inverse of [`working_to_xyz_d50`].
pub(crate) fn working_from_xyz_d50() -> Mat3 {
    mat_invert(&working_to_xyz_d50())
}

/// A delivery/proofing color space the pipeline can convert to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutputSpace {
    Srgb,
    DisplayP3,
    AdobeRgb,
}

impl OutputSpace {
    pub const ALL: [OutputSpace; 3] = [Self::Srgb, Self::DisplayP3, Self::AdobeRgb];

    pub fn name(self) -> &'static str {
        match self {
            Self::Srgb => "sRGB",
            Self::DisplayP3 => "Display P3",
            Self::AdobeRgb => "Adobe RGB (1998)",
        }
    }

    fn primaries_and_white(self) -> ([(f64, f64); 3], (f64, f64)) {
        match self {
            Self::Srgb => ([(0.640, 0.330), (0.300, 0.600), (0.150, 0.060)], D65),
            Self::DisplayP3 => ([(0.680, 0.320), (0.265, 0.690), (0.150, 0.060)], D65),
            Self::AdobeRgb => ([(0.640, 0.330), (0.210, 0.710), (0.150, 0.060)], D65),
        }
    }

    /// Linear RGB of this space -> XYZ(D50).
    pub fn to_xyz_d50(self) -> Mat3 {
        let (primaries, white) = self.primaries_and_white();
        let native = primaries_to_xyz(primaries, white);
        let adapt = bradford_adapt(xyz_from_xy(white.0, white.1), xyz_from_xy(D50.0, D50.1));
        mat_mul(&adapt, &native)
    }

    /// Linear ProPhoto (D50) -> linear RGB of this space. Values outside [0, 1] are out of this
    /// space's gamut; the caller decides whether to clip.
    pub fn from_working(self) -> Mat3 {
        let prophoto_to_xyz = primaries_to_xyz(PROPHOTO, D50);
        mat_mul(&mat_invert(&self.to_xyz_d50()), &prophoto_to_xyz)
    }

    /// Linear RGB of this space -> linear ProPhoto (D50); the inverse of [`Self::from_working`].
    pub fn to_working(self) -> Mat3 {
        mat_invert(&self.from_working())
    }

    /// [`Self::to_working`] as f32, ready for a shader uniform.
    pub fn to_working_f32(self) -> [[f32; 3]; 3] {
        self.to_working().map(|r| r.map(|v| v as f32))
    }

    /// [`Self::from_working`] as f32, ready for a shader uniform.
    pub fn from_working_f32(self) -> [[f32; 3]; 3] {
        self.from_working().map(|r| r.map(|v| v as f32))
    }

    /// Linear -> encoded, clamped to [0, 1] (final delivery/display output).
    pub fn encode(self, linear: f32) -> f32 {
        let c = f64::from(linear).clamp(0.0, 1.0);
        match self {
            Self::Srgb | Self::DisplayP3 => srgb_oetf(c) as f32,
            Self::AdobeRgb => c.powf(1.0 / ADOBE_RGB_GAMMA) as f32,
        }
    }

    /// Encoded -> linear (inverse of [`Self::encode`] on [0, 1]).
    pub fn decode(self, encoded: f32) -> f32 {
        let c = f64::from(encoded).clamp(0.0, 1.0);
        match self {
            Self::Srgb | Self::DisplayP3 => srgb_eotf(c) as f32,
            Self::AdobeRgb => c.powf(ADOBE_RGB_GAMMA) as f32,
        }
    }
}

fn srgb_oetf(c: f64) -> f64 {
    if c <= 0.003_130_8 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

fn srgb_eotf(c: f64) -> f64 {
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::mat_vec_mul;

    #[test]
    fn white_maps_to_white_in_every_space() {
        // ProPhoto white (1,1,1) is D50 white, which every space's Bradford-adapted matrix must
        // map back to (1,1,1).
        for s in OutputSpace::ALL {
            let v = mat_vec_mul(&s.from_working(), [1.0, 1.0, 1.0]);
            for c in v {
                assert!((c - 1.0).abs() < 1e-3, "{s:?}: {v:?}");
            }
        }
    }

    #[test]
    fn grey_stays_neutral() {
        for s in OutputSpace::ALL {
            let v = mat_vec_mul(&s.from_working(), [0.3, 0.3, 0.3]);
            assert!(
                (v[0] - v[1]).abs() < 1e-3 && (v[1] - v[2]).abs() < 1e-3,
                "{s:?}: {v:?}"
            );
        }
    }

    fn assert_matrix(actual: Mat3, expect: Mat3) {
        for i in 0..3 {
            for j in 0..3 {
                assert!(
                    (actual[i][j] - expect[i][j]).abs() < 1e-4,
                    "[{i}][{j}] = {}",
                    actual[i][j]
                );
            }
        }
    }

    #[test]
    fn srgb_matrix_matches_published_values() {
        // sRGB linear -> XYZ(D50), Bruce Lindbloom's published Bradford-adapted matrix.
        assert_matrix(
            OutputSpace::Srgb.to_xyz_d50(),
            [
                [0.4360747, 0.3850649, 0.1430804],
                [0.2225045, 0.7168786, 0.0606169],
                [0.0139322, 0.0971045, 0.7141733],
            ],
        );
    }

    #[test]
    fn adobe_rgb_matrix_matches_published_values() {
        // Adobe RGB (1998) linear -> XYZ(D50), Lindbloom.
        assert_matrix(
            OutputSpace::AdobeRgb.to_xyz_d50(),
            [
                [0.6097559, 0.2052401, 0.1492240],
                [0.3111242, 0.6256560, 0.0632197],
                [0.0194811, 0.0608902, 0.7448387],
            ],
        );
    }

    #[test]
    fn display_p3_is_wider_than_srgb() {
        // A saturated P3 green lands outside sRGB (negative red).
        let xyz = mat_vec_mul(&OutputSpace::DisplayP3.to_xyz_d50(), [0.0, 1.0, 0.0]);
        let srgb = mat_vec_mul(&mat_invert(&OutputSpace::Srgb.to_xyz_d50()), xyz);
        assert!(
            srgb[0] < -0.01,
            "expected negative red in sRGB, got {srgb:?}"
        );
    }

    #[test]
    fn transfer_functions_round_trip() {
        for s in OutputSpace::ALL {
            for i in 0..=20 {
                let x = i as f32 / 20.0;
                let back = s.decode(s.encode(x));
                assert!((back - x).abs() < 1e-5, "{s:?} {x} -> {back}");
            }
        }
    }

    #[test]
    fn encode_endpoints_and_clamping() {
        for s in OutputSpace::ALL {
            assert_eq!(s.encode(0.0), 0.0);
            assert!((s.encode(1.0) - 1.0).abs() < 1e-6);
            assert!((s.encode(2.0) - 1.0).abs() < 1e-6);
            assert_eq!(s.encode(-1.0), 0.0);
        }
    }
}
