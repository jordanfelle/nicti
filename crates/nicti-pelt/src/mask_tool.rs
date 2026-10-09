//! The masks tool (#49): the non-UI [`MaskBakeService`] that runs AI mask bakes and downloads their
//! model, and (below it) the panel and viewport gestures.
//!
//! The service is deliberately UI-free, like heal's `RemovalService`, so the whole request -> job ->
//! alpha path is testable without a window:
//!
//! - **Each frame** the app calls [`MaskBakeService::poll`] (collect finished bakes into the
//!   `DevelopView`) and [`MaskBakeService::request_missing`] (submit a job for every AI recipe the
//!   current masks need but don't have). Requests are keyed by `(photo, bake key)`, so a mask and its
//!   inverse share one job, and the same recipe on another photo gets its own.
//! - **Nothing downloads on its own** (ADR-0218). A recipe whose model isn't installed is *not*
//!   submitted; [`MaskBakeService::download_needed`] tells the panel how many bytes the user would
//!   agree to, and only [`MaskBakeService::start_install`] -- wired to a button -- fetches anything.
//! - **A failed bake is remembered**, not retried every frame; the panel offers Retry.
//! - **A result for a photo the user has since left is dropped**, never applied to the wrong one.
//! - **Disk tier (#353, ADR-0353).** With a Larder attached, a missing alpha is first looked up in
//!   the [Stash](crate::stash) (a Foreground `AlphaFetchJob`); only a miss falls through to the
//!   bake, and every finished bake is stored by a Background `AlphaStoreJob` -- also one that
//!   finishes after the user has left the photo, since the work is still valid.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nicti_groom::install::{InstallHandle, InstallModelsJob};
use nicti_groom::FramePixels;
use nicti_pounce::Pounce;
use nicti_siamese::backend::RegistryBackend;
use nicti_siamese::job::{
    MaskBakeJob, MaskBakeOutcome, SharedBackend, Slot, UnloadIdleJob, CANCELLED,
};
use nicti_siamese::providers::segmentation_registry;
use nicti_stalk::models::{self, HttpDownloader, ModelStore, Status};
use nicti_stalk::SegmentationRegistry;
use nicti_tapetum::mask::engine::AiAlpha;

use crate::render::DevelopDoc;
use crate::stash::{AlphaFetchJob, AlphaStoreJob, FetchOutcome};
use crate::t2::SharedLarder;

/// A bake that finished for the photo currently open.
pub struct MaskEvent {
    /// Which bake (`compose::ai_bake_key`); the app reads results back through the `DevelopView`,
    /// so only tests and future status UI look at this.
    #[cfg_attr(not(test), allow(dead_code))]
    pub bake_key: blake3::Hash,
    pub result: Result<Arc<AiAlpha>, String>,
}

pub struct MaskBakeService {
    store: Option<ModelStore>,
    registry: Arc<SegmentationRegistry>,
    backend: Option<SharedBackend>,
    /// An `UnloadIdleJob` is queued or running (#356).
    unload_in_flight: Arc<AtomicBool>,
    install: Option<InstallHandle>,
    /// Whether the running/last install is the optional NVIDIA GPU pack (for the panel's message).
    install_is_gpu_pack: bool,
    pending: Vec<Slot<MaskBakeOutcome>>,
    /// `(photo, bake key)` pairs with a job in flight.
    pending_keys: HashSet<(u64, blake3::Hash)>,
    /// Bakes that failed, by `(photo, bake key)`, so a broken model isn't re-run every frame.
    failed: HashMap<(u64, blake3::Hash), String>,
    /// The disk tier's Larder (#353); `None` = RAM only, exactly as before.
    larder: Option<SharedLarder>,
    /// Catalog id of the photo in Develop, which files its alphas in the Larder. Set by the app
    /// each frame; `None` (no catalog photo, e.g. a synthetic frame) disables the disk tier.
    open_asset: Option<i64>,
    /// Disk lookups in flight.
    fetching: Vec<Slot<FetchOutcome>>,
    /// `(photo, bake key)` pairs whose disk lookup has run and missed: don't ask again, bake. Cleared
    /// when the bake lands or on a hit, so an alpha pruned from RAM (its mask deleted, then undone)
    /// is looked up on disk again rather than re-baked.
    fetched: HashSet<(u64, blake3::Hash)>,
    /// The catalog id each in-flight bake was requested under, to file its alpha by.
    bake_asset: HashMap<(u64, blake3::Hash), i64>,
    /// Finished bakes waiting for a store job; flushed in `request_missing`, which has the Pounce
    /// handle `poll` doesn't.
    to_store: Vec<(i64, blake3::Hash, Arc<AiAlpha>)>,
    /// Bake keys the background pre-bake (#353) has in flight: wait for them to land on disk
    /// instead of running the same model a second time.
    deferred: HashSet<blake3::Hash>,
    /// Test seam: substitute the backend so the whole request -> job -> alpha path runs without
    /// ~1 GB of weights.
    #[cfg(test)]
    backend_override: Option<SharedBackend>,
}

impl Default for MaskBakeService {
    fn default() -> Self {
        Self::new()
    }
}

