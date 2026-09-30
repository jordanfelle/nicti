//! Pounce's first real clients (#55): `IngestJob`/`SyncJob` step Scruff's [`Ingest`] / Patrol's
//! [`PatrolSync`] one chunk (one file, or one already-cataloged asset) per
//! [`nicti_pounce::ChunkedJob::step`] call. Both run on Pounce's CPU lane, `Priority::Background`
//! -- CPU/disk-bound scan work that never needs the GPU/`ort` worker (#206's own finding: decode
//! is CPU-only and shouldn't serialize behind GPU dispatch). `BackupJob` (#25) is a third client,
//! splitting `ninelives::run_backup`'s four steps (stale-partial sweep + live quick_check,
//! snapshot, verify, rotate) across four chunks instead of running them in one call, so the
//! activity panel shows real per-step progress on a run that can take seconds at 2M rows.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use nicti_pounce::{ChunkedJob, JobError, JobKind, JobSpec, Lane, Priority, Progress, Step};

use crate::carry::{Carry, CarryOptions, CarryOutcome};
use crate::ninelives::{self, BackupOutcome, BackupPolicy, BackupReport};
use crate::patrol::{Sync as PatrolSync, SyncOptions, SyncReport};
use crate::scruff::{Ingest, IngestReport};
use crate::shred::{Shred, ShredOutcome};
use crate::{CatalogStore, SqliteCatalog};

/// A shared slot a caller can poll for the final report once a job reaches `Done`. Stays `None`
/// if the job was cancelled or failed instead -- check the activity panel's own
/// `nicti_pounce::JobState` for that; this slot only ever holds a *successful* report.
pub type ReportSlot<R> = Arc<Mutex<Option<R>>>;

pub struct IngestJob {
    store: Arc<dyn CatalogStore + Send + Sync>,
    ingest: Option<Ingest>,
    label: String,
    progress: Progress,
    result: ReportSlot<IngestReport>,
}

impl IngestJob {
    /// `store` is `Arc<dyn CatalogStore + Send + Sync>` (not the concrete `SqliteCatalog`) so this
    /// job doesn't need to know which catalog-store backend is running -- a caller upcasts via a
    /// plain `Arc` coercion (`store as Arc<dyn CatalogStore + Send + Sync>`).
    pub fn new(
        store: Arc<dyn CatalogStore + Send + Sync>,
        root_id: i64,
        root_path: &Path,
    ) -> (Self, ReportSlot<IngestReport>) {
        let result = Arc::new(Mutex::new(None));
        let job = IngestJob {
            store,
            ingest: Some(Ingest::new(root_id, root_path)),
            label: format!("Import: {}", root_path.display()),
            progress: Progress::default(),
            result: result.clone(),
        };
        (job, result)
    }
}

impl ChunkedJob for IngestJob {
    fn spec(&self) -> JobSpec {
        JobSpec {
            priority: Priority::Background,
            kind: JobKind::Import,
            lane: Lane::Cpu,
            vram_bytes: 0,
            image_index: None,
        }
    }

    fn label(&self) -> String {
        self.label.clone()
    }

    fn progress(&self) -> Progress {
        self.progress
    }

    fn step(&mut self) -> Result<Step, JobError> {
        let ingest = self
            .ingest
            .as_mut()
            .expect("IngestJob::step called again after it already reported Done");
        let more = ingest
            .step(self.store.as_ref())
            .map_err(|e| JobError::new(e.to_string()))?;
        self.progress = Progress {
            done: ingest.processed_count(),
            total: None, // Scruff's walk is lazy -- never a known total, see Ingest's own doc comment
        };
        if more {
            Ok(Step::Yield)
        } else {
            let report = self.ingest.take().unwrap().into_report();
            *self.result.lock().unwrap() = Some(report);
            Ok(Step::Done)
        }
    }
}

pub struct SyncJob {
    store: Arc<dyn CatalogStore + Send + Sync>,
    sync: Option<PatrolSync>,
    label: String,
    done_count: u64,
    progress: Progress,
    result: ReportSlot<SyncReport>,
}

