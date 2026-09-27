//! General-purpose image-embedding backbones for subject clustering, following
//! `spikes/litter/src/embed.rs`'s `ort`/`load-dynamic` pattern (ADR-0019 §3) and, per that ADR's
//! Consequences section, adapting `Dinov2Embedder` directly rather than depending on it (spikes
//! can't depend on other spikes).
//!
//! **Why not face recognition**: most subjects at a con are fursuiters, not bare human faces --
//! `docs/adr/0018-third-party-license-policy.md` and `docs/licensing.md` both steer #35 toward
//! general image embeddings (DINOv2/OpenCLIP) instead of InsightFace/RetinaFace, which are
//! license-excluded (non-commercial only) on top of not applying to fursuits at all.
//!
//! Three backbones are compared, per this ADR's decision rule:
//! - **DINOv2** (`Dinov2Embedder`) -- ported from `spikes/litter/src/embed.rs` verbatim (same
//!   224px/ImageNet-normalize preprocessing, same CLS-token convention). Already run against a
//!   real model in that spike; not re-verified here, same input/output contract.
//! - **DINOv3** (`Dinov3Embedder`) -- same CLS-token convention, ViT-S/16 (patch 16, not 14).
//!   License-gated (see `docs/licensing.md`'s new DINOv3 row): commercial use and redistribution
//!   are permitted with the Agreement text + a "Built with DINOv3" attribution notice shipped
//!   alongside; the `onnx-community` export additionally requires accepting Meta's gate on
//!   Hugging Face (an individual click-through, no scripted/CI download) before a `.onnx` file can
//!   be obtained.
//! - **OpenCLIP** (`OpenClipEmbedder`) -- no ready-made ONNX export of a clean-license, non-LAION
//!   checkpoint was found this pass (the one community ONNX family, Apple's MobileCLIP2, is
//!   research/non-commercial-only per `apple/ml-mobileclip`'s `LICENSE_MODELS` and is excluded,
//!   same as InsightFace). This implementation is written against the *expected* export shape
//!   (`open_clip`'s image tower, L2-normalized pooled output) documented in
//!   `docs/research/rosette-subject-grouping.md`'s export recipe, but has no real model to run
//!   against in this pass -- its `#[ignore]`d test is TBD, same "spec built, real measurement
//!   pending real inputs" shape ADR-0033 uses for the con-shoot ground truth.
//!
//! All three share the same `ensure_ort_environment`/`load_session` shape as the other four
//! `ort`/`load-dynamic` wrappers in this repo (`spikes/litter`, `spikes/groom`, `spikes/siamese`,
//! `spikes/rods`, `spikes/crouch`) -- keep all in sync (#179).

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use ort::session::Session;
use ort::value::Tensor;

const IMAGENET_MEAN: [f32; 3] = [0.485, 0.456, 0.406];
const IMAGENET_STD: [f32; 3] = [0.229, 0.224, 0.225];
/// OpenAI CLIP's own published preprocessing constants (distinct from ImageNet's), used here for
/// OpenCLIP too since most public OpenCLIP checkpoints (including the DataComp family) keep
/// CLIP's original normalization rather than switching to ImageNet's.
const CLIP_MEAN: [f32; 3] = [0.481_454_66, 0.457_827_5, 0.408_210_73];
const CLIP_STD: [f32; 3] = [0.268_629_54, 0.261_302_6, 0.275_777_1];

#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    #[error("model file not found: {0}")]
    ModelNotFound(PathBuf),
    #[error("ONNX Runtime error: {0}")]
    Ort(String),
    #[error("unexpected output shape: {0:?}")]
    UnexpectedOutputShape(Vec<i64>),
    #[error("image has a zero dimension: {0}x{1}")]
    InvalidImageDimensions(u32, u32),
}

fn ort_err(e: impl std::fmt::Display) -> EmbedError {
    EmbedError::Ort(e.to_string())
}

