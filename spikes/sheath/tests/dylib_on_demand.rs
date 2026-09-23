//! Proves a native dylib module is loaded on first use, not eagerly: the registry
//! enumerates a descriptor for it without touching the filesystem/dynamic loader, and
//! `libloading::Library::new` only runs when `LazyModule::get()` is actually called.

#[path = "support/mod.rs"]
mod support;

use sheath::dylib::DylibStage;
use sheath::registry::{Descriptor, LazyModule};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

static LOAD_CALLS: AtomicUsize = AtomicUsize::new(0);
static DYLIB_PATH: OnceLock<PathBuf> = OnceLock::new();

fn load_dewclaw() -> DylibStage {
    LOAD_CALLS.fetch_add(1, Ordering::SeqCst);
    let path = DYLIB_PATH.get().expect("path set before get() is called");
    // SAFETY: `path` points at the `dewclaw` fixture, built by this same test just above.
    unsafe {
        DylibStage::load(path).expect("dewclaw fixture should load with a matching ABI version")
    }
}

#[test]
fn dylib_is_not_loaded_until_first_use() {
    DYLIB_PATH
        .set(support::build_dewclaw(&[]))
        .expect("test runs once");

    let module: LazyModule<DylibStage> = LazyModule::new(
        Descriptor {
            id: "dewclaw.toy_stage",
            version: 1,
        },
        load_dewclaw,
    );

    assert_eq!(
        LOAD_CALLS.load(Ordering::SeqCst),
        0,
        "dylib loaded before first use"
    );
    assert_eq!(module.build_count(), 0);

    let stage = module.get();
    assert_eq!(LOAD_CALLS.load(Ordering::SeqCst), 1);
    assert_eq!(module.build_count(), 1);
    assert_eq!(stage.process(2.0, 3.0), 6.0);

    // A second `get()` reuses the already-loaded instance — no second load.
    let _ = module.get();
    assert_eq!(LOAD_CALLS.load(Ordering::SeqCst), 1);
}
