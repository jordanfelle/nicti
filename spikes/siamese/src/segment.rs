//! AI segmentation scaffolding: `ort`/`load-dynamic` wrappers for BiRefNet (subject/background,
//! single-pass) and MobileSAM (interactive, two-stage encoder+decoder), per ADR-0004 §3's
//! already-decided pattern (`ort::init_from(path)`), following `spikes/groom/src/ai.rs`'s
//! established shape for this repo (this spike doesn't depend on groom).
//!
//! **No real ONNX weights are committed or downloaded here.** A full BiRefNet ONNX export exists
//! publicly (~970MB, huggingface.co/onnx-community/BiRefNet-ONNX) but downloading and running it
//! is out of this pass's time budget -- same "obtaining actual checkpoints is out of scope for
//! this spike" call ADR-0007 made for LaMa/MobileSAM. Both wrappers below therefore prove only
//! the loading/error-handling shape (`ModelNotFound` on a missing file, a real `ort` session-load
//! attempt when a file *is* present), exactly as `spikes/groom/src/ai.rs` does -- see this
//! module's tests.
//!
//! **MobileSAM's real two-stage contract, unlike groom's single-tensor collapse for the
//! healing/removal case:** real MobileSAM is genuinely two ONNX sessions -- an image encoder
//! (expensive, once per image) producing a dense embedding, and a lightweight prompt decoder
//! (cheap, once per click/box) that consumes that embedding plus point/box tensors. This module's
//! `MobileSam::encode`/`decode` split matches that real shape rather than groom's "one input
//! tensor" simplification, because the split *is* the point for #48/#44: the embedding is what
//! Tapetum bakes and caches, so a user's second/third click after the first only re-runs the cheap
//! decoder. Exact input/output tensor names and shapes are still unverified against real weights
//! in this sandbox -- #51-style caveat applies equally here.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use ort::session::Session;
use ort::value::Tensor;

/// Which ONNX Runtime execution provider to request, mirroring the shape used in-flight for
/// `spikes/rods`'s own AI scaffolding (PR #167, not depended on here) -- `ort` falls back to CPU
/// on its own if a requested EP isn't available, and a `Session` doesn't expose which EP actually
/// ran, so this flag is a *request*, not a guarantee, same as that sibling spike found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionProviderKind {
    Cpu,
    Cuda,
    TensorRt,
}

#[derive(Debug, thiserror::Error)]
pub enum SegmentError {
    #[error("model file not found: {0}")]
    ModelNotFound(PathBuf),
    #[error("ONNX Runtime error: {0}")]
    Ort(String),
}

fn ort_err(e: impl std::fmt::Display) -> SegmentError {
    SegmentError::Ort(e.to_string())
}

/// Initializes the global `ort` environment exactly once -- same `OnceLock` pattern as
/// `spikes/groom/src/ai.rs::ensure_ort_environment`, since the environment is process-global and
/// `load-dynamic` requires this to run before any other `ort` API call.
///
/// **Known gap, shared with groom's identical implementation, deliberately not fixed here:**
/// `commit()` returns `false` both when the environment failed to initialize *and* when a
/// different caller already committed a (possibly compatible) environment first -- this code
/// treats both as a permanent error, cached forever by the `OnceLock`. If groom's and this
/// module's wrappers ever ran in the same process, whichever committed second would fail every
/// subsequent call, even with a perfectly usable environment already active. Fixing this properly
/// means deciding how to verify an already-committed environment's compatibility (EP support,
/// dylib path) before accepting it, which isn't a contained change and would need to touch both
/// modules together to stay consistent -- filed as issue #179 rather than fixed unilaterally here.
fn ensure_ort_environment(dylib_path: &Path) -> Result<(), SegmentError> {
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
    result.clone().map_err(SegmentError::Ort)
}

fn load_session(model_path: &Path, ort_dylib_path: &Path) -> Result<Session, SegmentError> {
    if !model_path.is_file() {
        return Err(SegmentError::ModelNotFound(model_path.to_path_buf()));
    }
    ensure_ort_environment(ort_dylib_path)?;
    Session::builder()
        .map_err(ort_err)?
        .commit_from_file(model_path)
        .map_err(ort_err)
}

/// A single-channel alpha mask, row-major, `1.0` = selected -- same convention as
/// `spikes/groom/src/ai.rs::Mask`.
#[derive(Debug, Clone)]
pub struct Alpha {
    pub width: usize,
    pub height: usize,
    pub data: Vec<f32>,
}

