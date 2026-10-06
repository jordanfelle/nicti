//! Background pre-bake of AI mask alphas (#353, ADR-0353, ADR-0052's deferred follow-up).
//!
//! Syncing or pasting a mask onto a selection only writes edit documents, so each photo would
//! otherwise re-run the model lazily the first time it is opened in Develop (~9 s on the CPU
//! build). After such a batch the app hands the touched photos here, nearest the cursor first, and
//! this service bakes their masks in the background and files the alphas in the [Stash](crate::stash)
//! -- so opening the photo later is a disk read.
//!
//! **One photo at a time.** A decoded full-resolution frame is hundreds of MB, so a photo's chain
//! runs to the end -- including its alphas being on disk -- before the next one starts:
//!
//! 1. `PlanJob` (CPU): read the photo and its document, name the bakes its masks need
//!    ([`spine::neutral_key`] + `compose::bake_requests`, no pixels), drop the ones already stored.
//! 2. `DecodeJob` (CPU): decode the RAW and submit one [`MaskBakeJob`] per remaining recipe.
//! 3. The bakes (GPU lane, **Background** priority, so a mask the user is waiting on always goes
//!    first, and a slider drag pauses them like any background chunk).
//! 4. One `AlphaStoreJob` per finished alpha.
//!
//! **Never downloads a model** (ADR-0218): a recipe whose model isn't installed is skipped. **Never
//! competes with the photo on screen**: that photo is not queued, and while a queued photo's bakes
//! are in flight [`PrebakeService::inflight_keys`] tells `MaskBakeService` to wait for them rather
//! than run the same model twice.
//!
//! Driven from the UI thread like the other services: [`PrebakeService::poll`] once per frame.

