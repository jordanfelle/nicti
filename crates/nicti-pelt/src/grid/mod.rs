//! The virtualized library grid (#30): an ordered id snapshot from the catalog, thumbnails
//! decoded from stored T0 previews on Pounce's CPU lane, and only the visible cells (plus a little
//! overscan) ever drawn or textured -- so scrolling stays smooth and memory stays flat from 10k to
//! 1M assets.
//!
//! - `layout`: pure geometry (columns, rows, visible index range, batching) -- unit-tested alone.
//! - `jobs`: `SnapshotJob` (one index-ordered `hunt_ids` scan) and `ThumbBatchJob` (batched T0
//!   decode + downsize).
//! - `selection`: the multi-selection (sorted index ranges), for culling's select-all-then-delete.
//! - `session`: `GridSession`, the state machine tying them together.
//! - `view`: the egui drawing and keyboard/mouse handling on top of a session.

pub mod jobs;
pub mod layout;
pub mod selection;
pub mod session;
pub mod view;

pub use session::{GridSession, DEFAULT_SORT};
pub use view::ViewState;
