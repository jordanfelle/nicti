//! Toy stage: `process(input, param) = input * param`. Exists only to be loaded through
//! `nicti_claw::dylib::DylibModule` — see `tests/abi_handshake.rs` and
//! `tests/dylib_on_demand.rs`.

use nicti_claw::VTable;

/// `#[repr(C)]` with `abi_version` first, per `VTable`'s safety contract.
#[repr(C)]
pub struct ScaleVTable {
    pub abi_version: u32,
    pub process: extern "C" fn(input: f32, param: f32) -> f32,
}

// SAFETY: `ScaleVTable` is `#[repr(C)]` with `abi_version: u32` as its first field, matching
// `VTable`'s safety contract.
unsafe impl VTable for ScaleVTable {
    const SYMBOL: &'static [u8] = b"claw_stage_vtable\0";
    const ABI_VERSION: u32 = 1;
}

// `#[allow(dead_code)]` throughout: CI's `--all-features` run turns on `bad-abi` and
// `no-export` simultaneously (a combination no real invocation of this fixture ever uses — each
// test builds with one feature set at a time via `support::build_dewclaw`), which would
// otherwise make these items legitimately unreachable under that specific feature-union and
// fail the workspace's `-D warnings` clippy gate.
#[allow(dead_code)]
extern "C" fn process(input: f32, param: f32) -> f32 {
    input * param
}

#[cfg(not(feature = "bad-abi"))]
const ABI_VERSION: u32 = ScaleVTable::ABI_VERSION;
// Deliberately wrong on purpose (see Cargo.toml's `bad-abi` feature doc).
#[cfg(feature = "bad-abi")]
const ABI_VERSION: u32 = ScaleVTable::ABI_VERSION + 1000;

#[allow(dead_code)]
static VTABLE: ScaleVTable = ScaleVTable {
    abi_version: ABI_VERSION,
    process,
};

#[cfg(all(not(feature = "no-export"), not(feature = "null-vtable")))]
#[no_mangle]
pub extern "C" fn claw_stage_vtable() -> *const ScaleVTable {
    &VTABLE
}

// A present-but-garbage export: the symbol exists (so `libloading::Library::get` succeeds), but
// calling it returns a null pointer, exercising `DylibModule::load`'s null-vtable check rather
// than its missing-symbol or ABI-mismatch paths.
#[cfg(all(not(feature = "no-export"), feature = "null-vtable"))]
#[no_mangle]
pub extern "C" fn claw_stage_vtable() -> *const ScaleVTable {
    std::ptr::null()
}
