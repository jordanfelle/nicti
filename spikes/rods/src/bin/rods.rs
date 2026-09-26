use std::path::PathBuf;

use clap::{Parser, Subcommand};
use rods::{align, display, linear_input};

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
        } => compare(
            &ref_tiff,
            &ref_json,
            &candidate_tiff,
            &candidate_json,
            shift_tolerance,
        ),
    }
}

fn compare(
    ref_tiff: &std::path::Path,
    ref_json: &std::path::Path,
    candidate_tiff: &std::path::Path,
    candidate_json: &std::path::Path,
    shift_tolerance: f64,
) -> anyhow::Result<()> {
    let reference = linear_input::load(ref_tiff, ref_json)?;
    let candidate = linear_input::load(candidate_tiff, candidate_json)?;

    anyhow::ensure!(
        reference.image.dimensions() == candidate.image.dimensions(),
        "reference {:?} and candidate {:?} dimensions differ -- common-crop them first",
        reference.image.dimensions(),
        candidate.image.dimensions(),
    );
    let (width, height) = reference.image.dimensions();
    let pixels = width as usize * height as usize;

    // Fixed display treatment: camera RGB (already WB-applied by `dump-classic`, black/white
    // scaled by `linearize_sample`) -> XYZ(D50) -> linear sRGB -> sRGB OETF, per pixel, for both
    // images, using each image's own decoder-reported `cam_xyz`.
    let to_srgb_planes =
        |img: &image::ImageBuffer<image::Rgb<u16>, Vec<u16>>, cam_xyz: &[f32; 12]| -> Vec<f32> {
            let mut out = vec![0.0f32; pixels * 3];
            for (i, px) in img.pixels().enumerate() {
                let cam_rgb = [
                    linear_input::linearize_sample(px[0]),
                    linear_input::linearize_sample(px[1]),
                    linear_input::linearize_sample(px[2]),
                ];
                let srgb = display::to_display_srgb(cam_rgb, cam_xyz);
                out[i * 3] = srgb[0];
                out[i * 3 + 1] = srgb[1];
                out[i * 3 + 2] = srgb[2];
            }
            out
        };

    let ref_srgb = to_srgb_planes(&reference.image, &reference.meta.cam_xyz);
    let cand_srgb = to_srgb_planes(&candidate.image, &candidate.meta.cam_xyz);

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
    let ref_luma = luma(&ref_srgb);
    let cand_luma = luma(&cand_srgb);

    let margin = 16.min(width / 4).min(height / 4);
    let shift = align::estimate_shift(&ref_luma, &cand_luma, margin, 30);
    println!(
        "shift: dx={:.4} dy={:.4} (tolerance {shift_tolerance})",
        shift.dx, shift.dy
    );
    if !shift.within_tolerance(shift_tolerance) {
        println!("WARNING: shift exceeds tolerance -- scores below may reflect misregistration");
    }

    // No sub-pixel resampling of the full RGB planes at the estimated shift is implemented yet
    // (only the luma plane resampling machinery exists, via `Plane::sample_bilinear`) -- P2's
    // remaining work item. For now, score directly (real RawNIND tripod frames are expected to be
    // near-zero shift already) and rely on the printed shift/warning above to flag anything that
    // needs the resample step before its numbers are trusted.
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