use std::collections::{HashSet, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nicti_cornea::RawDecoder;
use nicti_groom::{FramePixels, PixelSource};
use nicti_lair::CatalogStore;
use nicti_pounce::{
    ChunkedJob, JobError, JobKind, JobSpec, Lane, Pounce, Priority, Progress, Step, Submitter,
};
use nicti_siamese::job::{MaskBakeJob, MaskBakeOutcome, Slot, CANCELLED};
use nicti_tapetum::coat;
use nicti_tapetum::mask::compose::{bake_requests, BakeRequest};
use nicti_tapetum::mask::engine::AiAlpha;
use nicti_tapetum::mask::params::MaskParams;
use nicti_tapetum::spine;
use nicti_tapetum::stages::MASKS;

use crate::loupe::asset_cache_key;
use crate::mask_tool::MaskBakeService;
use crate::stash::{keyed, AlphaStoreJob};
use crate::t2::{lock_larder_within, SharedLarder};

const LOCK_WAIT: Duration = Duration::from_secs(2);

/// What the pre-bake needs from the app, cloned into each job.
#[derive(Clone)]
pub struct Env {
    pub decoder: Arc<dyn RawDecoder + Send + Sync>,
    pub store: Arc<dyn CatalogStore + Send + Sync>,
    pub larder: SharedLarder,
    pub submitter: Submitter,
}

/// What one photo needs baked.
struct Plan {
    asset_id: i64,
    identity: blake3::Hash,
    path: PathBuf,
    requests: Vec<BakeRequest>,
}

/// `None` = nothing to do for this photo (no AI masks, all stored, or it couldn't be read).
type PlanOutcome = Option<Plan>;

/// The bakes a decode submitted, or why it didn't.
type DecodeOutcome = Result<Vec<(blake3::Hash, Slot<MaskBakeOutcome>)>, String>;

fn frame_key(identity: &blake3::Hash) -> u64 {
    // The same derivation as `DevelopView::frame_key`.
    u64::from_le_bytes(identity.as_bytes()[..8].try_into().expect("8 bytes"))
}

fn spec(kind: JobKind) -> JobSpec {
    JobSpec {
        priority: Priority::Background,
        kind,
        lane: Lane::Cpu,
        vram_bytes: 0,
        image_index: None,
    }
}

// stage 1: plan -----------------------------------------------------------------------------------

struct PlanJob {
    env: Env,
    asset_id: i64,
    done: bool,
    slot: Slot<PlanOutcome>,
}

impl PlanJob {
    fn run(&self) -> PlanOutcome {
        let store = self.env.store.as_ref();
        let asset = store.get_asset(self.asset_id).ok().flatten()?;
        let root = store.get_root_path(asset.root_id).ok().flatten()?;
        let doc = store.get_master_edit(self.asset_id).ok().flatten()?;
        let identity = asset_cache_key(&asset);
        let params: MaskParams = doc
            .stages
            .get(MASKS)
            .map(|entry| coat::parse(&entry.params))
            .unwrap_or_default();
        let mut requests = bake_requests(&params, spine::neutral_key(&doc, identity));
        if requests.is_empty() {
            return None;
        }
        // A busy Larder (a compaction) skips the photo rather than parking a worker; it is
        // picked up the next time it is synced or opened.
        let larder = lock_larder_within(&self.env.larder, LOCK_WAIT)?;
        requests.retain(|r| {
            !larder
                .contains_keyed(keyed(self.asset_id, r.key.as_bytes()))
                .unwrap_or(false)
        });
        drop(larder);
        (!requests.is_empty()).then(|| Plan {
            asset_id: self.asset_id,
            identity,
            path: PathBuf::from(root).join(&asset.rel_path),
            requests,
        })
    }
}

impl ChunkedJob for PlanJob {
    fn spec(&self) -> JobSpec {
        spec(JobKind::Preview)
    }
    fn label(&self) -> String {
        "Pre-bake masks: plan".to_owned()
    }
    fn progress(&self) -> Progress {
        Progress {
            done: u64::from(self.done),
            total: Some(1),
        }
    }
    /// Never `Err`: Pounce would drop the job without touching the slot.
    fn step(&mut self) -> Result<Step, JobError> {
        let outcome = catch_unwind(AssertUnwindSafe(|| self.run())).unwrap_or(None);
        *self.slot.lock().unwrap() = Some(outcome);
        self.done = true;
        Ok(Step::Done)
    }
}

impl Drop for PlanJob {
    fn drop(&mut self) {
        if !self.done {
            if let Ok(mut slot) = self.slot.lock() {
                slot.get_or_insert(None);
            }
        }
    }
}

// stage 2: decode + submit the bakes --------------------------------------------------------------

struct DecodeJob {
    env: Env,
    plan: Plan,
    backend: nicti_siamese::job::SharedBackend,
    cancelled: Arc<AtomicBool>,
    done: bool,
    slot: Slot<DecodeOutcome>,
}

impl DecodeJob {
    fn run(&self) -> DecodeOutcome {
        if self.cancelled.load(Ordering::SeqCst) {
            return Err(CANCELLED.into());
        }
        let frame = self
            .env
            .decoder
            .decode_linear(&self.plan.path)
            .map(Arc::new)
            .map_err(|e| format!("couldn't decode: {e}"))?;
        // Checked again after the decode, which is the slow part: don't queue bakes nobody wants.
        if self.cancelled.load(Ordering::SeqCst) {
            return Err(CANCELLED.into());
        }
        let key = frame_key(&self.plan.identity);
        let source: Arc<dyn PixelSource> = Arc::new(FramePixels(Arc::clone(&frame)));
        let mut slots = Vec::with_capacity(self.plan.requests.len());
        for request in &self.plan.requests {
            let (job, slot) = MaskBakeJob::new(
                Arc::clone(&self.backend),
                Arc::clone(&source),
                key,
                frame.cam_mul,
                request.recipe.clone(),
                request.key,
            );
            // A refused submit (shutdown) drops the job, which resolves its slot as cancelled.
            let _ = self
                .env
                .submitter
                .submit(Box::new(job.with_priority(Priority::Background)));
            slots.push((request.key, slot));
        }
        Ok(slots)
    }
}

impl ChunkedJob for DecodeJob {
    fn spec(&self) -> JobSpec {
        spec(JobKind::Decode)
    }
    fn label(&self) -> String {
        format!("Pre-bake masks: {}", self.plan.path.display())
    }
    fn progress(&self) -> Progress {
        Progress {
            done: u64::from(self.done),
            total: Some(1),
        }
    }
    fn step(&mut self) -> Result<Step, JobError> {
        let outcome = catch_unwind(AssertUnwindSafe(|| self.run()))
            .unwrap_or_else(|_| Err("decode panicked".to_owned()));
        *self.slot.lock().unwrap() = Some(outcome);
        self.done = true;
        Ok(Step::Done)
    }
}

impl Drop for DecodeJob {
    fn drop(&mut self) {
        if !self.done {
            if let Ok(mut slot) = self.slot.lock() {
                slot.get_or_insert(Err(CANCELLED.to_owned()));
            }
        }
    }
}

/// `asset_ids` ordered nearest the grid cursor first (ties by grid position), so the photos the
/// user is most likely to open next are baked first. Photos not in `grid_ids` (a different filter
/// or folder is showing) go last, in the order given.
pub fn nearest_first(grid_ids: &[i64], cursor: Option<usize>, asset_ids: &[i64]) -> Vec<i64> {
    let wanted: HashSet<i64> = asset_ids.iter().copied().collect();
    let cursor = cursor.unwrap_or(0);
    let mut in_grid: Vec<(usize, i64)> = grid_ids
        .iter()
        .enumerate()
        .filter(|(_, id)| wanted.contains(id))
        .map(|(index, id)| (index, *id))
        .collect();
    in_grid.sort_by_key(|(index, _)| (index.abs_diff(cursor), *index));
    let placed: HashSet<i64> = in_grid.iter().map(|(_, id)| *id).collect();
    let mut out: Vec<i64> = in_grid.into_iter().map(|(_, id)| id).collect();
    out.extend(asset_ids.iter().copied().filter(|id| !placed.contains(id)));
    out
}

// the service -------------------------------------------------------------------------------------

struct Baking {
    key: blake3::Hash,
    slot: Slot<MaskBakeOutcome>,
    alpha: Option<Arc<AiAlpha>>,
    resolved: bool,
    /// The job was cancelled before it ran (the user, from the activity panel).
    cancelled: bool,
}

enum Stage {
    Planning {
        asset_id: i64,
        slot: Slot<PlanOutcome>,
    },
    Decoding {
        asset_id: i64,
        slot: Slot<DecodeOutcome>,
    },
    Baking {
        asset_id: i64,
        bakes: Vec<Baking>,
    },
    Storing {
        flags: Vec<Arc<AtomicBool>>,
    },
}

pub struct PrebakeService {
    env: Option<Env>,
    queue: VecDeque<i64>,
    queued: HashSet<i64>,
    stage: Option<Stage>,
    /// Bake keys of the photo in flight (set once its plan is accepted, until its alphas are on
    /// disk). `MaskBakeService` waits on these instead of baking the same thing.
    inflight: HashSet<blake3::Hash>,
    cancelled: Arc<AtomicBool>,
}

impl PrebakeService {
    /// `None` env (no catalog or no Larder) disables it: [`PrebakeService::enqueue`] does nothing.
    pub fn new(env: Option<Env>) -> Self {
        Self {
            env,
            queue: VecDeque::new(),
            queued: HashSet::new(),
            stage: None,
            inflight: HashSet::new(),
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Queues `asset_ids` in the order given (nearest the cursor first -- the caller orders them),
    /// skipping any already waiting. The photo on screen should not be passed: its own flow
    /// handles it.
    pub fn enqueue(&mut self, asset_ids: impl IntoIterator<Item = i64>) {
        if self.env.is_none() {
            return;
        }
        for id in asset_ids {
            if self.queued.insert(id) {
                self.queue.push_back(id);
            }
        }
    }

    /// True while there is anything queued or running (the app keeps repainting to poll it).
    pub fn busy(&self) -> bool {
        self.stage.is_some() || !self.queue.is_empty()
    }

    /// Photos waiting, not counting the one in flight.
    #[cfg(test)]
    pub fn queued_len(&self) -> usize {
        self.queue.len()
    }

    /// The bake keys of the photo in flight; see [`PrebakeService::inflight`]'s docs.
    pub fn inflight_keys(&self) -> &HashSet<blake3::Hash> {
        &self.inflight
    }

    /// Drops the queue and abandons the photo in flight: its decode (if not yet past it) will not
    /// submit bakes; bakes already submitted finish and their alphas are simply not stored.
    /// Called when the user cancels one of this service's jobs from the activity panel: they are
    /// stopping the pre-bake, not asking to skip a single photo.
    fn cancel_all(&mut self) {
        self.queue.clear();
        self.queued.clear();
        self.stage = None;
        self.inflight.clear();
        self.cancelled.store(true, Ordering::SeqCst);
        // A fresh flag for whatever is enqueued next; the old one stays raised for in-flight jobs.
        self.cancelled = Arc::new(AtomicBool::new(false));
    }

    /// Advances the machine; call once per frame. `open` is the photo in Develop (`(asset id,
    /// identity)`), which is never pre-baked.
    pub fn poll(
        &mut self,
        pounce: &Pounce,
        masks: &mut MaskBakeService,
        open: Option<(i64, blake3::Hash)>,
    ) {
        let Some(env) = self.env.clone() else { return };
        // A finished step can start the next one at once, so a photo with nothing to bake does not
        // cost a frame; bounded by the queue so a long run of them can't spin forever.
        for _ in 0..=self.queue.len() {
            let Some(stage) = self.stage.take() else {
                if !self.start_next(&env, pounce) {
                    return;
                }
                continue;
            };
            match self.advance(stage, &env, pounce, masks, open) {
                Some(stage) => {
                    self.stage = Some(stage);
                    return;
                }
                None => self.inflight.clear(),
            }
        }
    }

    fn start_next(&mut self, env: &Env, pounce: &Pounce) -> bool {
        let Some(asset_id) = self.queue.pop_front() else {
            return false;
        };
        self.queued.remove(&asset_id);
        let slot: Slot<PlanOutcome> = Arc::new(Mutex::new(None));
        pounce.submit(Box::new(PlanJob {
            env: env.clone(),
            asset_id,
            done: false,
            slot: Arc::clone(&slot),
        }));
        self.stage = Some(Stage::Planning { asset_id, slot });
        true
    }

    /// One step of the in-flight photo. `Some` = still going (the stage to keep); `None` = this
    /// photo is finished and the next can start.
    fn advance(
        &mut self,
        stage: Stage,
        env: &Env,
        pounce: &Pounce,
        masks: &mut MaskBakeService,
        open: Option<(i64, blake3::Hash)>,
    ) -> Option<Stage> {
        match stage {
            Stage::Planning { asset_id, slot } => {
                let outcome = slot.lock().unwrap().take();
                let Some(outcome) = outcome else {
                    return Some(Stage::Planning { asset_id, slot });
                };
                let mut plan = outcome?;
                // The user is looking at this photo now; its own flow bakes it.
                if open
                    .is_some_and(|(id, identity)| id == plan.asset_id || identity == plan.identity)
                {
                    return None;
                }
                // Only recipes whose model is already installed: a pre-bake never downloads.
                plan.requests.retain(|r| {
                    masks.knows_model(&r.recipe.model_id)
                        && !masks.model_missing(&r.recipe.model_id)
                });
                if plan.requests.is_empty() {
                    return None;
                }
                self.inflight = plan.requests.iter().map(|r| r.key).collect();
                let result: Slot<DecodeOutcome> = Arc::new(Mutex::new(None));
                pounce.submit(Box::new(DecodeJob {
                    env: env.clone(),
                    backend: masks.shared_backend(),
                    cancelled: Arc::clone(&self.cancelled),
                    plan,
                    done: false,
                    slot: Arc::clone(&result),
                }));
                Some(Stage::Decoding {
                    asset_id,
                    slot: result,
                })
            }
            Stage::Decoding { asset_id, slot } => {
                let outcome = slot.lock().unwrap().take();
                let Some(outcome) = outcome else {
                    return Some(Stage::Decoding { asset_id, slot });
                };
                let slots = match outcome {
                    Ok(slots) => slots,
                    Err(e) if e == CANCELLED => {
                        self.cancel_all();
                        return None;
                    }
                    // A failed decode just ends this photo; it is not retried every sync.
                    Err(_) => return None,
                };
                Some(Stage::Baking {
                    asset_id,
                    bakes: slots
                        .into_iter()
                        .map(|(key, slot)| Baking {
                            key,
                            slot,
                            alpha: None,
                            resolved: false,
                            cancelled: false,
                        })
                        .collect(),
                })
            }
            Stage::Baking {
                asset_id,
                mut bakes,
            } => {
                for bake in bakes.iter_mut().filter(|b| !b.resolved) {
                    if let Some(outcome) = bake.slot.lock().unwrap().take() {
                        bake.resolved = true;
                        bake.cancelled =
                            outcome.result.as_ref().err().map(String::as_str) == Some(CANCELLED);
                        bake.alpha = outcome.result.ok();
                    }
                }
                if bakes.iter().any(|b| !b.resolved) {
                    return Some(Stage::Baking { asset_id, bakes });
                }
                if bakes.iter().any(|b| b.cancelled) {
                    self.cancel_all();
                    return None;
                }
                let flags: Vec<Arc<AtomicBool>> = bakes
                    .into_iter()
                    .filter_map(|b| b.alpha.map(|alpha| (b.key, alpha)))
                    .map(|(key, alpha)| {
                        let flag = Arc::new(AtomicBool::new(false));
                        pounce.submit(Box::new(
                            AlphaStoreJob::new(Arc::clone(&env.larder), asset_id, key, alpha)
                                .with_finished_flag(Arc::clone(&flag)),
                        ));
                        flag
                    })
                    .collect();
                if flags.is_empty() {
                    return None;
                }
                Some(Stage::Storing { flags })
            }
            Stage::Storing { flags } => {
                if flags.iter().all(|f| f.load(Ordering::SeqCst)) {
                    None
                } else {
                    Some(Stage::Storing { flags })
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_claw::Module;
    use nicti_cornea::{DecodeError, LinearFrame};
    use nicti_lair::larder::{Larder, LarderConfig};
    use nicti_lair::{NewAsset, SqliteCatalog};
    use nicti_pawprint::EditDocument;
    use nicti_pounce::JobState;
    use nicti_siamese::backend::{BakeRequest as BackendRequest, MaskBackend};
    use nicti_siamese::providers::recipe_for;
    use nicti_stalk::{AlphaMap, SegmentError, SegmentTarget};
    use nicti_tapetum::mask::params::{
        LocalAdjust, LocalCorrection, MaskComponent, MaskGroup, MaskSource,
    };
    use std::path::Path;
    use std::sync::atomic::AtomicUsize;

    struct FakeDecoder {
        decodes: Arc<AtomicUsize>,
    }

    impl Module for FakeDecoder {
        fn id(&self) -> &str {
            "test.decoder.prebake"
        }
        fn schema_version(&self) -> u32 {
            1
        }
        fn migrate_params(&self, _: u32, _: serde_json::Value) -> Option<serde_json::Value> {
            None
        }
    }

    impl RawDecoder for FakeDecoder {
        fn decode_linear(&self, path: &Path) -> Result<LinearFrame, DecodeError> {
            self.decodes.fetch_add(1, Ordering::SeqCst);
            if path.to_string_lossy().contains("bad") {
                return Err(DecodeError::Io {
                    path: path.to_path_buf(),
                    source: std::io::Error::other("corrupt file"),
                });
            }
            Ok(crate::render::synthetic_linear_frame())
        }
    }

    struct FakeBackend {
        calls: Arc<AtomicUsize>,
    }

    impl MaskBackend for FakeBackend {
        fn bake(&mut self, _: &BackendRequest<'_>) -> Result<AlphaMap, SegmentError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            AlphaMap::new(2, 2, vec![1.0, 1.0, 0.0, 0.0])
        }
    }

    struct Fx {
        svc: PrebakeService,
        masks: MaskBakeService,
        pounce: Pounce,
        store: Arc<SqliteCatalog>,
        larder: SharedLarder,
        decodes: Arc<AtomicUsize>,
        bakes: Arc<AtomicUsize>,
        _dir: tempfile::TempDir,
        root: i64,
    }

    fn fx() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let larder: SharedLarder = Arc::new(Mutex::new(
            Larder::open(&dir.path().join("larder"), LarderConfig::default()).unwrap(),
        ));
        let store = Arc::new(SqliteCatalog::open_in_memory().unwrap());
        let volume = store.upsert_volume("v", None, None, 0).unwrap();
        let root = store
            .ensure_root(volume, &dir.path().to_string_lossy())
            .unwrap();
        let pounce = Pounce::new(u64::MAX, 2, 2, || {});
        let decodes = Arc::new(AtomicUsize::new(0));
        let bakes = Arc::new(AtomicUsize::new(0));
        let env = Env {
            decoder: Arc::new(FakeDecoder {
                decodes: Arc::clone(&decodes),
            }),
            store: store.clone(),
            larder: Arc::clone(&larder),
            submitter: pounce.submitter(),
        };
        let mut masks = MaskBakeService::with_store(None);
        masks.set_backend_for_test(Arc::new(Mutex::new(FakeBackend {
            calls: Arc::clone(&bakes),
        })));
        Fx {
            svc: PrebakeService::new(Some(env)),
            masks,
            pounce,
            store,
            larder,
            decodes,
            bakes,
            _dir: dir,
            root,
        }
    }

    impl Fx {
        fn add(&self, name: &str, doc: Option<EditDocument>) -> i64 {
            std::fs::write(self._dir.path().join(name), b"x").unwrap();
            let id = self
                .store
                .insert_asset(
                    self.root,
                    &NewAsset {
                        rel_path: name.into(),
                        rel_path_fold: name.to_lowercase(),
                        size_bytes: 1,
                        mtime_unix: 1,
                        fingerprint: Some(format!("fp-{name}")),
                        natural_key: None,
                        make: None,
                        model: None,
                        captured_at: None,
                        width: Some(64),
                        height: Some(64),
                        imported_at: 0,
                    },
                    None,
                )
                .unwrap();
            if let Some(doc) = doc {
                self.store.put_master_edit(id, &doc).unwrap();
            }
            id
        }

        /// Polls until the service goes idle (and every job has finished).
        fn run(&mut self, open: Option<(i64, blake3::Hash)>) {
            let start = std::time::Instant::now();
            while self.svc.busy() {
                self.svc.poll(&self.pounce, &mut self.masks, open);
                assert!(start.elapsed() < Duration::from_secs(30), "never finished");
                std::thread::sleep(Duration::from_millis(2));
            }
            for _ in 0..500 {
                if self
                    .pounce
                    .snapshot()
                    .iter()
                    .all(|s| !matches!(s.state, JobState::Queued | JobState::Running))
                {
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            panic!("jobs did not finish");
        }

        fn stored(&self, asset: i64, key: &blake3::Hash) -> bool {
            self.larder
                .lock()
                .unwrap()
                .contains_keyed(keyed(asset, key.as_bytes()))
                .unwrap()
        }
    }

    fn ai_doc(target: SegmentTarget) -> EditDocument {
        let mut doc = EditDocument::default();
        let params = MaskParams {
            corrections: vec![LocalCorrection {
                id: "c".into(),
                mask: MaskGroup {
                    components: vec![MaskComponent {
                        source: MaskSource::Ai(recipe_for(target)),
                        ..MaskComponent::default()
                    }],
                },
                adjust: LocalAdjust::default(),
                ..LocalCorrection::default()
            }],
        };
        doc.stages.insert(
            MASKS.to_string(),
            nicti_pawprint::StageEntry {
                schema_version: 1,
                params: serde_json::to_value(params).unwrap(),
            },
        );
        doc
    }

    /// The bake key a photo's document names, computed the way the foreground does.
    fn key_of(f: &Fx, asset: i64, doc: &EditDocument) -> blake3::Hash {
        let a = f.store.get_asset(asset).unwrap().unwrap();
        let params: MaskParams = coat::parse(&doc.stages[MASKS].params);
        bake_requests(&params, spine::neutral_key(doc, asset_cache_key(&a)))[0].key
    }

    #[test]
    fn a_synced_photo_is_baked_in_the_background_and_its_alpha_stored() {
        let mut f = fx();
        let doc = ai_doc(SegmentTarget::Sky);
        let id = f.add("a.NEF", Some(doc.clone()));
        let key = key_of(&f, id, &doc);
        f.svc.enqueue([id]);
        assert!(f.svc.busy());
        f.run(None);
        assert_eq!(f.bakes.load(Ordering::SeqCst), 1);
        assert!(f.stored(id, &key));
        assert!(!f.svc.busy());
        assert!(f.svc.inflight_keys().is_empty(), "released once stored");
    }

    #[test]
    fn photos_with_no_ai_masks_or_nothing_left_to_bake_cost_no_decode() {
        let mut f = fx();
        let plain = f.add("plain.NEF", Some(EditDocument::default()));
        let unedited = f.add("unedited.NEF", None);
        let doc = ai_doc(SegmentTarget::Sky);
        let done = f.add("done.NEF", Some(doc.clone()));
        // Already on disk: a previous run (or the foreground) stored it.
        let key = key_of(&f, done, &doc);
        f.larder
            .lock()
            .unwrap()
            .put_keyed(keyed(done, key.as_bytes()), b"stored earlier")
            .unwrap();
        f.svc
            .enqueue([plain, unedited, done, 9999 /* not in the catalog */]);
        f.run(None);
        assert_eq!(f.decodes.load(Ordering::SeqCst), 0);
        assert_eq!(f.bakes.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn it_works_through_the_queue_one_photo_at_a_time_in_the_order_given() {
        let mut f = fx();
        let docs: Vec<EditDocument> = (0..3).map(|_| ai_doc(SegmentTarget::Sky)).collect();
        let ids: Vec<i64> = ["c.NEF", "a.NEF", "b.NEF"]
            .iter()
            .zip(&docs)
            .map(|(n, d)| f.add(n, Some(d.clone())))
            .collect();
        f.svc.enqueue(ids.clone());
        f.svc.enqueue(ids.clone()); // already waiting: not queued twice
        assert_eq!(f.svc.queued_len(), 3);
        let mut max_inflight = 0;
        let start = std::time::Instant::now();
        while f.svc.busy() {
            f.svc.poll(&f.pounce, &mut f.masks, None);
            // The queue drains one photo per chain, never several in flight.
            max_inflight = max_inflight.max(f.svc.inflight_keys().len());
            assert!(start.elapsed() < Duration::from_secs(30));
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(max_inflight, 1, "one photo (one recipe) at a time");
        assert_eq!(f.decodes.load(Ordering::SeqCst), 3);
        for (id, doc) in ids.iter().zip(&docs) {
            assert!(f.stored(*id, &key_of(&f, *id, doc)));
        }
    }

    #[test]
    fn the_photo_on_screen_is_never_pre_baked() {
        let mut f = fx();
        let doc = ai_doc(SegmentTarget::Sky);
        let id = f.add("a.NEF", Some(doc.clone()));
        let identity = asset_cache_key(&f.store.get_asset(id).unwrap().unwrap());
        f.svc.enqueue([id]);
        f.run(Some((id, identity))); // the user opened it before its turn came
        assert_eq!(f.decodes.load(Ordering::SeqCst), 0);
        assert!(!f.stored(id, &key_of(&f, id, &doc)));
    }

    #[test]
    fn a_model_that_is_not_installed_is_skipped_never_downloaded() {
        let mut f = fx();
        // BiRefNet isn't in this test's (absent) store.
        let id = f.add("a.NEF", Some(ai_doc(SegmentTarget::Subject)));
        assert!(f
            .masks
            .model_missing(&recipe_for(SegmentTarget::Subject).model_id));
        f.svc.enqueue([id]);
        f.run(None);
        assert_eq!(f.decodes.load(Ordering::SeqCst), 0);
        assert!(!f.masks.is_installing());
    }

    #[test]
    fn a_photo_that_will_not_decode_is_dropped_and_the_queue_continues() {
        let mut f = fx();
        let doc = ai_doc(SegmentTarget::Sky);
        let bad = f.add("bad.NEF", Some(doc.clone()));
        let good = f.add("good.NEF", Some(doc.clone()));
        f.svc.enqueue([bad, good]);
        f.run(None);
        assert_eq!(f.decodes.load(Ordering::SeqCst), 2);
        assert!(!f.stored(bad, &key_of(&f, bad, &doc)));
        assert!(f.stored(good, &key_of(&f, good, &doc)));
    }

    #[test]
    fn the_foreground_waits_on_the_inflight_photo_instead_of_baking_it_twice() {
        let mut f = fx();
        let doc = ai_doc(SegmentTarget::Sky);
        let id = f.add("a.NEF", Some(doc.clone()));
        let key = key_of(&f, id, &doc);
        f.svc.enqueue([id]);
        let start = std::time::Instant::now();
        // Poll until the plan was accepted and the keys are announced.
        while f.svc.inflight_keys().is_empty() {
            f.svc.poll(&f.pounce, &mut f.masks, None);
            assert!(start.elapsed() < Duration::from_secs(30));
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(f.svc.inflight_keys().contains(&key));
        f.masks.set_deferred_keys(f.svc.inflight_keys());
        assert!(
            f.masks.is_pending(0, &key),
            "shown as in progress, not idle"
        );
        f.run(None);
        f.masks.set_deferred_keys(f.svc.inflight_keys());
        assert!(!f.masks.is_pending(0, &key), "released once it is on disk");
    }

    #[test]
    fn cancel_all_drops_the_queue_and_the_inflight_claim() {
        let mut f = fx();
        let doc = ai_doc(SegmentTarget::Sky);
        let a = f.add("a.NEF", Some(doc.clone()));
        let b = f.add("b.NEF", Some(doc));
        f.svc.enqueue([a, b]);
        f.svc.poll(&f.pounce, &mut f.masks, None);
        f.svc.cancel_all();
        assert!(!f.svc.busy());
        assert!(f.svc.inflight_keys().is_empty());
        // Let the abandoned plan/decode jobs finish, then nothing further is queued.
        f.run(None);
        assert_eq!(f.svc.queued_len(), 0);
    }

    #[test]
    fn photos_are_ordered_nearest_the_cursor_first_and_off_grid_ones_last() {
        let grid = [10, 11, 12, 13, 14, 15, 16];
        // Cursor on index 4 (id 14). Distances: 12 -> 2, 16 -> 2 (index tie: 12 first), 11 -> 3.
        assert_eq!(
            nearest_first(&grid, Some(4), &[11, 16, 12, 99, 14]),
            vec![14, 12, 16, 11, 99]
        );
        // No cursor behaves as index 0.
        assert_eq!(nearest_first(&grid, None, &[15, 11]), vec![11, 15]);
        // Nothing in the grid: the order given.
        assert_eq!(nearest_first(&[], Some(3), &[5, 4, 6]), vec![5, 4, 6]);
        assert!(nearest_first(&grid, Some(0), &[]).is_empty());
    }

    #[test]
    fn a_user_cancelling_a_decode_or_a_bake_stops_the_whole_queue() {
        let mut f = fx();
        let (env, pounce) = (f.svc.env.clone().unwrap(), &f.pounce);
        // Decode cancelled from the activity panel.
        f.svc.queue.extend([1, 2, 3]);
        f.svc.queued.extend([1, 2, 3]);
        let slot: Slot<DecodeOutcome> = Arc::new(Mutex::new(Some(Err(CANCELLED.into()))));
        let next = f.svc.advance(
            Stage::Decoding { asset_id: 9, slot },
            &env,
            pounce,
            &mut f.masks,
            None,
        );
        assert!(next.is_none());
        assert_eq!(
            f.svc.queued_len(),
            0,
            "the rest of the queue is dropped too"
        );

        // A bake cancelled from the activity panel.
        f.svc.queue.extend([4, 5]);
        f.svc.queued.extend([4, 5]);
        let slot: Slot<MaskBakeOutcome> = Arc::new(Mutex::new(Some(MaskBakeOutcome {
            image_key: 0,
            bake_key: blake3::hash(b"k"),
            result: Err(CANCELLED.to_owned()),
        })));
        let bakes = vec![Baking {
            key: blake3::hash(b"k"),
            slot,
            alpha: None,
            resolved: false,
            cancelled: false,
        }];
        let next = f.svc.advance(
            Stage::Baking { asset_id: 9, bakes },
            &env,
            pounce,
            &mut f.masks,
            None,
        );
        assert!(next.is_none());
        assert_eq!(f.svc.queued_len(), 0);

        // A decode that merely failed (a corrupt file) only skips its own photo.
        f.svc.queue.extend([6, 7]);
        f.svc.queued.extend([6, 7]);
        let slot: Slot<DecodeOutcome> = Arc::new(Mutex::new(Some(Err("couldn't decode".into()))));
        assert!(f
            .svc
            .advance(
                Stage::Decoding { asset_id: 9, slot },
                &env,
                pounce,
                &mut f.masks,
                None
            )
            .is_none());
        assert_eq!(f.svc.queued_len(), 2, "the queue carries on");
    }

    #[test]
    fn with_no_env_it_is_inert() {
        let mut svc = PrebakeService::new(None);
        svc.enqueue([1, 2, 3]);
        assert!(!svc.busy());
    }
}
