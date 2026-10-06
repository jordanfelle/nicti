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

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

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
        let (dylib, fp32, fp16, artifacts, ort_from_store, gpu_allowed) = match ctx.ort_dylib {
            Some(dylib) => {
                let model = ctx.store.installed_path(&BIREFNET).ok_or_else(|| {
                    SegmentError::NotInstalled(format!("{} is not downloaded", BIREFNET.label))
                })?;
                // A caller-supplied runtime (dev/tests) may use a GPU provider with the fp32 model.
                (
                    dylib.to_path_buf(),
                    model,
                    None,
                    mask_artifacts(),
                    false,
                    true,
                )
            }
            None => {
                let models = MaskModels::locate(ctx.store).ok_or_else(|| {
                    SegmentError::NotInstalled(format!(
                        "{} and an ONNX Runtime library are needed",
                        BIREFNET.label
                    ))
                })?;
                let from_store = std::env::var_os("NICTI_ORT_DYLIB").is_none_or(|v| v.is_empty());
                let artifacts = models.artifacts();
                // The store's GPU pack is only *used* once its fp16 model is there too: an
                // interrupted pack download leaves the CUDA runtime installed without it, and fp32
                // on CUDA needs ~13 GB of VRAM.
                let gpu_allowed = models.birefnet_gpu.is_some();
                (
                    models.ort_dylib,
                    models.birefnet,
                    models.birefnet_gpu,
                    artifacts,
                    from_store,
                    gpu_allowed,
                )
            }
        };
        verify_artifacts(ctx.store, &artifacts, ort_from_store).map_err(SegmentError::Verify)?;
        nicti_haw::ensure_ort_environment(&dylib).map_err(|e| SegmentError::Load(e.to_string()))?;
        // A CUDA build of the runtime asks for the CUDA provider (`NICTI_ORT_EP` overrides); the CPU
        // build -- or a GPU provider that can't start -- runs on the CPU.
        let preferred = if gpu_allowed {
            nicti_haw::ExecutionProvider::for_runtime(&dylib)
        } else {
            nicti_haw::ExecutionProvider::Cpu
        };
        let requested = nicti_haw::ExecutionProvider::resolve(preferred);
        let (session, active, fp16_used) = open_session(requested, &fp32, fp16.as_deref())?;
        eprintln!("nicti-siamese: BiRefNet session on the {active:?} execution provider");
        DECLARED_VRAM.store(vram_for(active, fp16_used), Ordering::Relaxed);
        let (input_name, output_name) = io_names(&session)?;
        Ok(Box::new(BiRefNetSegmenter {
            session,
            input_name,
            output_name,
            active,
            fp32,
        }))
    }
}

/// Peak VRAM one BiRefNet bake takes on the CUDA provider with the fp16 model, measured on the RTX
/// 5080 reference machine (#345, ADR-0049): ~7.7 GiB over the idle baseline, declared as 8 GiB.
/// The fp32 model peaks at ~12.5 GiB ([`GPU_FP32_VRAM_BYTES`]), which is why the GPU pack ships fp16.
pub const GPU_VRAM_BYTES: u64 = 8 << 30;

/// Peak VRAM of the fp32 model on the CUDA provider (measured ~12.5 GiB), declared as 13 GiB -- only
/// reached when a caller supplies its own GPU runtime and the fp32 file (dev/tests, `NICTI_ORT_DYLIB`).
pub const GPU_FP32_VRAM_BYTES: u64 = 13 << 30;

static DECLARED_VRAM: AtomicU64 = AtomicU64::new(0);

/// What a bake claims from Pounce's VRAM budget: [`GPU_VRAM_BYTES`] once BiRefNet is running on a
/// GPU provider, else 0 (the CPU provider uses no VRAM, and nothing is loaded before the first
/// bake). Lock-free because `MaskBakeJob::spec` runs on the UI thread while a bake may hold the
/// backend; process-wide because ONNX Runtime's environment is.
pub fn declared_vram_bytes() -> u64 {
    DECLARED_VRAM.load(Ordering::Relaxed)
}

