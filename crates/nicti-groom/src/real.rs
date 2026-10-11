//! The real ONNX-backed removal backend, loaded lazily.
//!
//! Loading LaMa (~200 MB) and the two MobileSAM sessions takes seconds, so it must not happen on
//! the UI thread or at app start for a user who never removes anything. [`LazyBackend`] holds the
//! resolved model paths and builds the [`RemovalEngine`] on the first `remove`, which runs inside a
//! [`crate::job::RemoveJob`] on Pounce's GPU-lane worker thread. A failed load is retried on the
//! next removal (the user may have fixed the install) rather than cached as a permanent failure.

use nicti_haw::ExecutionProvider;
use nicti_stalk::models::RemovalModels;
use nicti_tapetum::heal::RemovalPatch;

use crate::lama::Lama;
use crate::provider;
use crate::remove::{RemovalBackend, RemovalEngine, RemovalRequest};
use crate::sam::MobileSam;
use crate::RemovalError;

/// A check run before the sessions are loaded (see [`LazyBackend::verified_by`]).
pub type Verifier = Box<dyn Fn() -> Result<(), String> + Send>;

pub struct LazyBackend {
    models: RemovalModels,
    verify: Option<Verifier>,
    engine: Option<RemovalEngine<MobileSam, Lama>>,
    /// The provider the loaded sessions run on (CPU until loaded).
    active: ExecutionProvider,
}

impl LazyBackend {
    pub fn new(models: RemovalModels) -> Self {
        Self {
            models,
            verify: None,
            engine: None,
            active: ExecutionProvider::Cpu,
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
            self.load_engine(requested)?;
        }
        match self.engine.as_mut().expect("just loaded").remove(req) {
            // Any `Ort` error counts, not only GPU ones (the variant carries no origin): a
            // deterministic model/input bug costs one wasted CPU retry and returns the CPU's error.
            // The GPU provider registered but can't run (cuDNN missing from the pack, out of VRAM
            // beside the renderer, a driver reset): redo this removal -- and every later one -- on
            // the CPU rather than failing it.
            Err(RemovalError::Ort(e)) if self.active != ExecutionProvider::Cpu => {
                eprintln!(
                    "nicti-groom: removal failed on {:?} ({e}); using the CPU",
                    self.active
                );
                self.load_engine(ExecutionProvider::Cpu)?;
                self.engine.as_mut().expect("just loaded").remove(req)
            }
            other => other,
        }
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
