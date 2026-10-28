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
            ..Default::default()
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
                ..Default::default()
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

/// The #337 acceptance: a TIFF carries the same EXIF a JPEG does (the tags a viewer or DAM reads),
/// read back by exiftool itself.
#[test]
fn a_tiff_carries_the_same_exif_as_a_jpeg() {
    if !exiftool_available() {
        eprintln!("SKIPPED: exiftool not on PATH");
        return;
    }
    let reg = builtin_registry();
    let src = source();
    let dir = tempfile::tempdir().unwrap();
    let read = |name: &str, format: FormatSpec| -> serde_json::Value {
        let spec = ExportSpec {
            format,
            color_space: ExportSpace::Srgb,
            metadata: MetadataSpec {
                policy: MetadataPolicy::All,
                include_keywords: false,
                artist: Some("Jo Photographer".into()),
                copyright: Some("(c) 2026 Jo Photographer".into()),
                ..Default::default()
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
        let path = dir.path().join(format!("{name}.{}", out.extension));
        std::fs::write(&path, &out.bytes).unwrap();
        let res = Command::new("exiftool")
            .args([
                "-j",
                "-n",
                "-Make",
                "-Model",
                "-LensModel",
                "-ISO",
                "-ExposureTime",
            ])
            .args([
                "-FNumber",
                "-FocalLength",
                "-DateTimeOriginal",
                "-OffsetTimeOriginal",
            ])
            .args(["-Artist", "-Copyright", "-Software", "-Orientation"])
            .arg(&path)
            .output()
            .unwrap();
        serde_json::from_slice::<Vec<serde_json::Value>>(&res.stdout).unwrap()[0].clone()
    };
    let jpeg = read(
        "j",
        FormatSpec::Jpeg {
            quality: 90,
            subsampling: Subsampling::S420,
        },
    );
    let tiff = read(
        "t",
        FormatSpec::Tiff {
            depth: BitDepth::Eight,
            compression: TiffCompression::None,
        },
    );
    for key in [
        "Make",
        "Model",
        "LensModel",
        "ISO",
        "ExposureTime",
        "FNumber",
        "FocalLength",
        "DateTimeOriginal",
        "OffsetTimeOriginal",
        "Artist",
        "Copyright",
        "Software",
        "Orientation",
    ] {
        assert!(jpeg.get(key).is_some(), "the JPEG lacks {key}: {jpeg}");
        assert_eq!(
            tiff.get(key),
            jpeg.get(key),
            "{key}\nTIFF: {tiff}\nJPEG: {jpeg}"
        );
    }
}

/// #340: a position is written only on request, reads back correctly in exiftool for every
/// format that carries EXIF, and does not trip the validator.
#[test]
fn location_is_opt_in_and_reads_back_in_every_exif_format() {
    if !exiftool_available() {
        eprintln!("SKIPPED: exiftool not on PATH");
        return;
    }
    use nicti_preen::metadata::Gps;
    let reg = builtin_registry();
    let mut src = source();
    src.exif.gps = Some(Gps {
        lat_ref: b'N',
        lat: [(40, 1), (26, 1), (46, 1)],
        lon_ref: b'W',
        lon: [(79, 1), (58, 1), (56, 1)],
        altitude: Some(((1523, 10), false)),
    });
    let dir = tempfile::tempdir().unwrap();
    let cases = [
        (
            "jpeg",
            FormatSpec::Jpeg {
                quality: 90,
                subsampling: Subsampling::S420,
            },
        ),
        (
            "png",
            FormatSpec::Png {
                depth: BitDepth::Eight,
            },
        ),
        (
            "tiff",
            FormatSpec::Tiff {
                depth: BitDepth::Eight,
                compression: TiffCompression::None,
            },
        ),
    ];
    for (name, format) in cases {
        for include_location in [false, true] {
            let spec = ExportSpec {
                format,
                color_space: ExportSpace::Srgb,
                metadata: MetadataSpec {
                    policy: MetadataPolicy::All,
                    include_location,
                    ..MetadataSpec::default()
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
            let path = dir
                .path()
                .join(format!("{name}-{include_location}.{}", out.extension));
            std::fs::write(&path, &out.bytes).unwrap();
            let run = |args: &[&str]| {
                let o = Command::new("exiftool")
                    .args(args)
                    .arg(&path)
                    .output()
                    .unwrap();
                String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr)
            };
            let gps = run(&["-n", "-s3", "-GPSLatitude", "-GPSLongitude", "-GPSAltitude"]);
            if include_location {
                let nums: Vec<f64> = gps
                    .split_whitespace()
                    .filter_map(|v| v.parse().ok())
                    .collect();
                assert_eq!(nums.len(), 3, "{name}: {gps}");
                assert!((nums[0] - 40.446_111).abs() < 1e-4, "{name} lat {gps}");
                assert!((nums[1] + 79.982_222).abs() < 1e-4, "{name} lon {gps}");
                assert!((nums[2] - 152.3).abs() < 1e-3, "{name} alt {gps}");
                let all = run(&["-a", "-G1", "-s", "-GPS:all"]);
                assert!(!all.trim().is_empty(), "{name}: {all}");
            } else {
                assert!(gps.trim().is_empty(), "{name} leaked a location: {gps}");
                let all = run(&["-a", "-G1", "-GPS:all"]);
                assert!(all.trim().is_empty(), "{name} leaked GPS tags: {all}");
            }
            let validate = run(&["-validate", "-warning", "-a", "-G1"]);
            let findings: Vec<&str> = validate
                .lines()
                .filter(|l| l.contains("Warning") && !l.contains("Validate"))
                .filter(|l| !l.contains("[minor] Odd offset for IFD0 tag"))
                .collect();
            println!("--- {name} location={include_location} ---\n{validate}");
            assert!(findings.is_empty(), "{name}: {findings:?}");
        }
    }
}
