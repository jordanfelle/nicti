//! Export metadata (#57, promoted from `spikes/prey`; ADR-0056): reading the source photo's EXIF,
//! building the exported file's EXIF + XMP under a [`MetadataPolicy`], and the JPEG/PNG segment
//! and chunk embedding.
//!
//! - **EXIF** is built by `little_exif` (typed tags -> a real IFD; `img-parts` only stores a blob).
//! - **XMP** is a hand-rolled packet inserted as a JPEG APP1 segment / PNG `iTXt` chunk on
//!   `img-parts` primitives (adapted from `spikes/scent`, copied not depended on).
//! - `ColorSpace` is 1 (sRGB) only for sRGB output and 0xFFFF (uncalibrated) otherwise -- the spike
//!   hardcoded 1, which is wrong for P3/AdobeRGB. The embedded ICC profile carries the real space.
//! - Orientation is always 1: the render was already rotated upright.
//! - GPS is never written (it isn't stored or read anywhere in the catalog yet).

use std::io::{BufRead, Seek};
use std::path::Path;

use little_exif::exif_tag::ExifTag;
use little_exif::filetype::FileExtension;
use little_exif::metadata::Metadata;
use little_exif::rational::uR64;

use crate::orient::Orientation;
use crate::spec::{ExportSpace, MetadataPolicy, MetadataSpec};

#[derive(Debug, thiserror::Error)]
pub enum MetadataError {
    #[error("writing EXIF: {0}")]
    Exif(String),
    #[error("parsing image for metadata embed: {0}")]
    Parse(String),
    #[error(
        "XMP packet ({0} bytes) exceeds the single APP1 segment limit -- multi-segment XMP is not implemented"
    )]
    XmpTooLarge(usize),
}

// --- reading the source ------------------------------------------------------------------------

/// What export needs from the source photo's own EXIF. Every field is optional: a missing or
/// unreadable value is simply absent, never an error.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SourceExif {
    pub make: Option<String>,
    pub model: Option<String>,
    pub lens_model: Option<String>,
    /// (numerator, denominator).
    pub exposure_time: Option<(u32, u32)>,
    pub f_number: Option<(u32, u32)>,
    pub focal_length: Option<(u32, u32)>,
    pub iso: Option<u32>,
    /// EXIF's own `YYYY:MM:DD HH:MM:SS` form.
    pub date_time_original: Option<String>,
    /// `+HH:MM` / `-HH:MM`.
    pub offset_time_original: Option<String>,
    pub orientation: Orientation,
}

fn ascii(field: &exif::Field) -> Option<String> {
    match &field.value {
        exif::Value::Ascii(parts) => {
            let s = parts
                .iter()
                .map(|p| {
                    String::from_utf8_lossy(p)
                        .trim_matches('\0')
                        .trim()
                        .to_string()
                })
                .collect::<Vec<_>>()
                .join(" ");
            (!s.is_empty()).then_some(s)
        }
        _ => None,
    }
}

fn rational(field: &exif::Field) -> Option<(u32, u32)> {
    match &field.value {
        exif::Value::Rational(v) => v.first().filter(|r| r.denom != 0).map(|r| (r.num, r.denom)),
        _ => None,
    }
}

impl SourceExif {
    /// Reads from any TIFF/JPEG-container reader. A read failure yields the empty default.
    pub fn from_reader<R: BufRead + Seek>(reader: &mut R) -> Self {
        let Ok(exif) = exif::Reader::new().read_from_container(reader) else {
            return Self::default();
        };
        let get = |tag| exif.get_field(tag, exif::In::PRIMARY);
        SourceExif {
            make: get(exif::Tag::Make).and_then(ascii),
            model: get(exif::Tag::Model).and_then(ascii),
            lens_model: get(exif::Tag::LensModel).and_then(ascii),
            exposure_time: get(exif::Tag::ExposureTime).and_then(rational),
            f_number: get(exif::Tag::FNumber).and_then(rational),
            focal_length: get(exif::Tag::FocalLength).and_then(rational),
            iso: get(exif::Tag::PhotographicSensitivity).and_then(|f| f.value.get_uint(0)),
            date_time_original: get(exif::Tag::DateTimeOriginal).and_then(ascii),
            offset_time_original: get(exif::Tag::OffsetTimeOriginal).and_then(ascii),
            orientation: get(exif::Tag::Orientation)
                .and_then(|f| f.value.get_uint(0))
                .map(Orientation::from_exif)
                .unwrap_or_default(),
        }
    }

