//! Proves a dylib-backed module registered in a `Registry` is loaded on first use, not
//! eagerly: `descriptors()` enumerates it without touching the filesystem/dynamic loader, and
//! `libloading::Library::new` only runs when `Registry::get()` is actually called.

#[path = "support/mod.rs"]
mod support;

use dewclaw::ScaleVTable;
use nicti_claw::dylib::DylibModule;
use nicti_claw::{Descriptor, Module, Registry};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

static LOAD_CALLS: AtomicUsize = AtomicUsize::new(0);
static DYLIB_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Wraps a dylib-loaded `ScaleVTable` as a `Module`, so it can be registered like any other
/// Claw extension-point implementation.
struct DewclawModule(DylibModule<ScaleVTable>);

impl Module for DewclawModule {
    fn id(&self) -> &str {
        "dewclaw.toy_stage"
    }

    fn schema_version(&self) -> u32 {
        1
    }

    fn migrate_params(
        &self,
        _from_version: u32,
        params: serde_json::Value,
    ) -> Option<serde_json::Value> {
        Some(params)
    }
}

fn load_dewclaw() -> Arc<DewclawModule> {
    LOAD_CALLS.fetch_add(1, Ordering::SeqCst);
    let path = DYLIB_PATH.get().expect("path set before get() is called");
    // SAFETY: `path` points at the `dewclaw` fixture, built by this same test just above.
    let module = unsafe {
        DylibModule::load(path).expect("dewclaw fixture should load with a matching ABI version")
    };
    Arc::new(DewclawModule(module))
}

#[test]
fn dylib_is_not_loaded_until_first_use() {
    DYLIB_PATH
        .set(support::build_dewclaw(&[]))
        .expect("test runs once");

    let mut registry: Registry<DewclawModule> = Registry::new();
    registry
        .register(
            Descriptor {
                id: "dewclaw.toy_stage",
                schema_version: 1,
            },
            load_dewclaw,
        )
        .expect("registration should succeed");

    assert_eq!(
        LOAD_CALLS.load(Ordering::SeqCst),
        0,
        "dylib loaded before first use"
    );
    assert!(registry
        .descriptors()
        .iter()
        .any(|d| d.id == "dewclaw.toy_stage"));
    assert_eq!(
        LOAD_CALLS.load(Ordering::SeqCst),
        0,
        "enumerating descriptors must not build any instance"
    );

    let module = registry
        .get("dewclaw.toy_stage")
        .expect("module is registered");
    assert_eq!(LOAD_CALLS.load(Ordering::SeqCst), 1);
    assert_eq!((module.0.vtable().process)(2.0, 3.0), 6.0);

    // A second `get()` reuses the already-loaded instance — no second load.
    let _ = registry.get("dewclaw.toy_stage");
    assert_eq!(LOAD_CALLS.load(Ordering::SeqCst), 1);
}