/// Extracts the CLS-token embedding (index 0 along the sequence axis) from a `[1, seq, hidden]`
/// `last_hidden_state`-shaped output, shared by `Dinov2Embedder`/`Dinov3Embedder`. Validates that
/// `data` actually holds at least `hidden` elements before slicing -- a malformed/adversarial
/// `.onnx` model declaring a shape like `[1, 0, 384]` (zero sequence length) would otherwise
/// index-panic on `data[0..hidden]`; flagged by an adversarial review as untested against a real
/// model, since neither DINOv3 nor OpenCLIP's real output has been run against this pass (see
/// this module's doc comment). DINOv2's own real-model run in ADR-0033 never hit this because a
/// real model's sequence dimension is never zero, not because this was previously validated.
fn extract_cls_token(shape: &[i64], data: &[f32]) -> Result<Vec<f32>, EmbedError> {
    if shape.len() != 3 || shape[2] < 0 {
        return Err(EmbedError::UnexpectedOutputShape(shape.to_vec()));
    }
    let hidden = shape[2] as usize;
    if data.len() < hidden {
        return Err(EmbedError::UnexpectedOutputShape(shape.to_vec()));
    }
    Ok(data[0..hidden].to_vec())
}

/// Same process-global `OnceLock` pattern as the other four `ort`/`load-dynamic` copies in this
/// repo (`spikes/litter/src/embed.rs`, `spikes/groom/src/ai.rs`, `spikes/siamese/src/segment.rs`,
/// `spikes/rods/src/ai.rs`, `spikes/crouch/src/ort_contend.rs`). Keep all in sync (#179).
pub fn ensure_ort_environment(dylib_path: &Path) -> Result<(), EmbedError> {
    static INIT: OnceLock<Result<(), String>> = OnceLock::new();
    let result = INIT.get_or_init(|| {
        let builder =
            ort::init_from(dylib_path.to_string_lossy().into_owned()).map_err(|e| e.to_string())?;
        builder.commit();
        Ok(())
    });
    result.clone().map_err(EmbedError::Ort)
}

fn load_session(model_path: &Path, ort_dylib_path: &Path) -> Result<Session, EmbedError> {
    if !model_path.is_file() {
        return Err(EmbedError::ModelNotFound(model_path.to_path_buf()));
    }
    ensure_ort_environment(ort_dylib_path)?;
    Session::builder()
        .map_err(ort_err)?
        .commit_from_file(model_path)
        .map_err(ort_err)
}

/// Resizes (shorter edge to `size`) + center-crops + normalizes an RGB image into a NCHW
/// `[1, 3, size, size]` `f32` buffer -- shared by all three backbones below, parameterized on
/// target size and per-channel mean/std since DINOv3/OpenCLIP checkpoints don't all agree with
/// DINOv2's 224/ImageNet defaults.
fn preprocess(
    img: &image::RgbImage,
    size: usize,
    mean: [f32; 3],
    std: [f32; 3],
) -> Result<Vec<f32>, EmbedError> {
    use fast_image_resize as fr;

    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return Err(EmbedError::InvalidImageDimensions(w, h));
    }
    let scale = size as f64 / w.min(h) as f64;
    let (rw, rh) = (
        (w as f64 * scale).round().max(1.0) as u32,
        (h as f64 * scale).round().max(1.0) as u32,
    );

    let src = fr::images::Image::from_vec_u8(w, h, img.clone().into_raw(), fr::PixelType::U8x3)
        .expect("valid source image");
    let mut dst = fr::images::Image::new(rw, rh, fr::PixelType::U8x3);
    let mut resizer = fr::Resizer::new();
    resizer.resize(&src, &mut dst, None).expect("resize");
    let resized = dst.into_vec();

    let x0 = (rw.saturating_sub(size as u32)) / 2;
    let y0 = (rh.saturating_sub(size as u32)) / 2;

    let mut out = vec![0f32; 3 * size * size];
    for y in 0..size {
        for x in 0..size {
            let sx = (x0 as usize + x).min(rw as usize - 1);
            let sy = (y0 as usize + y).min(rh as usize - 1);
            let idx = (sy * rw as usize + sx) * 3;
            for c in 0..3 {
                let v = resized[idx + c] as f32 / 255.0;
                let normalized = (v - mean[c]) / std[c];
                out[c * size * size + y * size + x] = normalized;
            }
        }
    }
    Ok(out)
}

