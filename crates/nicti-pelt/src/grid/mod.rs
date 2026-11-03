//! The virtualized library grid (#30): an ordered id snapshot from the catalog, thumbnails
//! decoded from stored T0 previews on Pounce's CPU lane, and only the visible cells (plus a little
//! overscan) ever drawn or textured -- so scrolling stays smooth and memory stays flat from 10k to
//! 1M assets.
//!
//! - `layout`: pure geometry (columns, rows, visible index range, batching) -- unit-tested alone.
//! - `justified`: the pure core of justified rows with date-group headers (#550, ADR-0437 section 3):
//!   a streamed row-offset index, lookups, navigation and the rebuild gate.
//! - `layout_build`: `LayoutJob`, the Pounce job that streams the catalog's sizes/orientations/days
//!   into a `RowIndex` (#656); `view` draws it when the justified layout is on.
//! - `jobs`: `SnapshotJob` (one index-ordered `hunt_ids` scan) and `ThumbBatchJob` (batched T0
//!   decode + downsize).
//! - `selection`: the multi-selection (sorted index ranges), for culling's select-all-then-delete.
//! - `session`: `GridSession`, the state machine tying them together.
//! - `view`: the egui drawing and keyboard/mouse handling on top of a session.

pub mod jobs;
pub mod justified;
pub mod layout;
pub mod layout_build;
pub mod selection;
pub mod session;
pub mod view;

pub use session::{GridSession, DEFAULT_SORT};
pub use view::ViewState;
