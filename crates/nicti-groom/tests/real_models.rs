//! Runs the real MobileSAM and LaMa weights end to end. `#[ignore]`d: they need the ONNX files
//! (~250 MB, an explicit user download in the app -- `nicti_stalk::models`) and an ONNX Runtime
//! shared library, neither of which CI has.
//!
//! ```text
//! NICTI_TEST_ORT_DYLIB=/path/to/libonnxruntime.so \
//! NICTI_TEST_SAM_ENCODER=.../mobile_sam_image_encoder.onnx \
//! NICTI_TEST_SAM_DECODER=.../sam_mask_decoder_single.onnx \
//! NICTI_TEST_LAMA=.../lama_fp32.onnx \
//!   cargo test -p nicti-groom --release --test real_models -- --ignored --nocapture
//! ```
//!
//! These are what pin the wrappers' tensor contracts (names, layouts, LaMa's output scale) to the
//! actual models rather than to documentation.

use std::path::PathBuf;
use std::time::Instant;

use nicti_groom::lama::{self, Inpainter, Lama};
use nicti_groom::remove::{RemovalBackend, RemovalEngine, RemovalRequest};
use nicti_groom::sam::{MobileSam, ModelFrame, Prompt, Segmenter};
use nicti_groom::space::SpaceMap;
use nicti_groom::{PixelSource, RgbBuffer};

fn env_path(name: &str) -> PathBuf {
    PathBuf::from(std::env::var(name).unwrap_or_else(|_| panic!("set {name}")))
}

fn load_sam() -> MobileSam {
    MobileSam::load(
        &env_path("NICTI_TEST_SAM_ENCODER"),
        &env_path("NICTI_TEST_SAM_DECODER"),
        &env_path("NICTI_TEST_ORT_DYLIB"),
    )
    .expect("MobileSAM loads")
}

fn load_lama() -> Lama {
    Lama::load(
        &env_path("NICTI_TEST_LAMA"),
        &env_path("NICTI_TEST_ORT_DYLIB"),
    )
    .expect("LaMa loads")
}

/// A smooth sky-to-ground gradient (the "background") with a saturated red square ("the object")
/// at `(x0..x1, y0..y1)`. Camera-linear values, WB-neutral.
fn scene(w: u32, h: u32, obj: (u32, u32, u32, u32)) -> RgbBuffer {
    let background = |x: u32, y: u32| {
        let (fx, fy) = (x as f32 / w as f32, y as f32 / h as f32);
        [
            0.10 + 0.25 * fy,
            0.18 + 0.30 * fy,
            0.30 - 0.15 * fy + 0.05 * fx,
        ]
    };
    RgbBuffer {
        width: w,
        height: h,
        data: (0..w * h)
            .map(|i| {
                let (x, y) = (i % w, i / w);
                if x >= obj.0 && x < obj.2 && y >= obj.1 && y < obj.3 {
                    [0.85, 0.04, 0.03]
                } else {
                    background(x, y)
                }
            })
            .collect(),
    }
}

#[test]
#[ignore = "needs the real ONNX weights and an ONNX Runtime library"]
fn mobile_sam_segments_a_clicked_object() {
    let img = scene(640, 480, (280, 200, 360, 280));
    let space = SpaceMap::for_source([1.0; 4], &img);
    let frame = ModelFrame::build(&img, &space);
    let mut sam = load_sam();

    let click = Prompt::Click { x: 320.0, y: 240.0 }.scaled(frame.scale_x, frame.scale_y);
    let t = Instant::now();
    let mask = sam
        .segment(1, &frame, click)
        .expect("first segment (runs the encoder)");
    let first = t.elapsed();
    let t = Instant::now();
    sam.segment(1, &frame, click)
        .expect("second segment (cached embedding)");
    let second = t.elapsed();
    println!("MobileSAM: encoder+decoder {first:?}, decoder only {second:?}");
    assert!(
        second < first,
        "the second click must reuse the cached embedding"
    );

    // IoU of the mask against the true square, in the model frame.
    let (mut inter, mut uni) = (0usize, 0usize);
    for i in 0..mask.width * mask.height {
        let (mx, my) = ((i % mask.width) as f32 + 0.5, (i / mask.width) as f32 + 0.5);
        let (sx, sy) = (mx / frame.scale_x, my / frame.scale_y);
        let truth = (280.0..360.0).contains(&sx) && (200.0..280.0).contains(&sy);
        let pred = mask.logits[i] > 0.0;
        inter += usize::from(truth && pred);
        uni += usize::from(truth || pred);
    }
    let iou = inter as f32 / uni as f32;
    println!("MobileSAM IoU on a synthetic square: {iou:.3}");
    assert!(iou > 0.8, "IoU {iou}");
}

