//! Export (#57): render the selected photos with their edits and write them to disk.
//!
//! - [`jobs`] -- the run: planning, then decode -> render -> encode as chained Pounce jobs.
//! - [`sink`] -- collects tiled render output into one linear RGB buffer.
//!
//! The format-independent engine (settings, filename tokens, resize/color/watermark/metadata,
//! encoders, collision-safe writes) lives in `nicti-preen`; this module is the render + job wiring.

pub mod dialog;
pub mod jobs;
pub mod presets;
pub mod sink;

pub use dialog::ExportUi;
pub use jobs::{facts_for, ExportEnv};
