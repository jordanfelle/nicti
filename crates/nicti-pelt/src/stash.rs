//! Stash (#353, ADR-0353): a cat stashes its catch to eat later. The disk tier for baked AI mask
//! alphas -- ADR-0044's "mask alpha" row -- so reopening a photo loads its masks instead of
//! re-running the model (a bake is ~9 s on the CPU build).
//!
//! An alpha lives in the [`Larder`](nicti_lair::larder::Larder) under [`LarderKind::AiAlpha`],
//! keyed by its `ai_bake_key`. That key chains from the neutral render (photo identity + upstream
//! stages) and the recipe, so a stored alpha can never be applied to the wrong pixels: any change
//! that would alter the bake changes the key, and the old entry just ages out under the byte cap.
//!
//! This file is the two Pounce jobs ([`AlphaFetchJob`], [`AlphaStoreJob`]); the callers are
//! `mask_tool::MaskBakeService` (fetch before bake, store after) and [`crate::prebake`].
//!
//! Both jobs are `Preview`-kind CPU jobs: they are disk/CPU work that must never run on the UI
//! thread, and neither waits unboundedly on the Larder (a compaction holds it for a whole rewrite).

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nicti_lair::larder::{LarderKeyed, LarderKind};
use nicti_pounce::{ChunkedJob, JobError, JobKind, JobSpec, Lane, Priority, Progress, Step};
use nicti_siamese::job::Slot;
use nicti_tapetum::mask::engine::AiAlpha;

use crate::t2::{lock_larder_within, SharedLarder};

/// How long a job waits for the Larder lock. Past this a fetch reports a miss (the caller bakes)
/// and a store is dropped (the next open re-bakes and tries again).
const LOCK_WAIT: Duration = Duration::from_secs(2);

/// The Larder key of a baked alpha.
pub(crate) fn keyed(asset_id: i64, bake_key: &[u8; 32]) -> LarderKeyed<'_> {
    LarderKeyed {
        kind: LarderKind::AiAlpha,
        asset_id,
        key: bake_key.as_slice(),
    }
}

/// What a fetch found.
pub struct FetchOutcome {
    /// The photo it was asked for, so a result that lands after the user moved on is recognised.
    pub image_key: u64,
    pub bake_key: blake3::Hash,
    /// `None` is a miss: not stored, unreadable, malformed, or the Larder was busy. All the same
    /// to the caller, which bakes.
    pub alpha: Option<Arc<AiAlpha>>,
}

/// Loads one stored alpha. Foreground: the user just opened the photo and is waiting on its masks.
pub struct AlphaFetchJob {
    larder: SharedLarder,
    asset_id: i64,
    image_key: u64,
    bake_key: blake3::Hash,
    done: bool,
    slot: Slot<FetchOutcome>,
}

impl AlphaFetchJob {
    pub fn new(
        larder: SharedLarder,
        asset_id: i64,
        image_key: u64,
        bake_key: blake3::Hash,
    ) -> (Self, Slot<FetchOutcome>) {
        let slot: Slot<FetchOutcome> = Arc::new(Mutex::new(None));
        let job = Self {
            larder,
            asset_id,
            image_key,
            bake_key,
            done: false,
            slot: Arc::clone(&slot),
        };
        (job, slot)
    }

    fn outcome(&self, alpha: Option<Arc<AiAlpha>>) -> FetchOutcome {
        FetchOutcome {
            image_key: self.image_key,
            bake_key: self.bake_key,
            alpha,
        }
    }

    fn run(&self) -> Option<Arc<AiAlpha>> {
        let key = keyed(self.asset_id, self.bake_key.as_bytes());
        let bytes = {
            let mut larder = lock_larder_within(&self.larder, LOCK_WAIT)?;
            larder.get_keyed(key).ok().flatten()?
        };
        // Decoded outside the lock.
        let Some(alpha) = AiAlpha::decode(&bytes) else {
            // Intact but unreadable (another build's format): forget it, or it would also make the
            // re-bake's store skip as "already there".
            if let Some(mut larder) = lock_larder_within(&self.larder, LOCK_WAIT) {
                let _ = larder.forget_keyed(key);
            }
            return None;
        };
        Some(Arc::new(alpha))
    }
}

impl ChunkedJob for AlphaFetchJob {
    fn spec(&self) -> JobSpec {
        JobSpec {
            priority: Priority::Foreground,
            kind: JobKind::Preview,
            lane: Lane::Cpu,
            vram_bytes: 0,
            image_index: None,
        }
    }

    fn label(&self) -> String {
        "Load stored mask".to_owned()
    }

    fn progress(&self) -> Progress {
        Progress {
            done: u64::from(self.done),
            total: Some(1),
        }
    }

