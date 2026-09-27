//! Throwaway spike for #53 (ADR-0053): an MLP predicting all eight PV2012 develop sliders
//! (Exposure2012/Contrast2012/Highlights2012/Shadows2012/Whites2012/Blacks2012/Saturation/
//! Vibrance) from a downsampled embedded-JPEG preview, trained on the user's own real LRC edit
//! history (picked/rated keepers with a real non-default edit). See each module's doc comment for
//! its piece: `catalog` (keeper rows + develop-settings parsing from a `.lrcat`), `sample`
//! (deterministic per-folder-capped sampling), `split` (event and temporal train/holdout splits),
//! `features` (embedded-JPEG extraction, histogram feature vector, 32x32 thumbnail tensor),
//! `sliders` (the eight targets), `histogram` (percentile lookup), `baseline` (B0: predict the
//! training mean), `fit` (B1: ridge regression), `mlp` (M1/M2: candle MLP), `eval` (per-slider
//! MAE/p95/bias + the aggregate metric ADR-0053's decision rule reads).

pub mod baseline;
pub mod catalog;
pub mod dataset;
pub mod eval;
pub mod features;
pub mod fit;
pub mod histogram;
pub mod mlp;
pub mod sample;
pub mod sliders;
pub mod split;
