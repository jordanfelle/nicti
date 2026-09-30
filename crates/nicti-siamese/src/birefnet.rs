//! BiRefNet as a [`SegmentationProvider`] (ADR-0048's default subject model).
//!
//! Contract, from the model's own preprocessor config and README (and re-checked against the real
//! weights by `tests/real_models.rs`): one RGB image resized to **1024x1024** and normalized with
//! the ImageNet mean/std goes in as `[1, 3, 1024, 1024]` (planar `CHW`); one `[1, 1, 1024, 1024]`
//! tensor of **logits** comes out, and a sigmoid turns it into alpha. The tensor *names* are read
//! from the loaded graph, not trusted from constants: a different export of the same model just
//! works.
//!
//! The image is stretched (not letter-boxed) to 1024x1024, and so is the alpha: both live in the
//! same normalized coordinates, which is exactly how the tapetum guided-filter refine maps a
//! low-resolution alpha onto the full-resolution frame.
//!
//! Loading verifies the download's SHA-256 first (ADR-0218) -- the file is parsed by native code --
//! and runs on the caller's worker thread, because hashing and building a ~1 GB session takes
//! seconds. `ort`/`load-dynamic` keeps the runtime out of the binary until here.

use nicti_claw::Module;
use nicti_stalk::models::{mask_artifacts, verify_artifacts, MaskModels, BIREFNET};
use nicti_stalk::{
    AlphaMap, LoadContext, ModelImage, ModelProvider, SegmentError, SegmentTarget,
    SegmentationProvider, Segmenter,
};
use ort::session::Session;
use ort::value::Tensor;
use serde_json::Value;

pub const BIREFNET_ID: &str = "nicti.mask.birefnet";
/// Ties an edit to the exact pinned weights: the revision in `models::BIREFNET`'s URL. A different
/// export is a different version string, so an old edit is never silently re-run on it.
pub const BIREFNET_VERSION: &str = "onnx-community-534d3c82";

/// The model's fixed input/output side.
pub const MODEL_SIZE: usize = 1024;
const MEAN: [f32; 3] = [0.485, 0.456, 0.406];
const STD: [f32; 3] = [0.229, 0.224, 0.225];

/// Resizes `image` (display sRGB `0..=1`, `HWC`) to `MODEL_SIZE x MODEL_SIZE` bilinearly (pixel
/// centres aligned, like PIL/transformers' resample=2) and returns it ImageNet-normalized in planar
/// `CHW` order -- exactly the model's `input_image`.
pub fn preprocess(image: &ModelImage) -> Result<Vec<f32>, SegmentError> {
    image.validate()?;
    let (w, h) = (image.width, image.height);
    let px = |x: usize, y: usize, c: usize| image.rgb[(y * w + x) * 3 + c];
    let plane = MODEL_SIZE * MODEL_SIZE;
    let mut out = vec![0.0f32; 3 * plane];
    // Source coordinate of each output column/row, computed once.
    let axis = |n: usize, out_i: usize| {
        let f = (out_i as f32 + 0.5) * n as f32 / MODEL_SIZE as f32 - 0.5;
        let f0 = f.floor();
        let i0 = (f0.max(0.0) as usize).min(n - 1);
        let i1 = ((f0 + 1.0).max(0.0) as usize).min(n - 1);
        (i0, i1, f - f0)
    };
    let cols: Vec<_> = (0..MODEL_SIZE).map(|x| axis(w, x)).collect();
    for oy in 0..MODEL_SIZE {
        let (y0, y1, ty) = axis(h, oy);
        for (ox, &(x0, x1, tx)) in cols.iter().enumerate() {
            for c in 0..3 {
                let top = px(x0, y0, c) * (1.0 - tx) + px(x1, y0, c) * tx;
                let bottom = px(x0, y1, c) * (1.0 - tx) + px(x1, y1, c) * tx;
                let v = top * (1.0 - ty) + bottom * ty;
                out[c * plane + oy * MODEL_SIZE + ox] = (v - MEAN[c]) / STD[c];
            }
        }
    }
    Ok(out)
}

/// The model's logits -> alpha: a sigmoid. `logits` must be exactly `MODEL_SIZE^2` values.
pub fn postprocess(logits: &[f32]) -> Result<AlphaMap, SegmentError> {
    if logits.len() != MODEL_SIZE * MODEL_SIZE {
        return Err(SegmentError::Inference(format!(
            "expected {} output values, got {}",
            MODEL_SIZE * MODEL_SIZE,
            logits.len()
        )));
    }
    AlphaMap::new(
        MODEL_SIZE,
        MODEL_SIZE,
        logits.iter().map(|&x| 1.0 / (1.0 + (-x).exp())).collect(),
    )
}

