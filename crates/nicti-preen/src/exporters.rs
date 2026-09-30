//! The built-in JPEG / PNG / TIFF exporters (#57; ADR-0056's encoder picks).
//!
//! Each does only what is format-specific: encode the already-resized, already-converted,
//! already-quantized pixels and attach ICC / DPI / XMP / EXIF in whatever way that container
//! supports. Resize, color, orientation and watermark are format-independent and live in
//! [`crate::export_frame`].
//!
//! - **JPEG** -- `jpeg-encoder` (pure Rust, v1 pick per #223) with chroma subsampling pinned
//!   explicitly, JFIF density = DPI, ICC natively, then XMP (APP1) and EXIF via `img-parts` /
//!   `little_exif`.
//! - **PNG** -- `image`'s encoder (8/16-bit), then ICC (`iCCP`), `pHYs` and XMP (`iTXt`) in one
//!   `img-parts` pass, then EXIF (`eXIf`).
//! - **TIFF** -- the `tiff` crate directly: resolution, ICC (tag 34675) and XMP (tag 700) are
//!   written as tags. EXIF in TIFF stays deferred (ADR-0056 / ADR-0059: no safe way to build an
//!   Exif sub-IFD here yet).

use std::io::Cursor;
use std::sync::Arc;

use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, ImageEncoder};
use img_parts::ImageICC;
use nicti_claw::{Descriptor, Module, Registry};
use serde_json::Value;

use crate::color::OutputPixels;
use crate::metadata::{embed_xmp_jpeg, exif_tiff_bytes, write_exif_jpeg, xmp_png_chunk};
use crate::spec::{BitDepth, ExportFormat, FormatSpec, Subsampling, TiffCompression};
use crate::{Embed, EmbedSupport, ExportError, Exporter, ExporterRegistry, OutputImage};

pub const JPEG_ID: &str = "nicti.exporter.jpeg";
pub const PNG_ID: &str = "nicti.exporter.png";
pub const TIFF_ID: &str = "nicti.exporter.tiff";

/// The registry id of the built-in exporter for `format`.
pub fn builtin_id(format: ExportFormat) -> &'static str {
    match format {
        ExportFormat::Jpeg => JPEG_ID,
        ExportFormat::Png => PNG_ID,
        ExportFormat::Tiff => TIFF_ID,
    }
}

/// A registry holding the three built-in exporters.
pub fn builtin_registry() -> ExporterRegistry {
    fn jpeg() -> Arc<dyn Exporter> {
        Arc::new(JpegExporter)
    }
    fn png() -> Arc<dyn Exporter> {
        Arc::new(PngExporter)
    }
    fn tiff() -> Arc<dyn Exporter> {
        Arc::new(TiffExporter)
    }
    let mut registry: Registry<dyn Exporter> = Registry::new();
    for (id, factory) in [
        (JPEG_ID, jpeg as fn() -> Arc<dyn Exporter>),
        (PNG_ID, png),
        (TIFF_ID, tiff),
    ] {
        registry
            .register(
                Descriptor {
                    id,
                    schema_version: 1,
                },
                factory,
            )
            .expect("built-in exporter ids are namespaced and unique");
    }
    registry
}

macro_rules! module_impl {
    ($ty:ty, $id:expr) => {
        impl Module for $ty {
            fn id(&self) -> &str {
                $id
            }
            fn schema_version(&self) -> u32 {
                1
            }
            fn migrate_params(&self, _from_version: u32, params: Value) -> Option<Value> {
                Some(params)
            }
        }
    };
}

fn wrong_format(exporter: &str, got: &FormatSpec) -> ExportError {
    ExportError::Encode(format!("{exporter} can't encode {:?}", got.format()))
}

fn encode_err(what: &str, e: impl std::fmt::Display) -> ExportError {
    ExportError::Encode(format!("{what}: {e}"))
}

// --- JPEG -------------------------------------------------------------------------------------

pub struct JpegExporter;
module_impl!(JpegExporter, JPEG_ID);

impl Exporter for JpegExporter {
    fn format(&self) -> ExportFormat {
        ExportFormat::Jpeg
    }