    /// Never returns `Err`: a failure is a miss in the slot, so a poller is never left waiting.
    fn step(&mut self) -> Result<Step, JobError> {
        let alpha = catch_unwind(AssertUnwindSafe(|| self.run())).unwrap_or(None);
        *self.slot.lock().unwrap() = Some(self.outcome(alpha));
        self.done = true;
        Ok(Step::Done)
    }
}

impl Drop for AlphaFetchJob {
    /// Pounce drops a job cancelled while queued without running it; resolve the slot as a miss so
    /// the caller falls through to a bake instead of waiting forever.
    fn drop(&mut self) {
        if self.done {
            return;
        }
        if let Ok(mut slot) = self.slot.lock() {
            if slot.is_none() {
                *slot = Some(self.outcome(None));
            }
        }
    }
}

/// Encodes and stores one baked alpha. Background: nobody waits on it, and a slider drag
/// (`IS_EDITING`) pauses it like any other background chunk. Best effort -- a busy Larder or an
/// over-cap payload just means the next open bakes again.
pub struct AlphaStoreJob {
    larder: SharedLarder,
    asset_id: i64,
    bake_key: blake3::Hash,
    alpha: Arc<AiAlpha>,
    done: bool,
    /// Set when the job is over for any reason (stored, skipped, failed, or dropped unrun), so the
    /// pre-bake can hold a photo's keys "in flight" until its alphas are actually on disk and the
    /// foreground then finds them instead of baking again.
    finished: Option<Arc<AtomicBool>>,
}

impl AlphaStoreJob {
    pub fn new(
        larder: SharedLarder,
        asset_id: i64,
        bake_key: blake3::Hash,
        alpha: Arc<AiAlpha>,
    ) -> Self {
        Self {
            larder,
            asset_id,
            bake_key,
            alpha,
            done: false,
            finished: None,
        }
    }

    /// Raises `flag` when this job ends, whatever the outcome.
    pub fn with_finished_flag(mut self, flag: Arc<AtomicBool>) -> Self {
        self.finished = Some(flag);
        self
    }

    fn finish(&self) {
        if let Some(flag) = &self.finished {
            flag.store(true, Ordering::SeqCst);
        }
    }

    fn run(&self) {
        let key = keyed(self.asset_id, self.bake_key.as_bytes());
        // Already there (an earlier job, or a second session): skip the encode.
        if let Some(larder) = lock_larder_within(&self.larder, LOCK_WAIT) {
            if larder.contains_keyed(key).unwrap_or(false) {
                return;
            }
        } else {
            return;
        }
        let bytes = self.alpha.encode();
        if let Some(mut larder) = lock_larder_within(&self.larder, LOCK_WAIT) {
            let _ = larder.put_keyed(key, &bytes);
        }
    }
}

impl ChunkedJob for AlphaStoreJob {
    fn spec(&self) -> JobSpec {
        JobSpec {
            priority: Priority::Background,
            kind: JobKind::Preview,
            lane: Lane::Cpu,
            vram_bytes: 0,
            image_index: None,
        }
    }

    fn label(&self) -> String {
        "Save mask".to_owned()
    }

    fn progress(&self) -> Progress {
        Progress {
            done: u64::from(self.done),
            total: Some(1),
        }
    }

    fn step(&mut self) -> Result<Step, JobError> {
        let _ = catch_unwind(AssertUnwindSafe(|| self.run()));
        self.done = true;
        self.finish();
        Ok(Step::Done)
    }
}

impl Drop for AlphaStoreJob {
    /// A job Pounce drops unrun (cancelled, or shutdown) must still release whoever waits on it.
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_lair::larder::{Larder, LarderConfig};

    fn larder() -> (tempfile::TempDir, SharedLarder) {
        let dir = tempfile::tempdir().unwrap();
        let l = Larder::open(dir.path(), LarderConfig::default()).unwrap();
        (dir, Arc::new(Mutex::new(l)))
    }

    fn alpha() -> Arc<AiAlpha> {
        Arc::new(AiAlpha::quantized(4, 2, vec![0.0, 1.0, 1.0, 0.0, 0.5, 0.5, 1.0, 0.0]).unwrap())
    }

    fn fetch(l: &SharedLarder, asset: i64, key: blake3::Hash) -> FetchOutcome {
        let (mut job, slot) = AlphaFetchJob::new(Arc::clone(l), asset, 7, key);
        assert_eq!(job.step().unwrap(), Step::Done);
        let out = slot.lock().unwrap().take();
        out.expect("resolved")
    }

    #[test]
    fn a_stored_alpha_is_fetched_back_identical() {
        let (_dir, l) = larder();
        let key = blake3::hash(b"bake");
        let a = alpha();
        AlphaStoreJob::new(Arc::clone(&l), 3, key, Arc::clone(&a))
            .step()
            .unwrap();
        let out = fetch(&l, 3, key);
        assert_eq!((out.image_key, out.bake_key), (7, key));
        let got = out.alpha.expect("a hit");
        assert_eq!(got.alpha, a.alpha);
        assert_eq!(got.content_hash, a.content_hash);
    }

