//! Proves the ABI-version handshake actually protects the boundary: a dylib reporting a
//! different `abi_version` than the host expects is rejected with a clean error (never UB
//! or a crash), and so is a dylib missing the required export entirely.
//!
//! This deliberately tests the *handshake protocol* — a host that checks a declared
//! version number before trusting anything else — not a real cross-rustc-version struct
//! layout break (which would require building the fixture with a different compiler than
//! `sheath` itself, out of scope for a spike; see docs/adr/0004 for the caveat).

#[path = "support/mod.rs"]
mod support;

use sheath::dylib::{DylibError, DylibStage};

#[test]
fn matching_abi_version_loads_and_calls_correctly() {
    let path = support::build_dewclaw(&[]);
    let stage = unsafe { DylibStage::load(&path) }.expect("matching ABI version should load");
    assert_eq!(stage.process(2.0, 5.0), 10.0);
}

#[test]
fn mismatched_abi_version_is_rejected_cleanly() {
    let path = support::build_dewclaw(&["bad-abi"]);
    let err = unsafe { DylibStage::load(&path) }
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
    let err = unsafe { DylibStage::load(&path) }
        .expect_err("a dylib with no `claw_stage_vtable` export must not load");
    match err {
        DylibError::MissingSymbol(_) => {}
        other => panic!("expected MissingSymbol, got {other:?}"),
    }
}
