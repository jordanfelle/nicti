//! Camera color profile extension point (ADR-0004 §7/§8). `ColorProfile` settles identity and
//! versioning only, via `Module` — DCP parsing (color-matrix, HueSatMap 3D LUT, tone curve) is
//! owned by #38, and the display/output color-management path (ICC, sRGB/P3/AdobeRGB,
//! soft-proofing) is owned by #42.

use nicti_claw::{Module, Registry};

/// A camera color profile provider. No profile-application method is defined here yet — that
/// signature belongs to whichever of #38/#42 settles it.
pub trait ColorProfile: Module {}

/// Registry of color profile modules, keyed by namespaced id.
pub type ProfileRegistry = Registry<dyn ColorProfile>;

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_claw::Descriptor;
    use serde_json::Value;
    use std::sync::Arc;

    struct Dummy;

    impl Module for Dummy {
        fn id(&self) -> &str {
            "nicti.color.dummy"
        }

        fn schema_version(&self) -> u32 {
            1
        }

        fn migrate_params(&self, _from_version: u32, params: Value) -> Option<Value> {
            Some(params)
        }
    }

    impl ColorProfile for Dummy {}

    fn make_dummy() -> Arc<dyn ColorProfile> {
        Arc::new(Dummy)
    }

    #[test]
    fn dummy_registers_and_resolves_as_trait_object() {
        let mut registry: ProfileRegistry = Registry::new();
        registry
            .register(
                Descriptor {
                    id: "nicti.color.dummy",
                    schema_version: 1,
                },
                make_dummy,
            )
            .expect("registration should succeed");

        let resolved = registry
            .get("nicti.color.dummy")
            .expect("dummy profile is registered");
        assert_eq!(resolved.id(), "nicti.color.dummy");
    }
}
