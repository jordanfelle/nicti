//! Throwaway spike for #56 (ADR-0056): export stack research. Not a production crate -- see
//! `CLAUDE.md`'s package-map note on `spikes/*`. The signature this spike settles on informs
//! #57's real `Exporter` implementation; `crates/nicti-preen` itself is untouched by this pass.

pub mod encode;
pub mod gpu_resize;
pub mod icc;
pub mod metadata;
pub mod resize;
pub mod watermark;
