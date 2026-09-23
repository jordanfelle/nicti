//! Toy stage: `process(input, param) = input * param`. Exists only to be loaded through
//! `sheath::dylib::DylibStage` — see that module's tests.

use sheath::dylib::{StageVTable, CLAW_ABI_VERSION};

// `#[allow(dead_code)]` throughout: CI's `--all-features` run turns on `bad-abi` and
// `no-export` simultaneously (a combination no real invocation of this fixture ever uses —
// each test builds with one feature set at a time via `support::build_dewclaw`), which
// would otherwise make these items legitimately unreachable under that specific
// feature-union and fail the workspace's `-D warnings` clippy gate.
#[allow(dead_code)]
extern "C" fn process(input: f32, param: f32) -> f32 {
    input * param
}

#[cfg(not(feature = "bad-abi"))]
const ABI_VERSION: u32 = CLAW_ABI_VERSION;
// Deliberately wrong on purpose (see Cargo.toml's `bad-abi` feature doc).
#[cfg(feature = "bad-abi")]
const ABI_VERSION: u32 = CLAW_ABI_VERSION + 1000;

#[allow(dead_code)]
static VTABLE: StageVTable = StageVTable {
    abi_version: ABI_VERSION,
    process,
};

#[cfg(all(not(feature = "no-export"), not(feature = "null-vtable")))]
#[no_mangle]
pub extern "C" fn claw_stage_vtable() -> *const StageVTable {
    &VTABLE
}

// A present-but-garbage export: the symbol exists (so `libloading::Library::get` succeeds),
// but calling it returns a null pointer, exercising `DylibStage::load`'s null-vtable check
// rather than its missing-symbol or ABI-mismatch paths.
#[cfg(all(not(feature = "no-export"), feature = "null-vtable"))]
#[no_mangle]
pub extern "C" fn claw_stage_vtable() -> *const StageVTable {
    std::ptr::null()
}
