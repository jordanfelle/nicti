//! The real ONNX-backed removal backend, loaded lazily.
//!
//! Loading LaMa (~200 MB) and the two MobileSAM sessions takes seconds, so it must not happen on
//! the UI thread or at app start for a user who never removes anything. [`LazyBackend`] holds the
//! resolved model paths and builds the [`RemovalEngine`] on the first `remove`, which runs inside a
//! [`crate::job::RemoveJob`] on Pounce's GPU-lane worker thread. A failed load is retried on the
//! next removal (the user may have fixed the install) rather than cached as a permanent failure.

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

pub struct LazyBackend {
    models: RemovalModels,
    verify: Option<Verifier>,
    engine: Option<RemovalEngine<MobileSam, Lama>>,
    /// The provider the loaded sessions run on (CPU until loaded).
    active: ExecutionProvider,
    /// While the backend runs on the CPU after a GPU failure: when to try the GPU provider again.
    demotion: Demotion,
}

impl LazyBackend {
    pub fn new(models: RemovalModels) -> Self {
        Self {
            models,
            verify: None,
            engine: None,
            active: ExecutionProvider::Cpu,
            demotion: Demotion::default(),
        }
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
        let models = &self.models;
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
        self.active = sam.active_provider();
        provider::declare(self.active);
        self.engine = Some(RemovalEngine::new(sam, lama));
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
        if self.engine.is_none() {
            if let Some(verify) = &self.verify {
                verify().map_err(RemovalError::Integrity)?;
            }
            let requested = provider::requested_for(&self.models.ort_dylib);
            self.load_preferred(requested)?;
        } else if self.demotion.take_due(Instant::now()) {
            let requested = provider::requested_for(&self.models.ort_dylib);
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
}
