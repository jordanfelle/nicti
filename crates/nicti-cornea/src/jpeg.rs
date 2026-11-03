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
//! The ICC conversion is 8-bit and relative colorimetric, so colours outside sRGB clip (follow-up).
//! An 8-bit source cannot gain range: highlights stay clipped at 1.0 and shadows keep their 8-bit
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

/// True for the extensions [`JpegDecoder`] reads.
pub fn is_jpeg(path: &Path) -> bool {
    path.extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|e| e.eq_ignore_ascii_case("jpg") || e.eq_ignore_ascii_case("jpeg"))
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

        if icc.is_some() {
            // Source -> sRGB. Panic-guarded inside calico (moxcms parses untrusted profiles).
            static TO_SRGB: OnceLock<SourceTransforms> = OnceLock::new();
            TO_SRGB
                .get_or_init(|| SourceTransforms::new(&DisplayProfile::Space(OutputSpace::Srgb)))
                .convert_rgba8(icc.as_deref(), &mut rgba);
        }

        let mut pixels = Vec::with_capacity(width as usize * height as usize * 3);
        for px in rgba.as_chunks::<4>().0 {
            pixels.extend(px[..3].iter().map(|&v| srgb_to_linear_u16(v)));
        }

        let mut cam_xyz = [0.0f32; 12];
        cam_xyz[..9].copy_from_slice(&XYZ_D50_TO_SRGB);
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

    #[test]
    fn an_embedded_wide_gamut_profile_is_converted_to_srgb() {
        let d = tempfile::tempdir().unwrap();
        let icc = nicti_calico::icc::profile_bytes(OutputSpace::AdobeRgb).unwrap();
        let img = RgbImage::from_pixel(8, 8, Rgb([40, 200, 60]));
        let mut bytes = Vec::new();
        let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 100);
        enc.set_icc_profile(icc).unwrap();
        enc.write_image(img.as_raw(), 8, 8, image::ExtendedColorType::Rgb8)
            .unwrap();
        let tagged = d.path().join("adobe.jpg");
        std::fs::write(&tagged, bytes).unwrap();
        let plain = write_jpeg(d.path(), "plain.jpg", 8, 8, [40, 200, 60]);
        let (t, p) = (
            JpegDecoder.decode_linear(&tagged).unwrap(),
            JpegDecoder.decode_linear(&plain).unwrap(),
        );
        // Adobe RGB green is more saturated than sRGB green: in sRGB it needs a larger G and a
        // smaller R/B than the same bytes read as sRGB.
        assert!(
            t.pixels[1] > p.pixels[1],
            "{:?} vs {:?}",
            &t.pixels[..3],
            &p.pixels[..3]
        );
        assert!(
            t.pixels[0] < p.pixels[0],
            "{:?} vs {:?}",
            &t.pixels[..3],
            &p.pixels[..3]
        );
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
}