    /// Reads a file's EXIF; unreadable/absent -> the empty default.
    pub fn from_path(path: &Path) -> Self {
        match std::fs::File::open(path) {
            Ok(f) => Self::from_reader(&mut std::io::BufReader::new(f)),
            Err(_) => Self::default(),
        }
    }
}

/// Everything known about the source photo at export time: its own EXIF, plus catalog fields
/// that are the fallback when EXIF lacks them (and the only source for rating/label/keywords).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SourceMetadata {
    pub exif: SourceExif,
    pub catalog_make: Option<String>,
    pub catalog_model: Option<String>,
    /// The catalog's dashed `YYYY-MM-DD HH:MM:SS`.
    pub catalog_captured_at: Option<String>,
    /// `Some(-1)` = rejected, 1..=5 stars.
    pub rating: Option<i32>,
    pub label: Option<String>,
    pub keywords: Vec<String>,
}

impl SourceMetadata {
    fn make(&self) -> Option<&str> {
        self.exif.make.as_deref().or(self.catalog_make.as_deref())
    }

    fn model(&self) -> Option<&str> {
        self.exif.model.as_deref().or(self.catalog_model.as_deref())
    }

    /// EXIF-form capture time: the file's own, else the catalog's converted from dashed form.
    fn date_time_original(&self) -> Option<String> {
        self.exif.date_time_original.clone().or_else(|| {
            let c = self.catalog_captured_at.as_deref()?;
            let (date, time) = c.split_once(' ')?;
            Some(format!("{} {time}", date.replace('-', ":")))
        })
    }
}

// --- building the exported EXIF ------------------------------------------------------------------

/// The EXIF block for an exported file (opaque; write it with [`write_exif_jpeg`] /
/// [`write_exif_png`]).
pub struct ExifPayload(Metadata);

/// Inputs to [`build_exif`].
pub struct ExifContext<'a> {
    pub spec: &'a MetadataSpec,
    pub source: &'a SourceMetadata,
    /// The exported image's own pixel size (`ExifImageWidth`/`Height` are mandatory).
    pub width: u32,
    pub height: u32,
    pub space: ExportSpace,
    pub dpi: u32,
    pub software: &'a str,
}

fn urat(n: u32, d: u32) -> uR64 {
    uR64 {
        nominator: n,
        denominator: d,
    }
}

