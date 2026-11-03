//! Decode path for 8-bit JPEG assets (#576): a [`RawDecoder`] that yields the same
//! [`LinearFrame`] a RAW does, so loupe, develop and export need no JPEG special case.
//!
//! The JPEG's pixels are converted from the space its embedded ICC profile names (sRGB when it has
//! none or an unusable one) to sRGB, then the sRGB transfer curve is undone into 16-bit linear
//! light. The frame then declares sRGB primaries to the colour chain: `cam_xyz` is the
//! Bradford-adapted XYZ(D50) -> linear sRGB matrix, `cam_mul` is neutral (a JPEG is already white
//! balanced; the Temperature slider still works through `cam_xyz`), `black` is 0 and `maximum`
//! 65535. As with a RAW, the pixels are **not** rotated: `orientation` carries the EXIF value.
//!
//! A JPEG tagged with a wide-gamut profile (Adobe RGB, Display P3, ...) is instead converted to
//! 16-bit linear ProPhoto (the working space) and declares the working-space matrix, so colours
//! outside sRGB survive (#652). An 8-bit source cannot gain range: highlights stay clipped at 1.0 and shadows keep their 8-bit
//! steps, however far the edit pushes them.

use std::io::Cursor;
use std::path::Path;
use std::sync::OnceLock;

use image::metadata::Orientation;
use image::{ImageDecoder, ImageReader};
use nicti_calico::source_transform::SourceTransforms;
use nicti_calico::space::OutputSpace;
use nicti_calico::transform::DisplayProfile;
use nicti_claw::Module;

use crate::{DecodeError, LinearFrame, RawDecoder};

/// Largest JPEG file read: a bound against a hostile or runaway file.
const MAX_BYTES: u64 = 512 * 1024 * 1024;
/// Largest decoded image: the `image` JPEG decoder enforces no allocation limit of its own, and a
/// few-MB flat-colour file can advertise 65535 x 65535. Peak memory here is ~13 bytes/pixel (RGB8
/// decode, RGBA copy, u16 output), so 150 MP is ~2 GB.
const MAX_PIXELS: u64 = 150_000_000;

fn check_dimensions(width: u32, height: u32) -> Result<(), String> {
    let px = u64::from(width) * u64::from(height);
    if width == 0 || height == 0 || px > MAX_PIXELS {
        return Err(format!(
            "{width}x{height} is outside the supported 1..={MAX_PIXELS} pixel range"
        ));
    }
    Ok(())
}

/// Bradford-adapted XYZ(D50) -> linear sRGB, row-major (Lindbloom). `LinearFrame::cam_xyz` is
/// XYZ -> camera, so for an sRGB "camera" this is the matrix itself.
const XYZ_D50_TO_SRGB: [f32; 9] = [
    3.133_856,
    -1.616_866_7,
    -0.490_614_6, //
    -0.978_768_4,
    1.916_141_5,
    0.033_454, //
    0.071_945_3,
    -0.228_991_4,
    1.405_242_7,
];

/// The extensions [`JpegDecoder`] reads: the one list ingest, T0/T2 previews and decode all share
/// (#652; `.jpe` and `.jfif` are real JPEG extensions the three former copies missed).
pub const JPEG_EXTENSIONS: &[&str] = &["jpg", "jpeg", "jpe", "jfif"];

