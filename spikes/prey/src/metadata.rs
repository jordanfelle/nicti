//! Metadata write for exported images: EXIF (via `little_exif`), XMP (a hand-rolled segment/chunk
//! insert, generalizing `spikes/scent`'s JPEG APP1 approach -- copied and adapted, not depended
//! on, since spikes don't depend on each other in this repo), and ICC (via [`crate::icc`],
//! `img-parts`). TIFF write for all three is deferred -- see [`crate::icc`]'s module doc for why.
//!
//! `little_exif` was chosen over `img-parts`' own EXIF support because `img-parts` only stores
//! and replaces a raw EXIF byte blob -- it doesn't build the TIFF-structured IFD itself.
//! `little_exif::metadata::Metadata` builds that IFD from typed tags directly, so it's still the
//! encoder feeding `img-parts::ImageEXIF::set_exif` under the hood
//! ([`write_exif_jpeg`]/[`write_exif_png`] use `little_exif`'s own end-to-end `write_to_vec` for
//! JPEG/PNG, which is simpler than round-tripping through img-parts for the same result -- see
//! `docs/research/prey-export-stack.md` for the two paths measured against each other).

use little_exif::exif_tag::ExifTag;
use little_exif::filetype::FileExtension;
use little_exif::metadata::Metadata;
use little_exif::rational::uR64;

/// The EXIF export subset ADR-0056's decision rule requires. GPS is deferred this pass (needs a
/// GPSInfo sub-IFD little_exif exposes via a separate tag group not exercised here) -- see the
/// ADR's "what wasn't reachable" section.
#[derive(Debug, Clone, Default)]
pub struct ExportMetadata {
    pub make: Option<String>,
    pub model: Option<String>,
    pub lens_model: Option<String>,
    /// (numerator, denominator), e.g. (1, 200) for 1/200s.
    pub exposure_time: Option<(u32, u32)>,
    /// (numerator, denominator), e.g. (56, 10) for f/5.6.
    pub f_number: Option<(u32, u32)>,
    pub iso: Option<u16>,
    /// `"YYYY:MM:DD HH:MM:SS"`, EXIF's own datetime format.
    pub date_time_original: Option<String>,
    /// `"+HH:MM"` / `"-HH:MM"`.
    pub offset_time_original: Option<String>,
    pub artist: Option<String>,
    pub copyright: Option<String>,
    pub software: String,
    /// The exported image's own pixel dimensions -- required for `ExifImageWidth`/
    /// `ExifImageHeight` (0xa002/0xa003). Unlike the generic `ImageWidth`/`ImageHeight` (TIFF)
    /// tags, these EXIF-IFD copies are mandatory per the EXIF spec and some readers validate
    /// against them (`exiftool -validate` flags their absence).
    pub width: u32,
    pub height: u32,
}

/// EXIF version this spike declares (`ExifVersion` 0x9000 is a mandatory ExifIFD tag). 2.31 is
/// the version most current EXIF-aware tooling (including `little_exif` itself) targets.
const EXIF_VERSION: &[u8; 4] = b"0231";

