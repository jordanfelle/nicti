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

use crate::cardcopy::{CardCopy, CardCopyReport};
use crate::carry::{Carry, CarryOptions, CarryOutcome, ResumeMoves, Resumed};
use crate::ninelives::{self, BackupOutcome, BackupPolicy, BackupReport};
use crate::patrol::{Sync as PatrolSync, SyncOptions, SyncReport};
use crate::righting::{Righting, RightingRun};
use crate::scruff::{Ingest, IngestReport};
use crate::shred::{Shred, ShredOutcome};
use crate::trail::{self, GeotagReport};
use crate::verify::{Baseline, BaselineReport, Verify, VerifyReport};
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

/// A backup job dropped without reporting (its `step` panicked, or Pounce dropped it unrun at
/// shutdown/cancel) still resolves its slot: `poll_backup` only clears its pending result, and
/// `NineLives` only advances its "last backup" bookkeeping, from a *resolved* report, so an
/// unresolved slot would silently hide every later scheduled backup's failure.
impl Drop for BackupJob {
    fn drop(&mut self) {
        let mut slot = self.result.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            *slot = Some(BackupReport {
                outcome: BackupOutcome::Failed(
                    "the backup did not complete (cancelled, or the job failed unexpectedly)"
                        .into(),
                ),
                stale_partials_removed: self.stale_partials_removed,
                pruned: 0,
            });
        }
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

