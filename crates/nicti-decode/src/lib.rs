//! RAW decoder extension point (ADR-0004 §7/§8). `RawDecoder` settles identity and versioning
//! only, via `Module` — real decode execution (LibRaw vs. `rawler` vs. others, Nikon NEF
//! including HE/HE* TicoRAW) is owned by #37, and the demosaic/NR and GPU pipeline stages that
//! consume a decoded frame are owned by #40 and #41.

use nicti_claw::{Module, Registry};

/// A RAW decoder backend. No decode method is defined here yet — that signature belongs to
/// whichever of #37/#40/#41 settles it.
pub trait RawDecoder: Module {}

/// Registry of RAW decoder modules, keyed by namespaced id (e.g. `"nicti.decoder.libraw"`).
pub type DecoderRegistry = Registry<dyn RawDecoder>;

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_claw::Descriptor;
    use serde_json::Value;
    use std::sync::Arc;

    struct Dummy;

    impl Module for Dummy {
        fn id(&self) -> &str {
            "nicti.decoder.dummy"
        }

        fn schema_version(&self) -> u32 {
            1
        }

        fn migrate_params(&self, _from_version: u32, params: Value) -> Option<Value> {
            Some(params)
        }
    }

    impl RawDecoder for Dummy {}

    fn make_dummy() -> Arc<dyn RawDecoder> {
        Arc::new(Dummy)
    }

    #[test]
    fn dummy_registers_and_resolves_as_trait_object() {
        let mut registry: DecoderRegistry = Registry::new();
        registry
            .register(
                Descriptor {
                    id: "nicti.decoder.dummy",
                    schema_version: 1,
                },
                make_dummy,
            )
            .expect("registration should succeed");

        let resolved = registry
            .get("nicti.decoder.dummy")
            .expect("dummy decoder is registered");
        assert_eq!(resolved.id(), "nicti.decoder.dummy");
    }
}