/// EXIF for the export, or `None` under [`MetadataPolicy::None`].
pub fn build_exif(ctx: &ExifContext<'_>) -> Option<ExifPayload> {
    if ctx.spec.policy == MetadataPolicy::None {
        return None;
    }
    let mut exif = Metadata::new();
    // Mandatory ExifIFD tags `exiftool -validate` checks for.
    exif.set_tag(ExifTag::ExifVersion(b"0231".to_vec()));
    exif.set_tag(ExifTag::ComponentsConfiguration(vec![1, 2, 3, 0]));
    exif.set_tag(ExifTag::ColorSpace(vec![match ctx.space {
        ExportSpace::Srgb => 1,
        _ => 0xFFFF,
    }]));
    exif.set_tag(ExifTag::ExifImageWidth(vec![ctx.width]));
    exif.set_tag(ExifTag::ExifImageHeight(vec![ctx.height]));
    exif.set_tag(ExifTag::YCbCrPositioning(vec![1]));
    exif.set_tag(ExifTag::Orientation(vec![1]));
    exif.set_tag(ExifTag::XResolution(vec![urat(ctx.dpi, 1)]));
    exif.set_tag(ExifTag::YResolution(vec![urat(ctx.dpi, 1)]));
    exif.set_tag(ExifTag::ResolutionUnit(vec![2])); // inches

    if let Some(artist) = &ctx.spec.artist {
        exif.set_tag(ExifTag::Artist(artist.clone()));
    }
    if let Some(copyright) = &ctx.spec.copyright {
        exif.set_tag(ExifTag::Copyright(copyright.clone()));
    }

    if ctx.spec.policy == MetadataPolicy::All {
        let s = ctx.source;
        if let Some(make) = s.make() {
            exif.set_tag(ExifTag::Make(make.to_string()));
        }
        if let Some(model) = s.model() {
            exif.set_tag(ExifTag::Model(model.to_string()));
        }
        if let Some(lens) = &s.exif.lens_model {
            exif.set_tag(ExifTag::LensModel(lens.clone()));
        }
        if let Some((n, d)) = s.exif.exposure_time {
            exif.set_tag(ExifTag::ExposureTime(vec![urat(n, d)]));
        }
        if let Some((n, d)) = s.exif.f_number {
            exif.set_tag(ExifTag::FNumber(vec![urat(n, d)]));
        }
        if let Some((n, d)) = s.exif.focal_length {
            exif.set_tag(ExifTag::FocalLength(vec![urat(n, d)]));
        }
        if let Some(iso) = s.exif.iso {
            exif.set_tag(ExifTag::ISO(vec![iso.min(u16::MAX as u32) as u16]));
        }
        if let Some(dto) = s.date_time_original() {
            exif.set_tag(ExifTag::DateTimeOriginal(dto));
        }
        if let Some(off) = &s.exif.offset_time_original {
            exif.set_tag(ExifTag::OffsetTimeOriginal(off.clone()));
        }
        exif.set_tag(ExifTag::Software(ctx.software.to_string()));
    }
    Some(ExifPayload(exif))
}

/// Writes `exif` into an in-memory JPEG, returning the rewritten bytes.
pub fn write_exif_jpeg(jpeg: &[u8], exif: &ExifPayload) -> Result<Vec<u8>, MetadataError> {
    let mut buf = jpeg.to_vec();
    exif.0
        .write_to_vec(&mut buf, FileExtension::JPEG)
        .map_err(|e| MetadataError::Exif(e.to_string()))?;
    Ok(buf)
}

/// The raw TIFF-structured EXIF block (`II*\0`/`MM\0*` ..., no `Exif\0\0` prefix) -- what a PNG
/// `eXIf` chunk holds. `little_exif` can only write a PNG's EXIF as a non-standard ImageMagick-style
/// `zTXt` "Raw profile type exif" chunk (exiftool flags it), so we have it write a 1x1 JPEG and lift
/// the APP1 payload out instead.
pub fn exif_tiff_bytes(exif: &ExifPayload) -> Result<Vec<u8>, MetadataError> {
    use img_parts::ImageEXIF;
    let mut carrier = Vec::new();
    jpeg_encoder::Encoder::new(&mut carrier, 50)
        .encode(&[0u8; 3], 1, 1, jpeg_encoder::ColorType::Rgb)
        .map_err(|e| MetadataError::Exif(e.to_string()))?;
    let with_exif = write_exif_jpeg(&carrier, exif)?;
    let parsed = img_parts::jpeg::Jpeg::from_bytes(bytes::Bytes::from(with_exif))
        .map_err(|e| MetadataError::Parse(e.to_string()))?;
    parsed
        .exif()
        .map(|b| b.to_vec())
        .ok_or_else(|| MetadataError::Exif("EXIF carrier lost its APP1 segment".into()))
}

/// Reads EXIF back from an in-memory JPEG/PNG (round-trip verification).
pub fn read_exif(bytes: &[u8], file_type: FileExtension) -> Result<Metadata, MetadataError> {
    Metadata::new_from_vec(&bytes.to_vec(), file_type)
        .map_err(|e| MetadataError::Exif(e.to_string()))
}

// --- XMP ---------------------------------------------------------------------------------------

/// Escapes text for an XML element body/attribute and drops characters XML 1.0 forbids.
pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(c),
            c if (c as u32) < 0x20 || c == '\u{fffe}' || c == '\u{ffff}' => {}
            c => out.push(c),
        }
    }
    out
}