impl SyncJob {
    pub fn new(
        store: Arc<dyn CatalogStore + Send + Sync>,
        root_id: i64,
        root_path: &Path,
        opts: SyncOptions,
    ) -> (Self, ReportSlot<SyncReport>) {
        let result = Arc::new(Mutex::new(None));
        let job = SyncJob {
            store,
            sync: Some(PatrolSync::new(root_id, root_path, opts)),
            label: format!("Sync: {}", root_path.display()),
            done_count: 0,
            progress: Progress::default(),
            result: result.clone(),
        };
        (job, result)
    }
}

impl ChunkedJob for SyncJob {
    fn spec(&self) -> JobSpec {
        JobSpec {
            priority: Priority::Background,
            kind: JobKind::Sync,
            lane: Lane::Cpu,
            vram_bytes: 0,
            image_index: None,
        }
    }

    fn label(&self) -> String {
        self.label.clone()
    }

    fn progress(&self) -> Progress {
        self.progress
    }

    fn step(&mut self) -> Result<Step, JobError> {
        let sync = self
            .sync
            .as_mut()
            .expect("SyncJob::step called again after it already reported Done");
        let had_total = self.progress.total.is_some();
        let more = sync
            .step(self.store.as_ref())
            .map_err(|e| JobError::new(e.to_string()))?;
        let total = sync.total_hint();
        if total.is_some() && !had_total {
            // Just entered the catalog-side (`Checking`) phase, the first phase with a known
            // total -- `done_count` restarts here so it counts only this phase's own steps,
            // rather than continuing to include however many ingest chunks (and the one
            // `Listing` transition) already ran before it (found by CodeRabbit's review: without
            // this, `done` was already past `total` the moment the checking phase started).
            self.done_count = 0;
        } else {
            self.done_count += 1;
        }
        self.progress = Progress {
            done: self.done_count,
            // Once the phase with a known total ends (`Done`), `total_hint()` goes back to
            // `None` -- keep the last real total instead of reverting the row to an
            // indeterminate spinner on its very last step (also found by CodeRabbit's review).
            total: total.or(self.progress.total),
        };
        if more {
            Ok(Step::Yield)
        } else {
            let report = self.sync.take().unwrap().into_report();
            *self.result.lock().unwrap() = Some(report);
            Ok(Step::Done)
        }
    }
}

/// `BackupJob`'s own chunks, one per `ninelives` step -- see that module's `run_backup` doc
/// comment for why this job re-implements the same sequence chunk-by-chunk instead of calling it.
enum BackupPhase {
    QuickCheck,
    Snapshot,
    Verify { partial: PathBuf },
    Rotate { partial: PathBuf },
}

/// Nine Lives' (#25) own Pounce client: `nicti-pelt`'s app loop submits this whenever
/// `NineLives::due` says a backup should run. Four chunks (`QuickCheck` -> `Snapshot` -> `Verify`
/// -> `Rotate`), each a thin wrapper around one `ninelives` free function, so the activity panel
/// shows real progress across a run that can take a couple of seconds at 2M rows rather than one
/// opaque `Step::Done`.
///
/// Cancellation between chunks (`nicti_pounce::ChunkedJob`'s only cancellation granularity, see
/// its own doc comment) can leave a `.partial` file on disk if it happens after `Snapshot` but
/// before `Rotate` -- there's no on-cancel callback to delete it synchronously, so it's swept up
/// by the *next* run's own `QuickCheck` chunk instead (`ninelives::cleanup_stale_partials`).
pub struct BackupJob {
    catalog: Arc<SqliteCatalog>,
    policy: BackupPolicy,
    now_unix: i64,
    stale_partials_removed: u64,
    phase: Option<BackupPhase>,
    progress: Progress,
    result: ReportSlot<BackupReport>,
}

impl BackupJob {
    /// `now_unix` is read once at construction (not re-read per chunk) so every timestamp this run
    /// produces (the snapshot's own filename, in particular) reflects when the run was submitted,
    /// not whenever its `Snapshot` chunk happened to actually execute.
    pub fn new(
        catalog: Arc<SqliteCatalog>,
        policy: BackupPolicy,
        now_unix: i64,
    ) -> (Self, ReportSlot<BackupReport>) {
        let result = Arc::new(Mutex::new(None));
        let job = BackupJob {
            catalog,
            policy,
            now_unix,
            stale_partials_removed: 0,
            phase: Some(BackupPhase::QuickCheck),
            progress: Progress {
                done: 0,
                total: Some(4),
            },
            result: result.clone(),
        };
        (job, result)
    }