/// A backbone that turns a cropped/full-frame RGB image into a fixed-length global embedding.
/// Implemented by all three candidates so `cluster.rs`/the CLI can compare them interchangeably.
pub trait Embedder {
    fn embed(&self, img: &image::RgbImage) -> Result<Vec<f32>, EmbedError>;
    /// Embedding dimensionality, known statically per backbone (used to sanity-check output
    /// shape and to size synthetic-test fixtures without running a real model).
    fn dim(&self) -> usize;
    fn name(&self) -> &'static str;
}

/// DINOv2 ViT-S/14, ported from `spikes/litter/src/embed.rs::Dinov2Embedder` (#33/ADR-0033) --
/// same 224px/ImageNet preprocessing, same CLS-token (`last_hidden_state[:, 0, :]`) convention.
/// Already verified against a real `onnx-community/dinov2-small` export there; this copy carries
/// the same contract, not re-verified independently in this pass.
pub struct Dinov2Embedder {
    model_path: PathBuf,
    ort_dylib_path: PathBuf,
    session: OnceLock<Mutex<Session>>,
}

impl Dinov2Embedder {
    pub fn new(model_path: impl Into<PathBuf>, ort_dylib_path: impl Into<PathBuf>) -> Self {
        Self {
            model_path: model_path.into(),
            ort_dylib_path: ort_dylib_path.into(),
            session: OnceLock::new(),
        }
    }

    fn session(&self) -> Result<&Mutex<Session>, EmbedError> {
        if let Some(session) = self.session.get() {
            return Ok(session);
        }
        let built = load_session(&self.model_path, &self.ort_dylib_path)?;
        Ok(self.session.get_or_init(|| Mutex::new(built)))
    }
}

impl Embedder for Dinov2Embedder {
    fn embed(&self, img: &image::RgbImage) -> Result<Vec<f32>, EmbedError> {
        let session_lock = self.session()?;
        let pixels = preprocess(img, 224, IMAGENET_MEAN, IMAGENET_STD)?;
        let tensor = Tensor::from_array(([1usize, 3, 224, 224], pixels)).map_err(ort_err)?;
        let mut session = session_lock
            .lock()
            .map_err(|_| EmbedError::Ort("ONNX session lock poisoned".to_string()))?;
        let outputs = session
            .run(ort::inputs!["pixel_values" => tensor])
            .map_err(ort_err)?;
        let (shape, data) = outputs["last_hidden_state"]
            .try_extract_tensor::<f32>()
            .map_err(ort_err)?;
        extract_cls_token(shape, data)
    }

    fn dim(&self) -> usize {
        384
    }

    fn name(&self) -> &'static str {
        "dinov2"
    }
}

/// DINOv3 ViT-S/16 -- same CLS-token convention as DINOv2, different patch size (16 vs. 14) and
/// input resolution (`onnx-community/dinov3-vits16-pretrain-lvd1689m-ONNX` uses 224 too, so the
/// only real preprocessing difference from DINOv2 in practice is the model file itself; kept as
/// a distinct implementation rather than a DINOv2 type-alias so the two can diverge if a
/// different DINOv3 export size is used later).
pub struct Dinov3Embedder {
    model_path: PathBuf,
    ort_dylib_path: PathBuf,
    session: OnceLock<Mutex<Session>>,
}

impl Dinov3Embedder {
    pub fn new(model_path: impl Into<PathBuf>, ort_dylib_path: impl Into<PathBuf>) -> Self {
        Self {
            model_path: model_path.into(),
            ort_dylib_path: ort_dylib_path.into(),
            session: OnceLock::new(),
        }
    }

    fn session(&self) -> Result<&Mutex<Session>, EmbedError> {
        if let Some(session) = self.session.get() {
            return Ok(session);
        }
        let built = load_session(&self.model_path, &self.ort_dylib_path)?;
        Ok(self.session.get_or_init(|| Mutex::new(built)))
    }
}

