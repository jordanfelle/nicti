//! Throwaway spike for #44 (ADR-0044): Tapetum's stage-cached render graph. Not a production
//! crate -- see `CLAUDE.md`'s package-map note on `spikes/*`. `crates/nicti-render` is Tapetum's
//! real future home (#45); this spike exists only to validate the design (the cache-key/
//! invalidation model in `graph.rs`, the byte-budgeted cache tiers in `cache.rs`, crop-as-geometry
//! in `geometry.rs`, the mask-refine port in `refine.rs`, and the bake-scheduling contract in
//! `prefetch.rs`/`sim.rs`) against real numbers where a real GPU is available (`gpu.rs`).
//!
//! Name: a cat loaf -- the tucked-paws resting pose -- doubles as "baked", the whole idea behind
//! Tapetum reusing baked stage output instead of recomputing it.

pub mod cache;
pub mod cost_model;
pub mod geometry;
pub mod gpu;
pub mod graph;
pub mod hash;
pub mod prefetch;
pub mod refine;
pub mod sim;
