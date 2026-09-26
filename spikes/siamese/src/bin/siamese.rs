//! Minimal demo/bench binary for #48's spike -- `segment` runs a real BiRefNet/MobileSAM ONNX
//! model against an image if one is available (env-var paths, no bundled weights, see
//! `segment.rs`'s module doc), `compose` renders a small mask-group demo to a PNG, and `bench`
//! prints CPU timings for the geometry rasterizers and compose step as a small JSON summary, in
//! the spirit of `spikes/groom/src/bin/groom.rs`'s own JSON-bench-output convention.

use std::path::PathBuf;
use std::time::Instant;

use clap::{Parser, Subcommand};
use siamese::compose::{ai_bake_key, compose, AiRecipe, MaskComponent, MaskGroup, MaskSource, Op};
use siamese::geometry::{Dab, Geometry, RadialGradient, Stroke};
use siamese::image::{Field, Image};
use siamese::segment::{BiRefNet, ExecutionProviderKind};

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Segment an image with a real BiRefNet ONNX model, if one is present on disk.
    Segment {
        /// Path to a BiRefNet .onnx file.
        #[arg(long)]
        model: PathBuf,
        /// Path to a real ONNX Runtime shared library (libonnxruntime.so / .dylib / .dll).
        #[arg(long)]
        ort_dylib: PathBuf,
        /// Requested execution provider (a request, not a guarantee -- see segment.rs's doc).
        #[arg(long, default_value = "cpu")]
        ep: String,
        /// Output alpha PNG path.
        #[arg(long, default_value = "alpha.png")]
        out: PathBuf,
        /// Square input size (BiRefNet is typically run at a fixed resolution like 1024).
        #[arg(long, default_value_t = 256)]
        size: usize,
    },
    /// Render a small demo mask group (subject AI recipe + its inverse + a radial gradient +
    /// a brush stroke) to a PNG, proving the compose model end to end.
    Compose {
        #[arg(long, default_value = "compose.png")]
        out: PathBuf,
    },
    /// Print CPU timings for the geometry rasterizers and compose step.
    Bench,
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::Segment {
            model,
            ort_dylib,
            ep,
            out,
            size,
        } => run_segment(&model, &ort_dylib, &ep, &out, size),
        Command::Compose { out } => run_compose_demo(&out),
        Command::Bench => run_bench(),
    }
}

fn run_segment(model: &PathBuf, ort_dylib: &PathBuf, ep: &str, out: &PathBuf, size: usize) {
    let ep_kind = match ep {
        "cpu" => ExecutionProviderKind::Cpu,
        "cuda" => ExecutionProviderKind::Cuda,
        "tensorrt" => ExecutionProviderKind::TensorRt,
        other => {
            eprintln!("unknown --ep {other}, falling back to cpu");
            ExecutionProviderKind::Cpu
        }
    };
    eprintln!("requested execution provider: {ep_kind:?}");

    let birefnet = BiRefNet::new(model, ort_dylib);
    // A flat mid-gray placeholder input -- this binary doesn't decode a real photo (no RAW/JPEG
    // decode dependency in this spike), it exists to prove the model-load/inference path is
    // reachable end to end when real weights are supplied, per this module's doc comment.
    let image = vec![0.5f32; size * size * 3];
    let start = Instant::now();
    match birefnet.segment(&image, size, size) {
        Ok(alpha) => {
            let elapsed = start.elapsed();
            eprintln!("segment: {:?}", elapsed);
            let mut img = image::GrayImage::new(alpha.width as u32, alpha.height as u32);
            for (i, px) in img.pixels_mut().enumerate() {
                let v = (alpha.data[i].clamp(0.0, 1.0) * 255.0) as u8;
                *px = image::Luma([v]);
            }
            img.save(out).expect("failed to write alpha PNG");
            eprintln!("wrote {}", out.display());
        }
        Err(e) => eprintln!("segment failed: {e}"),
    }
}

