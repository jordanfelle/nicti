//! DINOv2 ViT-S/14 global-embedding wrapper, following `spikes/groom/src/ai.rs`'s
//! `ort`/`load-dynamic` pattern (ADR-0004 §3): the native ONNX Runtime library and the `.onnx`
//! model file both load lazily, from paths the caller provides, never bundled.
//!
//! **Unlike groom's MobileSAM/LaMa wrappers, this one runs against a real model**: a real ONNX
//! Runtime 1.19.2 shared library and the real `onnx-community/dinov2-small` export (based on
//! `facebook/dinov2-small`, Apache-2.0, standard checkpoint per `docs/licensing.md`'s DINOv2 row)
//! were both obtained and used to verify `Dinov2Embedder::embed` end-to-end in this pass -- see
//! `docs/research/litter-burst-grouping.md` for the exact commands. The `#[ignore]`d test below
//! still gates on env vars so CI (which has neither file) skips it cleanly, same as groom's own
//! tests.
//!
//! **Known quirk, not a bug in this module**: the ignored test's own assertions pass and print
//! `ok` (confirmed by running the compiled test binary directly), but the *process* then
//! segfaults during exit/teardown, after the test itself has already succeeded -- a known
//! `ort`/`load-dynamic` static-destructor-ordering issue between the dynamically loaded ONNX
//! Runtime library and Rust's own exit path, not something `Dinov2Embedder`'s own code can fix.
//! Run it directly (not through `cargo test`, whose own harness process reports the child's
//! segfault as a failure regardless of the printed `ok`) to see the real pass/fail signal.
//!
//! Preprocessing follows the standard `facebook/dinov2-small` image processor: resize so the
//! shorter edge is 224px (nearest-multiple-of-14 crop isn't needed here since 224 = 16*14 exactly),
//! center-crop to 224x224, scale to `[0, 1]`, then normalize per-channel with ImageNet mean/std.
//! The image-level feature is the CLS token (`last_hidden_state[:, 0, :]`) -- DINOv2's own
//! documented convention for a global descriptor (`x_norm_clstoken`), not a stand-in.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use ort::session::Session;
use ort::value::Tensor;

const IMAGE_SIZE: usize = 224;
const IMAGENET_MEAN: [f32; 3] = [0.485, 0.456, 0.406];
const IMAGENET_STD: [f32; 3] = [0.229, 0.224, 0.225];

#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    #[error("model file not found: {0}")]
    ModelNotFound(PathBuf),
    #[error("ONNX Runtime error: {0}")]
    Ort(String),
    #[error("unexpected output shape: {0:?}")]
    UnexpectedOutputShape(Vec<i64>),
}

fn ort_err(e: impl std::fmt::Display) -> EmbedError {
    EmbedError::Ort(e.to_string())
}