    /// Registers a callback for when the move lands the folder on an archive drive (#72): it
    /// gets the folder's asset ids, e.g. to drop their T2 cache entries.
    pub fn with_archived_hook(mut self, hook: crate::carry::ArchivedHook) -> Self {
        self.carry.set_archived_hook(hook);
        self
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

/// Folder bit-rot check (#304): steps [`Verify`] one slice of one file's re-hash per call. Never
/// returns `Err`; a catalog read failure is reported in [`VerifyReport::error`]. A cancelled job
/// is dropped without reporting, so `Drop` fills the slot with whatever was checked so far.
pub struct VerifyJob {
    store: Arc<dyn CatalogStore + Send + Sync>,
    verify: Option<Verify>,
    label: String,
    progress: Progress,
    result: ReportSlot<VerifyReport>,
}

impl VerifyJob {
    pub fn new(
        store: Arc<dyn CatalogStore + Send + Sync>,
        root_id: i64,
        root_path: &Path,
    ) -> (Self, ReportSlot<VerifyReport>) {
        let result = Arc::new(Mutex::new(None));
        let job = VerifyJob {
            store,
            verify: Some(Verify::new(root_id, root_path)),
            label: format!("Verify: {}", root_path.display()),
            progress: Progress::default(),
            result: result.clone(),
        };
        (job, result)
    }
}

impl ChunkedJob for VerifyJob {
    fn spec(&self) -> JobSpec {
        JobSpec {
            priority: Priority::Background,
            kind: JobKind::Verify,
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
        let verify = self
            .verify
            .as_mut()
            .expect("VerifyJob::step called again after it already reported Done");
        let more = verify.step(self.store.as_ref());
        let (done, total) = verify.progress();
        self.progress = Progress {
            done,
            total: (total > 0).then_some(total),
        };
        if more {
            Ok(Step::Yield)
        } else {
            let report = self.verify.take().unwrap().into_report();
            *self.result.lock().unwrap() = Some(report);
            Ok(Step::Done)
        }
    }
}

impl Drop for VerifyJob {
    fn drop(&mut self) {
        let mut slot = self.result.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            if let Some(v) = self.verify.take() {
                *slot = Some(v.cancel_into_report());
            }
        }
    }
}

/// Records a trusted baseline content hash for a root's unhashed assets (#386): steps [`Baseline`]
/// one slice of one file per call, committing in batches. Never returns `Err`; a catalog failure is
/// reported in [`BaselineReport::error`]. A cancelled job is dropped without reporting, so `Drop`
/// writes the batch already hashed and fills the slot with what was done.
pub struct BaselineJob {
    store: Arc<dyn CatalogStore + Send + Sync>,
    baseline: Option<Baseline>,
    label: String,
    progress: Progress,
    result: ReportSlot<BaselineReport>,
}

impl BaselineJob {
    pub fn new(
        store: Arc<dyn CatalogStore + Send + Sync>,
        root_id: i64,
        root_path: &Path,
    ) -> (Self, ReportSlot<BaselineReport>) {
        let result = Arc::new(Mutex::new(None));
        let job = BaselineJob {
            store,
            baseline: Some(Baseline::new(root_id, root_path)),
            label: format!("Record baseline: {}", root_path.display()),
            progress: Progress::default(),
            result: result.clone(),
        };
        (job, result)
    }
}

impl ChunkedJob for BaselineJob {
    fn spec(&self) -> JobSpec {
        JobSpec {
            priority: Priority::Background,
            kind: JobKind::Baseline,
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
        let baseline = self
            .baseline
            .as_mut()
            .expect("BaselineJob::step called again after it already reported Done");
        let more = baseline.step(self.store.as_ref());
        let (done, total) = baseline.progress();
        self.progress = Progress {
            done,
            total: (total > 0).then_some(total),
        };
        if more {
            Ok(Step::Yield)
        } else {
            let report = self.baseline.take().unwrap().into_report();
            *self.result.lock().unwrap() = Some(report);
            Ok(Step::Done)
        }
    }
}

impl Drop for BaselineJob {
    fn drop(&mut self) {
        let mut slot = self.result.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            if let Some(b) = self.baseline.take() {
                *slot = Some(b.cancel_into_report(self.store.as_ref()));
            }
        }
    }
}

/// Startup crash recovery for interrupted folder moves (#307): steps [`ResumeMoves`] one bounded
/// unit (one journal row's metadata check, or one file of a `committed` row's source cleanup) per
/// call, so a big interrupted move's hash-and-delete pass runs off the UI thread. Never returns
/// `Err`. A cancelled job is dropped without finishing: `Drop` hands over what it did settle, and
/// the rest stay journaled for the next start. Reports under [`JobKind::Move`] so every existing
/// "a move is running" guard (import, sync, move, delete, loupe open) also covers it.
pub struct ResumeMovesJob {
    store: Arc<dyn CatalogStore + Send + Sync>,
    resume: Option<ResumeMoves>,
    progress: Progress,
    result: ReportSlot<Vec<Resumed>>,
}

impl ResumeMovesJob {
    /// `None` if the journal has no open rows (nothing to submit).
    pub fn new(
        store: Arc<dyn CatalogStore + Send + Sync>,
    ) -> Option<(Self, ReportSlot<Vec<Resumed>>)> {
        let resume = ResumeMoves::new(store.as_ref());
        if resume.is_empty() {
            return None;
        }
        let result = Arc::new(Mutex::new(None));
        let job = ResumeMovesJob {
            store,
            resume: Some(resume),
            progress: Progress::default(),
            result: result.clone(),
        };
        Some((job, result))
    }
}

impl ChunkedJob for ResumeMovesJob {
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
        "Recovering an interrupted folder move".into()
    }

    fn progress(&self) -> Progress {
        self.progress
    }

    fn step(&mut self) -> Result<Step, JobError> {
        let resume = self
            .resume
            .as_mut()
            .expect("ResumeMovesJob::step called again after it already reported Done");
        let more = resume.step(self.store.as_ref());
        let (done, total) = resume.progress();
        self.progress = Progress {
            done,
            total: Some(total),
        };
        if more {
            Ok(Step::Yield)
        } else {
            let report = self.resume.take().unwrap().into_report();
            *self.result.lock().unwrap() = Some(report);
            Ok(Step::Done)
        }
    }
}

impl Drop for ResumeMovesJob {
    fn drop(&mut self) {
        let mut slot = self.result.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            *slot = Some(
                self.resume
                    .take()
                    .map(|r| r.into_report())
                    .unwrap_or_default(),
            );
        }
    }
}

/// Thumbnail-rotation repair and orientation backfill over the catalog's roots (#606): steps
/// [`Righting`] one chunk of assets per call. Reports under [`JobKind::Sync`] so the existing
/// "a sync is running" guards keep ingest/sync/move from racing it, and so the grid's busy -> idle
/// refresh (which also drops cached decoded thumbnails) fires when it ends. Never returns `Err`: a
/// catalog failure ends the run and is recorded in the report. A cancelled job is dropped; `Drop`
/// hands over what it settled (`cancelled = true`).
pub struct RightingJob {
    store: Arc<dyn CatalogStore + Send + Sync>,
    righting: Option<Righting>,
    progress: Progress,
    result: ReportSlot<RightingRun>,
}

