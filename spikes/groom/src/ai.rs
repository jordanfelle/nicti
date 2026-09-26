//! AI-removal scaffolding: thin `ort`/`load-dynamic` wrapper structs for a future MobileSAM
//! selector and LaMa inpainter, per ADR-0004 §3's already-decided pattern
//! (`ort::init_from(path)`, deferring the native ONNX Runtime library's own load rather than
//! linking it at build/startup time).
//!
//! **No real ONNX weights exist in this sandbox** (obtaining actual LaMa/MobileSAM checkpoints
//! is out of scope for this spike -- see `docs/research/groom-healing-removal.md`'s licensing
//! findings for why that's not just a download-size problem). Both wrappers therefore:
//!
//! - check the model file's existence up front and return a clean `Err` (never panic) when it's
//!   missing -- proven in this module's `model_missing_*` tests, which run in CI with no model
//!   file present;
//! - when a model file *is* present, actually attempt to initialize the ONNX Runtime environment
//!   and load a real `ort::session::Session` from it, per the real `ort` API -- proven, but only
//!   as far as this sandbox can prove it, in the `#[ignore]`d `runs_a_real_model_if_present`
//!   tests below, which need both a real model file and a real ONNX Runtime shared library on
//!   disk to actually execute.
//!
//! **Simplification, stated up front:** real MobileSAM is a two-stage encoder+decoder ONNX pair
//! (image encoder; then a decoder taking the encoder's embedding plus point/box prompt tensors,
//! a low-res mask hint, and `orig_im_size`), and real LaMa takes a paired image+mask input. This
//! spike collapses both to "one ONNX session, one named input tensor, one named output tensor" --
//! enough to prove the `ort`-load-dynamic loading/error-handling shape this ticket is actually
//! scoped to, without inventing a multi-input contract this sandbox has no real model to validate
//! against. Wiring the real multi-input contract is #51's job once actual weights are obtained.
//!
//! **Same caveat applies to outputs, and it's a sharper risk than the input side:** both
//! `segment()`/`inpaint()` unconditionally take `outputs[0]`. Real MobileSAM decoder exports
//! typically return several named outputs (masks, IoU predictions, low-res mask logits) in an
//! order that varies by exporter, and real LaMa exports may do the same. If a real model is
//! later dropped in without updating this code, `outputs[0]` might not be the mask/image tensor
//! at all -- this would return a shape-plausible `Mask`/inpainted crop full of garbage rather
//! than an `Err`, since nothing here validates output identity beyond its raw shape. This is
//! unverified against a real model in this sandbox (none exists here); #51 must confirm the real
//! output ordering/naming before trusting `outputs[0]`, not just assume this spike's shape proves
//! it.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use ort::session::Session;
use ort::value::Tensor;

#[derive(Debug, thiserror::Error)]
pub enum GroomAiError {
    #[error("model file not found: {0}")]
    ModelNotFound(PathBuf),
    #[error("ONNX Runtime error: {0}")]
    Ort(String),
}

fn ort_err(e: impl std::fmt::Display) -> GroomAiError {
    GroomAiError::Ort(e.to_string())
}

/// Initializes the global `ort` environment exactly once, per ADR-0004 §3's `ort::init_from`
/// pattern -- `load-dynamic` means this must run before any other `ort` API call, and must not
/// re-run per session (the environment is process-global). `dylib_path` points at the ONNX
/// Runtime shared library itself (`libonnxruntime.so`/`.dylib`/`onnxruntime.dll`), which is a
/// *different* file from either wrapper's own `model_path` (the `.onnx` model file) -- this is
/// the same "native runtime loads on demand, distinct from the Rust wrapper's own laziness"
/// distinction ADR-0004 §3 draws.
fn ensure_ort_environment(dylib_path: &Path) -> Result<(), GroomAiError> {
    static INIT: OnceLock<Result<(), String>> = OnceLock::new();
    let result = INIT.get_or_init(|| {
        let builder =
            ort::init_from(dylib_path.to_string_lossy().into_owned()).map_err(|e| e.to_string())?;
        if builder.commit() {
            Ok(())
        } else {
            Err("ort environment commit() returned false".to_string())
        }
    });
    result.clone().map_err(GroomAiError::Ort)
}