fn build_exif(meta: &ExportMetadata) -> Metadata {
    let mut exif = Metadata::new();
    // Mandatory ExifIFD tags `exiftool -validate` checks for on any file that has an ExifIFD at
    // all (which writing DateTimeOriginal/ExposureTime/etc. below implies) -- found missing on
    // this spike's own first real pipeline output, not from reading the spec cover to cover.
    exif.set_tag(ExifTag::ExifVersion(EXIF_VERSION.to_vec()));
    exif.set_tag(ExifTag::ComponentsConfiguration(vec![1, 2, 3, 0])); // YCbCr, no alpha
    exif.set_tag(ExifTag::ColorSpace(vec![1])); // sRGB
    exif.set_tag(ExifTag::ExifImageWidth(vec![meta.width]));
    exif.set_tag(ExifTag::ExifImageHeight(vec![meta.height]));
    exif.set_tag(ExifTag::YCbCrPositioning(vec![1])); // centered
    if let Some(make) = &meta.make {
        exif.set_tag(ExifTag::Make(make.clone()));
    }
    if let Some(model) = &meta.model {
        exif.set_tag(ExifTag::Model(model.clone()));
    }
    if let Some(lens) = &meta.lens_model {
        exif.set_tag(ExifTag::LensModel(lens.clone()));
    }
    if let Some((n, d)) = meta.exposure_time {
        exif.set_tag(ExifTag::ExposureTime(vec![uR64 {
            nominator: n,
            denominator: d,
        }]));
    }
    if let Some((n, d)) = meta.f_number {
        exif.set_tag(ExifTag::FNumber(vec![uR64 {
            nominator: n,
            denominator: d,
        }]));
    }
    if let Some(iso) = meta.iso {
        exif.set_tag(ExifTag::ISO(vec![iso]));
    }
    if let Some(dto) = &meta.date_time_original {
        exif.set_tag(ExifTag::DateTimeOriginal(dto.clone()));
    }
    if let Some(offset) = &meta.offset_time_original {
        exif.set_tag(ExifTag::OffsetTimeOriginal(offset.clone()));
    }
    if let Some(artist) = &meta.artist {
        exif.set_tag(ExifTag::Artist(artist.clone()));
    }
    if let Some(copyright) = &meta.copyright {
        exif.set_tag(ExifTag::Copyright(copyright.clone()));
    }
    exif.set_tag(ExifTag::Software(meta.software.clone()));
    // Export always resets orientation to 1: the render already applied any rotation, so a
    // downstream viewer must not apply it again.
    exif.set_tag(ExifTag::Orientation(vec![1]));
    exif
}

/// Writes `meta` into an in-memory JPEG buffer, returning the rewritten bytes.
pub fn write_exif_jpeg(jpeg_bytes: &[u8], meta: &ExportMetadata) -> anyhow::Result<Vec<u8>> {
    let exif = build_exif(meta);
    let mut buf = jpeg_bytes.to_vec();
    exif.write_to_vec(&mut buf, FileExtension::JPEG)
        .map_err(|e| anyhow::anyhow!("writing EXIF into JPEG: {e}"))?;
    Ok(buf)
}

/// Writes `meta` into an in-memory PNG buffer (an `eXIf` chunk), returning the rewritten bytes.
pub fn write_exif_png(png_bytes: &[u8], meta: &ExportMetadata) -> anyhow::Result<Vec<u8>> {
    let exif = build_exif(meta);
    let mut buf = png_bytes.to_vec();
    exif.write_to_vec(
        &mut buf,
        FileExtension::PNG {
            as_zTXt_chunk: false,
        },
    )
    .map_err(|e| anyhow::anyhow!("writing EXIF into PNG: {e}"))?;
    Ok(buf)
}

/// Reads back EXIF tags from an in-memory JPEG/PNG buffer, for round-trip verification.
pub fn read_exif(bytes: &[u8], file_type: FileExtension) -> anyhow::Result<Metadata> {
    Metadata::new_from_vec(&bytes.to_vec(), file_type)
        .map_err(|e| anyhow::anyhow!("reading EXIF back: {e}"))
}

// --- XMP embedding -----------------------------------------------------------------------
//
// Adapted from `spikes/scent/src/embedded.rs`'s JPEG APP1 approach: same signature constant,
// same single-segment 64KB limit (an export XMP packet -- rating/label/keywords/copyright,
// nothing per-pixel -- is a few hundred bytes, nowhere near that limit, unlike scent's own
// concern about `crs:` mask data). Built on `img-parts`'s generic segment API instead of
// scent's own hand-rolled byte walker, since img-parts is already a dependency here for
// ICC/EXIF and exposes exactly the segment insert/remove primitives needed.

const XMP_SIGNATURE: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
const APP1: u8 = 0xE1;
/// Segment length field is a u16 including itself -- max payload is 65535 - 2 = 65533 bytes.
const MAX_SEGMENT_PAYLOAD: usize = 65533;

