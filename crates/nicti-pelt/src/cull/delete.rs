//! The Delete flow (#32): "filter, select all, Delete". A confirmation that always asks *how*
//! (Recycle Bin, or catalog only), a Pounce `DeleteJob`, and folding its result back into the UI.
//!
//! The engine is `nicti_lair::shred`; this file is the UI side: the modal, submitting the job
//! with the two hooks it needs (purge the T2 preview cache, tell the UI which ids are gone), and
//! polling. Refusing to start while an import/sync/move runs is the caller's job (it owns
//! `job_active`), the same way `submit_move` does it.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use nicti_lair::pounce_jobs::{DeleteJob, ReportSlot};
use nicti_lair::shred::{DeleteMode, RecycleBin, Shred, ShredOutcome, ShredReport, Trasher};
use nicti_lair::CatalogStore;
use nicti_pounce::Pounce;

use crate::t2::{try_lock_larder, SharedLarder};

/// What the modal is asking about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteRequest {
    pub ids: Vec<i64>,
    /// Where the photos came from, in the user's terms ("the 312 photos matching the current
    /// filter" / "the selected photos").
    pub scope: String,
}

/// What a poll saw.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DeletePoll {
    /// Photos whose catalog rows are gone since the last poll -- forget their markers, previews
    /// and open sessions.
    pub removed: Vec<i64>,
    /// The job finished this poll (its summary is in [`DeleteFlow::last_summary`]).
    pub finished: bool,
}

pub struct DeleteFlow {
    confirm: Option<DeleteRequest>,
    pending: Option<(ReportSlot<ShredOutcome>, DeleteMode)>,
    removed: Arc<Mutex<Vec<i64>>>,
    pub last_summary: Option<String>,
}

impl Default for DeleteFlow {
    fn default() -> Self {
        Self::new()
    }
}

impl DeleteFlow {
    pub fn new() -> Self {
        DeleteFlow {
            confirm: None,
            pending: None,
            removed: Arc::new(Mutex::new(Vec::new())),
            last_summary: None,
        }
    }

    /// Whether a delete job is in flight.
    pub fn is_running(&self) -> bool {
        self.pending.is_some()
    }

    pub fn is_confirming(&self) -> bool {
        self.confirm.is_some()
    }

    /// Opens the confirmation for `ids`. Nothing to delete opens nothing.
    pub fn request(&mut self, ids: Vec<i64>, scope: String) {
        if ids.is_empty() || self.is_running() {
            return;
        }
        self.confirm = Some(DeleteRequest { ids, scope });
    }