/// True for the extensions [`JpegDecoder`] reads (case-insensitive).
pub fn is_jpeg(path: &Path) -> bool {
    path.extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|e| JPEG_EXTENSIONS.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

/// True for a 4-component (CMYK/YCCK) JPEG carrying Adobe's APP14 marker, which is how Photoshop
/// and friends write them -- with *inverted* ink values. `image`'s decoder converts those as if
/// they were plain CMYK, so the colours come out wrong; refuse them rather than show that (#652).
fn is_adobe_cmyk(data: &[u8]) -> bool {
    let (mut adobe, mut four) = (false, false);
    let mut i = 2;
    while i + 4 <= data.len() && data[i] == 0xFF {
        let marker = data[i + 1];
        if marker == 0xFF {
            i += 1; // fill byte
            continue;
        }
        if marker == 0xDA || marker == 0xD9 {
            break; // start of scan / end of image: no more headers
        }
        let len = usize::from(u16::from_be_bytes([data[i + 2], data[i + 3]]));
        let body = data.get(i + 4..i + 2 + len).unwrap_or(&[]);
        match marker {
            0xEE if body.starts_with(b"Adobe") => adobe = true,
            0xC0..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => {
                // precision(1) height(2) width(2) components(1)
                four = body.get(5) == Some(&4);
            }
            _ => {}
        }
        i += 2 + len;
    }
    adobe && four
}

pub struct JpegDecoder;

impl Module for JpegDecoder {
    fn id(&self) -> &str {
        "nicti.decoder.jpeg"
    }

    fn schema_version(&self) -> u32 {
        1
    }

    fn migrate_params(
        &self,
        _from_version: u32,
        params: serde_json::Value,
    ) -> Option<serde_json::Value> {
        Some(params)
    }
}

fn srgb_to_linear_u16(v: u8) -> u16 {
    static LUT: OnceLock<[u16; 256]> = OnceLock::new();
    LUT.get_or_init(|| {
        let mut t = [0u16; 256];
        for (i, o) in t.iter_mut().enumerate() {
            let c = i as f64 / 255.0;
            let lin = if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            };
            *o = (lin * 65535.0).round() as u16;
        }
        t
    })[v as usize]
}

impl RawDecoder for JpegDecoder {
    fn decode_linear(&self, path: &Path) -> Result<LinearFrame, DecodeError> {
        let io = |source| DecodeError::Io {
            path: path.to_path_buf(),
            source,
        };
        let len = std::fs::metadata(path).map_err(io)?.len();
        if len > MAX_BYTES {
            return Err(DecodeError::Jpeg {
                path: path.to_path_buf(),
                message: format!("{len} bytes exceeds the {MAX_BYTES} byte limit"),
            });
        }
        let data = std::fs::read(path).map_err(io)?;
        let fail = |message: String| DecodeError::Jpeg {
            path: path.to_path_buf(),
            message,
        };

        if is_adobe_cmyk(&data) {
            return Err(fail(
                "CMYK/YCCK JPEGs are not supported yet (Adobe-inverted ink values)".to_string(),
            ));
        }
        let mut decoder = ImageReader::new(Cursor::new(&data))
            .with_guessed_format()
            .map_err(|e| fail(e.to_string()))?
            .into_decoder()
            .map_err(|e| fail(e.to_string()))?;
        // A profile that cannot be read is the same as none: sRGB.
        let icc = decoder.icc_profile().ok().flatten();
        let orientation = decoder.orientation().map(Orientation::to_exif).unwrap_or(0);
        let (dw, dh) = decoder.dimensions();
        check_dimensions(dw, dh).map_err(fail)?;
        let image = image::DynamicImage::from_decoder(decoder).map_err(|e| fail(e.to_string()))?;
        let (width, height) = (image.width(), image.height());
        let mut rgba = image.into_rgba8().into_raw();

        // A tagged wide-gamut profile (Adobe RGB, Display P3, ...) goes straight to 16-bit linear
        // ProPhoto, the working space, so nothing outside sRGB clips and nothing is rounded to
        // 8 bits twice (#652). Everything else (untagged, sRGB, unusable profiles) takes the sRGB path.
        let wide = icc
            .as_deref()
            .and_then(|icc| nicti_calico::source_transform::rgba8_to_linear_prophoto16(icc, &rgba));
        let (pixels, matrix) = if let Some(pixels) = wide {
            (pixels, nicti_calico::space::xyz_d50_to_working_f32())
        } else {
            if icc.is_some() {
                // Source -> sRGB. Panic-guarded inside calico (moxcms parses untrusted profiles).
                static TO_SRGB: OnceLock<SourceTransforms> = OnceLock::new();
                TO_SRGB
                    .get_or_init(|| {
                        SourceTransforms::new(&DisplayProfile::Space(OutputSpace::Srgb))
                    })
                    .convert_rgba8(icc.as_deref(), &mut rgba);
            }
            let mut pixels = Vec::with_capacity(width as usize * height as usize * 3);
            for px in rgba.as_chunks::<4>().0 {
                pixels.extend(px[..3].iter().map(|&v| srgb_to_linear_u16(v)));
            }
            (pixels, XYZ_D50_TO_SRGB)
        };
        drop(rgba);

        let mut cam_xyz = [0.0f32; 12];
        cam_xyz[..9].copy_from_slice(&matrix);
        Ok(LinearFrame {
            make: String::new(),
            model: "JPEG".to_string(),
            width,
            height,
            black: 0,
            maximum: 65535,
            cam_mul: [1.0; 4],
            pre_mul: [1.0; 4],
            cam_xyz,
            cblack: [0; 4],
            pixels,
            dng_opcode_list3: None,
            nikon_lens_info: None,
            // 1 (upright) and "no tag" are indistinguishable from the decoder; both mean 0.
            orientation: if (2..=8).contains(&orientation) {
                orientation
            } else {
                0
            },
        })
    }
}