/// Converts an interleaved, row-major RGB buffer (`HWC`: `[r0,g0,b0, r1,g1,b1, ...]`, this
/// module's own documented input convention) into planar `CHW` (`[r0,r1,...,rN, g0,g1,...,gN,
/// b0,...,bN]`), the layout ONNX vision models -- including BiRefNet and MobileSAM's encoder --
/// declare via a `[1, 3, H, W]` input shape. Feeding HWC data directly into a tensor *labeled*
/// `[1, 3, H, W]` (as this module did before this fix) doesn't error -- the tensor's declared
/// shape and its actual memory layout simply disagree, so a real model would silently read wrong
/// channel/spatial values for any non-uniform image instead of failing loudly.
fn rgb_hwc_to_chw(image: &[f32], width: usize, height: usize) -> Vec<f32> {
    debug_assert_eq!(image.len(), width * height * 3);
    let pixel_count = width * height;
    let mut chw = vec![0.0f32; image.len()];
    for i in 0..pixel_count {
        chw[i] = image[i * 3];
        chw[pixel_count + i] = image[i * 3 + 1];
        chw[2 * pixel_count + i] = image[i * 3 + 2];
    }
    chw
}

/// Thin wrapper around a BiRefNet-style ONNX model: one RGB image in, one alpha mask out --
/// unlike MobileSAM, this genuinely is single-input/single-output in the real model, no
/// simplification needed on the input side. The output-identity caveat from groom's `ai.rs` still
/// applies: `outputs[0]` is assumed to be the alpha tensor, unverified against real weights.
pub struct BiRefNet {
    model_path: PathBuf,
    ort_dylib_path: PathBuf,
}

impl BiRefNet {
    pub fn new(model_path: impl Into<PathBuf>, ort_dylib_path: impl Into<PathBuf>) -> Self {
        Self {
            model_path: model_path.into(),
            ort_dylib_path: ort_dylib_path.into(),
        }
    }

    /// Segments `image` (RGB, row-major, `width * height * 3` `f32`s in `[0, 1]`) into a subject
    /// alpha mask at the model's own output resolution (typically the input resolution).
    pub fn segment(
        &self,
        image: &[f32],
        width: usize,
        height: usize,
    ) -> Result<Alpha, SegmentError> {
        debug_assert_eq!(image.len(), width * height * 3);
        let mut session = load_session(&self.model_path, &self.ort_dylib_path)?;
        let tensor = Tensor::from_array((
            [1usize, 3, height, width],
            rgb_hwc_to_chw(image, width, height),
        ))
        .map_err(ort_err)?;
        let outputs = session.run(ort::inputs![tensor]).map_err(ort_err)?;
        let (_shape, data) = outputs[0].try_extract_tensor::<f32>().map_err(ort_err)?;
        Ok(Alpha {
            width,
            height,
            data: data.to_vec(),
        })
    }
}

/// A click or box prompt, in the source image's own pixel coordinates -- same shape as
/// `spikes/groom/src/ai.rs::Prompt`.
#[derive(Debug, Clone, Copy)]
pub enum Prompt {
    Click { x: f32, y: f32 },
    Box { x0: f32, y0: f32, x1: f32, y1: f32 },
}

/// A cached image embedding from MobileSAM's encoder -- this is the value #44/#49 bake and cache
/// per image, so a second/third click's `decode()` call re-runs only the cheap decoder session.
#[derive(Debug, Clone)]
pub struct Embedding {
    pub data: Vec<f32>,
}

/// Thin wrapper around MobileSAM's real two-session contract. See this module's doc comment for
/// why `encode`/`decode` are split rather than collapsed into one call.
pub struct MobileSam {
    encoder_path: PathBuf,
    decoder_path: PathBuf,
    ort_dylib_path: PathBuf,
}