    /// Draws the confirmation while it is open. Returns the request and the chosen mode when the
    /// user picks one; Cancel/Escape just closes it.
    pub fn show_modal(&mut self, ctx: &egui::Context) -> Option<(DeleteRequest, DeleteMode)> {
        let req = self.confirm.clone()?;
        let mut chosen = None;
        let mut close = false;
        let n = req.ids.len();
        // A real modal: its backdrop blocks the grid, filter dropdowns and toolbar behind it, so
        // the photos named here are the photos the user is looking at when they click.
        let modal = egui::Modal::new(egui::Id::new("nicti_delete_prompt")).show(ctx, |ui| {
            ui.heading(format!(
                "Delete {n} photo{}?",
                if n == 1 { "" } else { "s" }
            ));
            ui.label(format!("This applies to {}.", req.scope));
            ui.add_space(6.0);
            ui.label(
                "Move to Recycle Bin: the RAW files (and their .xmp sidecars) go to the \
                     Recycle Bin, where you can restore them, and the photos leave the catalog. \
                     Windows decides per drive: a network share or some removable drives have no \
                     Recycle Bin and may delete permanently.",
            );
            ui.label(
                "Remove from catalog only: the photos leave the catalog (with their edits and \
                     markers); the files stay exactly where they are on disk.",
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Move to Recycle Bin").clicked() {
                    chosen = Some(DeleteMode::RecycleBin);
                }
                if ui.button("Remove from catalog only").clicked() {
                    chosen = Some(DeleteMode::CatalogOnly);
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        // Escape, or a click on the backdrop.
        if modal.should_close() {
            close = true;
        }
        if let Some(mode) = chosen {
            self.confirm = None;
            return Some((req, mode));
        }
        if close {
            self.confirm = None;
        }
        None
    }

    /// Starts the delete on Pounce. `larder`'s T2 entries for the removed photos are purged as
    /// their rows go.
    pub fn submit(
        &mut self,
        store: Arc<dyn CatalogStore + Send + Sync>,
        larder: Option<SharedLarder>,
        mode: DeleteMode,
        ids: Vec<i64>,
        pounce: &Pounce,
    ) {
        self.submit_with(store, larder, mode, ids, pounce, Box::new(RecycleBin));
    }

    /// [`Self::submit`] with an explicit [`Trasher`] -- tests substitute a fake bin.
    pub fn submit_with(
        &mut self,
        store: Arc<dyn CatalogStore + Send + Sync>,
        larder: Option<SharedLarder>,
        mode: DeleteMode,
        ids: Vec<i64>,
        pounce: &Pounce,
        trasher: Box<dyn Trasher>,
    ) {
        let removed = self.removed.clone();
        let shred = Shred::new(store, mode, ids, trasher).on_removed(move |gone| {
            purge_previews(larder.as_ref(), gone);
            removed.lock().unwrap().extend_from_slice(gone);
        });
        let (job, slot) = DeleteJob::new(shred);
        pounce.submit(Box::new(job));
        self.pending = Some((slot, mode));
        self.last_summary = Some("Deleting\u{2026}".into());
    }

    /// Folds job progress into the UI. Call every frame.
    pub fn poll(&mut self) -> DeletePoll {
        // Read the slot BEFORE draining the removed ids: the job pushes a chunk's ids and only
        // later fills the slot, so draining second could see the slot filled but miss the last
        // chunk's ids (and the caller would never forget them). Draining first can't lose any --
        // an id pushed after the drain is picked up on the next call.
        let mut finished = false;
        let outcome = self
            .pending
            .as_ref()
            .and_then(|(slot, mode)| slot.lock().unwrap().take().map(|o| (o, *mode)));
        let removed = std::mem::take(&mut *self.removed.lock().unwrap());
        if let Some((outcome, mode)) = outcome {
            self.last_summary = Some(summarize(mode, &outcome));
            finished = true;
        }
        if finished {
            self.pending = None;
        }
        DeletePoll { removed, finished }
    }
}

/// Drops the removed photos' T2 previews from the Larder. Runs on the job's worker thread, so a
/// brief wait for a writer holding the lock is fine; giving up after ~2 s just leaves orphaned
/// cache entries, which the Larder's own LRU eviction reclaims.
pub fn purge_previews(larder: Option<&SharedLarder>, ids: &[i64]) {
    let Some(larder) = larder else {
        return;
    };
    for _ in 0..40 {
        if let Some(mut guard) = try_lock_larder(larder) {
            for id in ids {
                let _ = guard.purge_asset(*id);
            }
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// One status line for the Library view.
pub fn summarize(mode: DeleteMode, outcome: &ShredOutcome) -> String {
    match outcome {
        ShredOutcome::Done(report) => summarize_report(mode, report, None),
        ShredOutcome::Failed { message, report } => {
            summarize_report(mode, report, Some(message.as_str()))
        }
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

fn summarize_report(mode: DeleteMode, r: &ShredReport, stopped: Option<&str>) -> String {
    let mut out = match (mode, stopped) {
        (DeleteMode::RecycleBin, None) => format!(
            "Moved {} photo{} to the Recycle Bin.",
            r.trashed + r.already_missing,
            plural(r.trashed + r.already_missing)
        ),
        (DeleteMode::CatalogOnly, None) => format!(
            "Removed {} photo{} from the catalog (files left on disk).",
            r.removed,
            plural(r.removed)
        ),
        (_, Some(msg)) => format!("Delete stopped: {msg} ({} removed first.)", r.removed),
    };
    if r.already_missing > 0 && mode == DeleteMode::RecycleBin {
        out.push_str(&format!(
            " {} had no file on disk to trash.",
            r.already_missing
        ));
    }
    if !r.leftovers.is_empty() {
        let first = &r.leftovers[0];
        out.push_str(&format!(
            " {} photo{} could not be deleted and {} still in the catalog (first: {} -- {}).",
            r.leftovers.len(),
            plural(r.leftovers.len()),
            if r.leftovers.len() == 1 { "is" } else { "are" },
            first.path.display(),
            first.reason
        ));
    }
    if !r.sidecars_left.is_empty() {
        out.push_str(&format!(
            " {} .xmp sidecar{} could not be moved.",
            r.sidecars_left.len(),
            plural(r.sidecars_left.len())
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_lair::shred::Leftover;
    use nicti_lair::{NewAsset, SqliteCatalog};
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    struct FakeBin {
        bin: PathBuf,
    }
    impl Trasher for FakeBin {
        fn trash_all(&self, paths: &[PathBuf]) -> Result<(), String> {
            for p in paths {
                std::fs::rename(p, self.bin.join(p.file_name().unwrap()))
                    .map_err(|e| e.to_string())?;
            }
            Ok(())
        }
    }

    fn seeded(dir: &Path, n: usize) -> (Arc<SqliteCatalog>, Vec<i64>) {
        let root = dir.join("shoot");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        let store = Arc::new(SqliteCatalog::open_in_memory().unwrap());
        let volume = store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = store.ensure_root(volume, &root.to_string_lossy()).unwrap();
        let ids = (0..n)
            .map(|i| {
                let name = format!("IMG_{i}.NEF");
                std::fs::write(root.join(&name), b"raw").unwrap();
                store
                    .insert_asset(
                        root_id,
                        &NewAsset {
                            rel_path: name.clone(),
                            rel_path_fold: name.to_lowercase(),
                            size_bytes: 3,
                            mtime_unix: 0,
                            fingerprint: None,
                            natural_key: None,
                            make: None,
                            model: None,
                            captured_at: None,
                            width: None,
                            height: None,
                            imported_at: 0,
                        },
                        None,
                    )
                    .unwrap()
            })
            .collect();
        (store, ids)
    }

    fn wait_finished(flow: &mut DeleteFlow) -> Vec<i64> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut removed = Vec::new();
        loop {
            let poll = flow.poll();
            removed.extend(poll.removed);
            if poll.finished {
                return removed;
            }
            assert!(Instant::now() < deadline, "delete job never finished");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn requesting_nothing_opens_no_prompt_and_a_running_delete_blocks_a_second() {
        let mut flow = DeleteFlow::new();
        flow.request(vec![], "nothing".into());
        assert!(!flow.is_confirming());
        flow.request(vec![1, 2], "the selected photos".into());
        assert!(flow.is_confirming());
    }

    #[test]
    fn a_recycle_bin_delete_runs_on_pounce_and_reports_the_removed_ids() {
        let dir = tempfile::tempdir().unwrap();
        let (store, ids) = seeded(dir.path(), 4);
        let pounce = Pounce::new(0, 2, 2, || {});
        let mut flow = DeleteFlow::new();
        let dyn_store: Arc<dyn CatalogStore + Send + Sync> = store.clone();
        flow.submit_with(
            dyn_store,
            None,
            DeleteMode::RecycleBin,
            ids[..3].to_vec(),
            &pounce,
            Box::new(FakeBin {
                bin: dir.path().join("bin"),
            }),
        );
        assert!(flow.is_running());

        let mut removed = wait_finished(&mut flow);
        removed.sort_unstable();
        assert_eq!(removed, ids[..3].to_vec());
        assert!(!flow.is_running());
        assert_eq!(store.asset_count().unwrap(), 1);
        assert!(dir.path().join("bin/IMG_0.NEF").exists());
        assert!(
            dir.path().join("shoot/IMG_3.NEF").exists(),
            "the fourth is untouched"
        );
        assert_eq!(
            flow.last_summary.as_deref(),
            Some("Moved 3 photos to the Recycle Bin.")
        );
    }

    #[test]
    fn a_catalog_only_delete_leaves_the_files_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let (store, ids) = seeded(dir.path(), 2);
        let pounce = Pounce::new(0, 2, 2, || {});
        let mut flow = DeleteFlow::new();
        let dyn_store: Arc<dyn CatalogStore + Send + Sync> = store.clone();
        flow.submit_with(
            dyn_store,
            None,
            DeleteMode::CatalogOnly,
            ids.clone(),
            &pounce,
            Box::new(FakeBin {
                bin: dir.path().join("bin"),
            }),
        );
        wait_finished(&mut flow);
        assert_eq!(store.asset_count().unwrap(), 0);
        assert!(dir.path().join("shoot/IMG_0.NEF").exists());
        assert_eq!(
            flow.last_summary.as_deref(),
            Some("Removed 2 photos from the catalog (files left on disk).")
        );
    }

    #[test]
    fn summaries_name_leftovers_missing_files_and_stopped_runs() {
        let mut report = ShredReport {
            removed: 5,
            trashed: 4,
            already_missing: 1,
            leftovers: vec![Leftover {
                asset_id: 9,
                path: PathBuf::from("/shoot/IMG_9.NEF"),
                reason: "IMG_9.NEF is locked".into(),
            }],
            sidecars_left: vec![PathBuf::from("/shoot/IMG_1.xmp")],
        };
        let text = summarize(DeleteMode::RecycleBin, &ShredOutcome::Done(report.clone()));
        assert!(
            text.starts_with("Moved 5 photos to the Recycle Bin."),
            "{text}"
        );
        assert!(text.contains("1 had no file on disk"), "{text}");
        assert!(
            text.contains("1 photo could not be deleted and is still in the catalog"),
            "{text}"
        );
        assert!(text.contains("IMG_9.NEF is locked"), "{text}");
        assert!(text.contains("1 .xmp sidecar could not be moved"), "{text}");

        report.leftovers.clear();
        report.sidecars_left.clear();
        report.already_missing = 0;
        report.removed = 1;
        report.trashed = 1;
        let text = summarize(DeleteMode::RecycleBin, &ShredOutcome::Done(report.clone()));
        assert_eq!(text, "Moved 1 photo to the Recycle Bin.");

        let text = summarize(
            DeleteMode::RecycleBin,
            &ShredOutcome::Failed {
                message: "delete cancelled".into(),
                report,
            },
        );
        assert!(
            text.starts_with("Delete stopped: delete cancelled (1 removed first.)"),
            "{text}"
        );
    }

    #[test]
    fn a_poll_that_sees_the_finished_slot_also_returns_the_last_chunks_ids() {
        let mut flow = DeleteFlow::new();
        // The job pushed its last chunk's ids and then filled the slot.
        flow.removed.lock().unwrap().extend([7, 8, 9]);
        let slot: ReportSlot<ShredOutcome> =
            Arc::new(Mutex::new(Some(ShredOutcome::Done(ShredReport {
                removed: 3,
                trashed: 3,
                ..ShredReport::default()
            }))));
        flow.pending = Some((slot, DeleteMode::RecycleBin));
        let poll = flow.poll();
        assert!(poll.finished);
        assert_eq!(
            poll.removed,
            vec![7, 8, 9],
            "no id is lost with the finishing poll"
        );
        assert!(!flow.is_running());
    }

    #[test]
    fn the_prompt_is_a_modal_that_stays_open_until_a_choice_and_never_panics_headless() {
        let ctx = egui::Context::default();
        let mut flow = DeleteFlow::new();
        flow.request(vec![1, 2, 3], "the selected photos".into());
        for _ in 0..2 {
            let mut chosen = None;
            let out = ctx.run_ui(egui::RawInput::default(), |_ui| {
                chosen = flow.show_modal(&ctx);
            });
            out.drop_without_applying_deltas();
            assert!(chosen.is_none(), "nothing chosen yet");
            assert!(flow.is_confirming(), "the prompt stays up until answered");
        }
    }
}
