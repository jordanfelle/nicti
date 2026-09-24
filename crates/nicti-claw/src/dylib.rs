//! C-ABI dylib loading with an explicit ABI-version handshake, checked before any other call
//! crosses the boundary (ADR-0004 §4). This is the isolation mechanism ADR-0003 requires for an
//! LGPL native dependency (e.g. a future `rawler`/`lensfun-rs`-style crate) that can't cleanly
//! satisfy LGPL's dynamic-linking safe harbor when statically linked in.
//!
//! Rust's own struct/vtable layout is not guaranteed stable across separate compilations
//! (doc.rust-lang.org/reference/type-layout.html: "Type layout can be changed with each
//! compilation... we only document what is guaranteed today", and the default `Rust`
//! representation explicitly does not guarantee field order) — `#[repr(C)]` plus an explicit
//! version handshake is the standard escape from that instability, rather than `abi_stable`'s
//! heavier compiler-version-independent machinery, which Nicti doesn't need because it controls
//! the toolchain on both sides of its own plugin dylibs.

use std::path::Path;

/// A dylib module's C-ABI surface. Implementors MUST be `#[repr(C)]` with `abi_version: u32` as
/// their first field: it's read and checked before any other field is trusted. Implementing
/// this trait is unsafe because that layout contract can't be checked by the compiler.
///
/// # Safety
/// The implementing type must be `#[repr(C)]` with `abi_version: u32` as its first field, and
/// `SYMBOL`/`ABI_VERSION` must accurately describe the exported-function name and the version
/// this host build expects.
pub unsafe trait VTable: Sized {
    /// The symbol every module dylib must export: `extern "C" fn() -> *const Self`.
    const SYMBOL: &'static [u8];
    /// Bump when this vtable's shape or calling convention changes incompatibly.
    const ABI_VERSION: u32;
}

/// Reads a `VTable` implementor's first field. Every `VTable` is `#[repr(C)]` with
/// `abi_version: u32` first (the trait's safety contract), so this is a valid, well-defined
/// read regardless of the rest of the struct's shape.
fn read_abi_version<V: VTable>(vtable: *const V) -> u32 {
    // SAFETY: `vtable` is non-null (checked by the caller) and points at a `V`, whose first
    // field is `abi_version: u32` per `VTable`'s safety contract — reading it through a `*const
    // u32` cast is sound regardless of the rest of `V`'s layout.
    unsafe { *(vtable as *const u32) }
}

#[derive(Debug)]
pub enum DylibError {
    Load(libloading::Error),
    MissingSymbol(libloading::Error),
    NullVtable,
    AbiMismatch { expected: u32, found: u32 },
}

impl std::fmt::Display for DylibError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DylibError::Load(e) => write!(f, "failed to load dylib: {e}"),
            DylibError::MissingSymbol(e) => write!(f, "missing vtable symbol: {e}"),
            DylibError::NullVtable => write!(f, "vtable symbol returned a null pointer"),
            DylibError::AbiMismatch { expected, found } => write!(
                f,
                "ABI version mismatch: host expects {expected}, dylib reports {found}"
            ),
        }
    }
}

impl std::error::Error for DylibError {}

/// A module implemented in a dynamically-loaded native library, loaded through a checked C-ABI
/// vtable rather than linked in at build time.
pub struct DylibModule<V: VTable> {
    // Kept alive for the struct's lifetime: `vtable` points into memory this library owns, and
    // unloading it would dangle that pointer.
    _lib: libloading::Library,
    vtable: *const V,
}

// Written by hand rather than `#[derive(Debug)]`: a derive would require `V: Debug`, but `V`'s
// only guaranteed field is `abi_version` (the rest is opaque C-ABI function pointers), so this
// prints the address and version only, regardless of `V`'s own Debug status.
impl<V: VTable> std::fmt::Debug for DylibModule<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DylibModule")
            .field("vtable", &self.vtable)
            .field("abi_version", &read_abi_version(self.vtable))
            .finish()
    }
}

// SAFETY: the loaded library is never unloaded before `DylibModule` is dropped (it's held in
// `_lib`), so `vtable` stays valid for `DylibModule`'s whole lifetime. `V: Send + Sync` is
// required here (not just `V: VTable`): `VTable`'s own safety contract only constrains the
// struct's layout (`#[repr(C)]`, `abi_version` first), not the thread-safety of whatever else it
// holds, so without this bound a `V` with e.g. an unsynchronized interior-mutable field would
// still compile as `Send + Sync` and let `vtable()` hand out a racy `&V` across threads.
unsafe impl<V: VTable + Send + Sync> Send for DylibModule<V> {}
unsafe impl<V: VTable + Send + Sync> Sync for DylibModule<V> {}

impl<V: VTable> DylibModule<V> {
    /// Loads the dylib at `path` and validates its ABI version before returning.
    ///
    /// # Safety
    /// The caller must trust `path` to point at a library that, if it exports `V::SYMBOL` at
    /// all, upholds `V`'s `#[repr(C)]` layout contract — loading an arbitrary dylib and calling
    /// into it is inherently unsafe (`libloading`'s own constraint), independent of the
    /// ABI-version check this function additionally performs.
    pub unsafe fn load(path: &Path) -> Result<Self, DylibError> {
        let lib = libloading::Library::new(path).map_err(DylibError::Load)?;
        let get_vtable: libloading::Symbol<unsafe extern "C" fn() -> *const V> =
            lib.get(V::SYMBOL).map_err(DylibError::MissingSymbol)?;
        let vtable = get_vtable();
        if vtable.is_null() {
            return Err(DylibError::NullVtable);
        }
        let found = read_abi_version(vtable);
        if found != V::ABI_VERSION {
            return Err(DylibError::AbiMismatch {
                expected: V::ABI_VERSION,
                found,
            });
        }
        Ok(Self { _lib: lib, vtable })
    }

    /// The validated vtable. Callers invoke its function-pointer fields directly.
    pub fn vtable(&self) -> &V {
        // SAFETY: `vtable` was validated (non-null, correct ABI version) at load time in
        // `load()` and outlives this call (see the `Send`/`Sync` impl comment above).
        unsafe { &*self.vtable }
    }
}
