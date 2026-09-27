//! CLI for the `prey` spike (#56/ADR-0056). Subcommands measure each export-stack candidate on a
//! synthetic image at a chosen size (no real NEF/render exists yet, see the module docs' "what
//! wasn't reachable" notes), and `pipeline` runs the full resize -> watermark -> encode ->
//! metadata chain end to end.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use image::{Rgb, RgbImage};
use nicti_prowl::perf::{write_report, HardwareIdentity, Protocol, RunReport};
use prey::metadata::ExportMetadata;

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Measure CPU (fast_image_resize linear, image-crate sRGB) and GPU resize candidates.
    Resize {
        #[arg(long, default_value_t = 8280)]
        src_width: u32,
        #[arg(long, default_value_t = 5520)]
        src_height: u32,
        #[arg(long, default_value_t = 2048)]
        dst_long_edge: u32,
        #[arg(long, default_value = "bench-results")]
        out_dir: PathBuf,
    },
    /// Measure JPEG encoder candidates (jpeg-encoder, image crate, and mozjpeg if built with
    /// `--features native`) at a chosen size and quality.
    Encode {
        #[arg(long, default_value_t = 2048)]
        width: u32,
        #[arg(long, default_value_t = 1365)]
        height: u32,
        #[arg(long, default_value_t = 90)]
        quality: u8,
        #[arg(long, default_value = "bench-results")]
        out_dir: PathBuf,
    },
    /// Measure EXIF+XMP+ICC metadata write on a JPEG at a chosen size.
    Metadata {
        #[arg(long, default_value_t = 2048)]
        width: u32,
        #[arg(long, default_value_t = 1365)]
        height: u32,
        #[arg(long, default_value = "bench-results")]
        out_dir: PathBuf,
    },
    /// Measure CPU-sequential vs. CPU-parallel (rayon) watermark compositing.
    Watermark {
        #[arg(long, default_value_t = 2048)]
        width: u32,
        #[arg(long, default_value_t = 1365)]
        height: u32,
        #[arg(long, default_value = "bench-results")]
        out_dir: PathBuf,
    },
    /// Run the full resize -> encode -> metadata -> watermark chain end to end and write a JPEG
    /// to `out_file` for manual inspection (e.g. `exiftool -validate -warning -a`).
    Pipeline {
        #[arg(long, default_value_t = 8280)]
        src_width: u32,
        #[arg(long, default_value_t = 5520)]
        src_height: u32,
        #[arg(long, default_value_t = 2048)]
        dst_long_edge: u32,
        #[arg(long, default_value_t = 90)]
        quality: u8,
        #[arg(long, default_value = "prey-pipeline-output.jpg")]
        out_file: PathBuf,
        #[arg(long, default_value = "bench-results")]
        out_dir: PathBuf,
    },
}

/// A gradient+checkerboard hybrid: smooth tonal range plus sharp high-contrast edges, so a resize
/// candidate's gamma-vs-linear behavior actually shows up (see `resize.rs`'s test of the same
/// concern) rather than measuring against a flat or low-contrast frame.
fn synthetic_frame(width: u32, height: u32) -> RgbImage {
    RgbImage::from_fn(width, height, |x, y| {
        if (x / 64 + y / 64) % 7 == 0 {
            Rgb([250, 250, 250])
        } else if (x / 64 + y / 64) % 7 == 3 {
            Rgb([5, 5, 5])
        } else {
            Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
        }
    })
}

fn dst_dims(src_width: u32, src_height: u32, dst_long_edge: u32) -> (u32, u32) {
    if src_width >= src_height {
        let h = (src_height as u64 * dst_long_edge as u64 / src_width as u64) as u32;
        (dst_long_edge, h.max(1))
    } else {
        let w = (src_width as u64 * dst_long_edge as u64 / src_height as u64) as u32;
        (w.max(1), dst_long_edge)
    }
}

