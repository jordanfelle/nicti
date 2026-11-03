//! The real ONNX-backed removal backend, loaded lazily.
//!
//! Loading LaMa (~200 MB) and the two MobileSAM sessions takes seconds, so it must not happen on
//! the UI thread or at app start for a user who never removes anything. [`LazyBackend`] holds the
//! resolved model paths and builds the [`RemovalEngine`] on the first `remove`, which runs inside a
//! [`crate::job::RemoveJob`] on Pounce's GPU-lane worker thread. A failed load is retried on the
//! next removal (the user may have fixed the install) rather than cached as a permanent failure.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nicti_haw::ExecutionProvider;
use nicti_stalk::models::RemovalModels;
use nicti_tapetum::heal::RemovalPatch;

use crate::lama::Lama;
use crate::provider;
use crate::remove::{RemovalBackend, RemovalEngine, RemovalRequest};
use crate::sam::MobileSam;
use crate::RemovalError;

/// First cool-down after a GPU provider failure. Each further consecutive failure doubles it, up
/// to `GPU_RETRY_MAX`, so a GPU that is permanently broken (cuDNN missing) costs one double model
/// load per hour at worst rather than one every few minutes.
const GPU_RETRY_BASE: Duration = Duration::from_secs(300);
const GPU_RETRY_MAX: Duration = Duration::from_secs(3600);

/// Substrings of an ONNX Runtime error message that mean "this GPU provider can't run" (missing
/// cuDNN/cuBLAS, out of memory, device lost/suspended), as opposed to a deterministic bad-input or
/// shape-mismatch error that would fail identically on the CPU. The DirectML wording does not say
/// "directml": its errors name `DmlFusedNode`, `DmlCommandRecorder` and HRESULT texts.
const PROVIDER_FAILURE_MARKERS: &[&str] = &[
    "cudnn",
    "cublas",
    "cuda",
    "dml",
    "d3d12",
    "dxgi",
    "hresult",
    "gpu device",
    "gpu will not respond",
    "execution provider",
    "out of memory",
    "not enough memory",
    "e_outofmemory",
    "failed to allocate",
    "memory allocation",
    "bfcarena",
    "device lost",
    "device removed",
    "device hung",
    "suspended",
];

/// True when `msg` (an `Ort` error's text) says the GPU provider itself failed.
fn is_provider_failure(msg: &str) -> bool {
    let msg = msg.to_ascii_lowercase();
    PROVIDER_FAILURE_MARKERS.iter().any(|m| msg.contains(m))
}

/// Tracks "running on the CPU after a GPU failure, try the GPU again at `retry_at`". Time is passed
/// in so the transitions are unit-testable without models.
#[derive(Debug, Default)]
struct Demotion {
    retry_at: Option<Instant>,
    /// Consecutive failed GPU attempts; drives the backoff.
    failures: u32,
}

impl Demotion {
    fn cooldown(&self) -> Duration {
        GPU_RETRY_BASE
            .saturating_mul(1u32 << self.failures.min(8))
            .min(GPU_RETRY_MAX)
    }

    /// The GPU just failed (a provider error, a failed load, or a silent fall back to the CPU).
    fn demote(&mut self, now: Instant) {
        self.retry_at = Some(now + self.cooldown());
        self.failures = self.failures.saturating_add(1);
    }

    /// True once it is time to try the GPU again; the caller then reloads, and `demote`s again if
    /// that attempt fails.
    fn take_due(&mut self, now: Instant) -> bool {
        match self.retry_at {
            Some(t) if now >= t => {
                self.retry_at = None;
                true
            }
            _ => false,
        }
    }

    /// A removal succeeded on the GPU.
    fn recovered(&mut self) {
        *self = Self::default();
    }

    fn is_demoted(&self) -> bool {
        self.retry_at.is_some()
    }
}

/// A check run before the sessions are loaded (see [`LazyBackend::verified_by`]).
pub type Verifier = Box<dyn Fn() -> Result<(), String> + Send>;

/// Builds the removal engine on a requested provider. The seam that lets the fallback/retry logic
/// in [`LazyBackend`] be tested without ONNX models: production uses [`RealLoader`].
pub trait EngineLoader: Send {
    /// The provider a load should ask for first (the runtime's, unless `NICTI_ORT_EP` overrides).
    fn requested(&self, models: &RemovalModels) -> ExecutionProvider;

