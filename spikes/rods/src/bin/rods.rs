use std::path::PathBuf;

use clap::{Parser, Subcommand};
use rods::{ai, align, display, linear_input};

#[derive(Parser)]
#[command(
    name = "rods",
    about = "Spike for #40 (ADR-0023): demosaic + denoise comparison"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Aligns and scores one candidate's `retina dump-classic`/`dump-linear` output against a
    /// reference pair (ground truth, or another candidate) -- PSNR/SSIM in the fixed display
    /// encoding both share, after sub-pixel alignment and a per-channel gain fit.
    Compare {
        #[arg(long)]
        ref_tiff: PathBuf,
        #[arg(long)]
        ref_json: PathBuf,
        #[arg(long)]
        candidate_tiff: PathBuf,
        #[arg(long)]
        candidate_json: PathBuf,
        /// Reject (report, don't fail) an alignment shift past this many pixels.
        #[arg(long, default_value_t = 0.25)]
        shift_tolerance: f64,
        /// Path B AI candidate: run this ONNX model over the candidate's fixed-display-sRGB
        /// image before scoring (an NCHW-float, dynamic-shape denoiser -- NAFNet-SIDD/SCUNet's
        /// `deepghs/image_restoration` ONNX exports, or any model sharing that contract).
        /// Requires --ort-dylib. Omit both to score the classic-demosaic-only baseline.
        #[arg(long, requires = "ort_dylib")]
        denoise_model: Option<PathBuf>,
        #[arg(long)]
        ort_dylib: Option<PathBuf>,
        #[arg(long, default_value_t = 512)]
        tile: u32,
        #[arg(long, default_value_t = 32)]
        overlap: u32,
        /// Score only a centered crop of this size (both dimensions), instead of the full image
        /// -- a fast CPU dev-loop knob, especially with --denoise-model on hardware with no GPU
        /// execution provider (full-resolution real numbers are a Windows-native/GPU job, not a
        /// WSL/CPU one). Omit for the full image.
        #[arg(long)]
        crop: Option<u32>,
        /// Times the denoise stage under `nicti_prowl::perf::Protocol` (1 warmup + 5 measured
        /// runs, p50/p95/max) instead of running it once. CPU-only dev-loop numbers, not the
        /// real ADR measurement -- that's a Windows-native/GPU job (P5). No-op without
        /// --denoise-model.
        #[arg(long)]
        time: bool,
        /// Execution provider for the denoise stage. CUDA/TensorRT need a matching Windows-
        /// native onnxruntime.dll + CUDA/cuDNN/TensorRT install (see the plan's P0) -- on WSL,
        /// where none of that exists, `ort` falls back to CPU silently (see `ai.rs`'s own doc
        /// comment on why that fallback can't be introspected after the fact).
        #[arg(long, value_enum, default_value_t = ai::ExecutionProviderKind::Cpu)]
        ep: ai::ExecutionProviderKind,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Compare {
            ref_tiff,
            ref_json,
            candidate_tiff,
            candidate_json,
            shift_tolerance,
            denoise_model,
            ort_dylib,
            tile,
            overlap,
            crop,
            time,
            ep,
        } => compare(
            &ref_tiff,
            &ref_json,
            &candidate_tiff,
            &candidate_json,
            shift_tolerance,
            denoise_model.as_deref(),
            ort_dylib.as_deref(),
            ai::TileConfig { tile, overlap },
            crop,
            time,
            ep,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn compare(
    ref_tiff: &std::path::Path,
    ref_json: &std::path::Path,
    candidate_tiff: &std::path::Path,
    candidate_json: &std::path::Path,
    shift_tolerance: f64,
    denoise_model: Option<&std::path::Path>,
    ort_dylib: Option<&std::path::Path>,
    tile_config: ai::TileConfig,
    crop: Option<u32>,
    time: bool,
    ep: ai::ExecutionProviderKind,
) -> anyhow::Result<()> {
    let reference = linear_input::load(ref_tiff, ref_json)?;
    let candidate = linear_input::load(candidate_tiff, candidate_json)?;

    anyhow::ensure!(
        reference.image.dimensions() == candidate.image.dimensions(),
        "reference {:?} and candidate {:?} dimensions differ -- common-crop them first",
        reference.image.dimensions(),
        candidate.image.dimensions(),
    );
    let (full_width, full_height) = reference.image.dimensions();

    if let Some(size) = crop {
        // A zero (or otherwise degenerate) crop flows unvalidated into every downstream stage --
        // caught in adversarial review producing a debug-build subtract-with-overflow panic (and
        // in release, an out-of-bounds slice index once the wrapped value gets `.min()`-clamped
        // back into range) inside ai.rs's build_padded_tile. Reject it here instead, where the
        // real invariant (a crop must be a real, positive size) actually belongs.
        anyhow::ensure!(size > 0, "--crop must be greater than 0, got {size}");
    }
    let (width, height) = match crop {
        Some(size) => (size.min(full_width), size.min(full_height)),
        None => (full_width, full_height),
    };
    let (x0, y0) = ((full_width - width) / 2, (full_height - height) / 2);
    if crop.is_some() {
        println!("cropping to {width}x{height} centered at ({x0},{y0})");
    }
    let crop_view = |img: &image::ImageBuffer<image::Rgb<u16>, Vec<u16>>| -> Vec<image::Rgb<u16>> {
        (y0..y0 + height)
            .flat_map(|y| (x0..x0 + width).map(move |x| *img.get_pixel(x, y)))
            .collect()
    };
    let reference_pixels = crop_view(&reference.image);
    let candidate_pixels = crop_view(&candidate.image);
    let pixels = width as usize * height as usize;

    // Fixed display treatment: camera RGB (already WB-applied by `dump-classic`, black/white
    // scaled by `linearize_sample`) -> XYZ(D50) -> linear sRGB -> sRGB OETF, per pixel, for both
    // images, using each image's own decoder-reported `cam_xyz`.
    let to_srgb_planes = |px: &[image::Rgb<u16>], cam_xyz: &[f32; 12]| -> Vec<f32> {
        let mut out = vec![0.0f32; pixels * 3];
        for (i, p) in px.iter().enumerate() {
            let cam_rgb = [
                linear_input::linearize_sample(p[0]),
                linear_input::linearize_sample(p[1]),
                linear_input::linearize_sample(p[2]),
            ];
            let srgb = display::to_display_srgb(cam_rgb, cam_xyz);
            out[i * 3] = srgb[0];
            out[i * 3 + 1] = srgb[1];
            out[i * 3 + 2] = srgb[2];
        }
        out
    };

    let ref_srgb = to_srgb_planes(&reference_pixels, &reference.meta.cam_xyz);
    let cand_srgb_raw = to_srgb_planes(&candidate_pixels, &candidate.meta.cam_xyz);

    // Luma plane (simple average, not a weighted luma -- good enough for the alignment estimate,
    // which only needs gradient structure, not colorimetric accuracy) for shift estimation.
    let luma = |srgb: &[f32]| -> align::Plane {
        let samples = srgb
            .as_chunks::<3>()
            .0
            .iter()
            .map(|px| (px[0] + px[1] + px[2]) / 3.0)
            .collect();
        align::Plane {
            width,
            height,
            samples,
        }
    };

    // Estimated from the *raw* (pre-denoise) candidate, always -- this is a real-camera geometry
    // question (how far apart were these two exposures), independent of which Path A/B candidate
    // gets scored against the same reference. Re-estimating per-candidate on a denoised image was
    // a real bug: a smoother candidate's gradient structure shifts where Lucas-Kanade converges,
    // producing a spurious "misregistration" warning that isn't a real geometric shift -- caught
    // when SCUNet's own smoother output tripped the 0.25px gate while the identical raw-candidate
    // shift did not.
    let ref_luma = luma(&ref_srgb);
    let raw_cand_luma = luma(&cand_srgb_raw);
    let margin = 16.min(width / 4).min(height / 4);
    let shift = align::estimate_shift(&ref_luma, &raw_cand_luma, margin, 30);
    println!(
        "shift (measured pre-denoise): dx={:.4} dy={:.4} (tolerance {shift_tolerance})",
        shift.dx, shift.dy
    );
    if !shift.within_tolerance(shift_tolerance) {
        println!("WARNING: shift exceeds tolerance -- scores below may reflect misregistration");
    }

    // Resample the candidate onto the reference's pixel grid before scoring or denoising --
    // otherwise a real sub-pixel-or-larger shift (a real second exposure, not just decoder
    // rounding) contaminates every metric below with misalignment error, not a demosaic/denoise
    // difference. A shift within tolerance still gets resampled (a no-op to within float
    // precision at that magnitude); only skipping this for an exact zero shift would be a
    // meaningless special case.
    let mut cand_srgb = align::resample_rgb(&cand_srgb_raw, width, height, shift);
    if let Some(model_path) = denoise_model {
        let ort_dylib = ort_dylib.expect("clap requires ort_dylib alongside denoise_model");
        println!(
            "running Path B AI denoise: {} (tile={}, overlap={}, ep={ep:?})",
            model_path.display(),
            tile_config.tile,
            tile_config.overlap
        );
        let mut denoiser = ai::TiledDenoiser::load(model_path, ort_dylib, ep)?;

        if time {
            // 1 warmup + 5 measured throwaway calls (docs/benchmarks.md's protocol), then one
            // more real call below whose output is what actually gets scored.
            let identity = nicti_prowl::perf::HardwareIdentity::capture();
            println!(
                "hardware: os={} arch={} cpus={} host={}",
                identity.os, identity.arch, identity.cpu_count, identity.hostname
            );
            let mut timed_result: Option<Result<Vec<f32>, ai::AiDenoiseError>> = None;
            let stats = nicti_prowl::perf::Protocol::default().run(|| {
                timed_result = Some(denoiser.denoise(&cand_srgb, width, height, tile_config))
            });
            let _ = timed_result; // only the timing matters here; scored again for real below
            println!(
                "denoise timing (ep={ep:?} -- docs/benchmarks.md's real ADR measurement is \
                 Windows-native; GPU/driver aren't in HardwareIdentity, record them by hand \
                 alongside this like the existing bench/ scripts do): \
                 p50={:.1}ms p95={:.1}ms max={:.1}ms",
                stats.p50_ms, stats.p95_ms, stats.max_ms,
            );
        }

        cand_srgb = denoiser.denoise(&cand_srgb, width, height, tile_config)?;
    }

    // The candidate was already resampled onto the reference's pixel grid above (before this
    // point), so the mask/gain-fit/metrics below operate on registered, not just cropped, data.
    let mask: Vec<bool> = align::clip_mask(&ref_srgb, &cand_srgb, 1.0 / 255.0, 1.0 - 1.0 / 255.0);
    let masked_frac = mask.iter().filter(|&&m| m).count() as f64 / mask.len() as f64;
    println!("unmasked (unclipped) fraction: {masked_frac:.4}");

    let gain = align::fit_gain(&ref_srgb, &cand_srgb, &mask);
    println!("fitted per-image gain: {gain:.6}");
    let gained: Vec<f32> = cand_srgb
        .iter()
        .map(|&v| (v as f64 * gain) as f32)
        .collect();

    let psnr = nicti_prowl::metrics::psnr(&ref_srgb, &gained);
    let ssim = nicti_prowl::metrics::ssim_rgb(&ref_srgb, &gained, width, height);
    println!("PSNR: {psnr:.3} dB");
    println!("SSIM: {ssim:.6}");

    Ok(())
}
