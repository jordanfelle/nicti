//! Runs the real BiRefNet weights end to end. `#[ignore]`d: they need the ~970 MB ONNX file (an
//! explicit user download in the app -- `nicti_stalk::models::BIREFNET`) and an ONNX Runtime shared
//! library, neither of which CI has.
//!
//! The model must be at its pinned location in the model store, because the provider verifies the
//! file's SHA-256 before loading it (ADR-0218): `<NICTI_MODELS_DIR>/birefnet/birefnet_fp32.onnx`,
//! the exact bytes of `nicti_stalk::models::BIREFNET`.
//!
//! ```text
//! NICTI_TEST_ORT_DYLIB=/path/to/libonnxruntime.so \
//! NICTI_MODELS_DIR=/path/to/models \
//!   cargo test -p nicti-siamese --release --test real_models -- --ignored --nocapture
//! ```
//!
//! This is what pins the wrapper's tensor contract (input/output names, `[1,3,1024,1024]` planar
//! ImageNet-normalized input, `[1,1,1024,1024]` logit output) to the actual model rather than to
//! its README, and records the CPU-provider latency that ADR-0048's "<= 1 s per bake" hypothesis
//! (which assumed CUDA) is judged against.

use std::path::PathBuf;
use std::time::Instant;

use nicti_groom::RgbBuffer;
use nicti_siamese::backend::{BakeRequest, MaskBackend, RegistryBackend};
use nicti_siamese::providers::{recipe_for, segmentation_registry};
use nicti_stalk::models::ModelStore;
use nicti_stalk::SegmentTarget;
use std::sync::Arc;

fn env_path(name: &str) -> PathBuf {
    PathBuf::from(std::env::var(name).unwrap_or_else(|_| panic!("set {name}")))
}

/// A bright, warm disc (the "subject") on a dark, cool background -- crude, but any salient-object
/// model must put its mass on the disc.
fn disc_scene(w: u32, h: u32) -> RgbBuffer {
    let (cx, cy, r) = (w as f32 * 0.5, h as f32 * 0.5, h.min(w) as f32 * 0.28);
    RgbBuffer {
        width: w,
        height: h,
        data: (0..w * h)
            .map(|i| {
                let (x, y) = ((i % w) as f32, (i / w) as f32);
                let d = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
                if d < r {
                    [0.75, 0.55, 0.35]
                } else {
                    [0.06, 0.08, 0.12]
                }
            })
            .collect(),
    }
}

#[test]
#[ignore = "needs the BiRefNet weights and an ONNX Runtime library on disk"]
fn birefnet_segments_a_subject_with_the_documented_tensor_contract() {
    let store = ModelStore::new(env_path("NICTI_MODELS_DIR"));
    let dylib = env_path("NICTI_TEST_ORT_DYLIB");
    let mut backend = RegistryBackend::new(Arc::new(segmentation_registry()), store, Some(dylib));
    let recipe = recipe_for(SegmentTarget::Subject);
    let scene = disc_scene(640, 480);

    let load_start = Instant::now();
    let cold = Instant::now();
    let alpha = backend
        .bake(&BakeRequest {
            image_key: 1,
            source: &scene,
            cam_mul: [1.0; 4],
            recipe: &recipe,
        })
        .expect("BiRefNet loads, verifies and runs");
    println!(
        "cold bake (load + verify + first run): {:.1?}",
        load_start.elapsed()
    );
    // The contract: a 1024x1024 alpha in 0..=1.
    assert_eq!((alpha.width, alpha.height), (1024, 1024));
    assert!(alpha.alpha.iter().all(|v| (0.0..=1.0).contains(v)));

    // A salient disc: much more mass inside it than outside (in the stretched 1024x1024 frame).
    let at = |fx: f32, fy: f32| alpha.alpha[(fy * 1023.0) as usize * 1024 + (fx * 1023.0) as usize];
    let inside = at(0.5, 0.5);
    let outside = at(0.05, 0.05);
    println!("alpha inside the disc {inside:.3}, in a corner {outside:.3}");
    assert!(
        inside > 0.6 && outside < 0.4,
        "subject not found: inside {inside}, outside {outside}"
    );

    // Warm run on another photo: no reload, and this is the latency that matters interactively.
    let warm_scene = disc_scene(800, 600);
    let warm = Instant::now();
    backend
        .bake(&BakeRequest {
            image_key: 2,
            source: &warm_scene,
            cam_mul: [1.0; 4],
            recipe: &recipe,
        })
        .unwrap();
    println!("warm bake (CPU execution provider): {:.1?}", warm.elapsed());
    let _ = cold;
}

#[test]
#[ignore = "needs the BiRefNet weights and an ONNX Runtime library on disk"]
fn a_wrong_size_model_file_is_refused_before_it_is_handed_to_onnx_runtime() {
    // A file of the wrong size at the pinned path must fail the integrity check, not reach ort.
    let dir = std::env::temp_dir().join(format!("nicti-birefnet-bad-{}", std::process::id()));
    let path = dir.join("birefnet").join("birefnet_fp32.onnx");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"not a model").unwrap();
    let mut backend = RegistryBackend::new(
        Arc::new(segmentation_registry()),
        ModelStore::new(&dir),
        Some(env_path("NICTI_TEST_ORT_DYLIB")),
    );
    let recipe = recipe_for(SegmentTarget::Subject);
    let scene = disc_scene(64, 48);
    let err = backend
        .bake(&BakeRequest {
            image_key: 1,
            source: &scene,
            cam_mul: [1.0; 4],
            recipe: &recipe,
        })
        .expect_err("a bogus model file must be refused");
    let message = err.to_string();
    assert!(
        message.contains("not installed") || message.contains("integrity"),
        "{message}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