    /// Loads an engine on `requested` and reports the provider it actually came up on (a session
    /// can quietly fall back to the CPU).
    fn load(
        &mut self,
        models: &RemovalModels,
        requested: ExecutionProvider,
    ) -> Result<(Box<dyn RemovalBackend + Send>, ExecutionProvider), RemovalError>;
}

/// The production loader: MobileSAM + LaMa over ONNX Runtime.
pub struct RealLoader;

impl EngineLoader for RealLoader {
    fn requested(&self, models: &RemovalModels) -> ExecutionProvider {
        provider::requested_for(&models.ort_dylib)
    }

    fn load(
        &mut self,
        models: &RemovalModels,
        requested: ExecutionProvider,
    ) -> Result<(Box<dyn RemovalBackend + Send>, ExecutionProvider), RemovalError> {
        let mut sam = MobileSam::load_on(
            &models.sam_encoder,
            &models.sam_decoder,
            &models.ort_dylib,
            requested,
        )?;
        let mut lama = Lama::load_on(&models.lama, &models.ort_dylib, requested)?;
        if sam.active_provider() != lama.active_provider() {
            // One fell back to the CPU on its own (a model that won't load on the GPU provider):
            // run the pair on the CPU so the retry guard and the VRAM claim describe reality.
            drop((sam, lama));
            sam = MobileSam::load_on(
                &models.sam_encoder,
                &models.sam_decoder,
                &models.ort_dylib,
                ExecutionProvider::Cpu,
            )?;
            lama = Lama::load_on(&models.lama, &models.ort_dylib, ExecutionProvider::Cpu)?;
        }
        let active = sam.active_provider();
        Ok((Box::new(RemovalEngine::new(sam, lama)), active))
    }
}

pub struct LazyBackend {
    models: RemovalModels,
    verify: Option<Verifier>,
    loader: Box<dyn EngineLoader>,
    engine: Option<Box<dyn RemovalBackend + Send>>,
    /// The provider the loaded sessions run on (CPU until loaded).
    active: ExecutionProvider,
    /// While the backend runs on the CPU after a GPU failure: when to try the GPU provider again.
    demotion: Demotion,
    /// Mirrors `demotion.is_demoted()` for the UI, which cannot lock the backend while a removal
    /// holds it. Updated at the end of every `remove`.
    demoted_flag: Arc<AtomicBool>,
}

impl LazyBackend {
    pub fn new(models: RemovalModels) -> Self {
        Self::with_loader(models, Box::new(RealLoader))
    }

    /// A backend that builds its engines with `loader` (tests use a stub).
    pub fn with_loader(models: RemovalModels, loader: Box<dyn EngineLoader>) -> Self {
        Self {
            models,
            verify: None,
            loader,
            engine: None,
            active: ExecutionProvider::Cpu,
            demotion: Demotion::default(),
            demoted_flag: Arc::new(AtomicBool::new(false)),
        }
    }

    /// A handle the UI polls to show "running on the CPU after a GPU error" (#599): `true` while
    /// the backend is demoted and waiting out its cool-down. Safe to read while a removal runs.
    pub fn demoted_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.demoted_flag)
    }

    /// Runs `verify` before the sessions are first loaded -- on the job's worker thread, so
    /// hashing hundreds of MB never blocks the UI. It is a `Fn`, not `FnOnce`, and runs on every
    /// load attempt while the engine is still unloaded: a failed check can't be bypassed just by
    /// trying again.
    pub fn verified_by(mut self, verify: impl Fn() -> Result<(), String> + Send + 'static) -> Self {
        self.verify = Some(Box::new(verify));
        self
    }

    /// True while removals run on the CPU because the GPU provider failed (the UI can surface it).
    pub fn is_demoted(&self) -> bool {
        self.demotion.is_demoted()
    }

    /// True once the sessions have been loaded.
    pub fn is_loaded(&self) -> bool {
        self.engine.is_some()
    }
}

