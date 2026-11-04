//! Apply a GPX track log to catalogued assets (#536): the engine behind "Geotag from GPX".
//!
//! For each asset the capture time (`asset.captured_at`, EXIF `DateTimeOriginal` text) is matched
//! against the log with `nicti_scent::gpx` (camera UTC offset + drift + max gap from
//! [`GeotagOptions`]); a found position is stored in the catalog (`asset_gps`, visible to any Info
//! display) and then pushed into the XMP through the ordinary sidecar write-back, so the usual
//! conflict/review rules apply -- a sidecar changed under us holds the write for review instead of
//! being clobbered. With auto-write off the position is stored and the asset marked dirty, exactly
//! like a marker edit.
//!
//! Known limitation: a log with overlapping tracks from two devices is interpolated in global time
//! order (`Tracklog::locate`), so a photo may be placed on the wrong device's track.

use std::path::PathBuf;

use nicti_scent::gpx::{camera_time_to_unix, GeotagOptions, Tracklog};

use crate::scent_sync::{self, SyncOutcome};
use crate::CatalogStore;

/// How a batch went. `tagged + kept_existing + no_position + no_capture_time + failed` equals the
/// number of assets passed in.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GeotagReport {
    /// A position was stored (and written, or queued dirty, per `auto_write`).
    pub tagged: usize,
    /// Already had a position and `overwrite` was off.
    pub kept_existing: usize,
    /// No track position within the max gap of the capture time.
    pub no_position: usize,
    /// The asset has no readable capture time.
    pub no_capture_time: usize,
    /// Positions stored whose sidecar write was held for review (a subset of `tagged`).
    pub held_for_review: usize,
    /// Assets that errored (catalog/sidecar I/O), with the reason.
    pub failed: Vec<(i64, String)>,
    /// The run was cancelled (or its job dropped) before every asset was handled; the counts
    /// cover only what it got through.
    pub stopped: bool,
    /// The GPX file could not be read or parsed (nothing was tagged), with the reason.
    pub gpx_error: Option<String>,
}

impl GeotagReport {
    /// Adds `other`'s counts to this report (a job runs the batch in chunks).
    pub fn merge(&mut self, other: GeotagReport) {
        self.tagged += other.tagged;
        self.kept_existing += other.kept_existing;
        self.no_position += other.no_position;
        self.no_capture_time += other.no_capture_time;
        self.held_for_review += other.held_for_review;
        self.failed.extend(other.failed);
        self.stopped |= other.stopped;
        if self.gpx_error.is_none() {
            self.gpx_error = other.gpx_error;
        }
    }

    /// Assets accounted for so far.
    pub fn total(&self) -> usize {
        self.tagged
            + self.kept_existing
            + self.no_position
            + self.no_capture_time
            + self.failed.len()
    }
}

/// Geotags `items` (`(asset_id, path to the asset's file)`) from `log`. `overwrite` replaces an
/// existing position; off, those are counted in `kept_existing`. `auto_write` writes the XMP now,
/// else the asset is only marked dirty for a later flush. Never panics on a bad asset: it is
/// reported in `failed` and the rest continue.
pub fn geotag_assets(
    store: &dyn CatalogStore,
    items: &[(i64, PathBuf)],
    log: &Tracklog,
    opts: &GeotagOptions,
    overwrite: bool,
    auto_write: bool,
    now_ms: i64,
) -> GeotagReport {
    let mut report = GeotagReport::default();
    for (id, path) in items {
        match geotag_one(store, *id, path, log, opts, overwrite, auto_write, now_ms) {
            Ok(Outcome::Tagged { held }) => {
                report.tagged += 1;
                report.held_for_review += usize::from(held);
            }
            Ok(Outcome::Kept) => report.kept_existing += 1,
            Ok(Outcome::NoPosition) => report.no_position += 1,
            Ok(Outcome::NoCaptureTime) => report.no_capture_time += 1,
            Err(e) => report.failed.push((*id, e)),
        }
    }
    report
}

enum Outcome {
    Tagged { held: bool },
    Kept,
    NoPosition,
    NoCaptureTime,
}

#[allow(clippy::too_many_arguments)]
fn geotag_one(
    store: &dyn CatalogStore,
    id: i64,
    path: &std::path::Path,
    log: &Tracklog,
    opts: &GeotagOptions,
    overwrite: bool,
    auto_write: bool,
    now_ms: i64,
) -> Result<Outcome, String> {
    let asset = store
        .get_asset(id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("asset {id} not found"))?;
    let Some(t) = asset
        .captured_at
        .as_deref()
        .and_then(|c| camera_time_to_unix(c, opts.camera_utc_offset_minutes, opts.drift_seconds))
    else {
        return Ok(Outcome::NoCaptureTime);
    };
    if !overwrite
        && store
            .get_gps(&[id])
            .map_err(|e| e.to_string())?
            .contains_key(&id)
    {
        return Ok(Outcome::Kept);
    }
    let Some((pos, _how)) = log.locate(t, opts.max_gap_seconds) else {
        return Ok(Outcome::NoPosition);
    };
    // Never create a sidecar for (or write into) a file that isn't there: an offline volume or a
    // moved file is a failure, not something to half-apply.
    if !path.is_file() {
        return Err(format!("{} is not reachable", path.display()));
    }
    store.set_gps(id, Some(pos)).map_err(|e| e.to_string())?;
    // Mark the catalog newer *before* the write, so a write that fails or is held leaves the
    // position pending (a later sync pushes it) instead of stranded; a successful write clears it.
    scent_sync::mark_catalog_dirty(store, id, path, now_ms).map_err(|e| e.to_string())?;
    if auto_write {
        let out = scent_sync::write_sidecar(store, id, path, now_ms).map_err(|e| e.to_string())?;
        Ok(Outcome::Tagged {
            held: out == SyncOutcome::NeedsReview,
        })
    } else {
        Ok(Outcome::Tagged { held: false })
    }
}
