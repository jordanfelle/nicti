//! Lens-correction data extension point (ADR-0004 §7/§8). `LensCorrection` settles identity and
//! versioning only, via `Module` — the correction data source (lensfun Rust binding vs.
//! embedded in-NEF data) and its application are owned by #39.

use nicti_claw::{Module, Registry};

/// A lens-correction data provider. No correction-application method is defined here yet —
/// that signature belongs to #39.
pub trait LensCorrection: Module {}

/// Registry of lens-correction modules, keyed by namespaced id.
pub type LensRegistry = Registry<dyn LensCorrection>;

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_claw::Descriptor;
    use serde_json::Value;
    use std::sync::Arc;

    struct Dummy;

    impl Module for Dummy {
        fn id(&self) -> &str {
            "nicti.lens.dummy"
        }

        fn schema_version(&self) -> u32 {
            1
        }

        fn migrate_params(&self, _from_version: u32, params: Value) -> Option<Value> {
            Some(params)
        }
    }

    impl LensCorrection for Dummy {}

    fn make_dummy() -> Arc<dyn LensCorrection> {
        Arc::new(Dummy)
    }

    #[test]
    fn dummy_registers_and_resolves_as_trait_object() {
        let mut registry: LensRegistry = Registry::new();
        registry
            .register(
                Descriptor {
                    id: "nicti.lens.dummy",
                    schema_version: 1,
                },
                make_dummy,
            )
            .expect("registration should succeed");

        let resolved = registry
            .get("nicti.lens.dummy")
            .expect("dummy lens module is registered");
        assert_eq!(resolved.id(), "nicti.lens.dummy");
    }
}