impl LazyBackend {
    /// Replaces the engine with fresh sessions on `requested`. The old engine is dropped first, so
    /// a failed load leaves a consistent "nothing loaded, nothing declared" state (the next
    /// removal loads from scratch) rather than a stale GPU engine, and the new sessions never
    /// coexist with the old ones in VRAM.
    fn load_engine(&mut self, requested: ExecutionProvider) -> Result<(), RemovalError> {
        self.engine = None;
        self.active = ExecutionProvider::Cpu;
        provider::declare(ExecutionProvider::Cpu);
        let (engine, active) = self.loader.load(&self.models, requested)?;
        self.active = active;
        provider::declare(self.active);
        self.engine = Some(engine);
        Ok(())
    }
}

impl LazyBackend {
    /// Loads on `requested`. If the GPU provider can't be used -- the load errors, or the sessions
    /// quietly came up on the CPU -- the backend ends up on a working CPU engine and is demoted
    /// (with backoff) rather than failing the removal or forgetting to try the GPU again.
    fn load_preferred(&mut self, requested: ExecutionProvider) -> Result<(), RemovalError> {
        if requested == ExecutionProvider::Cpu {
            return self.load_engine(ExecutionProvider::Cpu);
        }
        match self.load_engine(requested) {
            Ok(()) if self.active == ExecutionProvider::Cpu => {
                self.demotion.demote(Instant::now());
                Ok(())
            }
            Ok(()) => Ok(()),
            Err(e) => {
                eprintln!("nicti-groom: loading on {requested:?} failed ({e}); using the CPU");
                self.load_engine(ExecutionProvider::Cpu)?;
                self.demotion.demote(Instant::now());
                Ok(())
            }
        }
    }
}

impl Drop for LazyBackend {
    fn drop(&mut self) {
        // The sessions go with the backend; so does their claim on the VRAM budget.
        if self.active != ExecutionProvider::Cpu {
            provider::declare(ExecutionProvider::Cpu);
        }
    }
}

impl RemovalBackend for LazyBackend {
    fn remove(&mut self, req: &RemovalRequest<'_>) -> Result<RemovalPatch, RemovalError> {
        let result = self.remove_inner(req);
        self.demoted_flag
            .store(self.demotion.is_demoted(), Ordering::Relaxed);
        result
    }
}

impl LazyBackend {
    fn remove_inner(&mut self, req: &RemovalRequest<'_>) -> Result<RemovalPatch, RemovalError> {
        if self.engine.is_none() {
            if let Some(verify) = &self.verify {
                verify().map_err(RemovalError::Integrity)?;
            }
            let requested = self.loader.requested(&self.models);
            self.load_preferred(requested)?;
        } else if self.demotion.take_due(Instant::now()) {
            let requested = self.loader.requested(&self.models);
            self.load_preferred(requested)?;
        }
        let result = match self.engine.as_mut().expect("just loaded").remove(req) {
            // Only an error that says the GPU provider can't run (cuDNN missing from the pack, out
            // of VRAM beside the renderer, a driver reset) falls back: redo this removal on the CPU
            // and stay there until the cool-down passes. A deterministic model/input error is
            // returned as-is and leaves the GPU engine in place.
            Err(RemovalError::Ort(e))
                if self.active != ExecutionProvider::Cpu && is_provider_failure(&e) =>
            {
                eprintln!(
                    "nicti-groom: removal failed on {:?} ({e}); using the CPU",
                    self.active
                );
                self.load_engine(ExecutionProvider::Cpu)?;
                self.demotion.demote(Instant::now());
                self.engine.as_mut().expect("just loaded").remove(req)
            }
            other => {
                if other.is_ok() && self.active != ExecutionProvider::Cpu {
                    self.demotion.recovered();
                }
                other
            }
        };
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sam::Prompt;
    use crate::RgbBuffer;
    use std::path::PathBuf;

    fn request(img: &RgbBuffer) -> RemovalRequest<'_> {
        RemovalRequest {
            image_key: 1,
            source: img,
            cam_mul: [1.0; 4],
            prompt: Prompt::Click { x: 4.0, y: 4.0 },
            center: (4.0, 4.0),
            radius: 3.0,
        }
    }

    #[test]
    fn only_gpu_provider_failures_trigger_the_cpu_fallback() {
        for msg in [
            "CUDA failure 35: cudnn64_9.dll not found",
            "Failed to allocate memory: out of memory",
            "DXGI_ERROR_DEVICE_REMOVED",
            "Error in Execution Provider",
        ] {
            assert!(is_provider_failure(msg), "{msg}");
        }
        for msg in [
            "Got invalid dimensions for input: image for the following indices",
            "Non-zero status code returned while running Conv node",
        ] {
            assert!(!is_provider_failure(msg), "{msg}");
        }
    }