/// Loads a session from `model_path`, after confirming it exists (`ModelNotFound` is returned
/// cleanly, never a panic or an opaque `ort` I/O error) and initializing the ONNX Runtime
/// environment from `ort_dylib_path`.
fn load_session(model_path: &Path, ort_dylib_path: &Path) -> Result<Session, GroomAiError> {
    if !model_path.is_file() {
        return Err(GroomAiError::ModelNotFound(model_path.to_path_buf()));
    }
    ensure_ort_environment(ort_dylib_path)?;
    Session::builder()
        .map_err(ort_err)?
        .commit_from_file(model_path)
        .map_err(ort_err)
}

/// A click or box prompt for MobileSAM-style segmentation, in the source image's own pixel
/// coordinates.
#[derive(Debug, Clone, Copy)]
pub enum Prompt {
    Click { x: f32, y: f32 },
    Box { x0: f32, y0: f32, x1: f32, y1: f32 },
}

/// A single-channel mask, row-major, `1.0` = selected.
#[derive(Debug, Clone)]
pub struct Mask {
    pub width: usize,
    pub height: usize,
    pub data: Vec<f32>,
}

/// Thin wrapper around a MobileSAM ONNX model. See this module's doc comment for the
/// single-input/single-output simplification this spike makes.
pub struct MobileSamSelector {
    model_path: PathBuf,
    ort_dylib_path: PathBuf,
}

impl MobileSamSelector {
    pub fn new(model_path: impl Into<PathBuf>, ort_dylib_path: impl Into<PathBuf>) -> Self {
        Self {
            model_path: model_path.into(),
            ort_dylib_path: ort_dylib_path.into(),
        }
    }

    /// Segments `image` (RGB, row-major, `width * height * 3` `f32`s in `[0, 1]`) given `prompt`.
    /// Real MobileSAM's decoder needs point/box coordinates as *separate* named input tensors
    /// alongside the image embedding -- this spike instead concatenates the prompt into a fixed
    /// low-dimensional header ahead of the flattened image, matching the "one input tensor"
    /// simplification stated at module level; a real integration replaces `encode_input` with the
    /// model's actual documented input contract.
    pub fn segment(
        &self,
        image: &[f32],
        width: usize,
        height: usize,
        prompt: Prompt,
    ) -> Result<Mask, GroomAiError> {
        let mut session = load_session(&self.model_path, &self.ort_dylib_path)?;
        let input = encode_prompted_image(image, width, height, prompt);
        let tensor = Tensor::from_array(([1usize, input.len()], input)).map_err(ort_err)?;
        let outputs = session.run(ort::inputs![tensor]).map_err(ort_err)?;
        let (_shape, data) = outputs[0].try_extract_tensor::<f32>().map_err(ort_err)?;
        Ok(Mask {
            width,
            height,
            data: data.to_vec(),
        })
    }
}

fn encode_prompted_image(image: &[f32], width: usize, height: usize, prompt: Prompt) -> Vec<f32> {
    let mut out = Vec::with_capacity(image.len() + 5);
    match prompt {
        Prompt::Click { x, y } => out.extend_from_slice(&[0.0, x, y, 0.0, 0.0]),
        Prompt::Box { x0, y0, x1, y1 } => out.extend_from_slice(&[1.0, x0, y0, x1, y1]),
    }
    debug_assert_eq!(image.len(), width * height * 3);
    out.extend_from_slice(image);
    out
}

/// Thin wrapper around a LaMa-style ONNX inpainting model. Real LaMa takes an image tensor and a
/// mask tensor as two separate named inputs; this spike concatenates them into one input tensor
/// (mask channel appended after RGB), same simplification rationale as `MobileSamSelector`.
pub struct LamaInpainter {
    model_path: PathBuf,
    ort_dylib_path: PathBuf,
}

impl LamaInpainter {
    pub fn new(model_path: impl Into<PathBuf>, ort_dylib_path: impl Into<PathBuf>) -> Self {
        Self {
            model_path: model_path.into(),
            ort_dylib_path: ort_dylib_path.into(),
        }
    }