impl MobileSam {
    pub fn new(
        encoder_path: impl Into<PathBuf>,
        decoder_path: impl Into<PathBuf>,
        ort_dylib_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            encoder_path: encoder_path.into(),
            decoder_path: decoder_path.into(),
            ort_dylib_path: ort_dylib_path.into(),
        }
    }

    /// Runs the (expensive) image encoder once, producing an embedding to be cached and reused
    /// across every subsequent `decode()` call for the same image.
    pub fn encode(
        &self,
        image: &[f32],
        width: usize,
        height: usize,
    ) -> Result<Embedding, SegmentError> {
        debug_assert_eq!(image.len(), width * height * 3);
        let mut session = load_session(&self.encoder_path, &self.ort_dylib_path)?;
        let tensor = Tensor::from_array((
            [1usize, 3, height, width],
            rgb_hwc_to_chw(image, width, height),
        ))
        .map_err(ort_err)?;
        let outputs = session.run(ort::inputs![tensor]).map_err(ort_err)?;
        let (_shape, data) = outputs[0].try_extract_tensor::<f32>().map_err(ort_err)?;
        Ok(Embedding {
            data: data.to_vec(),
        })
    }

    /// Runs the (cheap) prompt decoder against an already-computed `embedding`, returning a mask
    /// at `(width, height)`. Real MobileSAM's decoder also takes a low-res mask hint and
    /// `orig_im_size` as separate named inputs; this spike concatenates the prompt into a fixed
    /// header ahead of the embedding, same "one input tensor" simplification groom's `ai.rs` uses
    /// for its own single-call case -- proving the loading shape, not the exact real contract.
    pub fn decode(
        &self,
        embedding: &Embedding,
        prompt: Prompt,
        width: usize,
        height: usize,
    ) -> Result<Alpha, SegmentError> {
        let mut session = load_session(&self.decoder_path, &self.ort_dylib_path)?;
        let mut input = Vec::with_capacity(embedding.data.len() + 5);
        match prompt {
            Prompt::Click { x, y } => input.extend_from_slice(&[0.0, x, y, 0.0, 0.0]),
            Prompt::Box { x0, y0, x1, y1 } => input.extend_from_slice(&[1.0, x0, y0, x1, y1]),
        }
        input.extend_from_slice(&embedding.data);
        let tensor = Tensor::from_array(([1usize, input.len()], input)).map_err(ort_err)?;
        let outputs = session.run(ort::inputs![tensor]).map_err(ort_err)?;
        let (_shape, data) = outputs[0].try_extract_tensor::<f32>().map_err(ort_err)?;
        Ok(Alpha {
            width,
            height,
            data: data.to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_hwc_to_chw_deinterleaves_channels_correctly() {
        // 2x1 image, HWC: pixel0=(1,2,3), pixel1=(4,5,6).
        let hwc = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        let chw = rgb_hwc_to_chw(&hwc, 2, 1);
        // CHW: all-R, then all-G, then all-B.
        assert_eq!(chw, vec![1.0, 4.0, 2.0, 5.0, 3.0, 6.0]);
    }

    #[test]
    fn birefnet_reports_model_not_found_cleanly() {
        let model = BiRefNet::new(
            "/nonexistent/birefnet.onnx",
            "/nonexistent/libonnxruntime.so",
        );
        let image = vec![0.5f32; 4 * 4 * 3];
        let err = model
            .segment(&image, 4, 4)
            .expect_err("missing model file must be a clean Err, not a panic");
        assert!(matches!(err, SegmentError::ModelNotFound(_)));
    }

    #[test]
    fn mobile_sam_encoder_reports_model_not_found_cleanly() {
        let model = MobileSam::new(
            "/nonexistent/encoder.onnx",
            "/nonexistent/decoder.onnx",
            "/nonexistent/libonnxruntime.so",
        );
        let image = vec![0.5f32; 4 * 4 * 3];
        let err = model
            .encode(&image, 4, 4)
            .expect_err("missing encoder file must be a clean Err, not a panic");
        assert!(matches!(err, SegmentError::ModelNotFound(_)));
    }

    #[test]
    fn mobile_sam_decoder_reports_model_not_found_cleanly() {
        let model = MobileSam::new(
            "/nonexistent/encoder.onnx",
            "/nonexistent/decoder.onnx",
            "/nonexistent/libonnxruntime.so",
        );
        let embedding = Embedding {
            data: vec![0.1; 16],
        };
        let err = model
            .decode(&embedding, Prompt::Click { x: 2.0, y: 2.0 }, 4, 4)
            .expect_err("missing decoder file must be a clean Err, not a panic");
        assert!(matches!(err, SegmentError::ModelNotFound(_)));
    }

    /// Requires a real BiRefNet `.onnx` file at `NICTI_TEST_BIREFNET_ONNX` and a real ONNX Runtime
    /// shared library at `NICTI_TEST_ORT_DYLIB` -- neither exists in CI, so this is `#[ignore]`d.
    /// Run manually with real weights via `cargo test -p siamese -- --ignored runs_birefnet_if_present`.
    #[test]
    #[ignore = "needs a real BiRefNet .onnx file and a real ONNX Runtime shared library on disk"]
    fn runs_birefnet_if_present() {
        let model_path =
            std::env::var("NICTI_TEST_BIREFNET_ONNX").expect("set NICTI_TEST_BIREFNET_ONNX");
        let dylib_path = std::env::var("NICTI_TEST_ORT_DYLIB").expect("set NICTI_TEST_ORT_DYLIB");
        let model = BiRefNet::new(model_path, dylib_path);
        let image = vec![0.5f32; 64 * 64 * 3];
        let alpha = model
            .segment(&image, 64, 64)
            .expect("a real model should load and run");
        assert_eq!(alpha.data.len(), alpha.width * alpha.height);
    }
}