#[test]
#[ignore = "needs the real ONNX weights and an ONNX Runtime library"]
fn lama_fills_a_hole_and_leaves_the_rest_alone() {
    let s = lama::SIZE;
    // Smooth gradient image in 0..1, CHW; a 96x96 hole in the middle.
    let mut image = vec![0.0f32; 3 * s * s];
    for y in 0..s {
        for x in 0..s {
            let (fx, fy) = (x as f32 / s as f32, y as f32 / s as f32);
            let px = [0.25 + 0.4 * fy, 0.35 + 0.3 * fy, 0.6 - 0.2 * fy + 0.1 * fx];
            for c in 0..3 {
                image[c * s * s + y * s + x] = px[c];
            }
        }
    }
    let original = image.clone();
    let mut mask = vec![0.0f32; s * s];
    for y in 208..304 {
        for x in 208..304 {
            mask[y * s + x] = 1.0;
            for c in 0..3 {
                image[c * s * s + y * s + x] = 1.0; // white blob to be removed
            }
        }
    }

    let mut lama = load_lama();
    let t = Instant::now();
    let out = lama.inpaint(&image, &mask).expect("LaMa runs");
    println!("LaMa 512x512 inpaint: {:?}", t.elapsed());
    assert!(out.iter().all(|v| (0.0..=1.0).contains(v)));

    // Outside the hole the output is the input; inside it is a plausible continuation of the
    // gradient (close to the original, far from the white we painted in).
    let (mut outside, mut n_out, mut inside, mut n_in) = (0.0f32, 0usize, 0.0f32, 0usize);
    for y in 0..s {
        for x in 0..s {
            for c in 0..3 {
                let i = c * s * s + y * s + x;
                let err = (out[i] - original[i]).abs();
                if mask[y * s + x] > 0.5 {
                    inside += err;
                    n_in += 1;
                } else {
                    outside += err;
                    n_out += 1;
                }
            }
        }
    }
    let (outside, inside) = (outside / n_out as f32, inside / n_in as f32);
    println!(
        "LaMa mean abs error vs the true background: outside hole {outside:.4}, inside {inside:.4}"
    );
    assert!(outside < 0.05, "unmasked region drifted: {outside}");
    assert!(
        inside < 0.15,
        "hole not filled plausibly: {inside} (white would be ~0.5)"
    );
}

#[test]
#[ignore = "needs the real ONNX weights and an ONNX Runtime library"]
fn a_click_removes_the_object_end_to_end() {
    let img = scene(1600, 1200, (700, 500, 860, 660));
    let mut engine = RemovalEngine::new(load_sam(), load_lama());
    let req = RemovalRequest {
        image_key: 42,
        source: &img,
        cam_mul: [1.0; 4],
        prompt: Prompt::Click { x: 780.0, y: 580.0 },
        center: (780.0, 580.0),
        radius: 200.0,
    };
    let t = Instant::now();
    let patch = engine.remove(&req).expect("removal succeeds");
    println!(
        "end-to-end removal (cold: load-free, first embedding): {:?}",
        t.elapsed()
    );
    let t = Instant::now();
    engine.remove(&req).expect("second removal");
    println!("end-to-end removal (embedding cached): {:?}", t.elapsed());

    // Compare the patch's fill inside the object against the true background there.
    let half = patch.side as i32 / 2;
    let background = |x: u32, y: u32| {
        let (fx, fy) = (x as f32 / 1600.0, y as f32 / 1200.0);
        [
            0.10 + 0.25 * fy,
            0.18 + 0.30 * fy,
            0.30 - 0.15 * fy + 0.05 * fx,
        ]
    };
    let (mut err, mut red_err, mut n) = (0.0f32, 0.0f32, 0usize);
    for y in 500..660u32 {
        for x in 700..860u32 {
            let (px, py) = (
                x as i32 - patch.center.0 + half,
                y as i32 - patch.center.1 + half,
            );
            let p = patch.pixel((py * patch.side as i32 + px) as usize);
            assert!(
                p[3] > 0.99,
                "object pixel ({x},{y}) not fully covered: weight {}",
                p[3]
            );
            let bg = background(x, y);
            for c in 0..3 {
                err += (p[c] - bg[c]).abs();
                red_err += (p[c] - img.pixel(x, y)[c]).abs();
            }
            n += 3;
        }
    }
    let (err, red_err) = (err / n as f32, red_err / n as f32);
    println!("fill vs true background: {err:.4}; fill vs the object it replaced: {red_err:.4}");
    assert!(err < 0.06, "fill should look like the background: {err}");
    assert!(
        err < red_err / 3.0,
        "fill must be much closer to background than to the object"
    );
}

/// The manifest's pinned sizes and SHA-256s must describe the real files, or a genuine download
/// would be rejected (or, worse, a wrong hash would never be noticed until a user hit it). Lays the
/// real files out as a model store and runs the production verifier over them.
#[test]
#[ignore = "needs the real ONNX weights"]
fn the_manifest_hashes_match_the_real_files() {
    use nicti_stalk::models::{
        verify_removal_install, ModelStore, Status, LAMA, MOBILE_SAM_DECODER, MOBILE_SAM_ENCODER,
    };
    let root = std::env::temp_dir().join(format!("nicti-real-store-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let store = ModelStore::new(&root);
    for (artifact, env) in [
        (&MOBILE_SAM_ENCODER, "NICTI_TEST_SAM_ENCODER"),
        (&MOBILE_SAM_DECODER, "NICTI_TEST_SAM_DECODER"),
        (&LAMA, "NICTI_TEST_LAMA"),
    ] {
        let dest = store.path(artifact);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::copy(env_path(env), &dest).unwrap();
        assert_eq!(
            store.status(artifact),
            Status::Installed,
            "{} has the wrong size",
            artifact.id
        );
        assert!(
            store.verify(artifact).unwrap(),
            "{} does not match its pinned SHA-256",
            artifact.id
        );
    }
    // ort_from_store = false: the Linux runtime here isn't a pinned artifact.
    verify_removal_install(&store, false).expect("the whole install verifies");
    let _ = std::fs::remove_dir_all(&root);
}
