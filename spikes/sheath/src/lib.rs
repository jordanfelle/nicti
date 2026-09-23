//! Throwaway spike for #19 (ADR-0004): proves three claims the ADR makes about Nicti's
//! module/plugin architecture (codename Claw) — a lazy module registry, a checked C-ABI
//! dylib boundary for LGPL isolation, and (in `tests/wasm_vs_native.rs`) a measured
//! comparison of a WASM guest pixel kernel against the same kernel in native Rust.
//! Not a production crate — see `CLAUDE.md`'s package-map note on `spikes/*`.

pub mod dylib;
pub mod registry;