    /// Inpaints `image` (RGB, `width * height * 3` `f32`s) where `mask` (`width * height` `f32`s,
    /// `1.0` = hole) marks the region to fill, returning the inpainted RGB crop at the same
    /// dimensions.
    pub fn inpaint(
        &self,
        image: &[f32],
        mask: &[f32],
        width: usize,
        height: usize,
    ) -> Result<Vec<f32>, GroomAiError> {
        debug_assert_eq!(image.len(), width * height * 3);
        debug_assert_eq!(mask.len(), width * height);
        let mut session = load_session(&self.model_path, &self.ort_dylib_path)?;
        let mut input = Vec::with_capacity(image.len() + mask.len());
        input.extend_from_slice(image);
        input.extend_from_slice(mask);
        let tensor = Tensor::from_array(([1usize, input.len()], input)).map_err(ort_err)?;
        let outputs = session.run(ort::inputs![tensor]).map_err(ort_err)?;
        let (_shape, data) = outputs[0].try_extract_tensor::<f32>().map_err(ort_err)?;
        Ok(data.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs in CI with no model file present -- proves the graceful-failure path, not the happy
    /// path (see `runs_a_real_model_if_present` below for that, which is `#[ignore]`d).
    #[test]
    fn mobile_sam_reports_model_not_found_cleanly() {
        let selector = MobileSamSelector::new(
            "/nonexistent/mobile_sam.onnx",
            "/nonexistent/libonnxruntime.so",
        );
        let image = vec![0.5f32; 4 * 4 * 3];
        let err = selector
            .segment(&image, 4, 4, Prompt::Click { x: 2.0, y: 2.0 })
            .expect_err("missing model file must be a clean Err, not a panic");
        assert!(matches!(err, GroomAiError::ModelNotFound(_)));
    }

    #[test]
    fn lama_reports_model_not_found_cleanly() {
        let inpainter =
            LamaInpainter::new("/nonexistent/lama.onnx", "/nonexistent/libonnxruntime.so");
        let image = vec![0.5f32; 4 * 4 * 3];
        let mask = vec![1.0f32; 4 * 4];
        let err = inpainter
            .inpaint(&image, &mask, 4, 4)
            .expect_err("missing model file must be a clean Err, not a panic");
        assert!(matches!(err, GroomAiError::ModelNotFound(_)));
    }

    #[test]
    fn model_not_found_error_names_the_missing_path() {
        let err = load_session(
            Path::new("/nonexistent/some_model.onnx"),
            Path::new("/nonexistent/libonnxruntime.so"),
        )
        .expect_err("nonexistent model path must error");
        match err {
            GroomAiError::ModelNotFound(path) => {
                assert_eq!(path, PathBuf::from("/nonexistent/some_model.onnx"));
            }
            other => panic!("expected ModelNotFound, got {other:?}"),
        }
    }

    /// Requires an actual MobileSAM ONNX file at `NICTI_TEST_MOBILE_SAM_ONNX` and a real ONNX
    /// Runtime shared library at `NICTI_TEST_ORT_DYLIB` -- neither exists in CI, so this is
    /// `#[ignore]`d. Run manually with real weights via
    /// `cargo test -p groom -- --ignored runs_a_real_model_if_present`.
    #[test]
    #[ignore = "needs a real MobileSAM .onnx file and a real ONNX Runtime shared library on disk"]
    fn runs_a_real_model_if_present() {
        let model_path =
            std::env::var("NICTI_TEST_MOBILE_SAM_ONNX").expect("set NICTI_TEST_MOBILE_SAM_ONNX");
        let dylib_path = std::env::var("NICTI_TEST_ORT_DYLIB").expect("set NICTI_TEST_ORT_DYLIB");
        let selector = MobileSamSelector::new(model_path, dylib_path);
        let image = vec![0.5f32; 64 * 64 * 3];
        let mask = selector
            .segment(&image, 64, 64, Prompt::Click { x: 32.0, y: 32.0 })
            .expect("a real model should load and run");
        assert_eq!(mask.data.len(), mask.width * mask.height);
    }
}
