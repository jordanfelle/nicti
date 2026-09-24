//! Proves the ABI-version handshake actually protects the boundary: a dylib reporting a
//! different `abi_version` than the host expects is rejected with a clean error (never UB or a
//! crash), and so is a dylib missing the required export entirely.
//!
//! This deliberately tests the *handshake protocol* — a host that checks a declared version
//! number before trusting anything else — not a real cross-rustc-version struct layout break
//! (which would require building the fixture with a different compiler than `nicti-claw`
//! itself, out of scope here; see docs/adr/0004 for the caveat).

#[path = "support/mod.rs"]
mod support;

use dewclaw::ScaleVTable;
use nicti_claw::dylib::{DylibError, DylibModule};

#[test]
fn matching_abi_version_loads_and_calls_correctly() {
    let path = support::build_dewclaw(&[]);
    let module = unsafe { DylibModule::<ScaleVTable>::load(&path) }
        .expect("matching ABI version should load");
    assert_eq!((module.vtable().process)(2.0, 5.0), 10.0);
}

#[test]
fn mismatched_abi_version_is_rejected_cleanly() {
    let path = support::build_dewclaw(&["bad-abi"]);
    let err = unsafe { DylibModule::<ScaleVTable>::load(&path) }
        .expect_err("a dylib declaring the wrong ABI version must not load");
    match err {
        DylibError::AbiMismatch { expected, found } => {
            assert_ne!(expected, found);
        }
        other => panic!("expected AbiMismatch, got {other:?}"),
    }
}

#[test]
fn missing_export_is_rejected_cleanly() {
    let path = support::build_dewclaw(&["no-export"]);
    let err = unsafe { DylibModule::<ScaleVTable>::load(&path) }
        .expect_err("a dylib with no `claw_stage_vtable` export must not load");
    match err {
        DylibError::MissingSymbol(_) => {}
        other => panic!("expected MissingSymbol, got {other:?}"),
    }
}

#[test]
fn null_vtable_is_rejected_cleanly() {
    // The export exists (so `libloading` itself succeeds) but returns a null pointer — a
    // present-but-garbage export, distinct from a missing symbol. Loading this must not
    // dereference the null pointer.
    let path = support::build_dewclaw(&["null-vtable"]);
    let err = unsafe { DylibModule::<ScaleVTable>::load(&path) }
        .expect_err("a dylib whose `claw_stage_vtable` returns null must not load");
    match err {
        DylibError::NullVtable => {}
        other => panic!("expected NullVtable, got {other:?}"),
    }
}