impl RightingJob {
    /// `skip` is the set of roots already righted (the caller's marker).
    pub fn new(
        store: Arc<dyn CatalogStore + Send + Sync>,
        skip: &std::collections::HashSet<i64>,
    ) -> Result<(Self, ReportSlot<RightingRun>), crate::CatalogError> {
        let righting = Righting::new(store.as_ref(), skip)?;
        let result = Arc::new(Mutex::new(None));
        let job = RightingJob {
            store,
            righting: Some(righting),
            progress: Progress::default(),
            result: result.clone(),
        };
        Ok((job, result))
    }
}

impl ChunkedJob for RightingJob {
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
        "Repairing thumbnail rotation".into()
    }

    fn progress(&self) -> Progress {
        self.progress
    }

    fn step(&mut self) -> Result<Step, JobError> {
        let righting = self
            .righting
            .as_mut()
            .expect("RightingJob::step called again after it already reported Done");
        let more = match righting.step(self.store.as_ref()) {
            Ok(more) => more,
            Err(e) => {
                let mut run = self.righting.take().unwrap().into_run();
                run.total
                    .failed
                    .push((PathBuf::new(), format!("catalog error: {e}")));
                *self.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(run);
                return Ok(Step::Done);
            }
        };
        self.progress = Progress {
            done: righting.progress(),
            total: None,
        };
        if more {
            Ok(Step::Yield)
        } else {
            let run = self.righting.take().unwrap().into_run();
            *self.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(run);
            Ok(Step::Done)
        }
    }
}

impl Drop for RightingJob {
    fn drop(&mut self) {
        let mut slot = self.result.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            *slot = Some(
                self.righting
                    .take()
                    .map(|r| r.into_run())
                    .unwrap_or_default(),
            );
        }
    }
}

/// Copy from a camera card (#602): steps [`CardCopy`] one file (then one registration chunk) per
/// call. Reports under [`JobKind::Import`], so the import/sync/move guards and the grid's busy ->
/// idle refresh cover it. The card is read-only to it. Never returns `Err`; a cancelled job is
/// dropped and `Drop` hands over what was done (the unattempted files are counted).
pub struct CardCopyJob {
    store: Arc<dyn CatalogStore + Send + Sync>,
    copy: Option<CardCopy>,
    label: String,
    progress: Progress,
    result: ReportSlot<CardCopyReport>,
}

impl CardCopyJob {
    pub fn new(
        store: Arc<dyn CatalogStore + Send + Sync>,
        copy: CardCopy,
        label: String,
    ) -> (Self, ReportSlot<CardCopyReport>) {
        let result = Arc::new(Mutex::new(None));
        let job = CardCopyJob {
            store,
            copy: Some(copy),
            label,
            progress: Progress::default(),
            result: result.clone(),
        };
        (job, result)
    }
}

impl ChunkedJob for CardCopyJob {
    fn spec(&self) -> JobSpec {
        JobSpec {
            priority: Priority::Foreground,
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
        let copy = self
            .copy
            .as_mut()
            .expect("CardCopyJob::step called again after it already reported Done");
        let more = match copy.step(self.store.as_ref(), &|| false) {
            Ok(more) => more,
            Err(e) => {
                let mut report = self.copy.take().unwrap().into_report();
                report.ingest_error = Some(e.to_string());
                *self.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(report);
                return Ok(Step::Done);
            }
        };
        let (done, total) = copy.progress();
        self.progress = Progress {
            done,
            total: Some(total),
        };
        if more {
            Ok(Step::Yield)
        } else {
            let report = self.copy.take().unwrap().into_report();
            *self.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(report);
            Ok(Step::Done)
        }
    }
}

impl Drop for CardCopyJob {
    fn drop(&mut self) {
        let mut slot = self.result.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            *slot = Some(
                self.copy
                    .take()
                    .map(|c| c.into_report())
                    .unwrap_or_default(),
            );
        }
    }
}

/// Assets handled per `step` by [`GeotagJob`] / [`ClearPositionJob`]: each costs a catalog read, a
/// write and a sidecar write, so a chunk this size keeps cancellation and progress responsive.
const GEOTAG_CHUNK: usize = 16;

/// The largest GPX file a [`GeotagJob`] will read (a day of 1 Hz logging is a few MB).
pub const MAX_GPX_BYTES: u64 = 64 * 1024 * 1024;