    fn extension(&self) -> &'static str {
        "jpg"
    }

    fn supports_depth(&self, depth: BitDepth) -> bool {
        depth == BitDepth::Eight
    }

    fn embeds(&self) -> EmbedSupport {
        EmbedSupport {
            exif: true,
            xmp: true,
            icc: true,
            dpi: true,
        }
    }

    fn encode(
        &self,
        img: &OutputImage<'_>,
        fmt: &FormatSpec,
        embed: &Embed<'_>,
    ) -> Result<Vec<u8>, ExportError> {
        let FormatSpec::Jpeg {
            quality,
            subsampling,
        } = fmt
        else {
            return Err(wrong_format("JpegExporter", fmt));
        };
        let OutputPixels::Rgb8(pixels) = img.pixels else {
            return Err(ExportError::Encode(
                "JPEG is 8-bit only; got 16-bit pixels".into(),
            ));
        };
        // jpeg-encoder takes u16 dimensions: a silent `as u16` would corrupt the encode.
        let w = u16::try_from(img.width).map_err(|_| {
            ExportError::Encode(format!("width {} exceeds JPEG's 65535px limit", img.width))
        })?;
        let h = u16::try_from(img.height).map_err(|_| {
            ExportError::Encode(format!(
                "height {} exceeds JPEG's 65535px limit",
                img.height
            ))
        })?;
        let dpi = u16::try_from(embed.dpi).unwrap_or(u16::MAX);

        let mut buf = Vec::new();
        {
            let mut enc = jpeg_encoder::Encoder::new(&mut buf, *quality);
            // Always pinned: the crate's own default flips 4:2:0 -> 4:4:4 at quality >= 90.
            enc.set_sampling_factor(match subsampling {
                Subsampling::S420 => jpeg_encoder::SamplingFactor::F_2_2,
                Subsampling::S444 => jpeg_encoder::SamplingFactor::F_1_1,
            });
            enc.set_density(jpeg_encoder::Density::Inch { x: dpi, y: dpi });
            if !embed.icc.is_empty() {
                enc.add_icc_profile(embed.icc)
                    .map_err(|e| encode_err("adding ICC profile", e))?;
            }
            enc.encode(pixels, w, h, jpeg_encoder::ColorType::Rgb)
                .map_err(|e| encode_err("JPEG encode", e))?;
        }
        if let Some(xmp) = embed.xmp {
            buf = embed_xmp_jpeg(&buf, xmp)?;
        }
        if let Some(exif) = embed.exif {
            buf = write_exif_jpeg(&buf, exif)?;
        }
        Ok(buf)
    }
}

// --- PNG --------------------------------------------------------------------------------------

pub struct PngExporter;
module_impl!(PngExporter, PNG_ID);

/// `pHYs` chunk: pixels per metre on both axes, unit byte 1 = metre.
fn phys_chunk(dpi: u32) -> img_parts::png::PngChunk {
    let ppm = ((dpi as f64) / 0.0254).round() as u32;
    let mut data = Vec::with_capacity(9);
    data.extend_from_slice(&ppm.to_be_bytes());
    data.extend_from_slice(&ppm.to_be_bytes());
    data.push(1);
    img_parts::png::PngChunk::new(*b"pHYs", bytes::Bytes::from(data))
}

impl Exporter for PngExporter {
    fn format(&self) -> ExportFormat {
        ExportFormat::Png
    }

