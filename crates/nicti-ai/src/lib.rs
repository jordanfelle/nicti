//! AI model provider extension point (ADR-0004 §7/§8). `ModelProvider` settles identity and
//! versioning only, via `Module` — inference execution, and the `ort`/`load-dynamic` boundary
//! that defers the ONNX Runtime's own native-library load (ADR-0004 §3), are owned by the
//! AI-labeled tickets (masking #48/#49, healing/removal #50/#51, culling #33/#34/#35/#36,
//! auto-tone #53).

use nicti_claw::{Module, Registry};

/// An AI model provider (e.g. a masking model, a denoise model). No inference method is
/// defined here yet — that signature belongs to whichever AI ticket settles it.
pub trait ModelProvider: Module {}

/// Registry of AI model provider modules, keyed by namespaced id.
pub type ModelRegistry = Registry<dyn ModelProvider>;

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_claw::Descriptor;
    use serde_json::Value;
    use std::sync::Arc;

    struct Dummy;

    impl Module for Dummy {
        fn id(&self) -> &str {
            "nicti.ai.dummy"
        }

        fn schema_version(&self) -> u32 {
            1
        }

        fn migrate_params(&self, _from_version: u32, params: Value) -> Option<Value> {
            Some(params)
        }
    }

    impl ModelProvider for Dummy {}

    fn make_dummy() -> Arc<dyn ModelProvider> {
        Arc::new(Dummy)
    }

    #[test]
    fn dummy_registers_and_resolves_as_trait_object() {
        let mut registry: ModelRegistry = Registry::new();
        registry
            .register(
                Descriptor {
                    id: "nicti.ai.dummy",
                    schema_version: 1,
                },
                make_dummy,
            )
            .expect("registration should succeed");

        let resolved = registry
            .get("nicti.ai.dummy")
            .expect("dummy model provider is registered");
        assert_eq!(resolved.id(), "nicti.ai.dummy");
    }
}
