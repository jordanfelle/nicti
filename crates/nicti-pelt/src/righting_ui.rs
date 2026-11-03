//! Thumbnail-rotation repair (#606): runs `nicti_lair::pounce_jobs::RightingJob` once per catalog
//! root at startup (a `<catalog>.migrations.json` records the roots already righted, so it never
//! re-reads every RAW on each start) and on the Library's "Repair thumbnails" action (which ignores
//! the marker). The job also fills `asset.orientation` for rows older than schema v11. State,
//! marker file and summary text live here; the pure parts are unit-tested.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nicti_lair::pounce_jobs::{ReportSlot, RightingJob};
use nicti_lair::righting::RightingRun;
use nicti_lair::CatalogStore;
use nicti_pounce::Pounce;
use serde::{Deserialize, Serialize};

/// `<catalog file name>.migrations.json`, next to the catalog.
pub fn marker_file_for(catalog_path: &Path) -> PathBuf {
    let mut name = catalog_path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| "nicti".into());
    name.push(".migrations.json");
    catalog_path.with_file_name(name)
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Markers {
    /// Root ids whose thumbnails were righted and orientations read with nothing left undone.
    #[serde(default)]
    righted_roots: BTreeSet<i64>,
}

fn load_markers(catalog_path: &Path) -> Markers {
    std::fs::read_to_string(marker_file_for(catalog_path))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// The roots already righted (empty when the file is absent or corrupt: a bad file only costs a
/// repeat pass).
pub fn righted_roots(catalog_path: &Path) -> std::collections::HashSet<i64> {
    load_markers(catalog_path)
        .righted_roots
        .into_iter()
        .collect()
}

/// Adds `roots` to the marker, atomically (temp file + rename).
pub fn mark_righted(catalog_path: &Path, roots: &[i64]) -> std::io::Result<()> {
    if roots.is_empty() {
        return Ok(());
    }
    let mut markers = load_markers(catalog_path);
    markers.righted_roots.extend(roots.iter().copied());
    let path = marker_file_for(catalog_path);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(
        &tmp,
        serde_json::to_string(&markers).map_err(std::io::Error::other)?,
    )?;
    std::fs::rename(&tmp, &path)
}

/// One line for a finished run.
pub fn summarize(run: &RightingRun) -> String {
    let t = &run.total;
    let mut s = format!(
        "Thumbnail repair{}: {} photo(s) checked, {} thumbnail(s) rotated, {} orientation(s) read",
        if run.cancelled { " (stopped)" } else { "" },
        t.examined,
        t.fixed_blobs + t.fixed_sidecars,
        t.orientation_filled,
    );
    if t.root_offline {
        s.push_str("; a folder was offline and will be retried");
    }
    if !t.failed.is_empty() {
        s.push_str(&format!("; {} could not be fixed", t.failed.len()));
    }
    if t.stale_file > 0 {
        s.push_str(&format!(
            "; {} changed on disk (sync, then repair again)",
            t.stale_file
        ));
    }
    s
}

/// The status line after a finished run: a startup pass that had nothing to do and nothing to
/// report stays silent.
fn status_after(run: &RightingRun, quiet: bool) -> Option<String> {
    let t = &run.total;
    let nothing = t.examined == 0 && !t.root_offline && t.failed.is_empty() && !run.cancelled;
    if quiet && nothing {
        None
    } else {
        Some(summarize(run))
    }
}

#[derive(Default)]
pub struct RightingUi {
    /// The running pass was started at launch (not by the user's click).
    quiet: bool,
    pending: Option<ReportSlot<RightingRun>>,
    /// Running / last summary / why a request was refused.
    pub status: Option<String>,
}

impl RightingUi {
    pub fn is_running(&self) -> bool {
        self.pending.is_some()
    }

    /// Submits a pass over every root not in `skip`. No-op while one is already running.
    pub fn start(
        &mut self,
        store: Arc<dyn CatalogStore + Send + Sync>,
        pounce: &Pounce,
        skip: &std::collections::HashSet<i64>,
        quiet: bool,
    ) {
        if self.pending.is_some() {
            return;
        }
        match RightingJob::new(store, skip) {
            Ok((job, slot)) => {
                pounce.submit(Box::new(job));
                self.pending = Some(slot);
                self.quiet = quiet;
                self.status = (!quiet).then(|| "Repairing thumbnail rotation\u{2026}".into());
            }
            Err(e) => self.status = Some(format!("Thumbnail repair could not start: {e}")),
        }
    }

    /// Folds in a finished run: records the roots that completed and keeps the summary. Returns
    /// `true` the frame it finishes.
    pub fn poll(&mut self, catalog_path: &Path) -> bool {
        let Some(slot) = self.pending.clone() else {
            return false;
        };
        let Some(run) = slot.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return false;
        };
        self.pending = None;
        let mut note = status_after(&run, self.quiet);
        if let Err(e) = mark_righted(catalog_path, &run.done_roots) {
            let msg = format!("could not save thumbnail repair progress: {e}");
            note = Some(match note {
                Some(n) => format!("{n} ({msg})"),
                None => msg,
            });
        }
        self.status = note;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_lair::righting::RightingReport;

    #[test]
    fn marker_file_sits_beside_the_catalog() {
        assert!(marker_file_for(Path::new("/x/cat.db")).ends_with("cat.db.migrations.json"));
    }

    #[test]
    fn marked_roots_accumulate_and_survive_a_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let cat = dir.path().join("cat.db");
        assert!(righted_roots(&cat).is_empty());
        mark_righted(&cat, &[1, 2]).unwrap();
        mark_righted(&cat, &[2, 5]).unwrap();
        mark_righted(&cat, &[]).unwrap();
        let got: BTreeSet<i64> = righted_roots(&cat).into_iter().collect();
        assert_eq!(got, BTreeSet::from([1, 2, 5]));
        std::fs::write(marker_file_for(&cat), "not json").unwrap();
        assert!(righted_roots(&cat).is_empty());
    }

    #[test]
    fn a_quiet_startup_pass_with_nothing_to_do_says_nothing() {
        let run = RightingRun::default();
        assert_eq!(status_after(&run, true), None);
        assert!(status_after(&run, false).is_some());
        let mut busy = RightingRun::default();
        busy.total.examined = 3;
        assert!(status_after(&busy, true).is_some());
        let mut offline = RightingRun::default();
        offline.total.root_offline = true;
        assert!(status_after(&offline, true).is_some());
    }

    #[test]
    fn the_summary_names_what_needs_attention() {
        let mut run = RightingRun {
            total: RightingReport {
                examined: 10,
                fixed_blobs: 2,
                orientation_filled: 7,
                root_offline: true,
                stale_file: 1,
                failed: vec![(PathBuf::from("a"), "x".into())],
                ..Default::default()
            },
            ..Default::default()
        };
        let s = summarize(&run);
        for want in [
            "10 photo(s)",
            "2 thumbnail(s)",
            "7 orientation(s)",
            "offline",
            "1 could not",
            "changed on disk",
        ] {
            assert!(s.contains(want), "{s}");
        }
        run.cancelled = true;
        assert!(summarize(&run).contains("(stopped)"));
    }
}
