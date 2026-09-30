//! The real ONNX-backed removal backend, loaded lazily.
//!
//! Loading LaMa (~200 MB) and the two MobileSAM sessions takes seconds, so it must not happen on
//! the UI thread or at app start for a user who never removes anything. [`LazyBackend`] holds the
//! resolved model paths and builds the [`RemovalEngine`] on the first `remove`, which runs inside a
//! [`crate::job::RemoveJob`] on Pounce's GPU-lane worker thread. A failed load is retried on the
//! next removal (the user may have fixed the install) rather than cached as a permanent failure.

use nicti_stalk::models::RemovalModels;
use nicti_tapetum::heal::RemovalPatch;

use crate::lama::Lama;
use crate::remove::{RemovalBackend, RemovalEngine, RemovalRequest};
use crate::sam::MobileSam;
use crate::RemovalError;

pub struct LazyBackend {
    models: RemovalModels,
    engine: Option<RemovalEngine<MobileSam, Lama>>,
}

impl LazyBackend {
    pub fn new(models: RemovalModels) -> Self {
        Self {
            models,
            engine: None,
        }
    }

    /// True once the sessions have been loaded.
    pub fn is_loaded(&self) -> bool {
        self.engine.is_some()
    }
}

impl RemovalBackend for LazyBackend {
    fn remove(&mut self, req: &RemovalRequest<'_>) -> Result<RemovalPatch, RemovalError> {
        if self.engine.is_none() {
            let sam = MobileSam::load(
                &self.models.sam_encoder,
                &self.models.sam_decoder,
                &self.models.ort_dylib,
            )?;
            let lama = Lama::load(&self.models.lama, &self.models.ort_dylib)?;
            self.engine = Some(RemovalEngine::new(sam, lama));
        }
        self.engine.as_mut().expect("just loaded").remove(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sam::Prompt;
    use crate::RgbBuffer;
    use std::path::PathBuf;

    #[test]
    fn missing_models_fail_cleanly_and_stay_unloaded_so_a_later_install_can_succeed() {
        let mut backend = LazyBackend::new(RemovalModels {
            ort_dylib: PathBuf::from("/nonexistent/libonnxruntime.so"),
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
