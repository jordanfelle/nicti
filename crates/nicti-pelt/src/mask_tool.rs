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

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use nicti_groom::install::{InstallHandle, InstallModelsJob};
use nicti_groom::FramePixels;
use nicti_pounce::Pounce;
use nicti_siamese::backend::RegistryBackend;
use nicti_siamese::job::{MaskBakeJob, MaskBakeOutcome, SharedBackend, Slot, CANCELLED};
use nicti_siamese::providers::segmentation_registry;
use nicti_stalk::models::{self, HttpDownloader, ModelStore, Status};
use nicti_stalk::SegmentationRegistry;
use nicti_tapetum::mask::engine::AiAlpha;

use crate::render::DevelopView;

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
    install: Option<InstallHandle>,
    pending: Vec<Slot<MaskBakeOutcome>>,
    /// `(photo, bake key)` pairs with a job in flight.
    pending_keys: HashSet<(u64, blake3::Hash)>,
    /// Bakes that failed, by `(photo, bake key)`, so a broken model isn't re-run every frame.
    failed: HashMap<(u64, blake3::Hash), String>,
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
            install: None,
            pending: Vec::new(),
            pending_keys: HashSet::new(),
            failed: HashMap::new(),
            #[cfg(test)]
            backend_override: None,
        }
    }

    /// True when the model `model_id` names still has something to download.
    pub fn model_missing(&self, model_id: &str) -> bool {
        let Some(provider) = self.registry.get(model_id) else {
            return false; // an unknown model is a resolve error at bake time, not a download
        };
        let artifacts = provider.artifacts();
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
    pub fn download_needed(&self, develop: &DevelopView) -> Option<u64> {
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

    fn start(&mut self, pounce: &Pounce, repair: bool) -> Result<(), String> {
        if self.install.is_some() {
            return Ok(());
        }
        let store = self
            .store
            .clone()
            .ok_or("No place to keep models: couldn't determine a data folder.")?;
        let artifacts = models::mask_artifacts();
        let (job, handle) = if repair {
            InstallModelsJob::new_repair(store, Arc::new(HttpDownloader), &artifacts)
        } else {
            InstallModelsJob::new(store, Arc::new(HttpDownloader), &artifacts)
        };
        let job = job.labelled(if repair {
            "Repair AI mask model"
        } else {
            "Download AI mask model"
        });
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

    /// The shared backend, created (with nothing loaded) on first use.
    fn shared_backend(&mut self) -> SharedBackend {
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
    pub fn request_missing(&mut self, pounce: &Pounce, develop: &DevelopView) -> usize {
        let image_key = develop.frame_key();
        let mut submitted = 0;
        for request in develop.mask_bake_requests() {
            let id = (image_key, request.key);
            if self.pending_keys.contains(&id) || self.failed.contains_key(&id) {
                continue;
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
            self.pending.push(slot);
            pounce.submit(Box::new(job));
            submitted += 1;
        }
        submitted
    }

    /// Collects finished bakes. A success for the open photo is stored in `develop` (and returned);
    /// a failure is remembered and returned; a result for a photo the user has left is dropped.
    pub fn poll(&mut self, develop: &mut DevelopView) -> Vec<MaskEvent> {
        let open = develop.frame_key();
        let mut events = Vec::new();
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
            self.pending_keys
                .remove(&(outcome.image_key, outcome.bake_key));
            if outcome.image_key != open {
                continue;
            }
            // Cancelled before it ran (from the activity panel): not a model failure, so don't
            // remember it as one -- the next `request_missing` simply submits it again.
            if outcome.result.as_ref().err().map(String::as_str) == Some(CANCELLED) {
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
        self.pending.len()
    }

    pub fn is_pending(&self, image_key: u64, bake_key: &blake3::Hash) -> bool {
        self.pending_keys.contains(&(image_key, *bake_key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn a_bake_cancelled_before_it_ran_is_not_a_failure_and_can_be_resubmitted() {
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
        assert!(
            svc.failure_for(image, &key).is_none(),
            "and never remembered as one"
        );
        assert_eq!(svc.pending_count(), 0, "the slot is released");
        assert_eq!(
            svc.request_missing(&p, &develop),
            1,
            "so it simply runs again"
        );
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
