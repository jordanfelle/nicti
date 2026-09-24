//! New coverage for #20: id validation and duplicate-id rejection at registration time —
//! neither was exercised by the `spikes/sheath` prototype, which never validated ids at all.

use nicti_claw::{Descriptor, Module, RegisterError, Registry};
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

fn factory() -> Arc<Noop> {
    Arc::new(Noop)
}

#[test]
fn duplicate_id_is_rejected() {
    let mut registry: Registry<Noop> = Registry::new();
    registry
        .register(
            Descriptor {
                id: "nicti.decoder.libraw",
                schema_version: 1,
            },
            factory,
        )
        .expect("first registration should succeed");

    let err = registry
        .register(
            Descriptor {
                id: "nicti.decoder.libraw",
                schema_version: 2,
            },
            factory,
        )
        .expect_err("a second registration with the same id must be rejected");
    assert_eq!(err, RegisterError::DuplicateId("nicti.decoder.libraw"));
}

#[test]
fn invalid_ids_are_rejected() {
    let cases = [
        "noNamespace",
        "a..b",
        "Upper.case",
        "",
        "trailing.",
        ".leading",
    ];
    for id in cases {
        let mut registry: Registry<Noop> = Registry::new();
        let err = registry
            .register(
                Descriptor {
                    id,
                    schema_version: 1,
                },
                factory,
            )
            .expect_err(&format!("{id:?} should be rejected as an invalid id"));
        assert_eq!(err, RegisterError::InvalidId(id), "wrong error for {id:?}");
    }
}

#[test]
fn valid_namespaced_ids_are_accepted() {
    let cases = [
        "nicti.decoder.libraw",
        "nicti.exporter.jpeg",
        "vendor.stage_name",
        "a.b",
    ];
    for id in cases {
        let mut registry: Registry<Noop> = Registry::new();
        registry
            .register(
                Descriptor {
                    id,
                    schema_version: 1,
                },
                factory,
            )
            .unwrap_or_else(|e| panic!("{id:?} should be a valid id, got {e}"));
    }
}
