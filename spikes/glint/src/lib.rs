//! Throwaway spike for #16 (ADR-0005): measured comparison of `wgpu` (WGSL) against raw CUDA
//! (via NVRTC) for the render/pixel-stage GPU compute API decision. Correctness, feature/limit
//! availability, throughput, CPU dispatch overhead, and host<->device interop cost — see
//! `docs/adr/0005-gpu-compute-api.md` for how each test here backs a claim in that ADR.
//! Not a production crate — see `CLAUDE.md`'s package-map note on `spikes/*`.

pub mod cpu_reference;
pub mod cuda;
pub mod gpu;
pub mod stats;