    fn finish(&mut self, outcome: BackupOutcome, pruned: u64) -> Step {
        *self.result.lock().unwrap() = Some(BackupReport {
            outcome,
            stale_partials_removed: self.stale_partials_removed,
            pruned,
        });
        Step::Done
    }
}

impl ChunkedJob for BackupJob {
    fn spec(&self) -> JobSpec {
        JobSpec {
            priority: Priority::Background,
            kind: JobKind::Backup,
            lane: Lane::Cpu,
            vram_bytes: 0,
            image_index: None,
        }
    }

    fn label(&self) -> String {
        "Catalog backup".to_string()
    }

    fn progress(&self) -> Progress {
        self.progress
    }

    /// Never returns `Err` -- any `CatalogError` from a `ninelives` step is caught and routed
    /// through `finish(BackupOutcome::Failed(_))` instead, so this job always reaches `Done` and
    /// its `ReportSlot` always resolves. An adversarial review caught that returning `Err` here
    /// (this crate's other `ChunkedJob`s, `IngestJob`/`SyncJob`, do exactly that on their own
    /// errors) would leave `nicti-pelt`'s `poll_backup` waiting on a `ReportSlot` that never
    /// fills in, since `nicti_pounce::Pounce` marks a job `JobState::Failed` and drops it on an
    /// `Err` return without ever calling back into the job itself -- silently and permanently
    /// hiding every future scheduled backup's failure, not just this one's, since `NineLives`
    /// only advances its own "last backup" bookkeeping from a *resolved* report (see
    /// `NineLives::due`'s doc comment and `app.rs::poll_backup`).
    fn step(&mut self) -> Result<Step, JobError> {
        let phase = self
            .phase
            .take()
            .expect("BackupJob::step called again after it already reported Done");
        match phase {
            BackupPhase::QuickCheck => {
                let stale_partials_removed = match ninelives::cleanup_stale_partials(&self.policy) {
                    Ok(n) => n,
                    Err(e) => return Ok(self.finish(BackupOutcome::Failed(e.to_string()), 0)),
                };
                self.stale_partials_removed = stale_partials_removed;
                match self.catalog.quick_check() {
                    Ok(Some(problem)) => {
                        return Ok(self.finish(BackupOutcome::LiveCorrupt(problem), 0));
                    }
                    Ok(None) => {}
                    Err(e) => return Ok(self.finish(BackupOutcome::Failed(e.to_string()), 0)),
                }
                self.progress = Progress {
                    done: 1,
                    total: Some(4),
                };
                self.phase = Some(BackupPhase::Snapshot);
                Ok(Step::Yield)
            }

            BackupPhase::Snapshot => {
                let partial =
                    match ninelives::snapshot_into(&self.catalog, &self.policy, self.now_unix) {
                        Ok(partial) => partial,
                        Err(e) => return Ok(self.finish(BackupOutcome::Failed(e.to_string()), 0)),
                    };
                self.progress = Progress {
                    done: 2,
                    total: Some(4),
                };
                self.phase = Some(BackupPhase::Verify { partial });
                Ok(Step::Yield)
            }

            BackupPhase::Verify { partial } => {
                match ninelives::verify(&self.catalog, &partial) {
                    Ok(None) => {}
                    Ok(Some(problem)) => {
                        let _ = std::fs::remove_file(&partial);
                        return Ok(self.finish(BackupOutcome::VerifyFailed(problem), 0));
                    }
                    Err(e) => {
                        let _ = std::fs::remove_file(&partial);
                        return Ok(self.finish(BackupOutcome::Failed(e.to_string()), 0));
                    }
                }
                self.progress = Progress {
                    done: 3,
                    total: Some(4),
                };
                self.phase = Some(BackupPhase::Rotate { partial });
                Ok(Step::Yield)
            }

            BackupPhase::Rotate { partial } => {
                let (final_path, pruned) = match ninelives::rotate(&self.policy, &partial) {
                    Ok(result) => result,
                    Err(e) => return Ok(self.finish(BackupOutcome::Failed(e.to_string()), 0)),
                };
                self.progress = Progress {
                    done: 4,
                    total: Some(4),
                };
                Ok(self.finish(BackupOutcome::Verified(final_path), pruned))
            }
        }
    }
}