/// Wraps an `<x:xmpmeta>` document in the xpacket processing-instruction pair (`exiftool
/// -validate` warns without it).
pub fn wrap_xpacket(xmp: &str) -> String {
    format!("<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n{xmp}\n<?xpacket end=\"w\"?>")
}

/// The XMP packet for an export, or `None` under [`MetadataPolicy::None`].
pub fn build_xmp(spec: &MetadataSpec, source: &SourceMetadata, software: &str) -> Option<String> {
    if spec.policy == MetadataPolicy::None {
        return None;
    }
    let mut body = String::new();
    if spec.policy == MetadataPolicy::All {
        body.push_str(&format!(
            "   <xmp:CreatorTool>{}</xmp:CreatorTool>\n",
            xml_escape(software)
        ));
        if let Some(r) = source.rating {
            body.push_str(&format!("   <xmp:Rating>{}</xmp:Rating>\n", r.clamp(-1, 5)));
        }
        if let Some(l) = source.label.as_deref().filter(|l| !l.is_empty()) {
            body.push_str(&format!("   <xmp:Label>{}</xmp:Label>\n", xml_escape(l)));
        }
    }
    if let Some(artist) = spec.artist.as_deref().filter(|a| !a.is_empty()) {
        body.push_str(&format!(
            "   <dc:creator><rdf:Seq><rdf:li>{}</rdf:li></rdf:Seq></dc:creator>\n",
            xml_escape(artist)
        ));
    }
    if let Some(c) = spec.copyright.as_deref().filter(|c| !c.is_empty()) {
        body.push_str(&format!(
            "   <dc:rights><rdf:Alt><rdf:li xml:lang=\"x-default\">{}</rdf:li></rdf:Alt></dc:rights>\n   <xmpRights:Marked>True</xmpRights:Marked>\n",
            xml_escape(c)
        ));
    }
    if spec.policy == MetadataPolicy::All && spec.include_keywords && !source.keywords.is_empty() {
        body.push_str("   <dc:subject><rdf:Bag>\n");
        for k in &source.keywords {
            body.push_str(&format!("    <rdf:li>{}</rdf:li>\n", xml_escape(k)));
        }
        body.push_str("   </rdf:Bag></dc:subject>\n");
    }
    if body.is_empty() {
        return None;
    }
    let xmp = format!(
        "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n  <rdf:Description rdf:about=\"\"\n   xmlns:dc=\"http://purl.org/dc/elements/1.1/\"\n   xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\"\n   xmlns:xmpRights=\"http://ns.adobe.com/xap/1.0/rights/\">\n{body}  </rdf:Description>\n </rdf:RDF>\n</x:xmpmeta>"
    );
    Some(wrap_xpacket(&xmp))
}

/// JFIF requires its APP0 segment immediately after SOI, but `little_exif` and the XMP insert both
/// put their APP1 at index 1. Moves a leading-APP0-elsewhere JFIF segment back to the front.
pub fn jfif_first(jpeg: &[u8]) -> Result<Vec<u8>, MetadataError> {
    let mut img = img_parts::jpeg::Jpeg::from_bytes(bytes::Bytes::copy_from_slice(jpeg))
        .map_err(|e| MetadataError::Parse(e.to_string()))?;
    let is_jfif = |s: &img_parts::jpeg::JpegSegment| {
        s.marker() == 0xE0 && s.contents().starts_with(b"JFIF\0")
    };
    if let Some(pos) = img.segments().iter().position(is_jfif) {
        if pos != 0 {
            let seg = img.segments_mut().remove(pos);
            img.segments_mut().insert(0, seg);
            return Ok(img.encoder().bytes().to_vec());
        }
    }
    Ok(jpeg.to_vec())
}

const XMP_SIGNATURE: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
const APP1: u8 = 0xE1;
/// A segment length field is a u16 that includes itself.
const MAX_SEGMENT_PAYLOAD: usize = 65_533;
const PNG_XMP_KEYWORD: &[u8] = b"XML:com.adobe.xmp";