/// Wraps an `<x:xmpmeta>...</x:xmpmeta>` document in the standard xpacket processing-instruction
/// pair every XMP-aware reader expects -- `exiftool -validate` flags a packet missing this as a
/// minor warning, caught on this spike's own first real pipeline output, not from the spec.
/// [`embed_xmp_jpeg`]/[`embed_xmp_png`] embed exactly the string they're given (matching
/// `spikes/scent`'s own low-level primitive); callers building a real packet from scratch should
/// wrap it with this first. The `id` is Adobe XMP Toolkit's own well-known packet-wrapper UUID,
/// used by convention (readers key off the xpacket PIs, not this string) rather than because it
/// carries meaning here.
pub fn wrap_xpacket(xmp_packet: &str) -> String {
    format!("<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n{xmp_packet}\n<?xpacket end=\"w\"?>")
}

/// Embeds an XMP packet into an in-memory JPEG buffer's APP1 segment, replacing any existing XMP
/// APP1 segment (an EXIF APP1 segment, signature `Exif\0\0`, is untouched -- JPEG allows both).
pub fn embed_xmp_jpeg(jpeg_bytes: &[u8], xmp_packet: &str) -> anyhow::Result<Vec<u8>> {
    if XMP_SIGNATURE.len() + xmp_packet.len() > MAX_SEGMENT_PAYLOAD {
        anyhow::bail!(
            "xmp packet ({} bytes) plus signature exceeds the {MAX_SEGMENT_PAYLOAD}-byte APP1 \
             segment limit -- would need JPEG's multi-segment extension, not implemented here",
            xmp_packet.len()
        );
    }

    let mut jpeg = img_parts::jpeg::Jpeg::from_bytes(bytes::Bytes::copy_from_slice(jpeg_bytes))
        .map_err(|e| anyhow::anyhow!("parsing JPEG for XMP embed: {e}"))?;

    jpeg.segments_mut().retain(|segment| {
        !(segment.marker() == APP1 && segment.contents().starts_with(XMP_SIGNATURE))
    });

    let mut contents = Vec::with_capacity(XMP_SIGNATURE.len() + xmp_packet.len());
    contents.extend_from_slice(XMP_SIGNATURE);
    contents.extend_from_slice(xmp_packet.as_bytes());
    let segment =
        img_parts::jpeg::JpegSegment::new_with_contents(APP1, bytes::Bytes::from(contents));

    // Insert right after SOI (index 1) -- order among APPn segments doesn't matter to a reader.
    // Corrected 2026-09-27 (adversarial review): this does NOT match img-parts' own EXIF/ICC
    // insert position, despite an earlier version of this comment claiming so -- img-parts'
    // `Jpeg::set_exif`/`set_icc_profile` both insert at index 3, not 1 (confirmed by reading
    // img-parts 0.4.0's own `src/jpeg/image.rs`). This function is never combined with
    // `crate::icc`'s img-parts-based EXIF/ICC embed in this spike's own pipeline (which uses
    // `little_exif`/`jpeg-encoder`'s native embedding instead), so the mismatch has no observed
    // effect, but a future caller mixing both should know the two don't share an insert-order
    // convention.
    jpeg.segments_mut().insert(1, segment);

    Ok(jpeg.encoder().bytes().to_vec())
}

/// Reads back an embedded XMP packet from a JPEG buffer, for round-trip verification.
pub fn read_xmp_jpeg(jpeg_bytes: &[u8]) -> anyhow::Result<Option<String>> {
    let jpeg = img_parts::jpeg::Jpeg::from_bytes(bytes::Bytes::copy_from_slice(jpeg_bytes))
        .map_err(|e| anyhow::anyhow!("parsing JPEG for XMP read: {e}"))?;
    for segment in jpeg.segments() {
        if segment.marker() == APP1 && segment.contents().starts_with(XMP_SIGNATURE) {
            let payload = &segment.contents()[XMP_SIGNATURE.len()..];
            return Ok(Some(String::from_utf8_lossy(payload).into_owned()));
        }
    }
    Ok(None)
}