/// Verified folder move (#26): steps [`Carry`] one chunk (a slice of one file's copy or verify
/// read, or a phase transition) per call. Never returns `Err` -- every failure is a
/// [`CarryOutcome`] in the report slot, same reasoning as [`BackupJob`]. Cancelling drops the
/// job, and `Carry`'s `Drop` discards the half-built destination if nothing was committed yet.
pub struct MoveJob {
    carry: Carry,
    progress: Progress,
    result: ReportSlot<CarryOutcome>,
}

impl MoveJob {
    pub fn new(
        store: Arc<dyn CatalogStore + Send + Sync>,
        root_id: i64,
        dest_parent: &Path,
        opts: CarryOptions,
        now_unix: i64,
    ) -> (Self, ReportSlot<CarryOutcome>) {
        let result = Arc::new(Mutex::new(None));
        let job = MoveJob {
            carry: Carry::new(store, root_id, dest_parent, opts, now_unix),
            progress: Progress::default(),
            result: result.clone(),
        };
        (job, result)
    }
}

impl ChunkedJob for MoveJob {
    fn spec(&self) -> JobSpec {
        JobSpec {
            priority: Priority::Background,
            kind: JobKind::Move,
            lane: Lane::Cpu,
            vram_bytes: 0,
            image_index: None,
        }
    }

    fn label(&self) -> String {
        self.carry.label()
    }

    fn progress(&self) -> Progress {
        self.progress
    }

    fn step(&mut self) -> Result<Step, JobError> {
        let outcome = self.carry.step();
        let (done, total) = self.carry.progress();
        self.progress = Progress { done, total };
        match outcome {
            Some(o) => {
                *self.result.lock().unwrap() = Some(o);
                Ok(Step::Done)
            }
            None => Ok(Step::Yield),
        }
    }
}

/// A cancelled (or panicked) move is dropped without ever reporting; fill the slot so the UI
/// doesn't sit on "Moving..." forever.
impl Drop for MoveJob {
    fn drop(&mut self) {
        let mut slot = self.result.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            *slot = Some(CarryOutcome::Failed(if self.carry.is_committed() {
                "move interrupted after the catalog was updated; the folder is at its new \
                 location and the original is cleaned up where the bytes match"
                    .to_string()
            } else {
                "move cancelled; nothing was changed".to_string()
            }));
        }
    }
}

/// Batch delete (#32): steps [`Shred`] one chunk of up to `shred::CHUNK` assets per call. Never
/// returns `Err` -- every failure is a [`ShredOutcome`] in the report slot, same reasoning as
/// [`BackupJob`]. A cancel drops the job between chunks (a chunk is never split), and the report
/// says how far it got.
pub struct DeleteJob {
    shred: Shred,
    progress: Progress,
    result: ReportSlot<ShredOutcome>,
}

impl DeleteJob {
    pub fn new(shred: Shred) -> (Self, ReportSlot<ShredOutcome>) {
        let result = Arc::new(Mutex::new(None));
        let job = DeleteJob {
            shred,
            progress: Progress::default(),
            result: result.clone(),
        };
        (job, result)
    }
}

impl ChunkedJob for DeleteJob {
    fn spec(&self) -> JobSpec {
        JobSpec {
            priority: Priority::Background,
            kind: JobKind::Delete,
            lane: Lane::Cpu,
            vram_bytes: 0,
            image_index: None,
        }
    }

    fn label(&self) -> String {
        self.shred.label()
    }

    fn progress(&self) -> Progress {
        self.progress
    }

    fn step(&mut self) -> Result<Step, JobError> {
        let outcome = self.shred.step();
        let (done, total) = self.shred.progress();
        self.progress = Progress { done, total };
        match outcome {
            Some(o) => {
                *self.result.lock().unwrap() = Some(o);
                Ok(Step::Done)
            }
            None => Ok(Step::Yield),
        }
    }
}

/// A cancelled (or panicked) delete is dropped without ever reporting; fill the slot with what it
/// did get through so the UI doesn't sit on "Deleting..." forever.
impl Drop for DeleteJob {
    fn drop(&mut self) {
        let mut slot = self.result.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            *slot = Some(ShredOutcome::Failed {
                message: "delete cancelled; photos not yet processed were left untouched".into(),
                report: self.shred.partial_report().clone(),
            });
        }
    }
}
