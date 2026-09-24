//! Toolkit-agnostic shared spike support for #68 (ADR-0006): the synthetic catalog/loupe/
//! live-chain fixtures and virtualized-grid math that `spikes/pelt-egui`, `spikes/pelt-iced`,
//! `spikes/pelt-gpui`, and `spikes/pelt-slint` each build an identical UI against, so the four
//! candidates are measured on the same workload. Deliberately holds no `wgpu` types: each
//! toolkit crate may pin a different `wgpu` version (see ADR-0006's gate on this), and this crate
//! is a dependency of all four. Not a production crate -- see `CLAUDE.md`'s package-map note on
//! `spikes/*`.

pub mod config;
pub mod live_chain;
pub mod loupe;
pub mod thumbnails;
pub mod virtualize;
