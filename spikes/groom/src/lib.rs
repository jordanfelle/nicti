//! Throwaway spike for #50 / `docs/adr/0007-healing-and-removal.md`.
//!
//! Covers two independent research threads gated on the same ADR:
//!
//! - **Classic clone/heal** (`cpu_reference`, `gpu`): a CPU reference implementation of clone
//!   stamp, gradient-domain (Poisson) spot healing, and a cheap SSD-over-a-ring auto-source-pick,
//!   plus a `wgpu` compute-shader Jacobi solver for the same Poisson blend, checked against the
//!   CPU reference in `tests/correctness.rs`.
//! - **AI removal scaffolding** (`ai`, `compositing`): thin `ort`/`load-dynamic` wrapper structs
//!   for a future MobileSAM selector and LaMa inpainter — no real ONNX weights exist in this
//!   sandbox, so these prove the loading/error-handling shape only — plus pure, model-independent
//!   crop/resize/feather-blend compositing math for LaMa-style inpainting.
//!
//! `spot` defines the `HealStage`/`Spot` edit-model representation per ADR-0002's
//! `StageEntry { schema_version, params }` pattern, with a pawprint-style `cache_key()`.
//!
//! Not a production crate — see `CLAUDE.md`'s package-map note on `spikes/*`.

pub mod ai;
pub mod compositing;
pub mod cpu_reference;
pub mod gpu;
pub mod spot;