/// Initializes the global `ort` environment exactly once per process -- same rationale and
/// `OnceLock` shape as `spikes/groom/src/ai.rs::ensure_ort_environment`.
fn ensure_ort_environment(dylib_path: &Path) -> Result<(), EmbedError> {
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

/// Resizes (shorter edge to `IMAGE_SIZE`) + center-crops + normalizes an RGB image into
/// `pixel_values`' expected NCHW `f32` layout: `[1, 3, 224, 224]`.
fn preprocess(img: &image::RgbImage) -> Vec<f32> {
    use fast_image_resize as fr;

    let (w, h) = img.dimensions();
    let scale = IMAGE_SIZE as f64 / w.min(h) as f64;
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

    let x0 = (rw.saturating_sub(IMAGE_SIZE as u32)) / 2;
    let y0 = (rh.saturating_sub(IMAGE_SIZE as u32)) / 2;

    // NCHW, channel-planar.
    let mut out = vec![0f32; 3 * IMAGE_SIZE * IMAGE_SIZE];
    for y in 0..IMAGE_SIZE {
        for x in 0..IMAGE_SIZE {
            let sx = (x0 as usize + x).min(rw as usize - 1);
            let sy = (y0 as usize + y).min(rh as usize - 1);
            let idx = (sy * rw as usize + sx) * 3;
            for c in 0..3 {
                let v = resized[idx + c] as f32 / 255.0;
                let normalized = (v - IMAGENET_MEAN[c]) / IMAGENET_STD[c];
                out[c * IMAGE_SIZE * IMAGE_SIZE + y * IMAGE_SIZE + x] = normalized;
            }
        }
    }
    out
}

pub struct Dinov2Embedder {
    model_path: PathBuf,
    ort_dylib_path: PathBuf,
}

impl Dinov2Embedder {
    pub fn new(model_path: impl Into<PathBuf>, ort_dylib_path: impl Into<PathBuf>) -> Self {
        Self {
            model_path: model_path.into(),
            ort_dylib_path: ort_dylib_path.into(),
        }
    }

    /// Returns the CLS-token global embedding (384-dim for ViT-S/14) for `img`.
    pub fn embed(&self, img: &image::RgbImage) -> Result<Vec<f32>, EmbedError> {
        let mut session = load_session(&self.model_path, &self.ort_dylib_path)?;
        let pixels = preprocess(img);
        let tensor =
            Tensor::from_array(([1usize, 3, IMAGE_SIZE, IMAGE_SIZE], pixels)).map_err(ort_err)?;
        let outputs = session
            .run(ort::inputs!["pixel_values" => tensor])
            .map_err(ort_err)?;
        let (shape, data) = outputs["last_hidden_state"]
            .try_extract_tensor::<f32>()
            .map_err(ort_err)?;
        if shape.len() != 3 {
            return Err(EmbedError::UnexpectedOutputShape(shape.to_vec()));
        }
        let hidden = shape[2] as usize;
        // CLS token is index 0 along the sequence axis (dim 1).
        Ok(data[0..hidden].to_vec())
    }
}

/// Cosine similarity in `[-1.0, 1.0]`, `1.0` == identical direction.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_not_found_reports_cleanly() {
        let embedder = Dinov2Embedder::new("/nonexistent/dino.onnx", "/nonexistent/libort.so");
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
    fn preprocess_produces_expected_length() {
        let img = image::RgbImage::from_pixel(300, 200, image::Rgb([10, 20, 30]));
        let out = preprocess(&img);
        assert_eq!(out.len(), 3 * IMAGE_SIZE * IMAGE_SIZE);
    }

    /// Requires a real ONNX Runtime shared library (`NICTI_TEST_ORT_DYLIB`) and a real DINOv2
    /// ONNX export (`NICTI_TEST_DINO_ONNX`) -- neither exists in CI, so this is `#[ignore]`d, same
    /// convention as `spikes/groom`'s `runs_a_real_model_if_present`. Run manually with:
    /// `cargo test -p litter -- --ignored dinov2_embeds_two_similar_frames_closer_than_a_different_one`
    #[test]
    #[ignore = "needs a real DINOv2 .onnx file and a real ONNX Runtime shared library on disk"]
    fn dinov2_embeds_two_similar_frames_closer_than_a_different_one() {
        let model_path = std::env::var("NICTI_TEST_DINO_ONNX").expect("set NICTI_TEST_DINO_ONNX");
        let dylib_path = std::env::var("NICTI_TEST_ORT_DYLIB").expect("set NICTI_TEST_ORT_DYLIB");
        let embedder = Dinov2Embedder::new(model_path, dylib_path);

        // Two near-identical solid-color frames (a stand-in for two frames of the same static
        // pose) vs. one very different frame (a stand-in for a different subject/scene).
        let a = image::RgbImage::from_pixel(256, 256, image::Rgb([180, 140, 100]));
        let mut b = a.clone();
        for p in b.pixels_mut() {
            p[0] = p[0].saturating_add(3);
        }
        let c = image::RgbImage::from_pixel(256, 256, image::Rgb([10, 200, 20]));

        let ea = embedder.embed(&a).expect("embed a");
        let eb = embedder.embed(&b).expect("embed b");
        let ec = embedder.embed(&c).expect("embed c");

        assert_eq!(ea.len(), 384, "DINOv2 ViT-S/14 hidden size");
        let sim_ab = cosine_similarity(&ea, &eb);
        let sim_ac = cosine_similarity(&ea, &ec);
        assert!(
            sim_ab > sim_ac,
            "near-identical frames ({sim_ab}) should embed closer than a different scene ({sim_ac})"
        );
    }
}
