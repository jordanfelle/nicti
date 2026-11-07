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

/// What the JPEG headers say about a 4-component (CMYK or YCCK) file (#681).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FourComponent {
    /// An Adobe APP14 marker is present: Photoshop and friends then store the ink values
    /// *inverted* (0 = full ink), and `transform` says whether the channels are YCCK.
    adobe: bool,
    /// The APP14 colour transform: 0 = CMYK, 2 = YCCK. Without a marker the file is plain CMYK.
    transform: u8,
    /// Every component has the same sampling factors (4:4:4:4).
    uniform: bool,
}

impl FourComponent {
    fn ycck(self) -> bool {
        self.adobe && self.transform == 2
    }
}

/// Reads the headers up to the start of scan: `Some` for a 4-component JPEG.
fn four_component(data: &[u8]) -> Option<FourComponent> {
    let (mut adobe, mut transform, mut four, mut uniform) = (false, 0u8, false, true);
    let mut i = 2;
    while i + 4 <= data.len() {
        if data[i] != 0xFF {
            i += 1; // padding between segments
            continue;
        }
        let marker = data[i + 1];
        if marker == 0xFF {
            i += 1; // fill byte
            continue;
        }
        if marker == 0x00 || (0xD0..=0xD8).contains(&marker) || marker == 0x01 {
            i += 2; // stuffed zero, RSTn, SOI, TEM: no length field
            continue;
        }
        if marker == 0xDA || marker == 0xD9 {
            break; // start of scan / end of image: no more headers
        }
        let len = usize::from(u16::from_be_bytes([data[i + 2], data[i + 3]]));
        let body = data.get(i + 4..i + 2 + len).unwrap_or(&[]);
        match marker {
            0xEE if body.starts_with(b"Adobe") => {
                adobe = true;
                // "Adobe" version(2) flags0(2) flags1(2) transform(1)
                transform = body.get(11).copied().unwrap_or(0);
            }
            0xC0..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => {
                // precision(1) height(2) width(2) components(1)
                four = body.get(5) == Some(&4);
                // Then `id, sampling (h<<4|v), quant table` per component.
                let factors: Vec<u8> = (0..4)
                    .filter_map(|i| body.get(7 + 3 * i).copied())
                    .collect();
                uniform = factors.len() == 4 && factors.iter().all(|f| *f == factors[0]);
            }
            _ => {}
        }
        i += 2 + len;
    }
    four.then_some(FourComponent {
        adobe,
        transform,
        uniform,
    })
}

/// The raw samples of a CMYK/YCCK JPEG, as ink coverage (#681).
struct CmykSamples {
    width: u32,
    height: u32,
    /// `C, M, Y, K` interleaved, 255 = full ink (the ICC convention): Adobe's inverted storage has
    /// already been undone, and YCCK has been turned back into CMYK.
    ink: Vec<u8>,
    /// The embedded profile, if any.
    icc: Option<Vec<u8>>,
}

