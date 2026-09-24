//! Proves the "sheathed until needed" claim: a module's factory never runs until first `get()`,
//! runs at most once even under a concurrent race to be the first caller, and enumerating
//! descriptors never builds anything.

use nicti_claw::{Descriptor, Module, Registry};
use serde_json::Value;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

struct Counted {
    label: &'static str,
}

impl Module for Counted {
    fn id(&self) -> &str {
        self.label
    }

    fn schema_version(&self) -> u32 {
        1
    }

    fn migrate_params(&self, _from_version: u32, params: Value) -> Option<Value> {
        Some(params)
    }
}

static FACTORY_CALLS: AtomicUsize = AtomicUsize::new(0);

fn expensive_thing() -> Arc<Counted> {
    FACTORY_CALLS.fetch_add(1, Ordering::SeqCst);
    Arc::new(Counted { label: "test.lazy" })
}

#[test]
fn factory_does_not_run_before_first_get() {
    let mut registry: Registry<Counted> = Registry::new();
    registry
        .register(
            Descriptor {
                id: "test.lazy",
                schema_version: 1,
            },
            expensive_thing,
        )
        .expect("registration should succeed");

    assert_eq!(
        FACTORY_CALLS.load(Ordering::SeqCst),
        0,
        "factory ran before any get() call"
    );
    assert!(registry.descriptors().iter().any(|d| d.id == "test.lazy"));
    assert_eq!(
        FACTORY_CALLS.load(Ordering::SeqCst),
        0,
        "enumerating descriptors must not build any instance"
    );

    let value = registry.get("test.lazy").expect("module is registered");
    assert_eq!(value.label, "test.lazy");
    assert_eq!(FACTORY_CALLS.load(Ordering::SeqCst), 1);
}

#[test]
fn factory_runs_exactly_once_under_concurrent_first_use() {
    static FACTORY_CALLS_CONCURRENT: AtomicUsize = AtomicUsize::new(0);
    fn factory() -> Arc<Counted> {
        FACTORY_CALLS_CONCURRENT.fetch_add(1, Ordering::SeqCst);
        Arc::new(Counted {
            label: "test.concurrent",
        })
    }

    let mut registry: Registry<Counted> = Registry::new();
    registry
        .register(
            Descriptor {
                id: "test.concurrent",
                schema_version: 1,
            },
            factory,
        )
        .expect("registration should succeed");
    let registry = Arc::new(registry);

    let handles: Vec<_> = (0..16)
        .map(|_| {
            let registry = Arc::clone(&registry);
            std::thread::spawn(move || {
                registry
                    .get("test.concurrent")
                    .expect("module is registered")
            })
        })
        .collect();

    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    assert_eq!(
        FACTORY_CALLS_CONCURRENT.load(Ordering::SeqCst),
        1,
        "factory ran more than once under a concurrent-first-use race"
    );
    let first = &results[0];
    for r in &results {
        assert!(Arc::ptr_eq(first, r), "callers got different instances");
    }
}