/// Embeds an XMP packet into an in-memory PNG buffer as an uncompressed `iTXt` chunk with
/// keyword `XML:com.adobe.xmp` (the Adobe/PNG-community convention every XMP reader recognizes),
/// replacing any existing one.
pub fn embed_xmp_png(png_bytes: &[u8], xmp_packet: &str) -> anyhow::Result<Vec<u8>> {
    const KEYWORD: &[u8] = b"XML:com.adobe.xmp";

    let mut png = img_parts::png::Png::from_bytes(bytes::Bytes::copy_from_slice(png_bytes))
        .map_err(|e| anyhow::anyhow!("parsing PNG for XMP embed: {e}"))?;

    png.chunks_mut()
        .retain(|chunk| !(chunk.kind() == *b"iTXt" && chunk.contents().starts_with(KEYWORD)));

    // iTXt layout (PNG spec 11.3.4.4): keyword \0 compression-flag compression-method
    // language-tag \0 translated-keyword \0 text. XMP convention: uncompressed, no language, no
    // translated keyword.
    let mut contents = Vec::with_capacity(KEYWORD.len() + 5 + xmp_packet.len());
    contents.extend_from_slice(KEYWORD);
    contents.push(0); // null terminator after keyword
    contents.push(0); // compression flag: uncompressed
    contents.push(0); // compression method: unused when uncompressed
    contents.push(0); // null-terminated (empty) language tag
    contents.push(0); // null-terminated (empty) translated keyword
    contents.extend_from_slice(xmp_packet.as_bytes());

    let chunk = img_parts::png::PngChunk::new(*b"iTXt", bytes::Bytes::from(contents));

    // Insert right after IHDR (index 1) -- ahead of IDAT, matching where img-parts inserts
    // iCCP/eXIf.
    png.chunks_mut().insert(1, chunk);

    Ok(png.encoder().bytes().to_vec())
}