    fn extension(&self) -> &'static str {
        "png"
    }

    fn supports_depth(&self, _depth: BitDepth) -> bool {
        true
    }

    fn embeds(&self) -> EmbedSupport {
        EmbedSupport {
            exif: true,
            xmp: true,
            icc: true,
            dpi: true,
        }
    }

    fn encode(
        &self,
        img: &OutputImage<'_>,
        fmt: &FormatSpec,
        embed: &Embed<'_>,
    ) -> Result<Vec<u8>, ExportError> {
        if !matches!(fmt, FormatSpec::Png { .. }) {
            return Err(wrong_format("PngExporter", fmt));
        }
        let mut buf = Vec::new();
        let encoder = PngEncoder::new(&mut buf);
        match img.pixels {
            OutputPixels::Rgb8(px) => {
                encoder.write_image(px, img.width, img.height, ExtendedColorType::Rgb8)
            }
            // `image` expects 16-bit samples as native-endian bytes.
            OutputPixels::Rgb16(px) => encoder.write_image(
                bytemuck::cast_slice(px),
                img.width,
                img.height,
                ExtendedColorType::Rgb16,
            ),
        }
        .map_err(|e| encode_err("PNG encode", e))?;

        // One img-parts pass for everything chunk-shaped. EXIF goes in as a standard `eXIf` chunk
        // (raw TIFF block), not via `little_exif`'s PNG writer, which emits a non-standard
        // ImageMagick-style zTXt profile and re-serializes any XMP `iTXt` it finds.
        let mut png = img_parts::png::Png::from_bytes(bytes::Bytes::from(buf))
            .map_err(|e| encode_err("re-parsing PNG", e))?;
        if !embed.icc.is_empty() {
            png.set_icc_profile(Some(bytes::Bytes::copy_from_slice(embed.icc)));
        }
        png.chunks_mut().insert(1, phys_chunk(embed.dpi));
        if let Some(exif) = embed.exif {
            png.chunks_mut().insert(
                1,
                img_parts::png::PngChunk::new(*b"eXIf", bytes::Bytes::from(exif_tiff_bytes(exif)?)),
            );
        }
        if let Some(xmp) = embed.xmp {
            png.chunks_mut().insert(1, xmp_png_chunk(xmp));
        }
        let out = png.encoder().bytes().to_vec();
        Ok(out)
    }
}

// --- TIFF -------------------------------------------------------------------------------------

pub struct TiffExporter;
module_impl!(TiffExporter, TIFF_ID);

const TIFF_TAG_XMP: u16 = 700;

/// An ICC profile as TIFF's UNDEFINED type (the tag's registered type). `[u8]` is BYTE in the
/// `tiff` crate, which exiftool flags as "non-standard format (int8u)".
struct Undefined<'a>(&'a [u8]);

impl tiff::encoder::TiffValue for Undefined<'_> {
    const BYTE_LEN: u8 = 1;
    const FIELD_TYPE: tiff::tags::Type = tiff::tags::Type::UNDEFINED;

    fn count(&self) -> usize {
        self.0.len()
    }

    fn data(&self) -> std::borrow::Cow<'_, [u8]> {
        std::borrow::Cow::Borrowed(self.0)
    }
}

impl Exporter for TiffExporter {
    fn format(&self) -> ExportFormat {
        ExportFormat::Tiff
    }