pub struct BiRefNetProvider;

impl Module for BiRefNetProvider {
    fn id(&self) -> &str {
        BIREFNET_ID
    }
    fn schema_version(&self) -> u32 {
        1
    }
    fn migrate_params(&self, _from_version: u32, params: Value) -> Option<Value> {
        Some(params)
    }
}

impl ModelProvider for BiRefNetProvider {
    fn label(&self) -> &str {
        "BiRefNet (subject / background)"
    }

    fn artifacts(&self) -> Vec<&'static nicti_stalk::models::Artifact> {
        mask_artifacts()
    }
}

impl SegmentationProvider for BiRefNetProvider {
    fn model_version(&self) -> &str {
        BIREFNET_VERSION
    }

    fn targets(&self) -> &[SegmentTarget] {
        &[SegmentTarget::Subject]
    }

    fn load(&self, ctx: &LoadContext) -> Result<Box<dyn Segmenter>, SegmentError> {
        let (dylib, model, ort_from_store) = match ctx.ort_dylib {
            Some(dylib) => {
                let model = ctx.store.installed_path(&BIREFNET).ok_or_else(|| {
                    SegmentError::NotInstalled(format!("{} is not downloaded", BIREFNET.label))
                })?;
                (dylib.to_path_buf(), model, false)
            }
            None => {
                let models = MaskModels::locate(ctx.store).ok_or_else(|| {
                    SegmentError::NotInstalled(format!(
                        "{} and an ONNX Runtime library are needed",
                        BIREFNET.label
                    ))
                })?;
                let from_store = std::env::var_os("NICTI_ORT_DYLIB").is_none_or(|v| v.is_empty());
                (models.ort_dylib, models.birefnet, from_store)
            }
        };
        verify_artifacts(ctx.store, &mask_artifacts(), ort_from_store)
            .map_err(SegmentError::Verify)?;
        nicti_haw::ensure_ort_environment(&dylib).map_err(|e| SegmentError::Load(e.to_string()))?;
        let session = Session::builder()
            .map_err(|e| SegmentError::Load(e.to_string()))?
            .commit_from_file(&model)
            .map_err(|e| SegmentError::Load(e.to_string()))?;
        let input_name = session
            .inputs()
            .first()
            .map(|i| i.name().to_owned())
            .ok_or_else(|| SegmentError::Load("the model declares no input".into()))?;
        let output_name = session
            .outputs()
            .first()
            .map(|o| o.name().to_owned())
            .ok_or_else(|| SegmentError::Load("the model declares no output".into()))?;
        Ok(Box::new(BiRefNetSegmenter {
            session,
            input_name,
            output_name,
        }))
    }
}

struct BiRefNetSegmenter {
    session: Session,
    input_name: String,
    output_name: String,
}