/// Decodes the raw 4-component samples with `zune-jpeg` (the decoder `image` uses, but asked for
/// the unconverted channels): the `image` crate would run its own CMYK -> RGB conversion, which
/// treats Adobe's inverted values as plain ink and bypasses the embedded CMYK profile.
fn decode_cmyk_samples(data: &[u8], kind: FourComponent) -> Result<CmykSamples, String> {
    use zune_jpeg::zune_core::bytestream::ZCursor;
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;

    // zune's raw plain-CMYK output is wrong when the planes are sampled differently (checked
    // against libjpeg-turbo's `djpeg`; YCCK is fine), and wrong colours would be silent: refuse.
    if !kind.ycck() && !kind.uniform {
        return Err(
            "CMYK JPEGs with mixed component subsampling are not supported yet".to_string(),
        );
    }
    let raw_space = if kind.ycck() {
        ColorSpace::YCCK
    } else {
        ColorSpace::CMYK
    };
    // `image` lifts zune's default 16384 px cap; `check_dimensions` below is the real bound.
    let options = DecoderOptions::default()
        .set_max_width(usize::MAX)
        .set_max_height(usize::MAX)
        .jpeg_set_out_colorspace(raw_space);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(data), options);
    decoder.decode_headers().map_err(|e| e.to_string())?;
    let info = decoder
        .info()
        .ok_or_else(|| "the JPEG header could not be read".to_string())?;
    let (width, height) = (u32::from(info.width), u32::from(info.height));
    check_dimensions(width, height)?;
    let size = decoder
        .output_buffer_size()
        .ok_or_else(|| "the JPEG output size could not be determined".to_string())?;
    if size != width as usize * height as usize * 4 {
        return Err(format!("unexpected CMYK buffer size {size}"));
    }
    let mut samples = vec![0u8; size];
    decoder
        .decode_into(&mut samples)
        .map_err(|e| e.to_string())?;
    let icc = decoder.icc_profile();

    if kind.ycck() {
        // libjpeg's YCCK -> CMYK: the first three channels are YCbCr of the *inverted* CMY, so
        // C = 255 - R(Y, Cb, Cr) and so on; K passes through unchanged.
        for px in samples.as_chunks_mut::<4>().0 {
            let (y, cb, cr) = (
                f32::from(px[0]),
                f32::from(px[1]) - 128.0,
                f32::from(px[2]) - 128.0,
            );
            let r = y + 1.402 * cr;
            let g = y - 0.344_136 * cb - 0.714_136 * cr;
            let b = y + 1.772 * cb;
            px[0] = 255 - r.round().clamp(0.0, 255.0) as u8;
            px[1] = 255 - g.round().clamp(0.0, 255.0) as u8;
            px[2] = 255 - b.round().clamp(0.0, 255.0) as u8;
        }
    }
    {
        // Every 4-component JPEG is stored inverted (255 = no ink): libjpeg, PIL and zune/`image`
        // all read it that way whether or not the Adobe marker survived, so a file that lost its
        // APP14 must not come out as a photographic negative.
        for v in &mut samples {
            *v = 255 - *v;
        }
    }
    Ok(CmykSamples {
        width,
        height,
        ink: samples,
        icc,
    })
}

/// Profile-less CMYK -> sRGB-encoded RGB8: the plain subtractive model every viewer falls back to
/// (`R = (1 - C)(1 - K)`). Only used when the file carries no usable CMYK profile.
fn naive_cmyk_to_rgb8(ink: &[u8]) -> Vec<u8> {
    let mut rgb = Vec::with_capacity(ink.len() / 4 * 3);
    for px in ink.as_chunks::<4>().0 {
        let k = 255 - u32::from(px[3]);
        for &c in &px[..3] {
            rgb.push((((255 - u32::from(c)) * k + 127) / 255) as u8);
        }
    }
    rgb
}

/// `(width, height, interleaved sRGB-encoded RGB8)`.
pub type SrgbImage8 = (u32, u32, Vec<u8>);

/// A 4-component JPEG converted to sRGB-encoded RGB8 for the preview tiers (#681): through the
/// embedded CMYK profile when there is a usable one, else the plain subtractive model. `None` for
/// any other JPEG, so a caller can hand those to its normal decoder; `Some(Err)` when the file
/// is 4-component but cannot be decoded.
pub fn four_component_to_srgb8(data: &[u8]) -> Option<Result<SrgbImage8, String>> {
    let kind = four_component(data)?;
    Some(decode_cmyk_samples(data, kind).map(|c| {
        let rgb = c
            .icc
            .as_deref()
            .and_then(|icc| cmyk_profile_to_srgb8(icc, &c.ink))
            .unwrap_or_else(|| naive_cmyk_to_rgb8(&c.ink));
        (c.width, c.height, rgb)
    }))
}