/// Where a [`GeotagJob`] gets its track log.
pub enum GpxSource {
    /// Already parsed.
    Parsed(nicti_scent::gpx::Tracklog),
    /// Read and parsed by the job's first step (size-capped), so a large log never blocks the UI.
    File(PathBuf),
}

/// An asset's file as the catalog resolves it (`root path` + `rel_path`), reachable or not.
fn asset_file(store: &dyn CatalogStore, id: i64) -> Result<PathBuf, String> {
    let asset = store
        .get_asset(id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("asset {id} not found"))?;
    let root = store
        .get_root_path(asset.root_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("asset {id}: its folder is not registered"))?;
    Ok(Path::new(&root).join(&asset.rel_path))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

/// Applies a GPX log to a selection (#668): reads the log on its first step, then runs
/// [`trail::geotag_assets`] over `GEOTAG_CHUNK` assets per call and merges the reports. Never
/// returns `Err` (a bad asset is counted in [`GeotagReport::failed`], a bad log in
/// [`GeotagReport::gpx_error`]); a cancelled or dropped job still resolves its slot, marked
/// `stopped`, so the UI never waits on it.
pub struct GeotagJob {
    store: Arc<dyn CatalogStore + Send + Sync>,
    ids: Vec<i64>,
    next: usize,
    source: Option<GpxSource>,
    log: Option<nicti_scent::gpx::Tracklog>,
    opts: nicti_scent::gpx::GeotagOptions,
    overwrite: bool,
    auto_write: bool,
    report: GeotagReport,
    progress: Progress,
    result: ReportSlot<GeotagReport>,
}

impl GeotagJob {
    /// `ids` are the assets to tag; `auto_write` is whether to write the XMP now (else the asset
    /// is only marked dirty for a later flush).
    pub fn new(
        store: Arc<dyn CatalogStore + Send + Sync>,
        ids: Vec<i64>,
        source: GpxSource,
        opts: nicti_scent::gpx::GeotagOptions,
        overwrite: bool,
        auto_write: bool,
    ) -> (Self, ReportSlot<GeotagReport>) {
        let result = Arc::new(Mutex::new(None));
        let total = ids.len() as u64;
        let job = GeotagJob {
            store,
            ids,
            next: 0,
            source: Some(source),
            log: None,
            opts,
            overwrite,
            auto_write,
            report: GeotagReport::default(),
            progress: Progress {
                done: 0,
                total: Some(total),
            },
            result: result.clone(),
        };
        (job, result)
    }

    fn load_log(&mut self) -> Result<(), String> {
        match self.source.take() {
            Some(GpxSource::Parsed(log)) => self.log = Some(log),
            Some(GpxSource::File(path)) => {
                let meta = std::fs::metadata(&path)
                    .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
                if meta.len() > MAX_GPX_BYTES {
                    return Err(format!(
                        "{} is {} MiB; GPX files over {} MiB are refused",
                        path.display(),
                        meta.len() >> 20,
                        MAX_GPX_BYTES >> 20
                    ));
                }
                let text = std::fs::read_to_string(&path)
                    .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
                self.log = Some(
                    nicti_scent::gpx::parse_gpx(&text)
                        .map_err(|e| format!("{}: {e}", path.display()))?,
                );
            }
            None => {}
        }
        Ok(())
    }

    fn finish(&mut self) -> Step {
        *self.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(self.report.clone());
        Step::Done
    }
}

impl ChunkedJob for GeotagJob {
    fn spec(&self) -> JobSpec {
        JobSpec {
            priority: Priority::Background,
            kind: JobKind::Geotag,
            lane: Lane::Cpu,
            vram_bytes: 0,
            image_index: None,
        }
    }

    fn label(&self) -> String {
        format!("Geotag {} photo(s) from GPX", self.ids.len())
    }

    fn progress(&self) -> Progress {
        self.progress
    }

    fn step(&mut self) -> Result<Step, JobError> {
        if self.log.is_none() {
            if let Err(e) = self.load_log() {
                self.report.gpx_error = Some(e);
                return Ok(self.finish());
            }
        }
        let Some(log) = self.log.as_ref() else {
            return Ok(self.finish());
        };
        let end = (self.next + GEOTAG_CHUNK).min(self.ids.len());
        let mut items = Vec::with_capacity(end - self.next);
        for &id in &self.ids[self.next..end] {
            match asset_file(self.store.as_ref(), id) {
                Ok(path) => items.push((id, path)),
                Err(e) => self.report.failed.push((id, e)),
            }
        }
        let part = trail::geotag_assets(
            self.store.as_ref(),
            &items,
            log,
            &self.opts,
            self.overwrite,
            self.auto_write,
            now_ms(),
        );
        self.report.merge(part);
        self.next = end;
        self.progress.done = end as u64;
        if end < self.ids.len() {
            return Ok(Step::Yield);
        }
        Ok(self.finish())
    }
}

impl Drop for GeotagJob {
    fn drop(&mut self) {
        let mut slot = self.result.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            let mut report = self.report.clone();
            report.stopped = true;
            *slot = Some(report);
        }
    }
}