    #[test]
    fn directml_and_arena_failures_are_provider_failures() {
        for msg in [
            "Non-zero status code returned while running DmlFusedNode_0_0 node. 887A0005 The GPU device instance has been suspended",
            "E_OUTOFMEMORY: Not enough memory resources are available to complete this operation",
            "BFCArena::AllocateRawInternal Available memory of 0 is smaller than requested bytes",
        ] {
            assert!(is_provider_failure(msg), "{msg}");
        }
    }

    #[test]
    fn demotion_waits_out_the_cooldown_then_backs_off_on_repeat_failures() {
        let t0 = Instant::now();
        let mut d = Demotion::default();
        assert!(!d.is_demoted() && !d.take_due(t0));
        d.demote(t0);
        assert!(d.is_demoted());
        assert!(!d.take_due(t0 + GPU_RETRY_BASE - Duration::from_secs(1)));
        assert!(d.take_due(t0 + GPU_RETRY_BASE));
        assert!(
            !d.is_demoted(),
            "due clears the timer; the reload re-demotes on failure"
        );
        // The retry failed again: the next wait is doubled, and capped.
        d.demote(t0);
        assert_eq!(d.retry_at, Some(t0 + GPU_RETRY_BASE * 2));
        for _ in 0..20 {
            d.demote(t0);
        }
        assert_eq!(d.retry_at, Some(t0 + GPU_RETRY_MAX));
    }

    #[test]
    fn a_gpu_success_resets_the_backoff() {
        let t0 = Instant::now();
        let mut d = Demotion::default();
        d.demote(t0);
        d.demote(t0);
        d.recovered();
        assert!(!d.is_demoted());
        d.demote(t0);
        assert_eq!(d.retry_at, Some(t0 + GPU_RETRY_BASE));
    }

    #[test]
    fn a_fresh_backend_is_not_demoted() {
        assert!(!LazyBackend::new(fake_models()).is_demoted());
    }

    fn tiny() -> RgbBuffer {
        RgbBuffer {
            width: 8,
            height: 8,
            data: vec![[0.1; 3]; 64],
        }
    }

    fn fake_models() -> RemovalModels {
        RemovalModels {
            ort_dylib: PathBuf::from("/nonexistent/libonnxruntime.so"),
            gpu_runtime: false,
            sam_encoder: PathBuf::from("/nonexistent/enc.onnx"),
            sam_decoder: PathBuf::from("/nonexistent/dec.onnx"),
            lama: PathBuf::from("/nonexistent/lama.onnx"),
        }
    }

    #[test]
    fn a_failed_integrity_check_stops_the_load_and_cannot_be_bypassed_by_retrying() {
        let mut backend = LazyBackend::new(fake_models()).verified_by(|| Err("tampered".into()));
        let img = tiny();
        for _ in 0..3 {
            // Integrity, not ModelNotFound: the verifier must run *before* any file is touched.
            assert!(matches!(
                backend.remove(&request(&img)),
                Err(RemovalError::Integrity(m)) if m == "tampered"
            ));
            assert!(!backend.is_loaded());
        }
    }