/// The APP1 segment carrying `xmp_packet`.
pub fn xmp_jpeg_segment(xmp_packet: &str) -> Result<img_parts::jpeg::JpegSegment, MetadataError> {
    if XMP_SIGNATURE.len() + xmp_packet.len() > MAX_SEGMENT_PAYLOAD {
        return Err(MetadataError::XmpTooLarge(xmp_packet.len()));
    }
    let mut contents = Vec::with_capacity(XMP_SIGNATURE.len() + xmp_packet.len());
    contents.extend_from_slice(XMP_SIGNATURE);
    contents.extend_from_slice(xmp_packet.as_bytes());
    Ok(img_parts::jpeg::JpegSegment::new_with_contents(
        APP1,
        bytes::Bytes::from(contents),
    ))
}

/// Embeds `xmp_packet` as a JPEG APP1 segment, replacing any existing XMP APP1 (an EXIF APP1 is
/// untouched).
pub fn embed_xmp_jpeg(jpeg: &[u8], xmp_packet: &str) -> Result<Vec<u8>, MetadataError> {
    let segment = xmp_jpeg_segment(xmp_packet)?;
    let mut img = img_parts::jpeg::Jpeg::from_bytes(bytes::Bytes::copy_from_slice(jpeg))
        .map_err(|e| MetadataError::Parse(e.to_string()))?;
    img.segments_mut()
        .retain(|s| !(s.marker() == APP1 && s.contents().starts_with(XMP_SIGNATURE)));
    // Right after SOI; order among APPn segments doesn't matter to a reader.
    img.segments_mut().insert(1, segment);
    Ok(img.encoder().bytes().to_vec())
}

/// Reads an embedded XMP packet back from a JPEG.
pub fn read_xmp_jpeg(jpeg: &[u8]) -> Result<Option<String>, MetadataError> {
    let img = img_parts::jpeg::Jpeg::from_bytes(bytes::Bytes::copy_from_slice(jpeg))
        .map_err(|e| MetadataError::Parse(e.to_string()))?;
    Ok(img
        .segments()
        .iter()
        .find(|s| s.marker() == APP1 && s.contents().starts_with(XMP_SIGNATURE))
        .map(|s| String::from_utf8_lossy(&s.contents()[XMP_SIGNATURE.len()..]).into_owned()))
}

/// The uncompressed `iTXt` chunk (`XML:com.adobe.xmp`) carrying `xmp_packet`.
pub fn xmp_png_chunk(xmp_packet: &str) -> img_parts::png::PngChunk {
    // iTXt: keyword \0 compression-flag compression-method language \0 translated-keyword \0 text
    let mut contents = Vec::with_capacity(PNG_XMP_KEYWORD.len() + 5 + xmp_packet.len());
    contents.extend_from_slice(PNG_XMP_KEYWORD);
    contents.extend_from_slice(&[0, 0, 0, 0, 0]);
    contents.extend_from_slice(xmp_packet.as_bytes());
    img_parts::png::PngChunk::new(*b"iTXt", bytes::Bytes::from(contents))
}

/// Embeds `xmp_packet` into a PNG, replacing any existing XMP `iTXt` chunk.
pub fn embed_xmp_png(png: &[u8], xmp_packet: &str) -> Result<Vec<u8>, MetadataError> {
    let mut img = img_parts::png::Png::from_bytes(bytes::Bytes::copy_from_slice(png))
        .map_err(|e| MetadataError::Parse(e.to_string()))?;
    img.chunks_mut()
        .retain(|c| !(c.kind() == *b"iTXt" && c.contents().starts_with(PNG_XMP_KEYWORD)));
    // Right after IHDR, ahead of IDAT.
    img.chunks_mut().insert(1, xmp_png_chunk(xmp_packet));
    Ok(img.encoder().bytes().to_vec())
}

