//! Throwaway spike for #99 (ADR-0099): classic (non-AI) auto-tone. See each module's doc comment
//! for its piece: `input` (retina dump-linear reader), `render` (fixed default-render color
//! treatment for the auto-tone histogram), `histogram` (percentile lookup), `sliders` (the six
//! PV2012 targets), `heuristic` (candidate A), `fit` (candidate B), `truth` (LRC ground truth from
//! a `.lrcat`), `eval` (per-slider MAE/p95/bias, the primary accuracy metric).

pub mod eval;
pub mod fit;
pub mod heuristic;
pub mod histogram;
pub mod input;
pub mod render;
pub mod sliders;
pub mod truth;