    #[test]
    fn a_passing_integrity_check_lets_loading_proceed_to_the_real_files() {
        let ran = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = ran.clone();
        let mut backend = LazyBackend::new(fake_models()).verified_by(move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        });
        assert!(matches!(
            backend.remove(&request(&tiny())),
            Err(RemovalError::ModelNotFound(_))
        ));
        assert_eq!(ran.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[test]
    fn missing_models_fail_cleanly_and_stay_unloaded_so_a_later_install_can_succeed() {
        let mut backend = LazyBackend::new(RemovalModels {
            ort_dylib: PathBuf::from("/nonexistent/libonnxruntime.so"),
            gpu_runtime: false,
            sam_encoder: PathBuf::from("/nonexistent/enc.onnx"),
            sam_decoder: PathBuf::from("/nonexistent/dec.onnx"),
            lama: PathBuf::from("/nonexistent/lama.onnx"),
        });
        let img = RgbBuffer {
            width: 8,
            height: 8,
            data: vec![[0.1; 3]; 64],
        };
        let req = RemovalRequest {
            image_key: 1,
            source: &img,
            cam_mul: [1.0; 4],
            prompt: Prompt::Click { x: 4.0, y: 4.0 },
            center: (4.0, 4.0),
            radius: 3.0,
        };
        for _ in 0..2 {
            assert!(matches!(
                backend.remove(&req),
                Err(RemovalError::ModelNotFound(_))
            ));
            assert!(!backend.is_loaded());
        }
    }

    /// A scripted engine: fails with `error` (an ORT message) or returns a 1x1 patch.
    struct StubEngine {
        error: Option<&'static str>,
    }

    impl RemovalBackend for StubEngine {
        fn remove(&mut self, _req: &RemovalRequest<'_>) -> Result<RemovalPatch, RemovalError> {
            match self.error {
                Some(e) => Err(RemovalError::Ort(e.to_string())),
                None => Ok(RemovalPatch::new((0, 0), 1, vec![[0.0; 4]]).unwrap()),
            }
        }
    }

    /// Asks for CUDA; its GPU engines fail with `gpu_error`, its CPU engines always work.
    /// `loads` records every provider a load was made on.
    struct StubLoader {
        gpu_error: Option<&'static str>,
        loads: Arc<std::sync::Mutex<Vec<ExecutionProvider>>>,
    }

    impl EngineLoader for StubLoader {
        fn requested(&self, _models: &RemovalModels) -> ExecutionProvider {
            ExecutionProvider::Cuda
        }

        fn load(
            &mut self,
            _models: &RemovalModels,
            requested: ExecutionProvider,
        ) -> Result<(Box<dyn RemovalBackend + Send>, ExecutionProvider), RemovalError> {
            self.loads.lock().unwrap().push(requested);
            let error = if requested == ExecutionProvider::Cpu {
                None
            } else {
                self.gpu_error
            };
            Ok((Box::new(StubEngine { error }), requested))
        }
    }

    fn stub_backend(
        gpu_error: Option<&'static str>,
    ) -> (LazyBackend, Arc<std::sync::Mutex<Vec<ExecutionProvider>>>) {
        let loads = Arc::new(std::sync::Mutex::new(Vec::new()));
        let backend = LazyBackend::with_loader(
            fake_models(),
            Box::new(StubLoader {
                gpu_error,
                loads: Arc::clone(&loads),
            }),
        );
        (backend, loads)
    }

    #[test]
    fn a_provider_failure_redoes_the_removal_on_the_cpu_and_raises_the_demoted_flag() {
        let (mut backend, loads) = stub_backend(Some("CUDA failure 35: cudnn64_9.dll not found"));
        let flag = backend.demoted_flag();
        let img = tiny();
        assert!(!flag.load(Ordering::Relaxed));
        assert!(backend.remove(&request(&img)).is_ok());
        assert_eq!(
            *loads.lock().unwrap(),
            vec![ExecutionProvider::Cuda, ExecutionProvider::Cpu]
        );
        assert!(backend.is_demoted());
        assert!(flag.load(Ordering::Relaxed));
        // Within the cool-down it stays on the CPU engine: no further load.
        assert!(backend.remove(&request(&img)).is_ok());
        assert_eq!(loads.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_deterministic_error_leaves_the_gpu_engine_alone() {
        let (mut backend, loads) = stub_backend(Some(
            "Got invalid dimensions for input: image for the following indices",
        ));
        let img = tiny();
        assert!(matches!(
            backend.remove(&request(&img)),
            Err(RemovalError::Ort(_))
        ));
        assert_eq!(*loads.lock().unwrap(), vec![ExecutionProvider::Cuda]);
        assert!(!backend.is_demoted());
        assert!(!backend.demoted_flag().load(Ordering::Relaxed));
        assert_eq!(backend.active, ExecutionProvider::Cuda);
    }

    #[test]
    fn a_working_gpu_never_demotes() {
        let (mut backend, loads) = stub_backend(None);
        let img = tiny();
        assert!(backend.remove(&request(&img)).is_ok());
        assert_eq!(*loads.lock().unwrap(), vec![ExecutionProvider::Cuda]);
        assert!(!backend.demoted_flag().load(Ordering::Relaxed));
    }
}
