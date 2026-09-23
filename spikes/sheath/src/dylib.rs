//! C-ABI dylib loading with an explicit ABI-version handshake, checked before any other
//! call crosses the boundary. This is the isolation mechanism ADR-0003 requires for an
//! LGPL native dependency (e.g. a future `rawler`/`lensfun-rs`-style crate) that can't
//! cleanly satisfy LGPL's dynamic-linking safe harbor when statically linked in.
//!
//! Rust's own struct/vtable layout is not guaranteed stable across separate compilations
//! (doc.rust-lang.org/reference/type-layout.html: "Type layout can be changed with each
//! compilation... we only document what is guaranteed today", and the default `Rust`
//! representation explicitly does not guarantee field order) — `#[repr(C)]` plus an
//! explicit version handshake is the standard escape from that instability, rather than
//! `abi_stable`'s heavier compiler-version-independent machinery, which this spike doesn't
//! need because Nicti controls the toolchain on both sides of its own plugin dylibs.

use std::path::Path;

/// A stage's C-ABI surface. `abi_version` MUST be the first field: it's read and checked
/// before any other field of this struct (including `process`) is trusted.
#[repr(C)]
pub struct StageVTable {
    pub abi_version: u32,
    pub process: extern "C" fn(input: f32, param: f32) -> f32,
}

/// Bump when `StageVTable`'s shape or calling convention changes incompatibly.
pub const CLAW_ABI_VERSION: u32 = 1;

/// The symbol every stage dylib must export: `extern "C" fn() -> *const StageVTable`.
pub const VTABLE_SYMBOL: &[u8] = b"claw_stage_vtable\0";

#[derive(Debug)]
pub enum DylibError {
    Load(libloading::Error),
    MissingSymbol(libloading::Error),
    AbiMismatch { expected: u32, found: u32 },
}

impl std::fmt::Display for DylibError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DylibError::Load(e) => write!(f, "failed to load dylib: {e}"),
            DylibError::MissingSymbol(e) => write!(f, "missing `{VTABLE_SYMBOL:?}` symbol: {e}"),
            DylibError::AbiMismatch { expected, found } => write!(
                f,
                "ABI version mismatch: host expects {expected}, dylib reports {found}"
            ),
        }
    }
}

impl std::error::Error for DylibError {}

/// A stage implemented in a dynamically-loaded native library, loaded through a checked
/// C-ABI vtable rather than linked in at build time.
#[derive(Debug)]
pub struct DylibStage {
    // Kept alive for the struct's lifetime: `vtable` points into memory this library owns,
    // and unloading it would dangle that pointer.
    _lib: libloading::Library,
    vtable: *const StageVTable,
}

// SAFETY: the loaded library is never unloaded before `DylibStage` is dropped (it's held in
// `_lib`), so `vtable` stays valid for `DylibStage`'s whole lifetime; the vtable's own
// `process` fn pointer is `extern "C"`, a plain function pointer with no captured state, so
// calling it from any thread is sound.
unsafe impl Send for DylibStage {}
unsafe impl Sync for DylibStage {}

impl DylibStage {
    /// Loads the dylib at `path` and validates its ABI version before returning.
    ///
    /// # Safety
    /// The caller must trust `path` to point at a library that, if it exports
    /// [`VTABLE_SYMBOL`] at all, upholds this module's `StageVTable` contract — loading an
    /// arbitrary dylib and calling into it is inherently unsafe (`libloading`'s own
    /// constraint), independent of the ABI-version check this function additionally performs.
    pub unsafe fn load(path: &Path) -> Result<Self, DylibError> {
        let lib = libloading::Library::new(path).map_err(DylibError::Load)?;
        let get_vtable: libloading::Symbol<unsafe extern "C" fn() -> *const StageVTable> =
            lib.get(VTABLE_SYMBOL).map_err(DylibError::MissingSymbol)?;
        let vtable = get_vtable();
        let found = (*vtable).abi_version;
        if found != CLAW_ABI_VERSION {
            return Err(DylibError::AbiMismatch {
                expected: CLAW_ABI_VERSION,
                found,
            });
        }
        Ok(Self { _lib: lib, vtable })
    }

    pub fn process(&self, input: f32, param: f32) -> f32 {
        // SAFETY: `vtable` was validated at load time and outlives this call (see the
        // `Send`/`Sync` impl comment above).
        unsafe { ((*self.vtable).process)(input, param) }
    }
}