impl Embedder for Dinov3Embedder {
    fn embed(&self, img: &image::RgbImage) -> Result<Vec<f32>, EmbedError> {
        let session_lock = self.session()?;
        let pixels = preprocess(img, 224, IMAGENET_MEAN, IMAGENET_STD)?;
        let tensor = Tensor::from_array(([1usize, 3, 224, 224], pixels)).map_err(ort_err)?;
        let mut session = session_lock
            .lock()
            .map_err(|_| EmbedError::Ort("ONNX session lock poisoned".to_string()))?;
        let outputs = session
            .run(ort::inputs!["pixel_values" => tensor])
            .map_err(ort_err)?;
        let (shape, data) = outputs["last_hidden_state"]
            .try_extract_tensor::<f32>()
            .map_err(ort_err)?;
        extract_cls_token(shape, data)
    }

    fn dim(&self) -> usize {
        // ViT-S/16 hidden size, matching DINOv2 ViT-S/14's own 384 (both "small" variants).
        384
    }

    fn name(&self) -> &'static str {
        "dinov3"
    }
}

/// OpenCLIP image tower. **No real ONNX weights obtained this pass** -- see this module's doc
/// comment. Written against `open_clip`'s documented pooled-and-L2-normalized image-embedding
/// output (a `[1, dim]` tensor, not a `[1, seq, dim]` hidden-state sequence like the two DINO
/// variants), which is why this preprocesses at 224 with CLIP's own normalization constants and
/// reads a differently-shaped output than `Dinov2Embedder`/`Dinov3Embedder`.
pub struct OpenClipEmbedder {
    model_path: PathBuf,
    ort_dylib_path: PathBuf,
    dim: usize,
    session: OnceLock<Mutex<Session>>,
}

impl OpenClipEmbedder {
    /// `dim` is the checkpoint's own output width (e.g. 512 for ViT-B/32) -- unlike the two DINO
    /// variants, OpenCLIP checkpoints vary this across model sizes, so it isn't a fixed constant.
    pub fn new(
        model_path: impl Into<PathBuf>,
        ort_dylib_path: impl Into<PathBuf>,
        dim: usize,
    ) -> Self {
        Self {
            model_path: model_path.into(),
            ort_dylib_path: ort_dylib_path.into(),
            dim,
            session: OnceLock::new(),
        }
    }

    fn session(&self) -> Result<&Mutex<Session>, EmbedError> {
        if let Some(session) = self.session.get() {
            return Ok(session);
        }
        let built = load_session(&self.model_path, &self.ort_dylib_path)?;
        Ok(self.session.get_or_init(|| Mutex::new(built)))
    }
}

impl Embedder for OpenClipEmbedder {
    fn embed(&self, img: &image::RgbImage) -> Result<Vec<f32>, EmbedError> {
        let session_lock = self.session()?;
        let pixels = preprocess(img, 224, CLIP_MEAN, CLIP_STD)?;
        let tensor = Tensor::from_array(([1usize, 3, 224, 224], pixels)).map_err(ort_err)?;
        let mut session = session_lock
            .lock()
            .map_err(|_| EmbedError::Ort("ONNX session lock poisoned".to_string()))?;
        let outputs = session
            .run(ort::inputs!["pixel_values" => tensor])
            .map_err(ort_err)?;
        let (shape, data) = outputs["image_embeds"]
            .try_extract_tensor::<f32>()
            .map_err(ort_err)?;
        if shape.len() != 2 || shape[1] as usize != self.dim {
            return Err(EmbedError::UnexpectedOutputShape(shape.to_vec()));
        }
        Ok(data.to_vec())
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn name(&self) -> &'static str {
        "openclip"
    }
}

/// Cosine similarity in `[-1.0, 1.0]`, `1.0` == identical direction. Copied from
/// `spikes/litter/src/embed.rs::cosine_similarity`, unchanged.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    debug_assert_eq!(a.len(), b.len());
    let dot: f64 = a.iter().zip(b).map(|(x, y)| *x as f64 * *y as f64).sum();
    let norm_a: f64 = a.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
    let norm_b: f64 = b.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

