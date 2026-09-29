//! ICC profiles for the built-in output spaces, generated at runtime with `moxcms` (ADR-0056
//! rejected vendoring `.icc` files; ADR-0042 keeps that).

use crate::space::OutputSpace;
use moxcms::{CmsError, ColorProfile};

/// The `moxcms` profile for `space`.
pub fn color_profile(space: OutputSpace) -> ColorProfile {
    match space {
        OutputSpace::Srgb => ColorProfile::new_srgb(),
        OutputSpace::DisplayP3 => ColorProfile::new_display_p3(),
        OutputSpace::AdobeRgb => ColorProfile::new_adobe_rgb(),
    }
}

/// Serialized ICC bytes for `space`, ready for `img-parts` embedding (ADR-0056).
pub fn profile_bytes(space: OutputSpace) -> Result<Vec<u8>, CmsError> {
    color_profile(space).encode()
}

/// The working-space profile the LUT builder converts *from*: ProPhoto RGB, gamma 1.8, matching
/// the shaper the display shader applies to linear working-space values.
pub(crate) fn working_profile() -> ColorProfile {
    ColorProfile::new_pro_photo_rgb()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_bytes_parse_back_for_every_space() {
        for s in OutputSpace::ALL {
            let bytes = profile_bytes(s).expect("encodes");
            ColorProfile::new_from_slice(&bytes).expect("parses back");
        }
    }

    #[test]
    fn colorants_match_our_matrices() {
        // The generated profile's red colorant (XYZ, D50 PCS) must agree with our own primaries
        // math, so the ICC embedded in an export describes the pixels we actually wrote.
        for s in OutputSpace::ALL {
            let parsed = ColorProfile::new_from_slice(&profile_bytes(s).unwrap()).unwrap();
            let ours = s.to_xyz_d50();
            let red = parsed.red_colorant;
            assert!(
                (red.x - ours[0][0]).abs() < 2e-3,
                "{s:?} X {red:?} vs {ours:?}"
            );
            assert!(
                (red.y - ours[1][0]).abs() < 2e-3,
                "{s:?} Y {red:?} vs {ours:?}"
            );
            assert!(
                (red.z - ours[2][0]).abs() < 2e-3,
                "{s:?} Z {red:?} vs {ours:?}"
            );
        }
    }
}