/// The production decoder for catalog assets: JPEG files go to [`JpegDecoder`], everything else
/// to LibRaw. One value serves loupe, develop, export and pre-bake, so a JPEG asset needs no
/// caller-side branching.
#[cfg(feature = "libraw")]
pub struct SourceDecoder;

#[cfg(feature = "libraw")]
impl Module for SourceDecoder {
    fn id(&self) -> &str {
        "nicti.decoder.source"
    }

    fn schema_version(&self) -> u32 {
        1
    }

    fn migrate_params(
        &self,
        _from_version: u32,
        params: serde_json::Value,
    ) -> Option<serde_json::Value> {
        Some(params)
    }
}

#[cfg(feature = "libraw")]
impl RawDecoder for SourceDecoder {
    fn decode_linear(&self, path: &Path) -> Result<LinearFrame, DecodeError> {
        if is_jpeg(path) {
            JpegDecoder.decode_linear(path)
        } else {
            crate::LibRawDecoder.decode_linear(path)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageEncoder, Rgb, RgbImage};

    fn write_jpeg(dir: &Path, name: &str, w: u32, h: u32, px: [u8; 3]) -> std::path::PathBuf {
        let img = RgbImage::from_pixel(w, h, Rgb(px));
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 100)
            .write_image(img.as_raw(), w, h, image::ExtendedColorType::Rgb8)
            .unwrap();
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn extension_match_is_case_insensitive_and_jpeg_only() {
        assert!(is_jpeg(Path::new("a.JPG")));
        assert!(is_jpeg(Path::new("a.jpeg")));
        assert!(is_jpeg(Path::new("a.JPE")));
        assert!(is_jpeg(Path::new("a.jfif")));
        assert!(!is_jpeg(Path::new("a.nef")));
        assert!(!is_jpeg(Path::new("a")));
    }

    #[test]
    fn white_and_black_map_to_the_ends_of_the_linear_range() {
        let d = tempfile::tempdir().unwrap();
        let white = JpegDecoder
            .decode_linear(&write_jpeg(d.path(), "w.jpg", 16, 8, [255, 255, 255]))
            .unwrap();
        assert_eq!((white.width, white.height), (16, 8));
        assert_eq!(white.pixels.len(), 16 * 8 * 3);
        assert!(white.pixels.iter().all(|&v| v >= 65500));
        let black = JpegDecoder
            .decode_linear(&write_jpeg(d.path(), "b.jpg", 16, 8, [0, 0, 0]))
            .unwrap();
        assert!(black.pixels.iter().all(|&v| v <= 40));
    }

    #[test]
    fn mid_grey_is_linearised_not_passed_through() {
        // sRGB 128 is ~21.6% linear; a pass-through would give ~50%.
        let d = tempfile::tempdir().unwrap();
        let f = JpegDecoder
            .decode_linear(&write_jpeg(d.path(), "g.jpg", 8, 8, [128, 128, 128]))
            .unwrap();
        let v = f.pixels[0] as f32 / 65535.0;
        assert!((v - 0.2158).abs() < 0.01, "got {v}");
    }

    #[test]
    fn frame_declares_srgb_primaries_that_keep_neutral_neutral() {
        let d = tempfile::tempdir().unwrap();
        let f = JpegDecoder
            .decode_linear(&write_jpeg(d.path(), "n.jpg", 8, 8, [90, 90, 90]))
            .unwrap();
        assert_eq!((f.black, f.maximum), (0, 65535));
        // D50 white through XYZ->sRGB must give equal channels (the matrix is D50-adapted).
        let w = [0.9642f32, 1.0, 0.8249];
        let m = &f.cam_xyz;
        let rgb: Vec<f32> = (0..3)
            .map(|r| m[r * 3] * w[0] + m[r * 3 + 1] * w[1] + m[r * 3 + 2] * w[2])
            .collect();
        for c in &rgb {
            assert!((c - 1.0).abs() < 0.002, "{rgb:?}");
        }
        assert_eq!(f.orientation, 0);
    }

    #[test]
    fn oversized_dimensions_are_refused_before_any_allocation() {
        assert!(check_dimensions(65535, 65535).is_err());
        assert!(check_dimensions(0, 10).is_err());
        assert!(check_dimensions(12000, 12000).is_ok());
    }

    #[test]
    fn grayscale_decodes_to_equal_channels() {
        let d = tempfile::tempdir().unwrap();
        let img = image::GrayImage::from_pixel(8, 8, image::Luma([128]));
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 100)
            .write_image(img.as_raw(), 8, 8, image::ExtendedColorType::L8)
            .unwrap();
        let p = d.path().join("g.jpg");
        std::fs::write(&p, bytes).unwrap();
        let f = JpegDecoder.decode_linear(&p).unwrap();
        assert_eq!(f.pixels[0], f.pixels[1]);
        assert_eq!(f.pixels[1], f.pixels[2]);
    }

