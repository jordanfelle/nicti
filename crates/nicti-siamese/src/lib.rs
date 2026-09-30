//! AI masks (#49, ADR-0048/ADR-0049), promoted from `spikes/siamese`.
//!
//! Layering, bottom to top:
//! - [`neutral`] -- the one image every model runs on: the photo shrunk to model size and mapped
//!   through `nicti_groom`'s `SpaceMap` to display-referred sRGB. It is a function of the *photo*
//!   only (never a slider), which is what lets a mask survive any tone edit unchanged.
//! - [`birefnet`] / [`sky`] -- the two [`nicti_stalk::SegmentationProvider`]s: BiRefNet over
//!   `ort`/`load-dynamic` for subject (and, inverted, background), and the interim non-AI sky
//!   heuristic. [`providers`] registers them and holds the *data table* that says which model serves
//!   which target -- so choosing a different or newer model is a different recipe, not new code.
//! - [`backend`] -- resolves a recipe to a provider, loads it once (verified, lazily, retried after a
//!   failed load) and runs it on the cached neutral image.
//! - [`job`] -- [`job::MaskBakeJob`], one bake on Pounce's GPU lane.
//!
//! Everything model-independent is unit-tested with fake providers; the ONNX wrapper is exercised
//! against the real weights by the `#[ignore]`d test in `tests/real_models.rs`. Weights are never
//! bundled or fetched from here (ADR-0218): paths come from `nicti_stalk::models`, installed only
//! by an explicit user action.

pub mod backend;
pub mod birefnet;
pub mod job;
pub mod neutral;
pub mod providers;
pub mod sky;