impl Segmenter for BiRefNetSegmenter {
    fn segment(&mut self, image: &ModelImage, params: &Value) -> Result<AlphaMap, SegmentError> {
        match SegmentTarget::from_params(params)? {
            SegmentTarget::Subject => {}
            other => return Err(SegmentError::UnsupportedTarget(other.as_str().to_owned())),
        }
        let input = preprocess(image)?;
        let tensor = Tensor::from_array(([1usize, 3, MODEL_SIZE, MODEL_SIZE], input))
            .map_err(|e| SegmentError::Inference(e.to_string()))?;
        let outputs = self
            .session
            .run(ort::inputs![self.input_name.as_str() => tensor])
            .map_err(|e| SegmentError::Inference(e.to_string()))?;
        let (_shape, data) = outputs[self.output_name.as_str()]
            .try_extract_tensor::<f32>()
            .map_err(|e| SegmentError::Inference(e.to_string()))?;
        postprocess(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(w: usize, h: usize, rgb: &[f32]) -> ModelImage<'_> {
        ModelImage {
            width: w,
            height: h,
            rgb,
            image_key: 3,
        }
    }

    #[test]
    fn a_solid_image_normalizes_to_the_imagenet_values_in_planar_order() {
        let rgb: Vec<f32> = (0..4 * 3).flat_map(|_| [1.0, 0.0, 0.5]).collect();
        let out = preprocess(&view(4, 3, &rgb)).unwrap();
        assert_eq!(out.len(), 3 * MODEL_SIZE * MODEL_SIZE);
        let plane = MODEL_SIZE * MODEL_SIZE;
        let want = [
            (1.0 - MEAN[0]) / STD[0],
            (0.0 - MEAN[1]) / STD[1],
            (0.5 - MEAN[2]) / STD[2],
        ];
        for c in 0..3 {
            // Planar: every value of channel c sits in its own contiguous plane.
            for i in [0, plane / 2, plane - 1] {
                assert!((out[c * plane + i] - want[c]).abs() < 1e-5, "c{c} i{i}");
            }
        }
    }

    #[test]
    fn resizing_keeps_the_layout_left_is_left_and_top_is_top() {
        // Left half red, right half blue; top half bright, bottom half dark.
        let (w, h) = (8, 8);
        let rgb: Vec<f32> = (0..w * h)
            .flat_map(|i| {
                let (x, y) = (i % w, i / w);
                let v = if y < h / 2 { 1.0 } else { 0.2 };
                if x < w / 2 {
                    [v, 0.0, 0.0]
                } else {
                    [0.0, 0.0, v]
                }
            })
            .collect();
        let out = preprocess(&view(w, h, &rgb)).unwrap();
        let plane = MODEL_SIZE * MODEL_SIZE;
        let at = |c: usize, x: usize, y: usize| out[c * plane + y * MODEL_SIZE + x];
        let red_hi = (1.0 - MEAN[0]) / STD[0];
        let red_lo = (0.0 - MEAN[0]) / STD[0];
        assert!(
            (at(0, 100, 100) - red_hi).abs() < 1e-3,
            "top-left is bright red"
        );
        assert!(
            (at(0, 900, 100) - red_lo).abs() < 1e-3,
            "top-right has no red"
        );
        assert!(
            at(0, 100, 100) > at(0, 100, 900),
            "top is brighter than bottom"
        );
        let blue_hi = (1.0 - MEAN[2]) / STD[2];
        assert!((at(2, 900, 100) - blue_hi).abs() < 1e-3);
    }

    #[test]
    fn a_bad_input_buffer_is_rejected_not_indexed() {
        let rgb = vec![0.5; 10];
        assert!(matches!(
            preprocess(&view(4, 4, &rgb)),
            Err(SegmentError::BadInput(_))
        ));
    }

    #[test]
    fn a_sigmoid_maps_logits_to_alpha() {
        let mut logits = vec![0.0f32; MODEL_SIZE * MODEL_SIZE];
        logits[0] = 20.0;
        logits[1] = -20.0;
        logits[2] = f32::NAN;
        let a = postprocess(&logits).unwrap();
        assert!(a.alpha[0] > 0.999 && a.alpha[1] < 0.001);
        assert_eq!(a.alpha[3], 0.5, "a logit of 0 is even odds");
        assert_eq!(a.alpha[2], 0.0, "NaN is scrubbed to 0, never propagated");
        assert_eq!((a.width, a.height), (MODEL_SIZE, MODEL_SIZE));
    }

    #[test]
    fn an_output_of_the_wrong_size_is_an_inference_error() {
        assert!(matches!(
            postprocess(&[0.0; 10]),
            Err(SegmentError::Inference(_))
        ));
    }

    #[test]
    fn the_provider_serves_subject_only_and_pins_the_artifact_revision() {
        let p = BiRefNetProvider;
        assert_eq!(p.targets(), &[SegmentTarget::Subject]);
        assert!(p.artifacts().iter().any(|a| a.id == BIREFNET.id));
        // The version string names the pinned revision, so swapping the weights forces a new edit.
        assert!(BIREFNET.url.contains("534d3c82d3bb8b2f"));
        assert!(BIREFNET_VERSION.ends_with("534d3c82"));
    }

    #[test]
    fn loading_without_the_model_installed_says_so_instead_of_crashing() {
        let store = nicti_stalk::models::ModelStore::new(
            std::env::temp_dir().join(format!("nicti-birefnet-missing-{}", std::process::id())),
        );
        let err = BiRefNetProvider
            .load(&LoadContext {
                store: &store,
                ort_dylib: Some(std::path::Path::new("/nonexistent/libonnxruntime.so")),
            })
            .err()
            .expect("nothing is installed");
        assert!(matches!(err, SegmentError::NotInstalled(_)), "{err:?}");
        let err = BiRefNetProvider
            .load(&LoadContext {
                store: &store,
                ort_dylib: None,
            })
            .err()
            .expect("nothing is installed");
        assert!(matches!(err, SegmentError::NotInstalled(_)), "{err:?}");
    }
}
