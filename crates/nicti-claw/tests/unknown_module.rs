//! ADR-0002's "a stage id this build doesn't recognize stays read-only rather than being
//! dropped or guessed at", generalized to the registry level: looking up an id with no
//! installed module returns `None` cleanly, never a panic or a guessed fallback.

use nicti_claw::{Descriptor, Module, Registry};
use serde_json::Value;
use std::sync::Arc;

struct Noop;

impl Module for Noop {
    fn id(&self) -> &str {
        "nicti.decoder.libraw"
    }

    fn schema_version(&self) -> u32 {
        1
    }

    fn migrate_params(&self, _from_version: u32, params: Value) -> Option<Value> {
        Some(params)
    }
}

#[test]
fn unknown_id_returns_none_not_a_guess() {
    let mut registry: Registry<Noop> = Registry::new();
    registry
        .register(
            Descriptor {
                id: "nicti.decoder.libraw",
                schema_version: 1,
            },
            || Arc::new(Noop),
        )
        .expect("registration should succeed");

    assert!(registry.find("nicti.decoder.libraw").is_some());
    assert!(registry.get("nicti.decoder.libraw").is_some());

    assert!(registry.find("someplugin.render_stage.dreamify").is_none());
    assert!(registry.get("someplugin.render_stage.dreamify").is_none());
}
