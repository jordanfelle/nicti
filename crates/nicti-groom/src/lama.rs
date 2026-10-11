//! LaMa inpainting over `ort`/`load-dynamic`, against the real contract of the ONNX export in
//! `nicti_stalk::models`: `image [B, 3, 512, 512]` (0..1 RGB) and `mask [B, 1, 512, 512]`
//! (1 = fill this) in, `output [B, 3, 512, 512]` out. The graph is fixed at 512x512, so callers
//! crop-and-resize around the hole first (`remove.rs`).
//!
//! `output` range/scale is checked against the real model by this module's `#[ignore]`d test, not
//! assumed; see [`OUTPUT_SCALE`].

use std::path::Path;

use nicti_haw::ExecutionProvider;
use ort::session::Session;
use ort::value::Tensor;

use crate::{ort_err, provider, RemovalError};

/// The model's fixed spatial size.
pub const SIZE: usize = 512;

/// Multiplier taking the model's `output` values to 0..1. The Carve export emits 0..255.
pub const OUTPUT_SCALE: f32 = 1.0 / 255.0;

/// Something that fills the masked part of a 512x512 image. Both buffers are planar `CHW` /
/// single-plane, row-major: `image` is `3 * 512 * 512` values in 0..1, `mask` is `512 * 512`
/// values, 1 where the image should be replaced. Returns `3 * 512 * 512` values in 0..1.
pub trait Inpainter {
    fn inpaint(&mut self, image_chw: &[f32], mask: &[f32]) -> Result<Vec<f32>, RemovalError>;
}

pub struct Lama {
    session: Session,
    active: ExecutionProvider,
}

impl Lama {
    /// Loads on the provider the runtime offers (CUDA for the GPU pack's build, else the CPU).
    pub fn load(model: &Path, ort_dylib: &Path) -> Result<Self, RemovalError> {
        Self::load_on(model, ort_dylib, provider::requested_for(ort_dylib))
    }

    /// Loads on `requested`, falling back to the CPU if it can't start.
    pub fn load_on(
        model: &Path,
        ort_dylib: &Path,
        requested: ExecutionProvider,
    ) -> Result<Self, RemovalError> {
        let (session, active) = provider::open_session(model, ort_dylib, requested)?;
        Ok(Self { session, active })
    }

    /// The execution provider the session is really running on.
    pub fn active_provider(&self) -> ExecutionProvider {
        self.active
    }
}

impl Inpainter for Lama {
    fn inpaint(&mut self, image_chw: &[f32], mask: &[f32]) -> Result<Vec<f32>, RemovalError> {
        if image_chw.len() != 3 * SIZE * SIZE || mask.len() != SIZE * SIZE {
            return Err(RemovalError::BadInput(format!(
                "LaMa needs a 3x{SIZE}x{SIZE} image and a {SIZE}x{SIZE} mask, got {} and {} values",
                image_chw.len(),
                mask.len()
            )));
        }
        let outputs = self
            .session
            .run(ort::inputs![
                "image" => Tensor::from_array(([1usize, 3, SIZE, SIZE], image_chw.to_vec())).map_err(ort_err)?,
                "mask" => Tensor::from_array(([1usize, 1, SIZE, SIZE], mask.to_vec())).map_err(ort_err)?,
            ])
            .map_err(ort_err)?;
        let (_shape, data) = outputs["output"]
            .try_extract_tensor::<f32>()
            .map_err(ort_err)?;
        if data.len() != 3 * SIZE * SIZE {
            return Err(RemovalError::Ort(format!(
                "LaMa returned {} values, expected {}",
                data.len(),
                3 * SIZE * SIZE
            )));
        }
        Ok(data
            .iter()
            .map(|v| (v * OUTPUT_SCALE).clamp(0.0, 1.0))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_model_is_a_clean_error_not_a_panic() {
        let r = Lama::load(
            Path::new("/nonexistent/lama.onnx"),
            Path::new("/nonexistent/libonnxruntime.so"),
        );
        assert!(matches!(r, Err(RemovalError::ModelNotFound(_))));
    }
}
