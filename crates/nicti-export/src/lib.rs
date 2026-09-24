//! Exporter extension point (ADR-0004 §7/§8). `Exporter` settles identity and versioning only,
//! via `Module` — resize/encode/metadata-write/watermark execution and export presets/routing
//! are owned by #56 (export stack research) and #57 (export pipeline).

use nicti_claw::{Module, Registry};

/// An export target (e.g. JPEG, TIFF, PNG). No export method is defined here yet — that
/// signature belongs to #56/#57.
pub trait Exporter: Module {}

/// Registry of exporter modules, keyed by namespaced id.
pub type ExporterRegistry = Registry<dyn Exporter>;

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_claw::Descriptor;
    use serde_json::Value;
    use std::sync::Arc;

    struct Dummy;

    impl Module for Dummy {
        fn id(&self) -> &str {
            "nicti.exporter.dummy"
        }

        fn schema_version(&self) -> u32 {
            1
        }

        fn migrate_params(&self, _from_version: u32, params: Value) -> Option<Value> {
            Some(params)
        }
    }

    impl Exporter for Dummy {}

    fn make_dummy() -> Arc<dyn Exporter> {
        Arc::new(Dummy)
    }

    #[test]
    fn dummy_registers_and_resolves_as_trait_object() {
        let mut registry: ExporterRegistry = Registry::new();
        registry
            .register(
                Descriptor {
                    id: "nicti.exporter.dummy",
                    schema_version: 1,
                },
                make_dummy,
            )
            .expect("registration should succeed");

        let resolved = registry
            .get("nicti.exporter.dummy")
            .expect("dummy exporter is registered");
        assert_eq!(resolved.id(), "nicti.exporter.dummy");
    }
}