    #[test]
    fn an_unknown_key_is_a_miss() {
        let (_dir, l) = larder();
        assert!(fetch(&l, 3, blake3::hash(b"nope")).alpha.is_none());
    }

    #[test]
    fn the_key_is_content_addressed_not_scoped_to_the_asset() {
        // Same bake key = same pixels by construction (it chains from the photo identity), so a
        // fetch under another asset id still hits.
        let (_dir, l) = larder();
        let key = blake3::hash(b"bake");
        AlphaStoreJob::new(Arc::clone(&l), 3, key, alpha())
            .step()
            .unwrap();
        assert!(fetch(&l, 99, key).alpha.is_some());
    }

    #[test]
    fn a_payload_this_build_cannot_read_is_a_miss_is_forgotten_and_can_be_replaced() {
        let (_dir, l) = larder();
        let key = blake3::hash(b"bake");
        l.lock()
            .unwrap()
            .put_keyed(keyed(3, key.as_bytes()), b"NAL9 from a newer build")
            .unwrap();
        assert!(fetch(&l, 3, key).alpha.is_none());
        assert!(
            !l.lock()
                .unwrap()
                .contains_keyed(keyed(3, key.as_bytes()))
                .unwrap(),
            "left in place it would make the re-bake's store skip"
        );
        AlphaStoreJob::new(Arc::clone(&l), 3, key, alpha())
            .step()
            .unwrap();
        assert!(fetch(&l, 3, key).alpha.is_some());
    }

    #[test]
    fn a_fetch_dropped_before_it_ran_resolves_as_a_miss() {
        let (_dir, l) = larder();
        let (job, slot) = AlphaFetchJob::new(l, 3, 7, blake3::hash(b"k"));
        drop(job); // what Pounce does to a job cancelled while queued
        let out = slot.lock().unwrap().take().expect("never stranded");
        assert!(out.alpha.is_none());
    }

    #[test]
    fn a_fetch_that_ran_is_not_overwritten_when_dropped_afterwards() {
        let (_dir, l) = larder();
        let key = blake3::hash(b"bake");
        AlphaStoreJob::new(Arc::clone(&l), 3, key, alpha())
            .step()
            .unwrap();
        let (mut job, slot) = AlphaFetchJob::new(l, 3, 7, key);
        job.step().unwrap();
        let first = slot.lock().unwrap().take().unwrap();
        assert!(first.alpha.is_some());
        drop(job);
        assert!(slot.lock().unwrap().is_none());
    }

    #[test]
    fn a_store_skips_an_alpha_that_is_already_there() {
        let (_dir, l) = larder();
        let key = blake3::hash(b"bake");
        let first = alpha();
        AlphaStoreJob::new(Arc::clone(&l), 3, key, Arc::clone(&first))
            .step()
            .unwrap();
        let before = l.lock().unwrap().stats().unwrap();
        AlphaStoreJob::new(Arc::clone(&l), 3, key, first)
            .step()
            .unwrap();
        assert_eq!(l.lock().unwrap().stats().unwrap(), before);
    }

    #[test]
    fn the_finished_flag_rises_when_a_store_ends_or_is_dropped_unrun() {
        let (_dir, l) = larder();
        let flag = Arc::new(AtomicBool::new(false));
        let mut job = AlphaStoreJob::new(Arc::clone(&l), 3, blake3::hash(b"k"), alpha())
            .with_finished_flag(Arc::clone(&flag));
        assert!(!flag.load(Ordering::SeqCst));
        job.step().unwrap();
        assert!(flag.load(Ordering::SeqCst));

        let flag = Arc::new(AtomicBool::new(false));
        let job = AlphaStoreJob::new(l, 3, blake3::hash(b"k2"), alpha())
            .with_finished_flag(Arc::clone(&flag));
        drop(job); // cancelled while queued
        assert!(flag.load(Ordering::SeqCst), "never strands a waiter");
    }

    #[test]
    fn jobs_declare_cpu_preview_work_at_the_right_priority() {
        let (_dir, l) = larder();
        let (fetch_job, _) = AlphaFetchJob::new(Arc::clone(&l), 1, 1, blake3::hash(b"k"));
        let spec = fetch_job.spec();
        assert_eq!(
            (spec.priority, spec.lane, spec.kind),
            (Priority::Foreground, Lane::Cpu, JobKind::Preview)
        );
        let store = AlphaStoreJob::new(l, 1, blake3::hash(b"k"), alpha());
        let spec = store.spec();
        assert_eq!(
            (spec.priority, spec.lane, spec.kind),
            (Priority::Background, Lane::Cpu, JobKind::Preview)
        );
    }
}
