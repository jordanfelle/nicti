//! Color management (#42, ADR-0042) and the camera color profile extension point (ADR-0019
//! §7/§8).
//!
//! - [`space`]: the sRGB / Display P3 / Adobe RGB output spaces — matrices from the linear
//!   ProPhoto (D50) working space, and transfer functions.
//! - [`icc`]: runtime-generated ICC profiles for those spaces (`moxcms`), for export embedding.
//! - [`transform`]: [`transform::DisplayTransform`], the display + soft-proof transform (analytic
//!   proof stage with an exact out-of-gamut flag; exact matrix display, or a baked 3D LUT for a
//!   non-built-in monitor profile).
//! - [`display_profile`]: the active monitor's ICC profile, degrading to sRGB.
//! - [`source_transform`]: [`source_transform::SourceTransforms`], JPEG-sourced 8-bit RGBA (grid
//!   thumbnails, T0/T2 previews) from its embedded ICC profile (else sRGB) to the display (#319).
//!
//! `ColorProfile` still settles identity and versioning only, via `Module`; DCP parsing
//! (color-matrix, HueSatMap 3D LUT, tone curve) is #38's research, promoted here by the second
//! PR of #42.

pub mod cct;
pub mod dcp;
pub mod display_profile;
pub mod huesatmap;
pub mod icc;
mod math;
pub mod profile;
pub mod source_transform;
pub mod space;
pub mod tonecurve;
pub mod transform;
pub mod xmp_profile;

use nicti_claw::{Module, Registry};
use serde_json::Value;
use space::OutputSpace;

/// A color profile provider.
pub trait ColorProfile: Module {
    /// The output space this provider represents, if it is one of the built-in delivery spaces.
    fn output_space(&self) -> Option<OutputSpace> {
        None
    }
}

/// Registry of color profile modules, keyed by namespaced id.
pub type ProfileRegistry = Registry<dyn ColorProfile>;

/// Built-in [`ColorProfile`] for one of the [`OutputSpace`]s.
pub struct BuiltinSpace(pub OutputSpace);

impl BuiltinSpace {
    pub const SRGB_ID: &'static str = "nicti.color.srgb";
    pub const DISPLAY_P3_ID: &'static str = "nicti.color.display-p3";
    pub const ADOBE_RGB_ID: &'static str = "nicti.color.adobe-rgb";

    pub fn id_for(space: OutputSpace) -> &'static str {
        match space {
            OutputSpace::Srgb => Self::SRGB_ID,
            OutputSpace::DisplayP3 => Self::DISPLAY_P3_ID,
            OutputSpace::AdobeRgb => Self::ADOBE_RGB_ID,
        }
    }
}

impl Module for BuiltinSpace {
    fn id(&self) -> &str {
        Self::id_for(self.0)
    }

    fn schema_version(&self) -> u32 {
        1
    }

    fn migrate_params(&self, _from_version: u32, params: Value) -> Option<Value> {
        Some(params)
    }
}

impl ColorProfile for BuiltinSpace {
    fn output_space(&self) -> Option<OutputSpace> {
        Some(self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_claw::Descriptor;
    use std::sync::Arc;

    fn make_srgb() -> Arc<dyn ColorProfile> {
        Arc::new(BuiltinSpace(OutputSpace::Srgb))
    }

    #[test]
    fn builtin_space_registers_and_resolves_as_trait_object() {
        let mut registry: ProfileRegistry = Registry::new();
        registry
            .register(
                Descriptor {
                    id: BuiltinSpace::SRGB_ID,
                    schema_version: 1,
                },
                make_srgb,
            )
            .expect("registration should succeed");

        let resolved = registry.get(BuiltinSpace::SRGB_ID).expect("registered");
        assert_eq!(resolved.id(), BuiltinSpace::SRGB_ID);
        assert_eq!(resolved.output_space(), Some(OutputSpace::Srgb));
    }
}