/// Reads back an embedded XMP packet from a PNG buffer, for round-trip verification.
pub fn read_xmp_png(png_bytes: &[u8]) -> anyhow::Result<Option<String>> {
    const KEYWORD: &[u8] = b"XML:com.adobe.xmp";

    let png = img_parts::png::Png::from_bytes(bytes::Bytes::copy_from_slice(png_bytes))
        .map_err(|e| anyhow::anyhow!("parsing PNG for XMP read: {e}"))?;
    for chunk in png.chunks() {
        if chunk.kind() == *b"iTXt" && chunk.contents().starts_with(KEYWORD) {
            // A well-formed chunk has at least a null terminator after the keyword plus the
            // 2-byte compression-flag/method pair; a chunk that's exactly `KEYWORD` bytes long
            // (no terminator, truncated/malformed) must not panic on the slice below --
            // regression test for a real adversarial-review finding (out-of-bounds slice).
            if chunk.contents().len() <= KEYWORD.len() + 1 {
                continue;
            }
            // Skip keyword\0 compression-flag compression-method lang\0 translated-keyword\0.
            let after_keyword = &chunk.contents()[KEYWORD.len() + 1..];
            if after_keyword.len() < 2 {
                continue;
            }
            let text_start = after_keyword
                .iter()
                .skip(2) // compression flag + method
                .position(|&b| b == 0) // end of (empty) language tag
                .and_then(|lang_end| {
                    after_keyword.get(2 + lang_end + 1..).and_then(|rest| {
                        rest.iter()
                            .position(|&b| b == 0) // end of (empty) translated keyword
                            .map(|tk_end| 2 + lang_end + 1 + tk_end + 1)
                    })
                })
                .unwrap_or(after_keyword.len());
            let text = &after_keyword[text_start.min(after_keyword.len())..];
            return Ok(Some(String::from_utf8_lossy(text).into_owned()));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_jpeg() -> Vec<u8> {
        let img = image::RgbImage::from_pixel(8, 8, image::Rgb([120, 130, 140]));
        let mut buf = Vec::new();
        img.write_to(
            &mut std::io::Cursor::new(&mut buf),
            image::ImageFormat::Jpeg,
        )
        .unwrap();
        buf
    }

    fn tiny_png() -> Vec<u8> {
        let img = image::RgbImage::from_pixel(8, 8, image::Rgb([10, 20, 30]));
        let mut buf = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        buf
    }

    fn sample_meta() -> ExportMetadata {
        ExportMetadata {
            make: Some("NIKON CORPORATION".into()),
            model: Some("NIKON Z8".into()),
            lens_model: Some("NIKKOR Z 24-70mm f/2.8 S".into()),
            exposure_time: Some((1, 200)),
            f_number: Some((56, 10)),
            iso: Some(400),
            date_time_original: Some("2026:09:27 12:00:00".into()),
            offset_time_original: Some("-04:00".into()),
            artist: Some("Jordan".into()),
            copyright: Some("(c) 2026 Jordan".into()),
            software: "Nicti (spikes/prey)".into(),
            width: 8,
            height: 8,
        }
    }

    #[test]
    fn jpeg_exif_roundtrips_and_stays_decodable() {
        let jpeg = tiny_jpeg();
        let written = write_exif_jpeg(&jpeg, &sample_meta()).unwrap();
        image::load_from_memory_with_format(&written, image::ImageFormat::Jpeg)
            .expect("JPEG with EXIF must still decode");

        let read_back = read_exif(&written, FileExtension::JPEG).unwrap();
        assert_eq!(
            read_back.get_tag(&ExifTag::Make(String::new())).next(),
            Some(&ExifTag::Make("NIKON CORPORATION".into()))
        );
        assert_eq!(
            read_back.get_tag(&ExifTag::Model(String::new())).next(),
            Some(&ExifTag::Model("NIKON Z8".into()))
        );
        assert_eq!(
            read_back.get_tag(&ExifTag::Orientation(vec![])).next(),
            Some(&ExifTag::Orientation(vec![1]))
        );
        // The mandatory ExifIFD/IFD0 tags exiftool -validate flagged as missing on this spike's
        // own first real pipeline output (regression test for that finding).
        assert_eq!(
            read_back.get_tag(&ExifTag::ExifVersion(vec![])).next(),
            Some(&ExifTag::ExifVersion(EXIF_VERSION.to_vec()))
        );
        assert_eq!(
            read_back.get_tag(&ExifTag::ColorSpace(vec![])).next(),
            Some(&ExifTag::ColorSpace(vec![1]))
        );
        assert_eq!(
            read_back.get_tag(&ExifTag::ExifImageWidth(vec![])).next(),
            Some(&ExifTag::ExifImageWidth(vec![8]))
        );
        assert_eq!(
            read_back.get_tag(&ExifTag::ExifImageHeight(vec![])).next(),
            Some(&ExifTag::ExifImageHeight(vec![8]))
        );
        assert_eq!(
            read_back.get_tag(&ExifTag::YCbCrPositioning(vec![])).next(),
            Some(&ExifTag::YCbCrPositioning(vec![1]))
        );
    }

    #[test]
    fn wrap_xpacket_adds_begin_and_end_processing_instructions() {
        let wrapped = wrap_xpacket("<x:xmpmeta/>");
        assert!(wrapped.starts_with("<?xpacket begin="));
        assert!(wrapped.trim_end().ends_with("<?xpacket end=\"w\"?>"));
        assert!(wrapped.contains("<x:xmpmeta/>"));
    }

    #[test]
    fn wrapped_xmp_roundtrips_through_jpeg_embed() {
        let jpeg = tiny_jpeg();
        let wrapped = wrap_xpacket("<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"/>");
        let written = embed_xmp_jpeg(&jpeg, &wrapped).unwrap();
        let read_back = read_xmp_jpeg(&written).unwrap().unwrap();
        assert_eq!(read_back, wrapped);
    }

    #[test]
    fn png_exif_roundtrips_and_stays_decodable() {
        let png = tiny_png();
        let written = write_exif_png(&png, &sample_meta()).unwrap();
        image::load_from_memory_with_format(&written, image::ImageFormat::Png)
            .expect("PNG with EXIF must still decode");

        let read_back = read_exif(
            &written,
            FileExtension::PNG {
                as_zTXt_chunk: false,
            },
        )
        .unwrap();
        assert_eq!(
            read_back.get_tag(&ExifTag::Model(String::new())).next(),
            Some(&ExifTag::Model("NIKON Z8".into()))
        );
    }

    #[test]
    fn jpeg_xmp_roundtrips_and_stays_decodable() {
        let jpeg = tiny_jpeg();
        let xmp = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description xmp:Rating="5" xmlns:xmp="http://ns.adobe.com/xap/1.0/"/></rdf:RDF></x:xmpmeta>"#;
        let written = embed_xmp_jpeg(&jpeg, xmp).unwrap();
        image::load_from_memory_with_format(&written, image::ImageFormat::Jpeg)
            .expect("JPEG with XMP must still decode");
        let read_back = read_xmp_jpeg(&written).unwrap().expect("xmp present");
        assert_eq!(read_back, xmp);
    }

    #[test]
    fn jpeg_xmp_embed_replaces_existing() {
        let jpeg = tiny_jpeg();
        let first = embed_xmp_jpeg(&jpeg, "first").unwrap();
        let second = embed_xmp_jpeg(&first, "second").unwrap();
        assert_eq!(read_xmp_jpeg(&second).unwrap().unwrap(), "second");
    }

    #[test]
    fn jpeg_xmp_and_exif_coexist() {
        let jpeg = tiny_jpeg();
        let with_exif = write_exif_jpeg(&jpeg, &sample_meta()).unwrap();
        let with_both = embed_xmp_jpeg(&with_exif, "<xmp/>").unwrap();
        image::load_from_memory_with_format(&with_both, image::ImageFormat::Jpeg)
            .expect("JPEG with both EXIF and XMP must still decode");
        assert_eq!(read_xmp_jpeg(&with_both).unwrap().unwrap(), "<xmp/>");
        let exif = read_exif(&with_both, FileExtension::JPEG).unwrap();
        assert_eq!(
            exif.get_tag(&ExifTag::Model(String::new())).next(),
            Some(&ExifTag::Model("NIKON Z8".into()))
        );
    }

    #[test]
    fn jpeg_xmp_over_segment_limit_is_rejected() {
        let jpeg = tiny_jpeg();
        let huge = "x".repeat(MAX_SEGMENT_PAYLOAD);
        let result = embed_xmp_jpeg(&jpeg, &huge);
        assert!(result.is_err());
    }

    #[test]
    fn png_xmp_roundtrips_and_stays_decodable() {
        let png = tiny_png();
        let xmp = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"/>"#;
        let written = embed_xmp_png(&png, xmp).unwrap();
        image::load_from_memory_with_format(&written, image::ImageFormat::Png)
            .expect("PNG with XMP must still decode");
        let read_back = read_xmp_png(&written).unwrap().expect("xmp present");
        assert_eq!(read_back, xmp);
    }

    #[test]
    fn png_xmp_embed_replaces_existing() {
        let png = tiny_png();
        let first = embed_xmp_png(&png, "first").unwrap();
        let second = embed_xmp_png(&first, "second").unwrap();
        assert_eq!(read_xmp_png(&second).unwrap().unwrap(), "second");
    }

    #[test]
    fn read_xmp_png_does_not_panic_on_truncated_itxt_chunk() {
        // Regression test for a real adversarial-review finding: an iTXt chunk whose contents
        // are exactly the XMP keyword bytes (no null terminator, no compression-flag/method
        // pair -- a plausible truncated/malformed PNG, not just adversarial input) used to panic
        // on an out-of-bounds slice in read_xmp_png. Build one directly via img-parts rather than
        // hand-writing PNG bytes.
        let mut png =
            img_parts::png::Png::from_bytes(bytes::Bytes::copy_from_slice(&tiny_png())).unwrap();
        let truncated = img_parts::png::PngChunk::new(
            *b"iTXt",
            bytes::Bytes::from_static(b"XML:com.adobe.xmp"), // exactly KEYWORD, nothing after it
        );
        png.chunks_mut().insert(1, truncated);
        let malformed = png.encoder().bytes().to_vec();

        // Must not panic, and there's no valid XMP text to extract from a chunk this short.
        assert_eq!(read_xmp_png(&malformed).unwrap(), None);
    }
}