fn vram_for(active: nicti_haw::ExecutionProvider, fp16: bool) -> u64 {
    match (active, fp16) {
        (nicti_haw::ExecutionProvider::Cpu, _) => 0,
        (_, true) => GPU_VRAM_BYTES,
        (_, false) => GPU_FP32_VRAM_BYTES,
    }
}

/// Builds the session on `requested`: the fp16 model on the CUDA provider (about 8 GB of VRAM,
/// versus ~13 GB for fp32), else the fp32 model. If the GPU provider can't register, or the model
/// won't load on it, falls back to the CPU provider on the fp32 model.
fn open_session(
    requested: nicti_haw::ExecutionProvider,
    fp32: &Path,
    fp16: Option<&Path>,
) -> Result<(Session, nicti_haw::ExecutionProvider, bool), SegmentError> {
    use nicti_haw::ExecutionProvider::Cpu;
    let nicti_haw::ConfiguredBuilder {
        builder: mut session_builder,
        active,
    } = nicti_haw::session_builder(requested).map_err(|e| SegmentError::Load(e.to_string()))?;
    let (model, fp16_used) = match (active, fp16) {
        (Cpu, _) | (_, None) => (fp32, false),
        (_, Some(fp16)) => (fp16, true),
    };
    match session_builder.commit_from_file(model) {
        Ok(session) => Ok((session, active, fp16_used)),
        Err(e) if active != Cpu => {
            eprintln!("nicti-siamese: BiRefNet failed to load on {active:?} ({e}); using the CPU");
            open_session(Cpu, fp32, None)
        }
        Err(e) => Err(SegmentError::Load(e.to_string())),
    }
}

fn io_names(session: &Session) -> Result<(String, String), SegmentError> {
    let input = session
        .inputs()
        .first()
        .map(|i| i.name().to_owned())
        .ok_or_else(|| SegmentError::Load("the model declares no input".into()))?;
    let output = session
        .outputs()
        .first()
        .map(|o| o.name().to_owned())
        .ok_or_else(|| SegmentError::Load("the model declares no output".into()))?;
    Ok((input, output))
}

struct BiRefNetSegmenter {
    session: Session,
    input_name: String,
    output_name: String,
    /// The provider the session is really running on.
    active: nicti_haw::ExecutionProvider,
    /// The fp32 model, which a failed GPU run falls back to on the CPU.
    fp32: PathBuf,
}

impl BiRefNetSegmenter {
    fn infer(&mut self, input: Vec<f32>) -> Result<AlphaMap, SegmentError> {
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

impl Segmenter for BiRefNetSegmenter {
    fn segment(&mut self, image: &ModelImage, params: &Value) -> Result<AlphaMap, SegmentError> {
        match SegmentTarget::from_params(params)? {
            SegmentTarget::Subject => {}
            other => return Err(SegmentError::UnsupportedTarget(other.as_str().to_owned())),
        }
        let input = preprocess(image)?;
        match self.infer(input.clone()) {
            Err(SegmentError::Inference(e)) if self.active != nicti_haw::ExecutionProvider::Cpu => {
                // The GPU provider registered but can't run (cuDNN missing from the pack, out of
                // VRAM beside the renderer, a driver reset): finish this bake -- and every later
                // one -- on the CPU rather than failing the mask.
                eprintln!(
                    "nicti-siamese: BiRefNet run failed on {:?} ({e}); using the CPU",
                    self.active
                );
                let (session, active, fp16_used) =
                    open_session(nicti_haw::ExecutionProvider::Cpu, &self.fp32, None)?;
                (self.input_name, self.output_name) = io_names(&session)?;
                self.session = session;
                self.active = active;
                DECLARED_VRAM.store(vram_for(active, fp16_used), Ordering::Relaxed);
                self.infer(input)
            }
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_gpu_provider_claims_vram() {
        use nicti_haw::ExecutionProvider::{Cpu, Cuda};
        assert_eq!(vram_for(Cpu, false), 0);
        assert_eq!(vram_for(Cpu, true), 0, "the CPU provider never claims VRAM");
        assert_eq!(vram_for(Cuda, true), GPU_VRAM_BYTES);
        // fp32 on a GPU (a caller-supplied runtime) takes far more than the fp16 the pack ships.
        assert_eq!(vram_for(Cuda, false), GPU_FP32_VRAM_BYTES);
    }

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