    #[test]
    fn a_garbage_icc_profile_degrades_to_srgb_instead_of_failing() {
        let d = tempfile::tempdir().unwrap();
        let img = RgbImage::from_pixel(8, 8, Rgb([40, 200, 60]));
        let mut bytes = Vec::new();
        let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 100);
        enc.set_icc_profile(vec![0xAB; 300]).unwrap();
        enc.write_image(img.as_raw(), 8, 8, image::ExtendedColorType::Rgb8)
            .unwrap();
        let bad = d.path().join("bad_icc.jpg");
        std::fs::write(&bad, bytes).unwrap();
        let plain = write_jpeg(d.path(), "plain.jpg", 8, 8, [40, 200, 60]);
        assert_eq!(
            JpegDecoder.decode_linear(&bad).unwrap().pixels[..3],
            JpegDecoder.decode_linear(&plain).unwrap().pixels[..3]
        );
    }

    #[test]
    fn corrupt_and_missing_files_are_errors_not_panics() {
        let d = tempfile::tempdir().unwrap();
        let bad = d.path().join("bad.jpg");
        std::fs::write(&bad, b"not a jpeg").unwrap();
        assert!(matches!(
            JpegDecoder.decode_linear(&bad),
            Err(DecodeError::Jpeg { .. })
        ));
        assert!(matches!(
            JpegDecoder.decode_linear(&d.path().join("none.jpg")),
            Err(DecodeError::Io { .. })
        ));
    }

    /// Linear sRGB of the first pixel, from whatever space the frame declares (`cam_xyz` is
    /// XYZ(D50) -> frame RGB, so invert it back to XYZ and on to sRGB).
    fn first_pixel_linear_srgb(f: &LinearFrame) -> [f64; 3] {
        let m: Vec<f64> = f.cam_xyz[..9].iter().map(|&v| f64::from(v)).collect();
        let det = m[0] * (m[4] * m[8] - m[5] * m[7]) - m[1] * (m[3] * m[8] - m[5] * m[6])
            + m[2] * (m[3] * m[7] - m[4] * m[6]);
        let inv = [
            (m[4] * m[8] - m[5] * m[7]) / det,
            (m[2] * m[7] - m[1] * m[8]) / det,
            (m[1] * m[5] - m[2] * m[4]) / det,
            (m[5] * m[6] - m[3] * m[8]) / det,
            (m[0] * m[8] - m[2] * m[6]) / det,
            (m[2] * m[3] - m[0] * m[5]) / det,
            (m[3] * m[7] - m[4] * m[6]) / det,
            (m[1] * m[6] - m[0] * m[7]) / det,
            (m[0] * m[4] - m[1] * m[3]) / det,
        ];
        let v: Vec<f64> = f.pixels[..3]
            .iter()
            .map(|&p| f64::from(p) / 65535.0)
            .collect();
        let xyz: Vec<f64> = (0..3)
            .map(|r| inv[r * 3] * v[0] + inv[r * 3 + 1] * v[1] + inv[r * 3 + 2] * v[2])
            .collect();
        let s: Vec<f64> = XYZ_D50_TO_SRGB.iter().map(|&x| f64::from(x)).collect();
        std::array::from_fn(|r| s[r * 3] * xyz[0] + s[r * 3 + 1] * xyz[1] + s[r * 3 + 2] * xyz[2])
    }

    fn tagged_jpeg(dir: &Path, name: &str, space: OutputSpace, px: [u8; 3]) -> std::path::PathBuf {
        let icc = nicti_calico::icc::profile_bytes(space).unwrap();
        let img = RgbImage::from_pixel(8, 8, Rgb(px));
        let mut bytes = Vec::new();
        let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 100);
        enc.set_icc_profile(icc).unwrap();
        enc.write_image(img.as_raw(), 8, 8, image::ExtendedColorType::Rgb8)
            .unwrap();
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn an_embedded_wide_gamut_profile_is_kept_wide_not_clipped_to_srgb() {
        let d = tempfile::tempdir().unwrap();
        // Saturated Display P3 green lies outside sRGB. JPEG chroma subsampling/quantisation
        // softens it a little, so only require it to stay clearly out of gamut.
        let f = JpegDecoder
            .decode_linear(&tagged_jpeg(
                d.path(),
                "p3.jpg",
                OutputSpace::DisplayP3,
                [0, 255, 0],
            ))
            .unwrap();
        let rgb = first_pixel_linear_srgb(&f);
        assert!(
            rgb[0] < -0.05,
            "red must go negative in sRGB terms: {rgb:?}"
        );
        assert!(rgb[1] > 1.0, "green must exceed sRGB white: {rgb:?}");
        // The frame declares the working-space matrix, not sRGB's.
        assert_ne!(f.cam_xyz[..9], XYZ_D50_TO_SRGB);
    }

    #[test]
    fn an_embedded_wide_gamut_profile_keeps_in_gamut_colours_where_srgb_would_put_them() {
        let d = tempfile::tempdir().unwrap();
        // Adobe RGB (40, 200, 60) in sRGB terms: a larger G and smaller R/B than the same bytes
        // read as sRGB (the old assertion, now through the declared matrix).
        let t = JpegDecoder
            .decode_linear(&tagged_jpeg(
                d.path(),
                "adobe.jpg",
                OutputSpace::AdobeRgb,
                [40, 200, 60],
            ))
            .unwrap();
        let p = JpegDecoder
            .decode_linear(&write_jpeg(d.path(), "plain.jpg", 8, 8, [40, 200, 60]))
            .unwrap();
        let (tr, pr) = (first_pixel_linear_srgb(&t), first_pixel_linear_srgb(&p));
        assert!(tr[1] > pr[1], "{tr:?} vs {pr:?}");
        assert!(tr[0] < pr[0], "{tr:?} vs {pr:?}");
    }

    #[test]
    fn an_srgb_tagged_jpeg_keeps_the_srgb_fast_path() {
        let d = tempfile::tempdir().unwrap();
        let tagged = JpegDecoder
            .decode_linear(&tagged_jpeg(
                d.path(),
                "s.jpg",
                OutputSpace::Srgb,
                [40, 200, 60],
            ))
            .unwrap();
        let plain = JpegDecoder
            .decode_linear(&write_jpeg(d.path(), "plain.jpg", 8, 8, [40, 200, 60]))
            .unwrap();
        assert_eq!(tagged.pixels[..3], plain.pixels[..3]);
        assert_eq!(tagged.cam_xyz, plain.cam_xyz);
    }

    #[cfg(feature = "libraw")]
    #[test]
    fn source_decoder_routes_jpegs_to_the_jpeg_path_and_the_rest_to_libraw() {
        let d = tempfile::tempdir().unwrap();
        let ok = SourceDecoder
            .decode_linear(&write_jpeg(d.path(), "a.JPG", 8, 8, [10, 20, 30]))
            .unwrap();
        assert_eq!(ok.model, "JPEG");
        // A non-JPEG extension reaches LibRaw, which reports its own I/O error for a missing file.
        assert!(matches!(
            SourceDecoder.decode_linear(&d.path().join("none.nef")),
            Err(DecodeError::Io { .. })
        ));
    }

    /// Inserts an APP1/EXIF segment holding only `Orientation = value` after SOI.
    fn with_exif_orientation(jpeg: &[u8], value: u16) -> Vec<u8> {
        let mut tiff = b"II*\0\x08\0\0\0".to_vec();
        tiff.extend_from_slice(&1u16.to_le_bytes()); // one IFD entry
        tiff.extend_from_slice(&0x0112u16.to_le_bytes());
        tiff.extend_from_slice(&3u16.to_le_bytes()); // SHORT
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&value.to_le_bytes());
        tiff.extend_from_slice(&[0, 0]);
        tiff.extend_from_slice(&0u32.to_le_bytes()); // no next IFD
        let mut seg = vec![0xFF, 0xE1];
        seg.extend_from_slice(&((tiff.len() + 8) as u16).to_be_bytes());
        seg.extend_from_slice(b"Exif\0\0");
        seg.extend_from_slice(&tiff);
        let mut out = jpeg[..2].to_vec();
        out.extend_from_slice(&seg);
        out.extend_from_slice(&jpeg[2..]);
        out
    }

    #[test]
    fn exif_orientation_1_is_upright_and_2_to_8_are_carried_on_a_real_tagged_jpeg() {
        let d = tempfile::tempdir().unwrap();
        let plain = std::fs::read(write_jpeg(d.path(), "p.jpg", 8, 8, [90, 60, 30])).unwrap();
        for (tag, want) in [
            (1u16, 0u8),
            (2, 2),
            (3, 3),
            (4, 4),
            (5, 5),
            (6, 6),
            (7, 7),
            (8, 8),
        ] {
            let p = d.path().join(format!("o{tag}.jpg"));
            std::fs::write(&p, with_exif_orientation(&plain, tag)).unwrap();
            let f = JpegDecoder.decode_linear(&p).unwrap();
            assert_eq!(f.orientation, want, "EXIF {tag}");
            // Pixels are never rotated, whatever the tag says.
            assert_eq!((f.width, f.height), (8, 8));
        }
        // An out-of-range tag is "no rotation", not a bogus value downstream.
        let p = d.path().join("o9.jpg");
        std::fs::write(&p, with_exif_orientation(&plain, 9)).unwrap();
        assert_eq!(JpegDecoder.decode_linear(&p).unwrap().orientation, 0);
    }

    fn cmyk_jpeg(adobe: bool, ycck: bool) -> Vec<u8> {
        let ink: Vec<u8> = (0..64).flat_map(|_| [255u8, 255, 255, 255]).collect();
        let mut bytes = Vec::new();
        let ct = if ycck {
            jpeg_encoder::ColorType::Ycck
        } else {
            jpeg_encoder::ColorType::Cmyk
        };
        jpeg_encoder::Encoder::new(&mut bytes, 100)
            .encode(&ink, 8, 8, ct)
            .unwrap();
        if !adobe {
            return bytes;
        }
        let mut seg = vec![0xFF, 0xEE, 0, 14];
        seg.extend_from_slice(b"Adobe");
        seg.extend_from_slice(&[0, 100, 0, 0, 0, 0, if ycck { 2 } else { 0 }]);
        let mut out = bytes[..2].to_vec();
        out.extend_from_slice(&seg);
        out.extend_from_slice(&bytes[2..]);
        out
    }

    #[test]
    fn adobe_cmyk_and_ycck_are_refused_not_shown_with_inverted_colours() {
        let d = tempfile::tempdir().unwrap();
        for ycck in [false, true] {
            let p = d.path().join(format!("c{ycck}.jpg"));
            std::fs::write(&p, cmyk_jpeg(true, ycck)).unwrap();
            let err = JpegDecoder.decode_linear(&p).unwrap_err().to_string();
            assert!(err.contains("CMYK"), "{err}");
        }
        // A 3-component JPEG that merely has an Adobe marker still decodes.
        let plain = std::fs::read(write_jpeg(d.path(), "p.jpg", 8, 8, [1, 2, 3])).unwrap();
        let mut seg = vec![0xFF, 0xEE, 0, 14];
        seg.extend_from_slice(b"Adobe");
        seg.extend_from_slice(&[0, 100, 0, 0, 0, 0, 1]);
        let mut with = plain[..2].to_vec();
        with.extend_from_slice(&seg);
        with.extend_from_slice(&plain[2..]);
        let p = d.path().join("a.jpg");
        std::fs::write(&p, with).unwrap();
        assert!(JpegDecoder.decode_linear(&p).is_ok());
    }

    #[test]
    fn a_gray_icc_profile_on_a_gray_jpeg_decodes_to_equal_channels() {
        let d = tempfile::tempdir().unwrap();
        let icc = moxcms::ColorProfile::new_gray_with_gamma(2.2)
            .encode()
            .unwrap();
        let img = image::GrayImage::from_pixel(8, 8, image::Luma([128]));
        let mut bytes = Vec::new();
        let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 100);
        enc.set_icc_profile(icc).unwrap();
        enc.write_image(img.as_raw(), 8, 8, image::ExtendedColorType::L8)
            .unwrap();
        let p = d.path().join("gray_icc.jpg");
        std::fs::write(&p, bytes).unwrap();
        let f = JpegDecoder.decode_linear(&p).unwrap();
        assert_eq!(f.pixels[0], f.pixels[1]);
        assert_eq!(f.pixels[1], f.pixels[2]);
        // Treated as sRGB (a gray profile is not an RGB source), so the usual matrix applies.
        assert_eq!(f.cam_xyz[..9], XYZ_D50_TO_SRGB);
    }

    #[test]
    fn the_wide_gamut_frame_declares_a_matrix_that_keeps_d50_white_neutral() {
        let d = tempfile::tempdir().unwrap();
        let f = JpegDecoder
            .decode_linear(&tagged_jpeg(
                d.path(),
                "w.jpg",
                OutputSpace::AdobeRgb,
                [128, 128, 128],
            ))
            .unwrap();
        // Working space is ProPhoto: D50 white -> (1, 1, 1), and a grey pixel has equal channels.
        let (w, m) = ([0.9642f32, 1.0, 0.8249], &f.cam_xyz);
        for r in 0..3 {
            let v = m[r * 3] * w[0] + m[r * 3 + 1] * w[1] + m[r * 3 + 2] * w[2];
            assert!((v - 1.0).abs() < 0.002, "row {r}: {v}");
        }
        let px = &f.pixels[..3];
        assert!((i32::from(px[0]) - i32::from(px[1])).abs() < 60, "{px:?}");
        assert!((i32::from(px[1]) - i32::from(px[2])).abs() < 60, "{px:?}");
    }
}
