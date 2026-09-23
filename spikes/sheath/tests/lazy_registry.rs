//! Proves the "sheathed until needed" claim: a module's factory never runs until first
//! use, and runs at most once even when multiple threads race to be the first caller.

use sheath::registry::{Descriptor, LazyModule};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

static FACTORY_CALLS: AtomicUsize = AtomicUsize::new(0);

fn expensive_thing() -> String {
    FACTORY_CALLS.fetch_add(1, Ordering::SeqCst);
    "built".to_string()
}

#[test]
fn factory_does_not_run_before_first_get() {
    let module = LazyModule::new(
        Descriptor {
            id: "test.lazy",
            version: 1,
        },
        expensive_thing,
    );

    assert_eq!(module.build_count(), 0, "factory ran before any get() call");
    assert_eq!(module.descriptor().id, "test.lazy");

    let value = module.get();
    assert_eq!(*value, "built");
    assert_eq!(module.build_count(), 1);
}

#[test]
fn factory_runs_exactly_once_under_concurrent_first_use() {
    static FACTORY_CALLS_CONCURRENT: AtomicUsize = AtomicUsize::new(0);
    fn factory() -> u64 {
        FACTORY_CALLS_CONCURRENT.fetch_add(1, Ordering::SeqCst);
        42
    }

    let module = Arc::new(LazyModule::new(
        Descriptor {
            id: "test.concurrent",
            version: 1,
        },
        factory,
    ));

    let handles: Vec<_> = (0..16)
        .map(|_| {
            let module = Arc::clone(&module);
            std::thread::spawn(move || module.get())
        })
        .collect();

    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    assert_eq!(
        module.build_count(),
        1,
        "factory ran more than once under a concurrent-first-use race"
    );
    let first = &results[0];
    for r in &results {
        assert!(Arc::ptr_eq(first, r), "callers got different instances");
    }
}