/// How a batch of [`ClearPositionJob`] went.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ClearReport {
    pub cleared: usize,
    pub nothing_to_clear: usize,
    /// The file carries a position and XMP auto-write is off, so nothing was changed.
    pub needs_auto_write: usize,
    /// The sidecar changed under us; the clear is held for review and nothing was changed.
    pub held_for_review: usize,
    pub failed: Vec<(i64, String)>,
    /// Cancelled (or dropped) before every asset was handled.
    pub stopped: bool,
}

/// Removes the stored GPS position from a selection (#668), from the catalog and the XMP, through
/// [`crate::scent_sync::clear_position`]. Never returns `Err`.
pub struct ClearPositionJob {
    store: Arc<dyn CatalogStore + Send + Sync>,
    ids: Vec<i64>,
    next: usize,
    auto_write: bool,
    report: ClearReport,
    progress: Progress,
    result: ReportSlot<ClearReport>,
}

impl ClearPositionJob {
    pub fn new(
        store: Arc<dyn CatalogStore + Send + Sync>,
        ids: Vec<i64>,
        auto_write: bool,
    ) -> (Self, ReportSlot<ClearReport>) {
        let result = Arc::new(Mutex::new(None));
        let total = ids.len() as u64;
        let job = ClearPositionJob {
            store,
            ids,
            next: 0,
            auto_write,
            report: ClearReport::default(),
            progress: Progress {
                done: 0,
                total: Some(total),
            },
            result: result.clone(),
        };
        (job, result)
    }
}

impl ChunkedJob for ClearPositionJob {
    fn spec(&self) -> JobSpec {
        JobSpec {
            priority: Priority::Background,
            kind: JobKind::Geotag,
            lane: Lane::Cpu,
            vram_bytes: 0,
            image_index: None,
        }
    }

    fn label(&self) -> String {
        format!("Clear position of {} photo(s)", self.ids.len())
    }

    fn progress(&self) -> Progress {
        self.progress
    }

    fn step(&mut self) -> Result<Step, JobError> {
        use crate::scent_sync::{clear_position, ClearOutcome};
        let end = (self.next + GEOTAG_CHUNK).min(self.ids.len());
        for &id in &self.ids[self.next..end] {
            let outcome = asset_file(self.store.as_ref(), id).and_then(|path| {
                clear_position(self.store.as_ref(), id, &path, self.auto_write, now_ms())
                    .map_err(|e| e.to_string())
            });
            match outcome {
                Ok(ClearOutcome::Cleared) => self.report.cleared += 1,
                Ok(ClearOutcome::NothingToClear) => self.report.nothing_to_clear += 1,
                Ok(ClearOutcome::NeedsAutoWrite) => self.report.needs_auto_write += 1,
                Ok(ClearOutcome::HeldForReview) => self.report.held_for_review += 1,
                Ok(ClearOutcome::Unreachable) => self.report.failed.push((
                    id,
                    "its file is not reachable (offline volume?); nothing was changed".into(),
                )),
                Err(e) => self.report.failed.push((id, e)),
            }
        }
        self.next = end;
        self.progress.done = end as u64;
        if end < self.ids.len() {
            return Ok(Step::Yield);
        }
        *self.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(self.report.clone());
        Ok(Step::Done)
    }
}

impl Drop for ClearPositionJob {
    fn drop(&mut self) {
        let mut slot = self.result.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            let mut report = self.report.clone();
            report.stopped = true;
            *slot = Some(report);
        }
    }
}