/// Reads an embedded XMP packet back from a PNG; malformed chunks are skipped, never a panic.
pub fn read_xmp_png(png: &[u8]) -> Result<Option<String>, MetadataError> {
    let img = img_parts::png::Png::from_bytes(bytes::Bytes::copy_from_slice(png))
        .map_err(|e| MetadataError::Parse(e.to_string()))?;
    for chunk in img.chunks() {
        if chunk.kind() != *b"iTXt" || !chunk.contents().starts_with(PNG_XMP_KEYWORD) {
            continue;
        }
        let rest = &chunk.contents()[PNG_XMP_KEYWORD.len()..];
        // rest: \0 flag method language \0 translated-keyword \0 text
        if rest.len() < 3 {
            continue;
        }
        let after_flags = &rest[3..];
        let Some(lang_end) = after_flags.iter().position(|&b| b == 0) else {
            continue;
        };
        let after_lang = &after_flags[lang_end + 1..];
        let Some(tk_end) = after_lang.iter().position(|&b| b == 0) else {
            continue;
        };
        return Ok(Some(
            String::from_utf8_lossy(&after_lang[tk_end + 1..]).into_owned(),
        ));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn tiny_jpeg() -> Vec<u8> {
        let mut buf = Vec::new();
        jpeg_encoder::Encoder::new(&mut buf, 90)
            .encode(&[120u8; 8 * 8 * 3], 8, 8, jpeg_encoder::ColorType::Rgb)
            .unwrap();
        buf
    }

    fn tiny_png() -> Vec<u8> {
        let mut buf = Vec::new();
        image::RgbImage::from_pixel(8, 8, image::Rgb([10, 20, 30]))
            .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        buf
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
                iso: Some(100_000),
                date_time_original: Some("2026:09:27 14:03:09".into()),
                offset_time_original: Some("-04:00".into()),
                orientation: Orientation::Rotate90Cw,
            },
            rating: Some(4),
            label: Some("Red".into()),
            keywords: vec!["cat".into(), "a&b <c>".into()],
            ..SourceMetadata::default()
        }
    }

    fn ctx<'a>(
        spec: &'a MetadataSpec,
        src: &'a SourceMetadata,
        space: ExportSpace,
    ) -> ExifContext<'a> {
        ExifContext {
            spec,
            source: src,
            width: 8,
            height: 8,
            space,
            dpi: 300,
            software: "Nicti 0.0",
        }
    }

    fn tag_debug(m: &Metadata) -> String {
        let probes = [
            ExifTag::Make(String::new()),
            ExifTag::Model(String::new()),
            ExifTag::LensModel(String::new()),
            ExifTag::Artist(String::new()),
            ExifTag::Copyright(String::new()),
            ExifTag::Software(String::new()),
            ExifTag::DateTimeOriginal(String::new()),
            ExifTag::OffsetTimeOriginal(String::new()),
            ExifTag::ISO(vec![]),
            ExifTag::ColorSpace(vec![]),
            ExifTag::ExposureTime(vec![]),
            ExifTag::FNumber(vec![]),
            ExifTag::FocalLength(vec![]),
            ExifTag::Orientation(vec![]),
            ExifTag::XResolution(vec![]),
            ExifTag::ResolutionUnit(vec![]),
        ];
        probes
            .iter()
            .filter_map(|p| m.get_tag(p).next())
            .map(|t| format!("{t:?}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn all_policy_writes_camera_exposure_and_required_tags_and_resets_orientation() {
        let spec = MetadataSpec {
            policy: MetadataPolicy::All,
            artist: Some("Jo".into()),
            copyright: Some("(c) Jo".into()),
            ..MetadataSpec::default()
        };
        let src = source();
        let payload = build_exif(&ctx(&spec, &src, ExportSpace::Srgb)).unwrap();
        let out = write_exif_jpeg(&tiny_jpeg(), &payload).unwrap();
        let back = read_exif(&out, FileExtension::JPEG).unwrap();
        let dump = tag_debug(&back);
        for needle in [
            "NIKON Z 8",
            "NIKKOR Z 24-70mm f/2.8 S",
            "Jo",
            "(c) Jo",
            "Nicti 0.0",
            "2026:09:27 14:03:09",
            "-04:00",
        ] {
            assert!(dump.contains(needle), "missing {needle}: {dump}");
        }
        // ISO above u16::MAX is clamped, not wrapped.
        assert!(dump.contains("65535"), "{dump}");
    }

    #[test]
    fn copyright_only_writes_no_camera_data_and_none_writes_nothing() {
        let src = source();
        let spec = MetadataSpec {
            policy: MetadataPolicy::CopyrightOnly,
            artist: Some("Jo".into()),
            copyright: Some("(c) Jo".into()),
            ..MetadataSpec::default()
        };
        let payload = build_exif(&ctx(&spec, &src, ExportSpace::Srgb)).unwrap();
        let dump = tag_debug(&payload.0);
        assert!(
            dump.contains("Jo") && !dump.contains("NIKON") && !dump.contains("Nicti"),
            "{dump}"
        );
        assert!(build_xmp(&spec, &src, "Nicti")
            .unwrap()
            .contains("dc:creator"));
        assert!(!build_xmp(&spec, &src, "Nicti")
            .unwrap()
            .contains("xmp:Rating"));

        let none = MetadataSpec {
            policy: MetadataPolicy::None,
            artist: Some("Jo".into()),
            ..MetadataSpec::default()
        };
        assert!(build_exif(&ctx(&none, &src, ExportSpace::Srgb)).is_none());
        assert!(build_xmp(&none, &src, "Nicti").is_none());
    }

    #[test]
    fn color_space_tag_is_srgb_only_for_srgb_output() {
        let src = source();
        let spec = MetadataSpec::default();
        let value = |space| {
            let p = build_exif(&ctx(&spec, &src, space)).unwrap();
            tag_debug(&p.0)
        };
        assert!(
            value(ExportSpace::Srgb).contains("ColorSpace([1])"),
            "{}",
            value(ExportSpace::Srgb)
        );
        assert!(value(ExportSpace::DisplayP3).contains("65535"));
        assert!(value(ExportSpace::AdobeRgb).contains("65535"));
    }

    #[test]
    fn catalog_values_fill_in_when_exif_lacks_them() {
        let src = SourceMetadata {
            catalog_make: Some("Canon".into()),
            catalog_model: Some("R5".into()),
            catalog_captured_at: Some("2026-09-27 14:03:09".into()),
            ..SourceMetadata::default()
        };
        assert_eq!(src.make(), Some("Canon"));
        assert_eq!(
            src.date_time_original().as_deref(),
            Some("2026:09:27 14:03:09")
        );
        let payload = build_exif(&ctx(&MetadataSpec::default(), &src, ExportSpace::Srgb)).unwrap();
        assert!(tag_debug(&payload.0).contains("R5"));
    }

    #[test]
    fn source_exif_round_trips_through_a_written_jpeg() {
        let spec = MetadataSpec::default();
        let src = source();
        let payload = build_exif(&ctx(&spec, &src, ExportSpace::Srgb)).unwrap();
        let jpeg = write_exif_jpeg(&tiny_jpeg(), &payload).unwrap();
        let read = SourceExif::from_reader(&mut Cursor::new(jpeg));
        assert_eq!(read.make.as_deref(), Some("NIKON CORPORATION"));
        assert_eq!(read.lens_model.as_deref(), Some("NIKKOR Z 24-70mm f/2.8 S"));
        assert_eq!(read.exposure_time, Some((1, 200)));
        assert_eq!(read.f_number, Some((28, 10)));
        assert_eq!(read.focal_length, Some((50, 1)));
        assert_eq!(
            read.date_time_original.as_deref(),
            Some("2026:09:27 14:03:09")
        );
        // The exported file is upright, whatever the source said.
        assert_eq!(read.orientation, Orientation::Normal);
        // Garbage and missing files are the empty default, not an error.
        assert_eq!(
            SourceExif::from_reader(&mut Cursor::new(b"nope".to_vec())),
            SourceExif::default()
        );
        assert_eq!(
            SourceExif::from_path(Path::new("/definitely/not/here.nef")),
            SourceExif::default()
        );
    }

    #[test]
    fn xmp_all_policy_has_rating_label_keywords_and_escapes_everything() {
        let spec = MetadataSpec {
            policy: MetadataPolicy::All,
            include_keywords: true,
            artist: Some("A & B".into()),
            copyright: Some("<c>".into()),
        };
        let xmp = build_xmp(&spec, &source(), "Nicti").unwrap();
        assert!(xmp.starts_with("<?xpacket begin=") && xmp.ends_with("<?xpacket end=\"w\"?>"));
        assert!(
            xmp.contains("<xmp:Rating>4</xmp:Rating>")
                && xmp.contains("<xmp:Label>Red</xmp:Label>")
        );
        assert!(
            xmp.contains("A &amp; B")
                && xmp.contains("&lt;c&gt;")
                && xmp.contains("a&amp;b &lt;c&gt;")
        );
        assert!(!xmp.contains("a&b <c>"));
        // Keywords only when asked for.
        let no_kw = MetadataSpec {
            include_keywords: false,
            ..spec
        };
        assert!(!build_xmp(&no_kw, &source(), "Nicti")
            .unwrap()
            .contains("dc:subject"));
    }

    #[test]
    fn xml_escape_drops_forbidden_control_characters() {
        assert_eq!(xml_escape("a\u{0}b\u{7}c\td\n"), "abc\td\n");
        assert_eq!(xml_escape("\"'"), "&quot;&apos;");
    }

    #[test]
    fn xmp_round_trips_through_jpeg_and_png_and_replaces_rather_than_duplicates() {
        let xmp = wrap_xpacket("<x:xmpmeta>one</x:xmpmeta>");
        let jpeg = embed_xmp_jpeg(&tiny_jpeg(), &xmp).unwrap();
        assert_eq!(read_xmp_jpeg(&jpeg).unwrap().as_deref(), Some(xmp.as_str()));
        let again = embed_xmp_jpeg(&jpeg, &wrap_xpacket("<x:xmpmeta>two</x:xmpmeta>")).unwrap();
        let img = img_parts::jpeg::Jpeg::from_bytes(bytes::Bytes::from(again.clone())).unwrap();
        assert_eq!(
            img.segments()
                .iter()
                .filter(|s| s.contents().starts_with(XMP_SIGNATURE))
                .count(),
            1
        );
        assert!(read_xmp_jpeg(&again).unwrap().unwrap().contains("two"));

        let png = embed_xmp_png(&tiny_png(), &xmp).unwrap();
        assert_eq!(read_xmp_png(&png).unwrap().as_deref(), Some(xmp.as_str()));
        let png2 = embed_xmp_png(&png, &wrap_xpacket("<x:xmpmeta>two</x:xmpmeta>")).unwrap();
        assert!(read_xmp_png(&png2).unwrap().unwrap().contains("two"));
        assert!(image::load_from_memory(&png2).is_ok());
    }

    #[test]
    fn oversized_xmp_is_refused_and_a_truncated_itxt_chunk_does_not_panic() {
        let big = "x".repeat(70_000);
        assert!(matches!(
            embed_xmp_jpeg(&tiny_jpeg(), &big),
            Err(MetadataError::XmpTooLarge(_))
        ));

        let mut img = img_parts::png::Png::from_bytes(bytes::Bytes::from(tiny_png())).unwrap();
        img.chunks_mut().insert(
            1,
            img_parts::png::PngChunk::new(*b"iTXt", bytes::Bytes::from_static(PNG_XMP_KEYWORD)),
        );
        let truncated = img.encoder().bytes().to_vec();
        assert_eq!(read_xmp_png(&truncated).unwrap(), None);
    }

    #[test]
    fn exif_and_xmp_coexist_in_one_jpeg() {
        let spec = MetadataSpec::default();
        let src = source();
        let payload = build_exif(&ctx(&spec, &src, ExportSpace::Srgb)).unwrap();
        let with_xmp =
            embed_xmp_jpeg(&tiny_jpeg(), &build_xmp(&spec, &src, "Nicti").unwrap()).unwrap();
        let both = write_exif_jpeg(&with_xmp, &payload).unwrap();
        assert!(read_xmp_jpeg(&both).unwrap().is_some());
        assert!(tag_debug(&read_exif(&both, FileExtension::JPEG).unwrap()).contains("NIKON Z 8"));
    }
}