fn report(name: &str, protocol: Protocol, stats: nicti_prowl::perf::Stats, out_dir: &PathBuf) {
    println!(
        "{name}: p50={:.2}ms p95={:.2}ms max={:.2}ms",
        stats.p50_ms, stats.p95_ms, stats.max_ms
    );
    let run = RunReport {
        name: name.to_string(),
        hardware: HardwareIdentity::capture(),
        protocol,
        stats,
        unix_time: nicti_prowl::perf::now_unix(),
    };
    match write_report(out_dir, &run) {
        Ok(path) => println!("  -> {}", path.display()),
        Err(e) => eprintln!("  (failed to write report: {e})"),
    }
}

/// The full resize -> watermark -> encode -> metadata chain, shared by `Pipeline`'s timed loop
/// and its real-output-file write below so the two can never drift apart (CodeRabbit review on
/// PR #221 caught the two paths having drifted: the timed loop composited a watermark but threw
/// the result away before re-encoding, and the real output file never watermarked at all --
/// fixed by making this the single source of truth for the chain both callers exercise).
/// Watermarking happens on the resized RGB frame directly, before encoding -- not by decoding a
/// JPEG back out and re-encoding it, which wastes a decode/encode round trip for no reason.
fn build_pipeline_jpeg(
    src: &RgbImage,
    dst_width: u32,
    dst_height: u32,
    quality: u8,
) -> anyhow::Result<Vec<u8>> {
    let resized = prey::resize::resize_fast_linear(src, dst_width, dst_height)?;

    let mut rgba: image::RgbaImage = image::DynamicImage::ImageRgb8(resized).to_rgba8();
    let logo_svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="200" height="60">
        <rect width="200" height="60" rx="8" fill="#000000" opacity="0.5"/>
        <text x="10" y="40" font-size="28" fill="#ffffff">Nicti</text>
    </svg>"##;
    let logo = prey::watermark::rasterize_svg(logo_svg, 200, 60)?;
    prey::watermark::composite_parallel(&mut rgba, &logo, 20, (dst_height as i64) - 80);
    let watermarked = image::DynamicImage::ImageRgba8(rgba).to_rgb8();

    let icc = prey::icc::srgb_icc_profile()?;
    let jpeg = prey::encode::encode_jpeg_encoder(&watermarked, quality, Some(&icc))?;
    let jpeg = prey::metadata::write_exif_jpeg(&jpeg, &sample_metadata(dst_width, dst_height))?;
    let xmp = prey::metadata::wrap_xpacket("<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"/>");
    prey::metadata::embed_xmp_jpeg(&jpeg, &xmp)
}

fn sample_metadata(width: u32, height: u32) -> ExportMetadata {
    ExportMetadata {
        make: Some("NIKON CORPORATION".into()),
        model: Some("NIKON Z8".into()),
        lens_model: Some("NIKKOR Z 24-70mm f/2.8 S".into()),
        exposure_time: Some((1, 200)),
        f_number: Some((56, 10)),
        iso: Some(400),
        date_time_original: Some("2026:09:27 12:00:00".into()),
        offset_time_original: Some("-04:00".into()),
        artist: Some("Nicti export spike".into()),
        copyright: Some("(c) 2026".into()),
        software: "Nicti (spikes/prey)".into(),
        width,
        height,
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let protocol = Protocol::default();

    match cli.command {
        Command::Resize {
            src_width,
            src_height,
            dst_long_edge,
            out_dir,
        } => {
            let src = synthetic_frame(src_width, src_height);
            let (dw, dh) = dst_dims(src_width, src_height, dst_long_edge);
            println!("resize {src_width}x{src_height} -> {dw}x{dh}");

            let stats = protocol.run(|| {
                prey::resize::resize_fast_linear(&src, dw, dh).unwrap();
            });
            report("resize-fast-linear-cpu", protocol, stats, &out_dir);

            let stats = protocol.run(|| {
                prey::resize::resize_image_crate_srgb(&src, dw, dh);
            });
            report("resize-image-crate-srgb-cpu", protocol, stats, &out_dir);

            match prey::gpu_resize::GpuLanczosResizer::new() {
                Ok(resizer) => {
                    let stats = protocol.run(|| {
                        resizer.resize(&src, dw, dh).unwrap();
                    });
                    report("resize-gpu-lanczos", protocol, stats, &out_dir);
                }
                Err(e) => eprintln!("skipping GPU resize: {e}"),
            }
        }

        Command::Encode {
            width,
            height,
            quality,
            out_dir,
        } => {
            let img = synthetic_frame(width, height);
            println!("encode {width}x{height} @ q{quality}");

            let mut jpeg_encoder_bytes = 0;
            let stats = protocol.run(|| {
                jpeg_encoder_bytes = prey::encode::encode_jpeg_encoder(&img, quality, None)
                    .unwrap()
                    .len();
            });
            println!("  jpeg-encoder output size: {jpeg_encoder_bytes} bytes");
            report("encode-jpeg-encoder", protocol, stats, &out_dir);

            let mut image_crate_bytes = 0;
            let stats = protocol.run(|| {
                image_crate_bytes = prey::encode::encode_image_crate_jpeg(&img, quality)
                    .unwrap()
                    .len();
            });
            println!("  image crate output size: {image_crate_bytes} bytes");
            report("encode-image-crate-jpeg", protocol, stats, &out_dir);

            #[cfg(feature = "native")]
            {
                let mut mozjpeg_bytes = 0;
                let stats = protocol.run(|| {
                    mozjpeg_bytes =
                        prey::encode::native::encode_mozjpeg(&img, quality as f32, None)
                            .unwrap()
                            .len();
                });
                println!("  mozjpeg output size: {mozjpeg_bytes} bytes");
                report("encode-mozjpeg-native", protocol, stats, &out_dir);
            }
            #[cfg(not(feature = "native"))]
            println!("  (mozjpeg skipped -- rebuild with --features native)");
        }

        Command::Metadata {
            width,
            height,
            out_dir,
        } => {
            let img = synthetic_frame(width, height);
            let jpeg = prey::encode::encode_jpeg_encoder(&img, 95, None)?;
            let meta = sample_metadata(width, height);
            println!(
                "metadata write on a {width}x{height} JPEG ({} bytes)",
                jpeg.len()
            );

            let stats = protocol.run(|| {
                prey::metadata::write_exif_jpeg(&jpeg, &meta).unwrap();
            });
            report("metadata-exif-jpeg", protocol, stats, &out_dir);

            let stats = protocol.run(|| {
                prey::metadata::embed_xmp_jpeg(&jpeg, "<xmp/>").unwrap();
            });
            report("metadata-xmp-jpeg", protocol, stats, &out_dir);

            let icc = prey::icc::srgb_icc_profile()?;
            let stats = protocol.run(|| {
                prey::icc::embed_icc_jpeg(&jpeg, &icc).unwrap();
            });
            report("metadata-icc-jpeg", protocol, stats, &out_dir);
        }

        Command::Watermark {
            width,
            height,
            out_dir,
        } => {
            let base_template: image::RgbaImage =
                image::DynamicImage::ImageRgb8(synthetic_frame(width, height)).to_rgba8();
            let logo_svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="200" height="60">
                <rect width="200" height="60" rx="8" fill="#000000" opacity="0.5"/>
                <text x="10" y="40" font-size="28" fill="#ffffff">Nicti</text>
            </svg>"##;
            let logo = prey::watermark::rasterize_svg(logo_svg, 200, 60)?;
            println!("watermark composite on a {width}x{height} frame");

            let stats = protocol.run(|| {
                let mut base: image::RgbaImage = base_template.clone();
                prey::watermark::composite_sequential(&mut base, &logo, 20, (height as i64) - 80);
            });
            report("watermark-composite-sequential", protocol, stats, &out_dir);

            let stats = protocol.run(|| {
                let mut base: image::RgbaImage = base_template.clone();
                prey::watermark::composite_parallel(&mut base, &logo, 20, (height as i64) - 80);
            });
            report("watermark-composite-parallel", protocol, stats, &out_dir);
        }

        Command::Pipeline {
            src_width,
            src_height,
            dst_long_edge,
            quality,
            out_file,
            out_dir,
        } => {
            let src = synthetic_frame(src_width, src_height);
            let (dw, dh) = dst_dims(src_width, src_height, dst_long_edge);
            println!("pipeline {src_width}x{src_height} -> {dw}x{dh} @ q{quality}");

            let stats = protocol.run(|| {
                build_pipeline_jpeg(&src, dw, dh, quality).unwrap();
            });
            report("pipeline-end-to-end", protocol, stats, &out_dir);

            // Write one real output file for manual inspection (exiftool etc.), outside the
            // timed loop above -- the exact same chain the timing measures, not a separate copy
            // that can drift from it (see build_pipeline_jpeg's own doc comment).
            let jpeg = build_pipeline_jpeg(&src, dw, dh, quality)?;
            std::fs::write(&out_file, &jpeg)?;
            println!("wrote {} ({} bytes)", out_file.display(), jpeg.len());
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;
    use nicti_prowl::golden::ssim;

    #[test]
    fn build_pipeline_jpeg_actually_applies_the_watermark() {
        // Regression test for a real CodeRabbit finding on PR #221: the pipeline's real output
        // used to never watermark at all (the timed loop composited one but discarded the result
        // before re-encoding). Prove the shared chain's output differs from an unwatermarked
        // encode of the same resize -- not just that it doesn't crash.
        let src = synthetic_frame(256, 256);
        let watermarked = build_pipeline_jpeg(&src, 128, 128, 90).unwrap();
        let watermarked_img =
            image::load_from_memory_with_format(&watermarked, image::ImageFormat::Jpeg)
                .unwrap()
                .to_rgb8();

        let resized = prey::resize::resize_fast_linear(&src, 128, 128).unwrap();
        let plain = prey::encode::encode_jpeg_encoder(&resized, 90, None).unwrap();
        let plain_img = image::load_from_memory_with_format(&plain, image::ImageFormat::Jpeg)
            .unwrap()
            .to_rgb8();

        assert_eq!(watermarked_img.dimensions(), plain_img.dimensions());
        let score = ssim(&watermarked_img, &plain_img);
        assert!(
            score < 0.98,
            "watermarked output should visibly differ from an unwatermarked encode, got ssim={score}"
        );

        // The watermark logo is a semi-transparent black rectangle over the bottom-left corner
        // (see build_pipeline_jpeg's own logo_svg) -- that region must be darker than the same
        // pixel in the unwatermarked image.
        let (x, y) = (25u32, 100u32);
        let watermarked_luma = watermarked_img
            .get_pixel(x, y)
            .0
            .iter()
            .map(|&c| c as u32)
            .sum::<u32>();
        let plain_luma = plain_img
            .get_pixel(x, y)
            .0
            .iter()
            .map(|&c| c as u32)
            .sum::<u32>();
        assert!(
            watermarked_luma < plain_luma,
            "expected the watermark region to be darkened, got watermarked={watermarked_luma} plain={plain_luma}"
        );
    }

    #[test]
    fn synthetic_frame_is_not_a_flat_color() {
        // Sanity check the test fixture itself isn't degenerate (a flat image would make the
        // watermark-darkening assertion above trivially true regardless of whether watermarking
        // actually ran). Picks two points landing in different 64px bands -- synthetic_frame's
        // banding only varies across multiple 64px blocks, so a frame smaller than 128px in
        // either dimension is entirely one (possibly overridden-flat) band.
        let frame = synthetic_frame(256, 256);
        let corner = frame.get_pixel(0, 0); // band (0+0)%7=0 -> forced white
        let other = frame.get_pixel(200, 200); // band (3+3)%7=6 -> real gradient value
        assert_ne!(corner, other);
        let _: &Rgb<u8> = corner;
    }
}