/// CMYK ink through its profile to sRGB-encoded RGB8; `None` when the profile is unusable.
fn cmyk_profile_to_srgb8(icc: &[u8], ink: &[u8]) -> Option<Vec<u8>> {
    nicti_calico::source_transform::cmyk8_to_srgb8(icc, ink)
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

/// The `LinearFrame` every JPEG decode path ends in: sRGB-like "camera" with a neutral white
/// balance, `matrix` taking XYZ(D50) to its linear RGB.
fn linear_frame(
    width: u32,
    height: u32,
    pixels: Vec<u16>,
    matrix: [f32; 9],
    orientation: u8,
) -> LinearFrame {
    let mut cam_xyz = [0.0f32; 12];
    cam_xyz[..9].copy_from_slice(&matrix);
    LinearFrame {
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
        lens_exif: Default::default(),
        exposure: Default::default(),
        // 1 (upright) and "no tag" are indistinguishable from the decoder; both mean 0.
        orientation: if (2..=8).contains(&orientation) {
            orientation
        } else {
            0
        },
    }
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
        // CMYK/YCCK: raw samples through the embedded CMYK profile (#681), never `image`'s
        // conversion, which ignores the embedded CMYK profile (#681).
        if let Some(kind) = four_component(&data) {
            // `image`'s decoder holds a second copy of the file; release it before the big buffers.
            drop(decoder);
            let cmyk = decode_cmyk_samples(&data, kind).map_err(fail)?;
            let profile = icc.as_deref().or(cmyk.icc.as_deref());
            let (pixels, matrix) = match profile.and_then(|p| {
                nicti_calico::source_transform::cmyk8_to_linear_prophoto16(p, &cmyk.ink)
            }) {
                Some(pixels) => (pixels, nicti_calico::space::xyz_d50_to_working_f32()),
                None => {
                    let rgb = naive_cmyk_to_rgb8(&cmyk.ink);
                    (
                        rgb.iter().map(|&v| srgb_to_linear_u16(v)).collect(),
                        XYZ_D50_TO_SRGB,
                    )
                }
            };
            return Ok(linear_frame(
                cmyk.width,
                cmyk.height,
                pixels,
                matrix,
                orientation,
            ));
        }
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

        Ok(linear_frame(width, height, pixels, matrix, orientation))
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

    /// An 8x8 CMYK/YCCK JPEG whose every pixel means the ink coverage `ink` (255 = full ink).
    /// `jpeg-encoder` writes the Adobe convention itself (an APP14 marker and *inverted* samples),
    /// so `adobe: true` is that file as-is; `adobe: false` strips the marker, leaving the same
    /// stored samples with no marker, which a reader must take as direct coverage (so the pixel
    /// then means `255 - ink`). `icc` is embedded as APP2 when given.
    fn cmyk_jpeg_with(adobe: bool, ycck: bool, ink: [u8; 4], icc: Option<&[u8]>) -> Vec<u8> {
        cmyk_jpeg_sized(adobe, ycck, ink, icc, (8, 8), 100)
    }

    fn cmyk_jpeg_sized(
        adobe: bool,
        ycck: bool,
        ink: [u8; 4],
        icc: Option<&[u8]>,
        (w, h): (u16, u16),
        quality: u8,
    ) -> Vec<u8> {
        // `ColorType::Cmyk` writes the Adobe convention itself (marker + inverted samples), but
        // `ColorType::Ycck` takes the four planes *already* in YCCK and writes them untouched, so
        // build them as libjpeg does: stored CMY = 255 - ink, then YCbCr of (255 - stored) = ink,
        // and K stored inverted like the others.
        let px: Vec<u8> = if ycck {
            let [c, m, y, k] = ink.map(f32::from);
            let yy = 0.299 * c + 0.587 * m + 0.114 * y;
            let cb = -0.168_736 * c - 0.331_264 * m + 0.5 * y + 128.0;
            let cr = 0.5 * c - 0.418_688 * m - 0.081_312 * y + 128.0;
            let q = |v: f32| v.round().clamp(0.0, 255.0) as u8;
            let plane = [q(yy), q(cb), q(cr), 255 - q(k)];
            (0..usize::from(w) * usize::from(h))
                .flat_map(|_| plane)
                .collect()
        } else {
            (0..usize::from(w) * usize::from(h))
                .flat_map(|_| ink)
                .collect()
        };
        let mut bytes = Vec::new();
        let ct = if ycck {
            jpeg_encoder::ColorType::Ycck
        } else {
            jpeg_encoder::ColorType::Cmyk
        };
        let mut enc = jpeg_encoder::Encoder::new(&mut bytes, quality);
        if let Some(icc) = icc {
            enc.add_icc_profile(icc).unwrap();
        }
        enc.encode(&px, w, h, ct).unwrap();
        assert!(
            four_component(&bytes).is_some_and(|k| k.adobe),
            "the encoder is expected to write the Adobe marker"
        );
        if adobe {
            return bytes;
        }
        // Remove the APP14 segment.
        let mut i = 2;
        while i + 4 <= bytes.len() && bytes[i] == 0xFF {
            let len = usize::from(u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]));
            if bytes[i + 1] == 0xEE {
                bytes.drain(i..i + 2 + len);
                break;
            }
            i += 2 + len;
        }
        assert_eq!(four_component(&bytes).map(|k| k.adobe), Some(false));
        bytes
    }

    fn cmyk_jpeg(adobe: bool, ycck: bool) -> Vec<u8> {
        cmyk_jpeg_with(adobe, ycck, [255; 4], None)
    }

    /// The first pixel of a decoded frame as sRGB-encoded 8-bit, whatever working space the frame
    /// is in: the frame's own `cam_xyz` (XYZ -> its RGB) is inverted back to XYZ, then sent
    /// through sRGB's matrix and curve.
    fn first_pixel_srgb8(f: &LinearFrame) -> [u8; 3] {
        let m: [[f64; 3]; 3] =
            std::array::from_fn(|r| std::array::from_fn(|c| f64::from(f.cam_xyz[r * 3 + c])));
        let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
            - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
            + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
        let inv = [
            [
                (m[1][1] * m[2][2] - m[1][2] * m[2][1]) / det,
                (m[0][2] * m[2][1] - m[0][1] * m[2][2]) / det,
                (m[0][1] * m[1][2] - m[0][2] * m[1][1]) / det,
            ],
            [
                (m[1][2] * m[2][0] - m[1][0] * m[2][2]) / det,
                (m[0][0] * m[2][2] - m[0][2] * m[2][0]) / det,
                (m[0][2] * m[1][0] - m[0][0] * m[1][2]) / det,
            ],
            [
                (m[1][0] * m[2][1] - m[1][1] * m[2][0]) / det,
                (m[0][1] * m[2][0] - m[0][0] * m[2][1]) / det,
                (m[0][0] * m[1][1] - m[0][1] * m[1][0]) / det,
            ],
        ];
        let lin = [0, 1, 2].map(|i| f64::from(f.pixels[i]) / 65535.0);
        let xyz = [0, 1, 2].map(|r| (0..3).map(|c| inv[r][c] * lin[c]).sum::<f64>());
        let enc = |v: f64| {
            let v = v.clamp(0.0, 1.0);
            let e = if v <= 0.003_130_8 {
                v * 12.92
            } else {
                1.055 * v.powf(1.0 / 2.4) - 0.055
            };
            (e * 255.0).round() as u8
        };
        [0, 1, 2].map(|r| {
            enc((0..3)
                .map(|c| f64::from(XYZ_D50_TO_SRGB[r * 3 + c]) * xyz[c])
                .sum::<f64>())
        })
    }

    fn near(got: [u8; 3], want: [u8; 3], tol: i32) {
        for c in 0..3 {
            assert!(
                (i32::from(got[c]) - i32::from(want[c])).abs() <= tol,
                "got {got:?}, want {want:?} (+-{tol})"
            );
        }
    }

    fn decode_bytes(bytes: &[u8]) -> LinearFrame {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("c.jpg");
        std::fs::write(&p, bytes).unwrap();
        JpegDecoder.decode_linear(&p).unwrap()
    }

    #[test]
    fn adobe_cmyk_and_ycck_decode_with_their_inverted_storage_undone() {
        // The ink coverage written is what must come out: none = paper white, a single full plate =
        // its subtractive colour, full K = black.
        for ycck in [false, true] {
            for (ink, want) in [
                ([0, 0, 0, 0], [255, 255, 255]),
                ([255, 0, 0, 0], [0, 255, 255]),
                ([0, 255, 0, 0], [255, 0, 255]),
                ([0, 0, 255, 0], [255, 255, 0]),
                ([0, 0, 0, 255], [0, 0, 0]),
                ([255, 255, 255, 255], [0, 0, 0]),
            ] {
                let f = decode_bytes(&cmyk_jpeg_with(true, ycck, ink, None));
                near(first_pixel_srgb8(&f), want, 6);
                assert_eq!(f.cam_xyz[..9], XYZ_D50_TO_SRGB);
            }
        }
    }

    #[test]
    fn a_four_component_file_without_the_adobe_marker_is_read_the_same_way() {
        // libjpeg, PIL and zune/`image` all treat 4-component data as inverted whether or not the
        // APP14 marker survived; a file that lost it must not become a photographic negative.
        for ink in [[0, 0, 0, 0], [255, 0, 0, 0], [0, 0, 0, 255]] {
            let with = decode_bytes(&cmyk_jpeg_with(true, false, ink, None));
            let without = decode_bytes(&cmyk_jpeg_with(false, false, ink, None));
            assert_eq!(
                first_pixel_srgb8(&with),
                first_pixel_srgb8(&without),
                "{ink:?}"
            );
        }
        near(
            first_pixel_srgb8(&decode_bytes(&cmyk_jpeg_with(
                false,
                false,
                [255, 0, 0, 0],
                None,
            ))),
            [0, 255, 255],
            6,
        );
    }

    #[test]
    fn large_and_subsampled_ycck_files_decode_across_mcu_boundaries() {
        // 37x19 is not a multiple of any MCU size; quality 75 makes jpeg-encoder subsample.
        let bytes = cmyk_jpeg_sized(true, true, [255, 0, 0, 0], None, (37, 19), 75);
        let f = decode_bytes(&bytes);
        assert_eq!((f.width, f.height), (37, 19));
        let last = f.pixels.len() / 3 - 1;
        for px in [0, 18 * 37 + 20, last] {
            let g = LinearFrame {
                pixels: f.pixels[px * 3..px * 3 + 3].to_vec(),
                ..f.clone()
            };
            near(first_pixel_srgb8(&g), [0, 255, 255], 12);
        }
    }

    #[test]
    fn plain_cmyk_with_mixed_component_sampling_is_refused_not_decoded_wrongly() {
        // jpeg-encoder samples CMY 1x1 and K 2x2 below quality 90; zune's raw output for that is
        // wrong, so the decoder must say so instead of returning plausible garbage.
        let bytes = cmyk_jpeg_sized(true, false, [255, 0, 0, 0], None, (37, 19), 75);
        assert_eq!(four_component(&bytes).map(|k| k.uniform), Some(false));
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("mixed.jpg");
        std::fs::write(&p, bytes).unwrap();
        let err = JpegDecoder.decode_linear(&p).unwrap_err().to_string();
        assert!(err.contains("subsampling"), "{err}");
    }

    #[test]
    fn a_cmyk_file_wider_than_zunes_default_limit_decodes() {
        // zune caps width/height at 16384 by default; `image` lifts that and so must we.
        let bytes = cmyk_jpeg_sized(true, false, [0, 0, 0, 0], None, (16_500, 4), 100);
        let f = decode_bytes(&bytes);
        assert_eq!((f.width, f.height), (16_500, 4));
    }

    #[test]
    fn four_component_previews_use_the_same_inversion_as_the_decoder() {
        let bytes = cmyk_jpeg_with(true, false, [255, 0, 0, 0], None);
        let (w, h, rgb) = four_component_to_srgb8(&bytes).unwrap().unwrap();
        assert_eq!((w, h, rgb.len()), (8, 8, 8 * 8 * 3));
        near([rgb[0], rgb[1], rgb[2]], [0, 255, 255], 6);
        // Anything that is not 4-component is left to the normal decoder.
        let d = tempfile::tempdir().unwrap();
        let rgb_jpeg = std::fs::read(write_jpeg(d.path(), "r.jpg", 8, 8, [1, 2, 3])).unwrap();
        assert!(four_component_to_srgb8(&rgb_jpeg).is_none());
    }

    /// A minimal but real CMYK output profile (ICC v2, `prtr`, `A2B0` as a 16-bit lut with a 2^4
    /// CLUT) built here because no CMYK profile ships in the sandbox. Each of the 16 ink corners
    /// maps to the Lab of the plain subtractive colour `R = (1-C)(1-K)`, so a corner ink value must
    /// land on that colour whatever channel order or inversion the decoder applies.
    fn synthetic_cmyk_icc() -> Vec<u8> {
        fn lab_d50(rgb: [f64; 3]) -> [f64; 3] {
            let m = [
                [0.436_074_7, 0.385_064_9, 0.143_080_4],
                [0.222_504_5, 0.716_878_6, 0.060_616_9],
                [0.013_932_2, 0.097_104_5, 0.714_173_3],
            ];
            let xyz = [0, 1, 2].map(|r| (0..3).map(|c| m[r][c] * rgb[c]).sum::<f64>());
            let white = [0.9642, 1.0, 0.8249];
            let f = |t: f64| {
                if t > 216.0 / 24389.0 {
                    t.cbrt()
                } else {
                    (24389.0 / 27.0 * t + 16.0) / 116.0
                }
            };
            let (fx, fy, fz) = (
                f(xyz[0] / white[0]),
                f(xyz[1] / white[1]),
                f(xyz[2] / white[2]),
            );
            [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)]
        }
        let be16 = |v: u16| v.to_be_bytes();
        let be32 = |v: u32| v.to_be_bytes();
        // mft2: 4 in, 3 out, grid 2.
        let mut lut = Vec::new();
        lut.extend_from_slice(b"mft2");
        lut.extend_from_slice(&[0; 4]);
        lut.extend_from_slice(&[4, 3, 2, 0]);
        for v in [1.0f64, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0] {
            lut.extend_from_slice(&be32((v * 65536.0) as u32));
        }
        lut.extend_from_slice(&be16(2));
        lut.extend_from_slice(&be16(2));
        for _ in 0..4 {
            lut.extend_from_slice(&be16(0));
            lut.extend_from_slice(&be16(65535));
        }
        // CLUT: first input (C) varies slowest, K fastest.
        for c in 0..2u32 {
            for m in 0..2u32 {
                for y in 0..2u32 {
                    for k in 0..2u32 {
                        let rgb = [c, m, y].map(|ink| f64::from(1 - ink) * f64::from(1 - k));
                        let [l, a, b] = lab_d50(rgb);
                        // ICC v2 16-bit Lab: L = v / 652.8, a/b = v / 256 - 128.
                        lut.extend_from_slice(
                            &be16((l * 652.8).round().clamp(0.0, 65535.0) as u16),
                        );
                        lut.extend_from_slice(&be16(
                            ((a + 128.0) * 256.0).round().clamp(0.0, 65535.0) as u16,
                        ));
                        lut.extend_from_slice(&be16(
                            ((b + 128.0) * 256.0).round().clamp(0.0, 65535.0) as u16,
                        ));
                    }
                }
            }
        }
        for _ in 0..3 {
            lut.extend_from_slice(&be16(0));
            lut.extend_from_slice(&be16(65535));
        }
        let xyz_tag = |x: f64, y: f64, z: f64| {
            let mut t = b"XYZ \0\0\0\0".to_vec();
            for v in [x, y, z] {
                t.extend_from_slice(&((v * 65536.0).round() as i32).to_be_bytes());
            }
            t
        };
        let desc = {
            let name = b"synthetic cmyk\0";
            let mut t = b"desc\0\0\0\0".to_vec();
            t.extend_from_slice(&be32(name.len() as u32));
            t.extend_from_slice(name);
            t.extend_from_slice(&[0; 4 + 4 + 2 + 1 + 67]);
            t
        };
        let tags: Vec<(&[u8; 4], Vec<u8>)> = vec![
            (b"desc", desc),
            (b"wtpt", xyz_tag(0.9642, 1.0, 0.8249)),
            (b"A2B0", lut.clone()),
            (b"A2B1", lut),
        ];
        let table_len = 4 + tags.len() * 12;
        let mut offset = 128 + table_len;
        let mut table = be32(tags.len() as u32).to_vec();
        let mut data = Vec::new();
        for (sig, body) in &tags {
            table.extend_from_slice(*sig);
            table.extend_from_slice(&be32(offset as u32));
            table.extend_from_slice(&be32(body.len() as u32));
            let padded = body.len().div_ceil(4) * 4;
            data.extend_from_slice(body);
            data.resize(data.len() + (padded - body.len()), 0);
            offset += padded;
        }
        let mut header = vec![0u8; 128];
        header[8..12].copy_from_slice(&[2, 0x10, 0, 0]);
        header[12..16].copy_from_slice(b"prtr");
        header[16..20].copy_from_slice(b"CMYK");
        header[20..24].copy_from_slice(b"Lab ");
        header[36..40].copy_from_slice(b"acsp");
        // PCS illuminant D50.
        for (i, v) in [0.9642f64, 1.0, 0.8249].iter().enumerate() {
            header[68 + i * 4..72 + i * 4]
                .copy_from_slice(&((*v * 65536.0).round() as i32).to_be_bytes());
        }
        let mut out = header;
        out.extend_from_slice(&table);
        out.extend_from_slice(&data);
        let total = out.len() as u32;
        out[0..4].copy_from_slice(&total.to_be_bytes());
        out
    }

    #[test]
    fn an_embedded_cmyk_profile_is_applied_to_the_ink_values() {
        let icc = synthetic_cmyk_icc();
        // The profile itself must be one moxcms accepts as CMYK, or this test proves nothing.
        let parsed = moxcms::ColorProfile::new_from_slice(&icc).unwrap();
        assert_eq!(parsed.color_space, moxcms::DataColorSpace::Cmyk);
        for ycck in [false, true] {
            for (ink, want) in [
                ([0, 0, 0, 0], [255, 255, 255]), // no ink: paper
                ([255, 0, 0, 0], [0, 255, 255]), // full cyan
                ([0, 255, 0, 0], [255, 0, 255]), // full magenta
                ([0, 0, 0, 255], [0, 0, 0]),     // full black
            ] {
                let f = decode_bytes(&cmyk_jpeg_with(true, ycck, ink, Some(&icc)));
                // Through a profile the frame is in the working space, not sRGB.
                assert_ne!(f.cam_xyz[..9], XYZ_D50_TO_SRGB);
                // The corners sit on the sRGB gamut boundary, where the Lab round trip through
                // 16-bit table values and the gamut clamp leave a few levels of slack; a wrong
                // channel order or inversion would be 255 off.
                near(first_pixel_srgb8(&f), want, 14);
            }
        }
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

    #[test]
    fn adobe_cmyk_is_still_recognised_with_padding_between_segments() {
        let mut jpeg = cmyk_jpeg(true, false);
        // A stray zero pad right after the APP14 segment.
        let at = jpeg
            .windows(2)
            .position(|w| w == [0xFF, 0xEE])
            .expect("the encoder wrote an APP14 segment");
        let len = usize::from(u16::from_be_bytes([jpeg[at + 2], jpeg[at + 3]]));
        jpeg.insert(at + 2 + len, 0x00);
        assert_eq!(
            four_component(&jpeg),
            Some(FourComponent {
                adobe: true,
                transform: 0,
                uniform: true
            })
        );
    }
}
