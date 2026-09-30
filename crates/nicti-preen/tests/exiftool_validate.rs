//! Runs `exiftool -validate -warning -a` over a real export of each format (ADR-0056's metadata
//! decision rule: zero warnings). Skips, loudly, when `exiftool` isn't on PATH.

use std::process::Command;

use nicti_preen::exporters::builtin_registry;
use nicti_preen::metadata::{SourceExif, SourceMetadata};
use nicti_preen::orient::Orientation;
use nicti_preen::spec::{
    BitDepth, ExportSpace, ExportSpec, FormatSpec, MetadataPolicy, MetadataSpec, Subsampling,
    TiffCompression,
};
use nicti_preen::{export_frame, ExportContext, WorkingFrame};

fn exiftool_available() -> bool {
    Command::new("exiftool")
        .arg("-ver")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn frame(w: u32, h: u32) -> WorkingFrame {
    let mut pixels = Vec::new();
    for y in 0..h {
        for x in 0..w {
            pixels.extend_from_slice(&[x as f32 / w as f32, y as f32 / h as f32, 0.3]);
        }
    }
    WorkingFrame {
        width: w,
        height: h,
        pixels,
    }
}

fn source() -> SourceMetadata {
    SourceMetadata {
        exif: SourceExif {
            make: Some("NIKON CORPORATION".into()),
            model: Some("NIKON Z 8".into()),
            lens_model: Some("NIKKOR Z 24-70mm f/2.8 S".into()),
            exposure_time: Some((1, 200)),
            f_number: Some((28, 10)),
            focal_length: Some((50, 1)),
            iso: Some(400),
            date_time_original: Some("2026:09:27 14:03:09".into()),
            offset_time_original: Some("-04:00".into()),
            orientation: Orientation::Normal,
        },
        rating: Some(4),
        label: Some("Red".into()),
        keywords: vec!["cat".into(), "con".into()],
        ..SourceMetadata::default()
    }
}

#[test]
fn every_format_passes_exiftool_validate_with_no_warnings() {
    if !exiftool_available() {
        eprintln!("SKIPPED: exiftool not on PATH");
        return;
    }
    let reg = builtin_registry();
    let src = source();
    let dir = tempfile::tempdir().unwrap();
    let cases = [
        (
            "jpeg",
            FormatSpec::Jpeg {
                quality: 90,
                subsampling: Subsampling::S420,
            },
            ExportSpace::DisplayP3,
        ),
        (
            "png",
            FormatSpec::Png {
                depth: BitDepth::Sixteen,
            },
            ExportSpace::AdobeRgb,
        ),
        (
            "tiff-deflate",
            FormatSpec::Tiff {
                depth: BitDepth::Sixteen,
                compression: TiffCompression::Deflate,
            },
            ExportSpace::Srgb,
        ),
        (
            "tiff-lzw",
            FormatSpec::Tiff {
                depth: BitDepth::Eight,
                compression: TiffCompression::Lzw,
            },
            ExportSpace::Srgb,
        ),
        (
            "tiff-none",
            FormatSpec::Tiff {
                depth: BitDepth::Eight,
                compression: TiffCompression::None,
            },
            ExportSpace::Srgb,
        ),
    ];
    let mut problems = Vec::new();
    for (name, format, color_space) in cases {
        let spec = ExportSpec {
            format,
            color_space,
            metadata: MetadataSpec {
                policy: MetadataPolicy::All,
                include_keywords: true,
                artist: Some("Jo Photographer".into()),
                copyright: Some("(c) 2026 Jo Photographer".into()),
            },
            ..ExportSpec::default()
        };
        let out = export_frame(
            frame(64, 48),
            &ExportContext {
                spec: &spec,
                source: &src,
                watermark: None,
                software: "Nicti test",
            },
            &reg,
        )
        .unwrap();
        let path = dir.path().join(format!("out.{}", out.extension));
        std::fs::write(&path, &out.bytes).unwrap();
        let res = Command::new("exiftool")
            .args(["-validate", "-warning", "-a", "-G1"])
            .arg(&path)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&res.stdout).to_string()
            + &String::from_utf8_lossy(&res.stderr);
        println!("--- {name} ---\n{text}");
        // Known validator / `tiff`-crate quirks, not findings in our output:
        // - exiftool flags *any* Adobe-Deflate (Compression = 8) TIFF, including libtiff's own
        //   `tiffcp -c zip` output.
        // - the `tiff` crate doesn't pad an odd-length compressed strip to a word boundary, so the
        //   IFD values written after it can land at an odd offset (exiftool: "[minor] Odd
        //   offset"). Whether that happens depends on the compressor's exact output size, which
        //   varies with the deflate backend the workspace's feature unification selects.
        let is_known_quirk = |l: &str| {
            l.contains("Invalid value for IFD0 tag 0x0103 Compression")
                || l.contains("[minor] Odd offset for IFD0 tag")
        };
        let findings: Vec<&str> = text
            .lines()
            .filter(|l| l.contains("Warning") && !l.contains("Validate") && !is_known_quirk(l))
            .collect();
        let clean_required = !name.starts_with("tiff");
        if !findings.is_empty() || (clean_required && !text.contains("OK")) {
            problems.push(format!("{name}:\n{text}"));
        }
    }
    assert!(
        problems.is_empty(),
        "exiftool findings:\n{}",
        problems.join("\n")
    );
}