/// Cosine *distance* (`1.0 - cosine_similarity`), in `[0.0, 2.0]` -- what `cluster.rs`'s DBSCAN
/// wants (a distance metric, not a similarity score).
pub fn cosine_distance(a: &[f32], b: &[f32]) -> f64 {
    1.0 - cosine_similarity(a, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dinov2_model_not_found_reports_cleanly() {
        let embedder = Dinov2Embedder::new("/nonexistent/dino.onnx", "/nonexistent/libort.so");
        let img = image::RgbImage::from_pixel(16, 16, image::Rgb([128, 128, 128]));
        let err = embedder
            .embed(&img)
            .expect_err("missing model must be a clean Err");
        assert!(matches!(err, EmbedError::ModelNotFound(_)));
    }

    #[test]
    fn dinov3_model_not_found_reports_cleanly() {
        let embedder = Dinov3Embedder::new("/nonexistent/dino3.onnx", "/nonexistent/libort.so");
        let img = image::RgbImage::from_pixel(16, 16, image::Rgb([128, 128, 128]));
        let err = embedder
            .embed(&img)
            .expect_err("missing model must be a clean Err");
        assert!(matches!(err, EmbedError::ModelNotFound(_)));
    }

    #[test]
    fn openclip_model_not_found_reports_cleanly() {
        let embedder =
            OpenClipEmbedder::new("/nonexistent/clip.onnx", "/nonexistent/libort.so", 512);
        let img = image::RgbImage::from_pixel(16, 16, image::Rgb([128, 128, 128]));
        let err = embedder
            .embed(&img)
            .expect_err("missing model must be a clean Err");
        assert!(matches!(err, EmbedError::ModelNotFound(_)));
    }

    #[test]
    fn cosine_similarity_identical_vectors_is_one() {
        let v = vec![1.0f32, 2.0, 3.0];
        assert!((cosine_similarity(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_orthogonal_vectors_is_zero() {
        let a = vec![1.0f32, 0.0];
        let b = vec![0.0f32, 1.0];
        assert!(cosine_similarity(&a, &b).abs() < 1e-6);
    }

    #[test]
    fn cosine_distance_identical_vectors_is_zero() {
        let v = vec![1.0f32, 2.0, 3.0];
        assert!(cosine_distance(&v, &v).abs() < 1e-6);
    }

    #[test]
    fn preprocess_produces_expected_length() {
        let img = image::RgbImage::from_pixel(300, 200, image::Rgb([10, 20, 30]));
        let out = preprocess(&img, 224, IMAGENET_MEAN, IMAGENET_STD).expect("preprocess");
        assert_eq!(out.len(), 3 * 224 * 224);
    }

    #[test]
    fn preprocess_rejects_zero_dimension_image_cleanly() {
        let img = image::RgbImage::new(1, 0);
        assert!(matches!(
            preprocess(&img, 224, IMAGENET_MEAN, IMAGENET_STD),
            Err(EmbedError::InvalidImageDimensions(1, 0))
        ));
    }

    /// Requires a real ONNX Runtime shared library (`NICTI_TEST_ORT_DYLIB`) and a real DINOv2
    /// ONNX export (`NICTI_TEST_DINO_ONNX`) -- same env vars `spikes/litter` uses, since this is
    /// the identical checkpoint. Run with:
    /// `cargo test -p rosette -- --ignored dinov2_embeds_two_similar_frames_closer_than_a_different_one`
    #[test]
    #[ignore = "needs a real DINOv2 .onnx file and a real ONNX Runtime shared library on disk"]
    fn dinov2_embeds_two_similar_frames_closer_than_a_different_one() {
        let model_path = std::env::var("NICTI_TEST_DINO_ONNX").expect("set NICTI_TEST_DINO_ONNX");
        let dylib_path = std::env::var("NICTI_TEST_ORT_DYLIB").expect("set NICTI_TEST_ORT_DYLIB");
        let embedder = Dinov2Embedder::new(model_path, dylib_path);

        let a = image::RgbImage::from_pixel(256, 256, image::Rgb([180, 140, 100]));
        let mut b = a.clone();
        for p in b.pixels_mut() {
            p[0] = p[0].saturating_add(3);
        }
        let c = image::RgbImage::from_pixel(256, 256, image::Rgb([10, 200, 20]));

        let ea = embedder.embed(&a).expect("embed a");
        let eb = embedder.embed(&b).expect("embed b");
        let ec = embedder.embed(&c).expect("embed c");

        assert_eq!(ea.len(), 384);
        let sim_ab = cosine_similarity(&ea, &eb);
        let sim_ac = cosine_similarity(&ea, &ec);
        assert!(sim_ab > sim_ac);
    }

    /// New env var vs. litter's own set: a real DINOv3 ONNX export
    /// (`onnx-community/dinov3-vits16-pretrain-lvd1689m-ONNX`) is gated behind a Hugging Face
    /// click-through (see this module's doc comment) -- no automated fetch is possible, so this
    /// stays `#[ignore]`d/TBD until someone manually accepts the gate and downloads the file.
    #[test]
    #[ignore = "needs a real DINOv3 .onnx file (Hugging Face gate, manual accept required) and a real ONNX Runtime shared library on disk"]
    fn dinov3_embeds_two_similar_frames_closer_than_a_different_one() {
        let model_path =
            std::env::var("NICTI_TEST_DINOV3_ONNX").expect("set NICTI_TEST_DINOV3_ONNX");
        let dylib_path = std::env::var("NICTI_TEST_ORT_DYLIB").expect("set NICTI_TEST_ORT_DYLIB");
        let embedder = Dinov3Embedder::new(model_path, dylib_path);

        let a = image::RgbImage::from_pixel(256, 256, image::Rgb([180, 140, 100]));
        let mut b = a.clone();
        for p in b.pixels_mut() {
            p[0] = p[0].saturating_add(3);
        }
        let c = image::RgbImage::from_pixel(256, 256, image::Rgb([10, 200, 20]));

        let ea = embedder.embed(&a).expect("embed a");
        let eb = embedder.embed(&b).expect("embed b");
        let ec = embedder.embed(&c).expect("embed c");

        let sim_ab = cosine_similarity(&ea, &eb);
        let sim_ac = cosine_similarity(&ea, &ec);
        assert!(sim_ab > sim_ac);
    }

    /// TBD this pass -- no OpenCLIP ONNX export exists to test against (see this module's doc
    /// comment). Left `#[ignore]`d against the export recipe's expected env var so a future pass
    /// that produces `rosette-openclip.onnx` can run it unchanged.
    #[test]
    #[ignore = "needs a self-exported OpenCLIP .onnx file (no ready-made clean-license export exists -- see docs/research/rosette-subject-grouping.md's export recipe) and a real ONNX Runtime shared library on disk"]
    fn openclip_embeds_two_similar_frames_closer_than_a_different_one() {
        let model_path =
            std::env::var("NICTI_TEST_OPENCLIP_ONNX").expect("set NICTI_TEST_OPENCLIP_ONNX");
        let dylib_path = std::env::var("NICTI_TEST_ORT_DYLIB").expect("set NICTI_TEST_ORT_DYLIB");
        let embedder = OpenClipEmbedder::new(model_path, dylib_path, 512);

        let a = image::RgbImage::from_pixel(256, 256, image::Rgb([180, 140, 100]));
        let mut b = a.clone();
        for p in b.pixels_mut() {
            p[0] = p[0].saturating_add(3);
        }
        let c = image::RgbImage::from_pixel(256, 256, image::Rgb([10, 200, 20]));

        let ea = embedder.embed(&a).expect("embed a");
        let eb = embedder.embed(&b).expect("embed b");
        let ec = embedder.embed(&c).expect("embed c");

        let sim_ab = cosine_similarity(&ea, &eb);
        let sim_ac = cosine_similarity(&ea, &ec);
        assert!(sim_ab > sim_ac);
    }
}