/// True when any of `artifacts` still has to be downloaded. `NICTI_ORT_DYLIB` (the dev/CI/Linux
/// override) supplies the ONNX Runtime, so when it is set the store's own copy isn't needed and
/// must not make the panel prompt for a download that would be wrong.
fn needs_download(
    artifacts: &[&'static models::Artifact],
    ort_overridden: bool,
    installed: impl Fn(&models::Artifact) -> bool,
) -> bool {
    artifacts
        .iter()
        .copied()
        .filter(|a| !(ort_overridden && a.id == models::ORT_RUNTIME.id))
        .any(|a| !installed(a))
}

impl MaskBakeService {
    /// The store lives at `NICTI_MODELS_DIR` if set (also how a Linux dev drops in models by hand),
    /// else the platform default -- the same place AI removal keeps its models.
    pub fn new() -> Self {
        let root = std::env::var_os("NICTI_MODELS_DIR")
            .filter(|p| !p.is_empty())
            .map(std::path::PathBuf::from)
            .or_else(ModelStore::default_root);
        Self::with_store(root.map(ModelStore::new))
    }

    pub fn with_store(store: Option<ModelStore>) -> Self {
        Self {
            store,
            registry: Arc::new(segmentation_registry()),
            backend: None,
            unload_in_flight: Arc::new(AtomicBool::new(false)),
            install: None,
            install_is_gpu_pack: false,
            pending: Vec::new(),
            pending_keys: HashSet::new(),
            failed: HashMap::new(),
            larder: None,
            open_asset: None,
            fetching: Vec::new(),
            fetched: HashSet::new(),
            bake_asset: HashMap::new(),
            to_store: Vec::new(),
            deferred: HashSet::new(),
            #[cfg(test)]
            backend_override: None,
        }
    }

    /// Attaches (or detaches) the disk tier.
    pub fn set_larder(&mut self, larder: Option<SharedLarder>) {
        self.larder = larder;
    }

    /// The bake keys the pre-bake is working on right now (call every frame). A request for one is
    /// neither fetched nor baked here -- it shows as pending until the pre-bake has stored it, and
    /// then the ordinary disk lookup finds it.
    pub fn set_deferred_keys(&mut self, keys: &HashSet<blake3::Hash>) {
        if &self.deferred != keys {
            self.deferred = keys.clone();
        }
    }

    /// Test seam for modules that drive this service (the pre-bake): substitute the model backend.
    #[cfg(test)]
    pub(crate) fn set_backend_for_test(&mut self, backend: SharedBackend) {
        self.backend_override = Some(backend);
    }

    /// Tells the service which catalog photo is open in Develop (call every frame, cheap).
    pub fn set_open_asset(&mut self, asset_id: Option<i64>) {
        self.open_asset = asset_id;
    }

    /// True when the model `model_id` names still has something to download.
    pub fn model_missing(&self, model_id: &str) -> bool {
        let Some(provider) = self.registry.get(model_id) else {
            return false; // an unknown model is a resolve error at bake time, not a download
        };
        // With the GPU pack's runtime in use, the CPU runtime isn't part of what's needed.
        let gpu_runtime = self
            .store
            .as_ref()
            .is_some_and(|store| store.gpu_runtime_path().is_some());
        let artifacts: Vec<_> = provider
            .artifacts()
            .into_iter()
            .filter(|a| !(gpu_runtime && a.id == models::ORT_RUNTIME.id))
            .collect();
        if artifacts.is_empty() {
            return false;
        }
        let ort_overridden = std::env::var_os("NICTI_ORT_DYLIB").is_some_and(|v| !v.is_empty());
        needs_download(&artifacts, ort_overridden, |a| {
            self.store
                .as_ref()
                .is_some_and(|store| store.status(a) == Status::Installed)
        })
    }

    /// True if a provider is registered under `model_id` (an edit from a newer build, or an
    /// import, may name one this build doesn't have -- the panel shows it as unavailable).
    pub fn knows_model(&self, model_id: &str) -> bool {
        self.registry.get(model_id).is_some()
    }

    /// Bytes the user would download to satisfy the masks that are waiting on a model, or `None`
    /// when nothing waits on one. Drives the panel's "Download model (970 MB)" prompt.
    pub fn download_needed(&self, develop: &DevelopDoc) -> Option<u64> {
        let waiting = develop
            .mask_bake_requests()
            .iter()
            .any(|r| self.model_missing(&r.recipe.model_id));
        waiting.then(|| self.store.as_ref().map_or(0, models::mask_download_bytes))
    }

    pub fn is_installing(&self) -> bool {
        self.install.is_some()
    }

    /// `(downloaded, total)` bytes of the running install.
    pub fn install_progress(&self) -> Option<(u64, u64)> {
        self.install.as_ref().map(|h| {
            (
                h.bytes.load(Ordering::Relaxed),
                h.total.load(Ordering::Relaxed),
            )
        })
    }

    pub fn cancel_install(&self) {
        if let Some(h) = &self.install {
            h.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Submits the explicit, user-initiated download of the AI-mask model (and the ONNX Runtime
    /// where Nicti ships one).
    pub fn start_install(&mut self, pounce: &Pounce) -> Result<(), String> {
        self.start(pounce, false)
    }

    /// Re-verifies the installed model and re-downloads it if corrupt -- the way out when a bake
    /// reports a failed integrity check on a file whose size looks right.
    pub fn start_repair(&mut self, pounce: &Pounce) -> Result<(), String> {
        self.start(pounce, true)
    }

    /// Bytes of the optional NVIDIA GPU pack still to download (#345), or `None` when it isn't
    /// offered: no NVIDIA driver, `NICTI_ORT_DYLIB` supplies the runtime, no model store, or it is
    /// already installed.
    pub fn gpu_pack_offer(&self) -> Option<u64> {
        let store = self.store.as_ref()?;
        let ort_overridden = std::env::var_os("NICTI_ORT_DYLIB").is_some_and(|v| !v.is_empty());
        if ort_overridden || !models::nvidia_driver_present() {
            return None;
        }
        let bytes = models::gpu_pack_download_bytes(store);
        (bytes > 0).then_some(bytes)
    }

    /// Submits the explicit, user-initiated download of the NVIDIA GPU pack. Takes effect on the
    /// next start of Nicti: ONNX Runtime's environment is process-wide and may already be
    /// initialised on the CPU build.
    pub fn start_gpu_pack_install(&mut self, pounce: &Pounce) -> Result<(), String> {
        self.start_with(
            pounce,
            false,
            models::gpu_pack_artifacts(),
            "Download NVIDIA GPU pack",
        )
    }

    /// True if the install that just finished (or is running) is the GPU pack.
    pub fn installing_gpu_pack(&self) -> bool {
        self.install_is_gpu_pack
    }

    fn start(&mut self, pounce: &Pounce, repair: bool) -> Result<(), String> {
        let label = if repair {
            "Repair AI mask model"
        } else {
            "Download AI mask model"
        };
        let store = self
            .store
            .as_ref()
            .ok_or("No place to keep models: couldn't determine a data folder.")?;
        let artifacts = if repair {
            models::mask_repair_artifacts(store)
        } else {
            models::mask_artifacts_for(store)
        };
        self.start_with(pounce, repair, artifacts, label)
    }

    fn start_with(
        &mut self,
        pounce: &Pounce,
        repair: bool,
        artifacts: Vec<&'static models::Artifact>,
        label: &str,
    ) -> Result<(), String> {
        if self.install.is_some() {
            return Ok(());
        }
        let store = self
            .store
            .clone()
            .ok_or("No place to keep models: couldn't determine a data folder.")?;
        self.install_is_gpu_pack = artifacts.iter().any(|a| a.id == models::BIREFNET_FP16.id);
        let (job, handle) = if repair {
            InstallModelsJob::new_repair(store, Arc::new(HttpDownloader), &artifacts)
        } else {
            InstallModelsJob::new(store, Arc::new(HttpDownloader), &artifacts)
        };
        let job = job.labelled(label);
        self.install = Some(handle);
        pounce.submit(Box::new(job));
        Ok(())
    }

    /// Polls the install; `Some` exactly once, when it finishes.
    pub fn poll_install(&mut self) -> Option<Result<(), String>> {
        let done = self
            .install
            .as_ref()
            .and_then(|h| h.result.lock().unwrap().take())?;
        self.install = None;
        if done.is_ok() {
            // The files on disk may have just been replaced (a repair): drop any backend built over
            // the old ones, and let bakes that failed for want of them run again.
            self.backend = None;
            self.failed.clear();
        }
        Some(done)
    }

    /// Frees the loaded AI models once they have been idle for `ttl` (#356) by queueing an
    /// `UnloadIdleJob` on the GPU lane. Returns how long until this is worth calling again (the
    /// caller schedules a repaint then, since an idle window runs no frames), or `None` when
    /// nothing is loaded. Never blocks: a backend mid-bake (its mutex held) is simply busy.
    pub fn poll_idle_unload(
        &mut self,
        pounce: &Pounce,
        now: Instant,
        ttl: Duration,
    ) -> Option<Duration> {
        let backend = self.existing_backend()?;
        if self.unload_in_flight.load(Ordering::SeqCst) {
            return Some(ttl);
        }
        let Ok(guard) = backend.try_lock() else {
            // Mid-bake; it will have been used just now, so look again after a full ttl.
            return Some(ttl);
        };
        let left = guard.idle_unload_in(now, ttl)?;
        drop(guard);
        if left > Duration::ZERO {
            return Some(left);
        }
        // Queued or in-flight bakes would just reload it; wait for them.
        if self.pending_count() > 0 || !self.deferred.is_empty() {
            return Some(ttl);
        }
        pounce.submit(Box::new(UnloadIdleJob::new(
            Arc::clone(backend),
            ttl,
            Arc::clone(&self.unload_in_flight),
        )));
        Some(ttl)
    }

    /// The shared backend if one has been created; unlike `shared_backend` never makes one.
    fn existing_backend(&self) -> Option<&SharedBackend> {
        #[cfg(test)]
        if let Some(b) = &self.backend_override {
            return Some(b);
        }
        self.backend.as_ref()
    }

    /// The shared backend, created (with nothing loaded) on first use.
    pub(crate) fn shared_backend(&mut self) -> SharedBackend {
        #[cfg(test)]
        if let Some(b) = &self.backend_override {
            return Arc::clone(b);
        }
        if let Some(b) = &self.backend {
            return Arc::clone(b);
        }
        // A providers-with-nothing-to-download (the sky heuristic) never touches the store, so a
        // machine with no data folder can still make those masks.
        let store = self
            .store
            .clone()
            .unwrap_or_else(|| ModelStore::new(std::env::temp_dir().join("nicti-no-models")));
        let backend: SharedBackend = Arc::new(Mutex::new(RegistryBackend::new(
            Arc::clone(&self.registry),
            store,
            None,
        )));
        self.backend = Some(Arc::clone(&backend));
        backend
    }

    /// Submits a job for every AI recipe the current masks need but don't have, skipping ones in
    /// flight, ones that already failed on this photo, and ones waiting on a download the user has
    /// not agreed to. Returns how many jobs it submitted.
    pub fn request_missing(&mut self, pounce: &Pounce, develop: &DevelopDoc) -> usize {
        self.flush_stores(pounce);
        // "Before" renders the default document, so its neutral key is not the one the pre-bake and
        // the stored alphas use; asking now would fetch and bake under a key nothing else wants.
        if develop.show_before {
            return 0;
        }
        let image_key = develop.frame_key();
        let mut submitted = 0;
        for request in develop.mask_bake_requests() {
            let id = (image_key, request.key);
            if self.pending_keys.contains(&id)
                || self.failed.contains_key(&id)
                || self.deferred.contains(&request.key)
            {
                continue;
            }
            // Disk first, even for a model that isn't installed: a stored alpha needs no model.
            if let (Some(larder), Some(asset_id)) = (&self.larder, self.open_asset) {
                if !self.fetched.contains(&id) {
                    let (job, slot) =
                        AlphaFetchJob::new(Arc::clone(larder), asset_id, image_key, request.key);
                    self.fetched.insert(id);
                    self.pending_keys.insert(id);
                    self.fetching.push(slot);
                    pounce.submit(Box::new(job));
                    submitted += 1;
                    continue;
                }
            }
            if self.model_missing(&request.recipe.model_id) {
                continue; // waits for an explicit download
            }
            let frame = develop.frame_arc();
            let cam_mul = frame.cam_mul;
            let backend = self.shared_backend();
            let (job, slot) = MaskBakeJob::new(
                backend,
                Arc::new(FramePixels(frame)),
                image_key,
                cam_mul,
                request.recipe,
                request.key,
            );
            self.pending_keys.insert(id);
            if let Some(asset_id) = self.open_asset {
                self.bake_asset.insert(id, asset_id);
            }
            self.pending.push(slot);
            pounce.submit(Box::new(job));
            submitted += 1;
        }
        submitted
    }

    /// Submits a store job for every bake that finished since the last call.
    fn flush_stores(&mut self, pounce: &Pounce) {
        let Some(larder) = &self.larder else {
            self.to_store.clear();
            return;
        };
        for (asset_id, bake_key, alpha) in self.to_store.drain(..) {
            pounce.submit(Box::new(AlphaStoreJob::new(
                Arc::clone(larder),
                asset_id,
                bake_key,
                alpha,
            )));
        }
    }

    /// Collects finished bakes. A success for the open photo is stored in `develop` (and returned);
    /// a failure is remembered and returned; a result for a photo the user has left is dropped.
    pub fn poll(&mut self, develop: &mut DevelopDoc) -> Vec<MaskEvent> {
        let open = develop.frame_key();
        // A recorded miss only means "bake instead" for the photo it was made on; coming back later
        // must look again, since the pre-bake may have stored it meanwhile.
        self.fetched.retain(|(photo, _)| *photo == open);
        let mut events = Vec::new();
        let mut fetched_back = Vec::new();
        self.fetching
            .retain(|slot| match slot.lock().unwrap().take() {
                Some(outcome) => {
                    fetched_back.push(outcome);
                    false
                }
                None => true,
            });
        for outcome in fetched_back {
            let id = (outcome.image_key, outcome.bake_key);
            self.pending_keys.remove(&id);
            // A hit, or a result for a photo the user has left, is done with: forget the lookup so
            // a later request (an undo, coming back) asks the disk again. A miss for the open photo
            // stays recorded, so the next `request_missing` bakes instead of asking again.
            let hit = outcome.alpha.is_some();
            if hit || outcome.image_key != open {
                self.fetched.remove(&id);
            }
            if outcome.image_key != open {
                continue;
            }
            if let Some(alpha) = outcome.alpha {
                develop.set_ai_alpha(outcome.bake_key, Arc::clone(&alpha));
                events.push(MaskEvent {
                    bake_key: outcome.bake_key,
                    result: Ok(alpha),
                });
            }
        }
        let mut finished = Vec::new();
        self.pending
            .retain(|slot| match slot.lock().unwrap().take() {
                Some(outcome) => {
                    finished.push(outcome);
                    false
                }
                None => true,
            });
        for outcome in finished {
            let id = (outcome.image_key, outcome.bake_key);
            self.pending_keys.remove(&id);
            self.fetched.remove(&id);
            // File a successful bake on disk, whichever photo is open now: it is still valid work.
            let asset = self.bake_asset.remove(&id);
            if let (Some(asset_id), Ok(alpha)) = (asset, &outcome.result) {
                self.to_store
                    .push((asset_id, outcome.bake_key, Arc::clone(alpha)));
            }
            if outcome.image_key != open {
                continue;
            }
            // Cancelled before it ran (from the activity panel). Remember it so the per-frame
            // `request_missing` doesn't submit it straight back (which would make a queued bake
            // uncancellable while the panel is open); Retry clears it like any other failure.
            // No event: the user did this on purpose.
            if outcome.result.as_ref().err().map(String::as_str) == Some(CANCELLED) {
                self.failed.insert(
                    (outcome.image_key, outcome.bake_key),
                    "Cancelled".to_owned(),
                );
                continue;
            }
            match &outcome.result {
                Ok(alpha) => develop.set_ai_alpha(outcome.bake_key, Arc::clone(alpha)),
                Err(message) => {
                    self.failed
                        .insert((outcome.image_key, outcome.bake_key), message.clone());
                }
            }
            events.push(MaskEvent {
                bake_key: outcome.bake_key,
                result: outcome.result,
            });
        }
        events
    }

    /// The message of a bake that failed on the open photo, if any.
    pub fn failure_for(&self, image_key: u64, bake_key: &blake3::Hash) -> Option<&str> {
        self.failed.get(&(image_key, *bake_key)).map(String::as_str)
    }

    /// True if any failure on any photo reports a failed model integrity check (the panel then
    /// offers Repair).
    pub fn needs_repair(&self) -> bool {
        self.failed.values().any(|m| m.contains("integrity check"))
    }

    /// True if any bake has failed and not been retried.
    pub fn has_failures(&self) -> bool {
        !self.failed.is_empty()
    }

    /// Forgets every failure so the next `request_missing` tries again (the Retry button).
    pub fn retry_failed(&mut self) {
        self.failed.clear();
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len() + self.fetching.len()
    }

    pub fn is_pending(&self, image_key: u64, bake_key: &blake3::Hash) -> bool {
        self.pending_keys.contains(&(image_key, *bake_key)) || self.deferred.contains(bake_key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::DevelopView;
    use nicti_pounce::JobState;
    use nicti_siamese::backend::{BakeRequest, MaskBackend};
    use nicti_siamese::providers::recipe_for;
    use nicti_stalk::{AlphaMap, SegmentError, SegmentTarget};
    use nicti_tapetum::coat::MaskRecipe;
    use nicti_tapetum::mask::params::{
        LocalAdjust, LocalCorrection, MaskComponent, MaskGroup, MaskParams, MaskSource,
    };
    use nicti_tapetum::stages::MASKS;
    use std::sync::atomic::AtomicUsize;

    struct Fake {
        fail: bool,
        calls: Arc<AtomicUsize>,
    }

    impl MaskBackend for Fake {
        fn bake(&mut self, _req: &BakeRequest<'_>) -> Result<AlphaMap, SegmentError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(SegmentError::Verify(
                    "model failed its integrity check".into(),
                ));
            }
            AlphaMap::new(2, 2, vec![1.0, 1.0, 0.0, 0.0])
        }
    }

    /// A backend that reports a fixed idle time left and counts unloads.
    struct Idle {
        left: Option<Duration>,
        unloads: Arc<AtomicUsize>,
    }

    impl MaskBackend for Idle {
        fn bake(&mut self, _req: &BakeRequest<'_>) -> Result<AlphaMap, SegmentError> {
            unreachable!("no bakes in this test")
        }
        fn idle_unload_in(&self, _now: Instant, _ttl: Duration) -> Option<Duration> {
            self.left
        }
        fn unload_if_idle(&mut self, _now: Instant, _ttl: Duration) -> bool {
            self.unloads.fetch_add(1, Ordering::SeqCst);
            self.left = None;
            true
        }
    }

    const TTL: Duration = Duration::from_secs(60);

    fn idle_service(
        left: Option<Duration>,
    ) -> (MaskBakeService, Arc<Mutex<Idle>>, Arc<AtomicUsize>) {
        let unloads = Arc::new(AtomicUsize::new(0));
        let backend = Arc::new(Mutex::new(Idle {
            left,
            unloads: Arc::clone(&unloads),
        }));
        let mut svc = MaskBakeService::with_store(None);
        svc.backend_override = Some(backend.clone());
        (svc, backend, unloads)
    }

    #[test]
    fn idle_unload_does_nothing_without_a_backend_or_loaded_models() {
        let p = pounce();
        let mut svc = MaskBakeService::with_store(None);
        assert_eq!(svc.poll_idle_unload(&p, Instant::now(), TTL), None);
        let (mut svc, _b, unloads) = idle_service(None);
        assert_eq!(svc.poll_idle_unload(&p, Instant::now(), TTL), None);
        drain(&p);
        assert_eq!(unloads.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn idle_unload_waits_out_the_ttl_and_reports_the_time_left() {
        let p = pounce();
        let (mut svc, _b, unloads) = idle_service(Some(TTL / 2));
        assert_eq!(svc.poll_idle_unload(&p, Instant::now(), TTL), Some(TTL / 2));
        drain(&p);
        assert_eq!(unloads.load(Ordering::SeqCst), 0, "not due: nothing queued");
    }

    #[test]
    fn a_due_idle_unload_runs_once_and_clears_its_flag() {
        let p = pounce();
        let (mut svc, _b, unloads) = idle_service(Some(Duration::ZERO));
        svc.poll_idle_unload(&p, Instant::now(), TTL);
        drain(&p);
        assert_eq!(unloads.load(Ordering::SeqCst), 1);
        assert!(!svc.unload_in_flight.load(Ordering::SeqCst));
        // The fake now reports nothing loaded: no further jobs.
        assert_eq!(svc.poll_idle_unload(&p, Instant::now(), TTL), None);
        drain(&p);
        assert_eq!(unloads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_due_idle_unload_is_not_queued_twice_while_one_is_in_flight() {
        let p = pounce();
        let (mut svc, _b, unloads) = idle_service(Some(Duration::ZERO));
        svc.unload_in_flight.store(true, Ordering::SeqCst);
        svc.poll_idle_unload(&p, Instant::now(), TTL);
        drain(&p);
        assert_eq!(unloads.load(Ordering::SeqCst), 0, "a job is already queued");
    }

    #[test]
    fn a_due_idle_unload_waits_for_pending_or_deferred_bakes() {
        let p = pounce();
        let (mut svc, _b, unloads) = idle_service(Some(Duration::ZERO));
        svc.deferred.insert(blake3::hash(b"prebake in flight"));
        svc.poll_idle_unload(&p, Instant::now(), TTL);
        drain(&p);
        assert_eq!(
            unloads.load(Ordering::SeqCst),
            0,
            "a bake is about to need it"
        );
        svc.deferred.clear();
        svc.pending.push(Arc::new(Mutex::new(None)));
        svc.poll_idle_unload(&p, Instant::now(), TTL);
        drain(&p);
        assert_eq!(unloads.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn idle_unload_never_blocks_on_a_backend_that_is_mid_bake() {
        let p = pounce();
        let (mut svc, backend, unloads) = idle_service(Some(Duration::ZERO));
        let held = backend.lock().unwrap(); // what a running bake does
        assert_eq!(svc.poll_idle_unload(&p, Instant::now(), TTL), Some(TTL));
        drop(held);
        drain(&p);
        assert_eq!(unloads.load(Ordering::SeqCst), 0);
    }

    fn pounce() -> Pounce {
        Pounce::new(u64::MAX, 2, 1, || {})
    }

    fn drain(p: &Pounce) {
        for _ in 0..500 {
            if p.snapshot()
                .iter()
                .all(|s| !matches!(s.state, JobState::Queued | JobState::Running))
            {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("jobs did not finish");
    }

    fn develop() -> Option<DevelopView> {
        crate::test_gpu::shared().map(|gpu| {
            let mut d = DevelopView::new(gpu);
            d.load_real_frame(
                d.frame_arc(),
                blake3::hash(b"photo"),
                nicti_pawprint::EditDocument::default(),
            );
            d
        })
    }

    fn ai(recipe: MaskRecipe, invert: bool) -> LocalCorrection {
        LocalCorrection {
            id: format!("c{invert}"),
            mask: MaskGroup {
                components: vec![MaskComponent {
                    source: MaskSource::Ai(recipe),
                    invert,
                    ..MaskComponent::default()
                }],
            },
            adjust: LocalAdjust {
                exposure: 1.0,
                ..LocalAdjust::default()
            },
            ..LocalCorrection::default()
        }
    }

    fn service(fail: bool) -> (MaskBakeService, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut svc = MaskBakeService::with_store(Some(ModelStore::new(
            std::env::temp_dir().join(format!("nicti-mask-svc-{}", std::process::id())),
        )));
        svc.backend_override = Some(Arc::new(Mutex::new(Fake {
            fail,
            calls: Arc::clone(&calls),
        })));
        (svc, calls)
    }

    fn set_masks(develop: &mut DevelopView, corrections: Vec<LocalCorrection>) {
        develop.set_stage_params(MASKS, &MaskParams { corrections });
    }

    #[test]
    fn a_mask_and_its_inverse_share_one_job_and_the_alpha_lands_in_the_view() {
        let Some(mut develop) = develop() else { return };
        let p = pounce();
        let (mut svc, calls) = service(false);
        // The sky heuristic needs no download, so it is submitted straight away.
        let sky = recipe_for(SegmentTarget::Sky);
        set_masks(&mut develop, vec![ai(sky.clone(), false), ai(sky, true)]);
        assert_eq!(svc.request_missing(&p, &develop), 1, "one recipe, one job");
        assert_eq!(svc.request_missing(&p, &develop), 0, "already in flight");
        drain(&p);
        let events = svc.poll(&mut develop);
        assert_eq!(events.len(), 1);
        assert!(events[0].result.is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(develop.has_ai_alpha(&events[0].bake_key));
        assert!(
            develop.mask_bake_requests().is_empty(),
            "nothing left to bake"
        );
        assert_eq!(svc.request_missing(&p, &develop), 0);
    }

    #[test]
    fn the_pre_bakes_pixel_free_keys_match_what_the_open_photo_computes() {
        // The pre-bake (#353) names a photo's bakes from its document and identity alone. They
        // must be the keys Develop looks for once that photo is open, or the stored alphas would
        // never be found.
        let Some(mut develop) = develop() else { return };
        let identity = blake3::hash(b"photo");
        let sky = recipe_for(SegmentTarget::Sky);
        set_masks(&mut develop, vec![ai(sky, false)]);
        develop.set_stage_params(
            nicti_tapetum::stages::EXPOSURE,
            &nicti_tapetum::coat::ExposureParams { stops: 1.25 },
        );
        let _ = develop.render(); // applies the whole document to the graph
        let doc = develop.document().clone();
        assert_eq!(
            develop.neutral_key(),
            nicti_tapetum::spine::neutral_key(&doc, identity)
        );
        let params: MaskParams = nicti_tapetum::coat::parse(&doc.stages[MASKS].params);
        let expected = nicti_tapetum::mask::compose::bake_requests(
            &params,
            nicti_tapetum::spine::neutral_key(&doc, identity),
        );
        assert_eq!(develop.mask_bake_requests(), expected);
    }

    // --- #353: the disk tier ------------------------------------------------------------

    fn larder() -> (tempfile::TempDir, SharedLarder) {
        use nicti_lair::larder::{Larder, LarderConfig};
        let dir = tempfile::tempdir().unwrap();
        let l = Larder::open(dir.path(), LarderConfig::default()).unwrap();
        (dir, Arc::new(Mutex::new(l)))
    }

    /// One frame of the app's loop: collect, then request (which also flushes stores).
    fn frame(svc: &mut MaskBakeService, p: &Pounce, develop: &mut DevelopView) -> Vec<MaskEvent> {
        drain(p);
        let events = svc.poll(develop);
        svc.request_missing(p, develop);
        events
    }

    fn stored(l: &SharedLarder, asset: i64, key: &blake3::Hash) -> bool {
        l.lock()
            .unwrap()
            .contains_keyed(crate::stash::keyed(asset, key.as_bytes()))
            .unwrap()
    }

    #[test]
    fn a_baked_alpha_is_stored_and_a_fresh_session_loads_it_without_running_the_model() {
        let (Some(mut first), Some(mut second)) = (develop(), develop()) else {
            return;
        };
        let (_dir, l) = larder();
        let p = pounce();
        let sky = recipe_for(SegmentTarget::Sky);
        set_masks(&mut first, vec![ai(sky.clone(), false)]);
        let key = first.mask_bake_requests()[0].key;

        let (mut svc, calls) = service(false);
        svc.set_larder(Some(Arc::clone(&l)));
        svc.set_open_asset(Some(5));
        assert_eq!(
            svc.request_missing(&p, &first),
            1,
            "the disk is asked first"
        );
        assert!(svc.is_pending(first.frame_key(), &key));
        assert!(
            frame(&mut svc, &p, &mut first).is_empty(),
            "a miss is no event"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0, "...and has not baked yet");
        let events = frame(&mut svc, &p, &mut first); // the bake lands; its store is queued
        assert!(events[0].result.is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        drain(&p); // the store job
        assert!(stored(&l, 5, &key));
        let baked = events[0].result.as_ref().unwrap().content_hash;

        // Another run of the app: nothing in RAM, a model that must not be called.
        set_masks(&mut second, vec![ai(sky, false)]);
        let (mut svc2, calls2) = service(false);
        svc2.set_larder(Some(Arc::clone(&l)));
        svc2.set_open_asset(Some(5));
        assert_eq!(svc2.request_missing(&p, &second), 1);
        let events = frame(&mut svc2, &p, &mut second);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].result.as_ref().unwrap().content_hash, baked);
        assert!(second.has_ai_alpha(&key));
        assert_eq!(calls2.load(Ordering::SeqCst), 0, "no model run");
        assert!(second.mask_bake_requests().is_empty());
        assert_eq!(svc2.request_missing(&p, &second), 0);
    }

    #[test]
    fn a_stored_alpha_loads_even_when_its_model_is_not_installed() {
        let Some(mut develop) = develop() else { return };
        let (_dir, l) = larder();
        let p = pounce();
        set_masks(
            &mut develop,
            vec![ai(recipe_for(SegmentTarget::Subject), false)],
        );
        let key = develop.mask_bake_requests()[0].key;
        let a = AiAlpha::quantized(2, 2, vec![1.0, 1.0, 0.0, 0.0]).unwrap();
        l.lock()
            .unwrap()
            .put_keyed(crate::stash::keyed(5, key.as_bytes()), &a.encode())
            .unwrap();
        let (mut svc, calls) = service(false);
        assert!(svc.model_missing(&recipe_for(SegmentTarget::Subject).model_id));
        svc.set_larder(Some(l));
        svc.set_open_asset(Some(5));
        assert_eq!(svc.request_missing(&p, &develop), 1);
        assert_eq!(frame(&mut svc, &p, &mut develop).len(), 1);
        assert!(develop.has_ai_alpha(&key));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(
            svc.download_needed(&develop).is_none(),
            "nothing waits on a model"
        );
    }

    #[test]
    fn a_disk_miss_for_a_missing_model_still_waits_for_the_download() {
        let Some(mut develop) = develop() else { return };
        let (_dir, l) = larder();
        let p = pounce();
        set_masks(
            &mut develop,
            vec![ai(recipe_for(SegmentTarget::Subject), false)],
        );
        let (mut svc, calls) = service(false);
        svc.set_larder(Some(l));
        svc.set_open_asset(Some(5));
        assert_eq!(svc.request_missing(&p, &develop), 1, "the lookup");
        assert!(frame(&mut svc, &p, &mut develop).is_empty());
        for _ in 0..3 {
            assert_eq!(
                svc.request_missing(&p, &develop),
                0,
                "waits, no repeat lookup"
            );
        }
        assert!(svc.download_needed(&develop).is_some());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn an_alpha_pruned_from_ram_comes_back_from_disk_instead_of_re_baking() {
        let Some(mut develop) = develop() else { return };
        let (_dir, l) = larder();
        let p = pounce();
        let masks = vec![ai(recipe_for(SegmentTarget::Sky), false)];
        set_masks(&mut develop, masks.clone());
        let key = develop.mask_bake_requests()[0].key;
        let (mut svc, calls) = service(false);
        svc.set_larder(Some(Arc::clone(&l)));
        svc.set_open_asset(Some(5));
        svc.request_missing(&p, &develop);
        frame(&mut svc, &p, &mut develop); // miss
        frame(&mut svc, &p, &mut develop); // bake lands, store queued
        drain(&p);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // The user deletes the mask (the alpha is pruned), then undoes it.
        set_masks(&mut develop, Vec::new());
        develop.prune_ai_alphas();
        assert!(!develop.has_ai_alpha(&key));
        set_masks(&mut develop, masks);
        assert_eq!(svc.request_missing(&p, &develop), 1, "asks the disk again");
        assert_eq!(frame(&mut svc, &p, &mut develop).len(), 1);
        assert!(develop.has_ai_alpha(&key));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "no second bake");
    }

    #[test]
    fn a_bake_that_lands_after_the_user_left_is_still_stored() {
        let Some(mut develop) = develop() else { return };
        let (_dir, l) = larder();
        let p = pounce();
        set_masks(
            &mut develop,
            vec![ai(recipe_for(SegmentTarget::Sky), false)],
        );
        let key = develop.mask_bake_requests()[0].key;
        let (mut svc, _) = service(false);
        svc.set_larder(Some(Arc::clone(&l)));
        svc.set_open_asset(Some(5));
        svc.request_missing(&p, &develop);
        frame(&mut svc, &p, &mut develop); // the lookup misses; the bake is submitted
                                           // The user moves to another photo before the bake lands.
        develop.load_real_frame(
            develop.frame_arc(),
            blake3::hash(b"another photo"),
            nicti_pawprint::EditDocument::default(),
        );
        svc.set_open_asset(Some(6));
        assert!(
            frame(&mut svc, &p, &mut develop).is_empty(),
            "dropped from the view"
        );
        drain(&p);
        assert!(
            stored(&l, 5, &key),
            "filed under the photo it was baked for"
        );
    }

    #[test]
    fn without_an_open_catalog_photo_the_disk_tier_stays_out_of_the_way() {
        let Some(mut develop) = develop() else { return };
        let (_dir, l) = larder();
        let p = pounce();
        set_masks(
            &mut develop,
            vec![ai(recipe_for(SegmentTarget::Sky), false)],
        );
        let (mut svc, calls) = service(false);
        svc.set_larder(Some(Arc::clone(&l)));
        // No `set_open_asset`: a synthetic frame has no catalog id to file under.
        assert_eq!(svc.request_missing(&p, &develop), 1);
        assert_eq!(frame(&mut svc, &p, &mut develop).len(), 1);
        drain(&p);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(l.lock().unwrap().stats().unwrap().entry_count, 0);
    }

    #[test]
    fn a_result_for_a_photo_the_user_has_left_is_dropped() {
        let Some(mut develop) = develop() else { return };
        let p = pounce();
        let (mut svc, _) = service(false);
        set_masks(
            &mut develop,
            vec![ai(recipe_for(SegmentTarget::Sky), false)],
        );
        assert_eq!(svc.request_missing(&p, &develop), 1);
        // The user moves to another photo before the bake lands.
        develop.load_real_frame(
            develop.frame_arc(),
            blake3::hash(b"another photo"),
            nicti_pawprint::EditDocument::default(),
        );
        drain(&p);
        let events = svc.poll(&mut develop);
        assert!(events.is_empty(), "a stale result must not be reported...");
        assert!(
            develop.mask_bake_requests().is_empty(),
            "(no masks on the new photo)"
        );
        assert_eq!(svc.pending_count(), 0, "...and its slot is released");
    }

    #[test]
    fn a_failed_bake_is_remembered_not_retried_every_frame_until_retry() {
        let Some(mut develop) = develop() else { return };
        let p = pounce();
        let (mut svc, calls) = service(true);
        set_masks(
            &mut develop,
            vec![ai(recipe_for(SegmentTarget::Sky), false)],
        );
        svc.request_missing(&p, &develop);
        drain(&p);
        let events = svc.poll(&mut develop);
        assert!(events[0].result.is_err());
        let key = events[0].bake_key;
        assert!(svc
            .failure_for(develop.frame_key(), &key)
            .unwrap()
            .contains("integrity"));
        assert!(svc.needs_repair(), "an integrity failure offers Repair");

        // Frames pass; the failure is not re-run.
        for _ in 0..3 {
            assert_eq!(svc.request_missing(&p, &develop), 0);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        svc.retry_failed();
        assert_eq!(svc.request_missing(&p, &develop), 1, "Retry runs it again");
        drain(&p);
    }

    #[test]
    fn a_recipe_waiting_on_a_download_is_not_submitted_and_reports_the_size() {
        let Some(mut develop) = develop() else { return };
        let p = pounce();
        let (mut svc, calls) = service(false);
        set_masks(
            &mut develop,
            vec![ai(recipe_for(SegmentTarget::Subject), false)],
        );
        assert_eq!(
            svc.request_missing(&p, &develop),
            0,
            "nothing downloads or runs without the user agreeing"
        );
        let bytes = svc.download_needed(&develop).expect("the model is missing");
        assert!(bytes >= models::BIREFNET.download_size, "{bytes}");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(!svc.is_installing());

        // The heuristic needs no download, so it never asks for one.
        set_masks(
            &mut develop,
            vec![ai(recipe_for(SegmentTarget::Sky), false)],
        );
        assert_eq!(svc.download_needed(&develop), None);
    }

    #[test]
    fn an_overridden_onnx_runtime_is_not_a_download() {
        let arts: [&'static models::Artifact; 2] = [&models::ORT_RUNTIME, &models::BIREFNET];
        let only_model = |a: &models::Artifact| a.id == models::BIREFNET.id;
        assert!(
            needs_download(&arts, false, only_model),
            "the runtime is missing"
        );
        assert!(
            !needs_download(&arts, true, only_model),
            "NICTI_ORT_DYLIB supplies it, so only the model counts"
        );
        assert!(
            needs_download(&arts, true, |_| false),
            "the model is still missing"
        );
        assert!(!needs_download(&arts, false, |_| true));
    }

    #[test]
    fn a_cancelled_bake_stays_cancelled_until_retry_and_is_not_an_event() {
        let Some(mut develop) = develop() else { return };
        let p = pounce();
        let (mut svc, _) = service(false);
        set_masks(
            &mut develop,
            vec![ai(recipe_for(SegmentTarget::Sky), false)],
        );
        let key = develop.mask_bake_requests()[0].key;
        let image = develop.frame_key();
        // What `MaskBakeJob`'s Drop leaves in the slot when Pounce cancels it while still queued.
        let slot: Slot<MaskBakeOutcome> = Arc::new(Mutex::new(Some(MaskBakeOutcome {
            image_key: image,
            bake_key: key,
            result: Err(CANCELLED.to_owned()),
        })));
        svc.pending.push(slot);
        svc.pending_keys.insert((image, key));
        assert_eq!(
            svc.request_missing(&p, &develop),
            0,
            "in flight, not resubmitted"
        );

        let events = svc.poll(&mut develop);
        assert!(events.is_empty(), "a cancel is not reported as an error");
        assert_eq!(svc.pending_count(), 0, "the slot is released");
        assert_eq!(
            svc.request_missing(&p, &develop),
            0,
            "not resubmitted behind the user's back"
        );
        assert!(!svc.needs_repair(), "a cancel is not an integrity failure");
        svc.retry_failed();
        assert_eq!(svc.request_missing(&p, &develop), 1, "Retry runs it again");
        drain(&p);
    }

    #[test]
    fn an_unknown_model_is_not_a_download_and_fails_at_bake_time() {
        let Some(mut develop) = develop() else { return };
        let (svc, _) = service(false);
        let mut r = recipe_for(SegmentTarget::Sky);
        r.model_id = "someone.else_model".into();
        set_masks(&mut develop, vec![ai(r, false)]);
        assert_eq!(svc.download_needed(&develop), None);
    }

    #[test]
    fn with_no_data_folder_a_download_cannot_start_but_says_why() {
        let mut svc = MaskBakeService::with_store(None);
        let p = pounce();
        let err = svc.start_install(&p).unwrap_err();
        assert!(err.contains("data folder"), "{err}");
        assert!(!svc.is_installing());
    }
}
