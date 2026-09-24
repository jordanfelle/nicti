//! Render stage extension point (ADR-0004 §7/§8). `RenderStage` settles identity and versioning
//! only, via `Module` — the real per-frame execution signature (GPU buffer bindings, cache-key
//! interaction with Tapetum's stage cache) is deliberately left open here, owned by #16 (GPU
//! compute API) and #44/#45 (Tapetum, the stage-cached render graph).

use nicti_claw::{Module, Registry};

/// A render stage (e.g. white balance, a local-adjustment mask, an AI denoise stage). No
/// execution method is defined here yet — that signature belongs to #16/#44/#45.
pub trait RenderStage: Module {}

/// Registry of render stage modules, keyed by namespaced id (ADR-0002's `vendor.stage_name`
/// convention).
pub type StageRegistry = Registry<dyn RenderStage>;

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_claw::Descriptor;
    use serde_json::Value;
    use std::sync::Arc;

    struct Dummy;

    impl Module for Dummy {
        fn id(&self) -> &str {
            "nicti.stage.dummy"
        }

        fn schema_version(&self) -> u32 {
            1
        }

        fn migrate_params(&self, _from_version: u32, params: Value) -> Option<Value> {
            Some(params)
        }
    }

    impl RenderStage for Dummy {}

    fn make_dummy() -> Arc<dyn RenderStage> {
        Arc::new(Dummy)
    }

    #[test]
    fn dummy_registers_and_resolves_as_trait_object() {
        let mut registry: StageRegistry = Registry::new();
        registry
            .register(
                Descriptor {
                    id: "nicti.stage.dummy",
                    schema_version: 1,
                },
                make_dummy,
            )
            .expect("registration should succeed");

        let resolved = registry
            .get("nicti.stage.dummy")
            .expect("dummy stage is registered");
        assert_eq!(resolved.id(), "nicti.stage.dummy");
    }
}