fn run_compose_demo(out: &PathBuf) {
    let (width, height) = (128usize, 96usize);
    let upstream = blake3::hash(b"demo-neutral-render");
    let recipe = AiRecipe {
        model_id: "nicti.ai.birefnet".to_string(),
        model_version: "1.4.0".to_string(),
        params: serde_json::json!({ "target": "subject" }),
        seed: None,
    };
    // Stand in for a real baked BiRefNet alpha: a soft ellipse in the middle of the frame.
    let mut baked = Field::new(width, height, 0.0);
    for y in 0..height {
        for x in 0..width {
            let (dx, dy) = (
                (x as f32 - width as f32 / 2.0) / (width as f32 / 4.0),
                (y as f32 - height as f32 / 2.0) / (height as f32 / 3.0),
            );
            let d = (dx * dx + dy * dy).sqrt();
            baked.data[y * width + x] = (1.0 - d).clamp(0.0, 1.0);
        }
    }
    let key = ai_bake_key(&MaskSource::Ai(recipe.clone()), upstream).unwrap();

    let group = MaskGroup {
        components: vec![
            MaskComponent {
                source: MaskSource::Ai(recipe.clone()),
                op: Op::Add,
                invert: false,
                opacity: 1.0,
            },
            MaskComponent {
                source: MaskSource::Geometry(Geometry::RadialGradient(RadialGradient {
                    center: (width as f32 * 0.75, height as f32 * 0.25),
                    radii: (30.0, 20.0),
                    angle: 0.3,
                    feather: 8.0,
                    invert: false,
                })),
                op: Op::Add,
                invert: false,
                opacity: 0.6,
            },
            MaskComponent {
                source: MaskSource::Geometry(Geometry::Brush(vec![Stroke {
                    dabs: vec![Dab {
                        center: (20.0, 20.0),
                        radius: 15.0,
                        feather: 4.0,
                        flow: 1.0,
                    }],
                    erase: false,
                }])),
                op: Op::Subtract,
                invert: false,
                opacity: 1.0,
            },
        ],
    };
    let composed = compose(&group, width, height, upstream, |k| {
        (k == key).then(|| baked.clone())
    });

    let inverse_group = MaskGroup {
        components: vec![MaskComponent {
            source: MaskSource::Ai(recipe),
            op: Op::Add,
            invert: true,
            opacity: 1.0,
        }],
    };
    let inverse = compose(&inverse_group, width, height, upstream, |k| {
        (k == key).then(|| baked.clone())
    });

    let mut img = Image::new(width * 2, height, [0.0; 4]);
    for y in 0..height {
        for x in 0..width {
            let v = composed.get(x as i32, y as i32);
            img.set(x as i32, y as i32, [v, v, v, 1.0]);
            let iv = inverse.get(x as i32, y as i32);
            img.set((x + width) as i32, y as i32, [iv, iv, iv, 1.0]);
        }
    }
    let mut out_img = image::RgbImage::new(img.width as u32, img.height as u32);
    for (i, px) in out_img.pixels_mut().enumerate() {
        let [r, g, b, _] = img.data[i];
        *px = image::Rgb([
            (r.clamp(0.0, 1.0) * 255.0) as u8,
            (g.clamp(0.0, 1.0) * 255.0) as u8,
            (b.clamp(0.0, 1.0) * 255.0) as u8,
        ]);
    }
    out_img.save(out).expect("failed to write compose PNG");
    eprintln!("wrote {} (left: group, right: its inverse)", out.display());
}

fn run_bench() {
    let (width, height) = (2048usize, 1536usize);
    let gradient = Geometry::RadialGradient(RadialGradient {
        center: (width as f32 / 2.0, height as f32 / 2.0),
        radii: (400.0, 300.0),
        angle: 0.2,
        feather: 40.0,
        invert: false,
    });
    let brush = Geometry::Brush(vec![Stroke {
        dabs: (0..50)
            .map(|i| Dab {
                center: (i as f32 * 30.0, height as f32 / 2.0),
                radius: 40.0,
                feather: 10.0,
                flow: 1.0,
            })
            .collect(),
        erase: false,
    }]);

    let start = Instant::now();
    let _ = gradient.rasterize(width, height);
    let radial_ms = start.elapsed().as_secs_f64() * 1000.0;

    let start = Instant::now();
    let _ = brush.rasterize(width, height);
    let brush_ms = start.elapsed().as_secs_f64() * 1000.0;

    println!(
        "{}",
        serde_json::json!({
            "resolution": [width, height],
            "radial_gradient_ms": radial_ms,
            "brush_50_dabs_ms": brush_ms,
        })
    );
}