    fn extension(&self) -> &'static str {
        "tif"
    }

    fn supports_depth(&self, _depth: BitDepth) -> bool {
        true
    }

    fn embeds(&self) -> EmbedSupport {
        EmbedSupport {
            exif: false,
            xmp: true,
            icc: true,
            dpi: true,
        }
    }

    fn encode(
        &self,
        img: &OutputImage<'_>,
        fmt: &FormatSpec,
        embed: &Embed<'_>,
    ) -> Result<Vec<u8>, ExportError> {
        use tiff::encoder::{colortype, Compression, DeflateLevel, Rational, TiffEncoder};
        use tiff::tags::{ResolutionUnit, Tag};

        let FormatSpec::Tiff {
            compression: requested,
            ..
        } = fmt
        else {
            return Err(wrong_format("TiffExporter", fmt));
        };
        let compression = match requested {
            TiffCompression::None => Compression::Uncompressed,
            TiffCompression::Lzw => Compression::Lzw,
            TiffCompression::Deflate => Compression::Deflate(DeflateLevel::default()),
        };

        let mut cursor = Cursor::new(Vec::new());
        let mut encoder = TiffEncoder::new(&mut cursor)
            .map_err(|e| encode_err("TIFF init", e))?
            .with_compression(compression);

        macro_rules! write_image {
            ($color:ty, $data:expr) => {{
                let mut image = encoder
                    .new_image::<$color>(img.width, img.height)
                    .map_err(|e| encode_err("TIFF image", e))?;
                image.resolution(ResolutionUnit::Inch, Rational { n: embed.dpi, d: 1 });
                if !embed.icc.is_empty() {
                    image
                        .encoder()
                        .write_tag(Tag::IccProfile, Undefined(embed.icc))
                        .map_err(|e| encode_err("TIFF ICC tag", e))?;
                }
                if let Some(xmp) = embed.xmp {
                    image
                        .encoder()
                        .write_tag(Tag::Unknown(TIFF_TAG_XMP), xmp.as_bytes())
                        .map_err(|e| encode_err("TIFF XMP tag", e))?;
                }
                image
                    .write_data($data)
                    .map_err(|e| encode_err("TIFF write", e))?;
            }};
        }
        match img.pixels {
            OutputPixels::Rgb8(px) => write_image!(colortype::RGB8, px),
            OutputPixels::Rgb16(px) => write_image!(colortype::RGB16, px),
        }
        Ok(cursor.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{
        build_exif, build_xmp, read_exif, read_xmp_jpeg, read_xmp_png, ExifContext, ExifPayload,
        SourceExif, SourceMetadata,
    };
    use crate::spec::{ExportSpace, MetadataPolicy, MetadataSpec};
    use nicti_calico::space::OutputSpace;

    fn gradient8(w: u32, h: u32) -> OutputPixels {
        OutputPixels::Rgb8(
            (0..w * h)
                .flat_map(|i| {
                    let (x, y) = (i % w, i / w);
                    [(x * 255 / w) as u8, (y * 255 / h) as u8, 128]
                })
                .collect(),
        )
    }

    fn gradient16(w: u32, h: u32) -> OutputPixels {
        OutputPixels::Rgb16(
            (0..w * h)
                .flat_map(|i| {
                    let (x, y) = (i % w, i / w);
                    [(x * 65535 / w) as u16, (y * 65535 / h) as u16, 32768]
                })
                .collect(),
        )
    }

    struct Fixture {
        icc: Vec<u8>,
        exif: ExifPayload,
        xmp: String,
    }

    fn fixture(space: OutputSpace, w: u32, h: u32) -> Fixture {
        let spec = MetadataSpec {
            policy: MetadataPolicy::All,
            include_keywords: true,
            artist: Some("Jo".into()),
            copyright: Some("(c) Jo".into()),
        };
        let src = SourceMetadata {
            exif: SourceExif {
                make: Some("NIKON".into()),
                model: Some("Z 8".into()),
                ..SourceExif::default()
            },
            rating: Some(3),
            keywords: vec!["cat".into()],
            ..SourceMetadata::default()
        };
        let ctx = ExifContext {
            spec: &spec,
            source: &src,
            width: w,
            height: h,
            space: ExportSpace::Srgb,
            dpi: 300,
            software: "Nicti test",
        };
        Fixture {
            icc: nicti_calico::icc::profile_bytes(space).unwrap(),
            exif: build_exif(&ctx).unwrap(),
            xmp: build_xmp(&spec, &src, "Nicti test").unwrap(),
        }
    }

    fn embed<'a>(f: &'a Fixture, dpi: u32) -> Embed<'a> {
        Embed {
            icc: &f.icc,
            dpi,
            exif: Some(&f.exif),
            xmp: Some(&f.xmp),
        }
    }

    fn jpeg_spec(q: u8, s: Subsampling) -> FormatSpec {
        FormatSpec::Jpeg {
            quality: q,
            subsampling: s,
        }
    }

    /// (horizontal, vertical) sampling of the first (luma) component from the SOF marker.
    fn jpeg_luma_sampling(jpeg: &[u8]) -> (u8, u8) {
        let mut i = 2;
        while i + 4 < jpeg.len() {
            assert_eq!(jpeg[i], 0xFF, "lost sync at {i}");
            let marker = jpeg[i + 1];
            let len = u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize;
            if matches!(marker, 0xC0..=0xC2) {
                // len(2) precision(1) height(2) width(2) ncomp(1) then id, sampling, tq per comp
                let s = jpeg[i + 4 + 1 + 2 + 2 + 1 + 1];
                return (s >> 4, s & 0x0F);
            }
            i += 2 + len;
        }
        panic!("no SOF marker");
    }

    #[test]
    fn builtin_registry_resolves_all_three_with_stable_ids() {
        let reg = builtin_registry();
        for (fmt, ext) in [
            (ExportFormat::Jpeg, "jpg"),
            (ExportFormat::Png, "png"),
            (ExportFormat::Tiff, "tif"),
        ] {
            let e = reg.get(builtin_id(fmt)).expect("registered");
            assert_eq!(e.format(), fmt);
            assert_eq!(e.extension(), ext);
        }
        assert!(!reg.get(JPEG_ID).unwrap().supports_depth(BitDepth::Sixteen));
        assert!(reg.get(PNG_ID).unwrap().supports_depth(BitDepth::Sixteen));
        assert!(!reg.get(TIFF_ID).unwrap().embeds().exif);
    }

    #[test]
    fn jpeg_decodes_carries_icc_dpi_xmp_exif_and_pins_subsampling() {
        let (w, h) = (48u32, 32u32);
        let f = fixture(OutputSpace::DisplayP3, w, h);
        let img = OutputImage {
            width: w,
            height: h,
            pixels: &gradient8(w, h),
        };
        let out420 = JpegExporter
            .encode(&img, &jpeg_spec(95, Subsampling::S420), &embed(&f, 240))
            .unwrap();
        // quality >= 90 would silently become 4:4:4 without the pin.
        assert_eq!(jpeg_luma_sampling(&out420), (2, 2));
        let out444 = JpegExporter
            .encode(&img, &jpeg_spec(50, Subsampling::S444), &embed(&f, 240))
            .unwrap();
        assert_eq!(jpeg_luma_sampling(&out444), (1, 1));

        let decoded = image::load_from_memory(&out420).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (w, h));

        let parsed = img_parts::jpeg::Jpeg::from_bytes(bytes::Bytes::from(out420.clone())).unwrap();
        assert_eq!(parsed.icc_profile().unwrap().as_ref(), f.icc.as_slice());
        // JFIF APP0 density: unit 1 (dpi), x/y = 240.
        let jfif = parsed
            .segments()
            .iter()
            .find(|s| s.marker() == 0xE0)
            .unwrap();
        let c = jfif.contents();
        assert_eq!(&c[..5], b"JFIF\0");
        assert_eq!(c[7], 1);
        assert_eq!(u16::from_be_bytes([c[8], c[9]]), 240);
        assert_eq!(u16::from_be_bytes([c[10], c[11]]), 240);
        assert_eq!(
            read_xmp_jpeg(&out420).unwrap().as_deref(),
            Some(f.xmp.as_str())
        );
        let exif = read_exif(&out420, little_exif::filetype::FileExtension::JPEG).unwrap();
        assert!(format!(
            "{:?}",
            exif.get_tag(&little_exif::exif_tag::ExifTag::Model(String::new()))
                .next()
        )
        .contains("Z 8"));
    }

    #[test]
    fn jpeg_refuses_16_bit_and_oversized_and_wrong_format() {
        let f = fixture(OutputSpace::Srgb, 4, 4);
        let px16 = gradient16(4, 4);
        let img = OutputImage {
            width: 4,
            height: 4,
            pixels: &px16,
        };
        assert!(JpegExporter
            .encode(&img, &jpeg_spec(90, Subsampling::S420), &embed(&f, 300))
            .is_err());
        let px8 = gradient8(4, 4);
        let img = OutputImage {
            width: 4,
            height: 4,
            pixels: &px8,
        };
        assert!(JpegExporter
            .encode(
                &img,
                &FormatSpec::Png {
                    depth: BitDepth::Eight
                },
                &embed(&f, 300)
            )
            .is_err());
        let huge = OutputImage {
            width: 70_000,
            height: 1,
            pixels: &px8,
        };
        assert!(JpegExporter
            .encode(&huge, &jpeg_spec(90, Subsampling::S420), &embed(&f, 300))
            .is_err());
    }

    #[test]
    fn png_8_and_16_bit_round_trip_with_icc_phys_xmp_and_exif() {
        let (w, h) = (16u32, 8u32);
        let f = fixture(OutputSpace::AdobeRgb, w, h);
        for (depth, px) in [
            (BitDepth::Eight, gradient8(w, h)),
            (BitDepth::Sixteen, gradient16(w, h)),
        ] {
            let img = OutputImage {
                width: w,
                height: h,
                pixels: &px,
            };
            let out = PngExporter
                .encode(&img, &FormatSpec::Png { depth }, &embed(&f, 300))
                .unwrap();
            let decoded = image::load_from_memory(&out).unwrap();
            assert_eq!((decoded.width(), decoded.height()), (w, h));
            match (&px, &decoded) {
                (OutputPixels::Rgb16(src), image::DynamicImage::ImageRgb16(d)) => {
                    assert_eq!(d.as_raw(), src, "16-bit samples survive unchanged");
                }
                (OutputPixels::Rgb8(src), image::DynamicImage::ImageRgb8(d)) => {
                    assert_eq!(d.as_raw(), src);
                }
                other => panic!("unexpected decode type {:?}", other.1.color()),
            }

            let png = img_parts::png::Png::from_bytes(bytes::Bytes::from(out.clone())).unwrap();
            assert_eq!(png.icc_profile().unwrap().as_ref(), f.icc.as_slice());
            let phys = png.chunks().iter().find(|c| c.kind() == *b"pHYs").unwrap();
            let c = phys.contents();
            // 300 dpi = 11811 pixels per metre.
            assert_eq!(u32::from_be_bytes([c[0], c[1], c[2], c[3]]), 11_811);
            assert_eq!(u32::from_be_bytes([c[4], c[5], c[6], c[7]]), 11_811);
            assert_eq!(c[8], 1);
            assert_eq!(read_xmp_png(&out).unwrap().as_deref(), Some(f.xmp.as_str()));
            assert!(read_exif(
                &out,
                little_exif::filetype::FileExtension::PNG {
                    as_zTXt_chunk: false
                }
            )
            .is_ok());
            // Every chunk before IDAT is in a legal position: pHYs/iCCP precede the first IDAT.
            let kinds: Vec<_> = png.chunks().iter().map(|c| c.kind()).collect();
            let idat = kinds.iter().position(|k| k == b"IDAT").unwrap();
            for k in [b"pHYs", b"iCCP"] {
                assert!(kinds.iter().position(|x| x == k).unwrap() < idat);
            }
        }
    }

    #[test]
    fn tiff_8_and_16_bit_round_trip_with_resolution_icc_and_xmp() {
        use tiff::decoder::{Decoder, DecodingResult};
        use tiff::tags::Tag;
        let (w, h) = (16u32, 8u32);
        let f = fixture(OutputSpace::Srgb, w, h);
        for compression in [
            TiffCompression::None,
            TiffCompression::Lzw,
            TiffCompression::Deflate,
        ] {
            for (depth, px) in [
                (BitDepth::Eight, gradient8(w, h)),
                (BitDepth::Sixteen, gradient16(w, h)),
            ] {
                let img = OutputImage {
                    width: w,
                    height: h,
                    pixels: &px,
                };
                let out = TiffExporter
                    .encode(
                        &img,
                        &FormatSpec::Tiff { depth, compression },
                        &embed(&f, 300),
                    )
                    .unwrap();
                let mut dec = Decoder::new(Cursor::new(&out)).unwrap();
                assert_eq!(dec.dimensions().unwrap(), (w, h));
                match (dec.read_image().unwrap(), &px) {
                    (DecodingResult::U8(d), OutputPixels::Rgb8(s)) => assert_eq!(&d, s),
                    (DecodingResult::U16(d), OutputPixels::Rgb16(s)) => assert_eq!(&d, s),
                    _ => panic!("sample type mismatch"),
                }
                let icc = dec.get_tag_u8_vec(Tag::IccProfile).unwrap();
                assert_eq!(icc, f.icc);
                let xmp = dec.get_tag_u8_vec(Tag::Unknown(TIFF_TAG_XMP)).unwrap();
                assert_eq!(String::from_utf8(xmp).unwrap(), f.xmp);
                assert_eq!(dec.get_tag_u32(Tag::ResolutionUnit).unwrap(), 2);
                assert_eq!(
                    dec.get_tag(Tag::XResolution).unwrap(),
                    tiff::decoder::ifd::Value::Rational(300, 1)
                );
            }
        }
    }

    #[test]
    fn no_icc_or_metadata_still_encodes() {
        let px = gradient8(8, 8);
        let img = OutputImage {
            width: 8,
            height: 8,
            pixels: &px,
        };
        let bare = Embed {
            icc: &[],
            dpi: 72,
            exif: None,
            xmp: None,
        };
        for (e, fmt) in [
            (
                &JpegExporter as &dyn Exporter,
                jpeg_spec(80, Subsampling::S420),
            ),
            (
                &PngExporter,
                FormatSpec::Png {
                    depth: BitDepth::Eight,
                },
            ),
            (
                &TiffExporter,
                FormatSpec::Tiff {
                    depth: BitDepth::Eight,
                    compression: TiffCompression::None,
                },
            ),
        ] {
            let out = e.encode(&img, &fmt, &bare).unwrap();
            assert!(image::load_from_memory(&out).is_ok(), "{:?}", e.format());
        }
    }
}
