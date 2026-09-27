//! #40/ADR-0025's demosaic+denoise comparison harness: alignment, full-reference metrics (via
//! `nicti-prowl::metrics`), and a fixed display encoding applied uniformly to every candidate
//! (classic demosaic, Path A/B AI denoise, LRC's own export) so quality differences measure the
//! candidates, not a second color pipeline. See `docs/research/rods-demosaic-denoise.md`.

pub mod ai;
pub mod align;
pub mod display;
pub mod linear_input;
