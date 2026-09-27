//! ICC profile embedding for exported images. Only sRGB IEC61966-2.1 output is built this pass --
//! Display P3/AdobeRGB output profiles belong to #42's own color-management/soft-proofing scope
//! (`crates/nicti-color`'s doc comment), not this ticket. This module only settles *how* an ICC
//! profile gets embedded into each container format, using whatever profile bytes #42 eventually
//! hands it.
//!
//! Format support, matched against what `img-parts` (used for JPEG/PNG below) actually offers:
//! - **JPEG**: `img-parts::jpeg::Jpeg`'s `ImageICC` impl already splits a profile across multiple
//!   APP2 segments per the ICC-in-JPEG spec (`ICC_PROFILE\0` + sequence/count bytes), unlike
//!   `spikes/scent`'s single-segment XMP splice -- an sRGB v2 profile is ~3KB so this doesn't
//!   matter in practice, but a wider-gamut v4 profile can exceed one 64KB segment.
//! - **PNG**: `img-parts::png::Png`'s `ImageICC` impl writes an `iCCP` chunk (zlib-compressed,
//!   per the PNG spec -- `img-parts` handles the deflate itself).
//! - **TIFF**: deferred. Embedding tag 34675 needs building the TIFF IFD from scratch (the same
//!   class of gap ADR-0059 already flagged for TIFF/DNG XMP write) -- no crate in this workspace
//!   builds arbitrary TIFF tags today. Follow-up issue filed alongside this ADR.

use bytes::Bytes;
use img_parts::jpeg::Jpeg;
use img_parts::png::Png;
use img_parts::{ImageEXIF, ImageICC};
use moxcms::ColorProfile;

/// Encodes a fresh sRGB IEC61966-2.1 ICC profile. Built at runtime with `moxcms` rather than
/// vendoring a binary `.icc` file -- there is no ICC profile checked into this repo yet, and a
/// generated profile means no license/provenance question about a downloaded one.
pub fn srgb_icc_profile() -> anyhow::Result<Vec<u8>> {
    ColorProfile::new_srgb()
        .encode()
        .map_err(|e| anyhow::anyhow!("encoding sRGB ICC profile: {e:?}"))
}

/// Embeds `icc_bytes` into an in-memory JPEG buffer, returning the rewritten bytes.
pub fn embed_icc_jpeg(jpeg_bytes: &[u8], icc_bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut jpeg = Jpeg::from_bytes(Bytes::copy_from_slice(jpeg_bytes))
        .map_err(|e| anyhow::anyhow!("parsing JPEG for ICC embed: {e}"))?;
    jpeg.set_icc_profile(Some(Bytes::copy_from_slice(icc_bytes)));
    Ok(jpeg.encoder().bytes().to_vec())
}

/// Embeds `icc_bytes` into an in-memory PNG buffer (as an `iCCP` chunk), returning the rewritten
/// bytes.
pub fn embed_icc_png(png_bytes: &[u8], icc_bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut png = Png::from_bytes(Bytes::copy_from_slice(png_bytes))
        .map_err(|e| anyhow::anyhow!("parsing PNG for ICC embed: {e}"))?;
    png.set_icc_profile(Some(Bytes::copy_from_slice(icc_bytes)));
    Ok(png.encoder().bytes().to_vec())
}

/// Reads back an embedded ICC profile from a JPEG buffer, for round-trip verification.
pub fn read_icc_jpeg(jpeg_bytes: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
    let jpeg = Jpeg::from_bytes(Bytes::copy_from_slice(jpeg_bytes))
        .map_err(|e| anyhow::anyhow!("parsing JPEG for ICC read: {e}"))?;
    Ok(jpeg.icc_profile().map(|b| b.to_vec()))
}

/// Reads back an embedded ICC profile from a PNG buffer, for round-trip verification.
pub fn read_icc_png(png_bytes: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
    let png = Png::from_bytes(Bytes::copy_from_slice(png_bytes))
        .map_err(|e| anyhow::anyhow!("parsing PNG for ICC read: {e}"))?;
    Ok(png.icc_profile().map(|b| b.to_vec()))
}

/// `img-parts`' `ImageEXIF`/`ImageICC` traits are also how [`crate::metadata`] embeds EXIF bytes
/// (built by `little_exif`) into JPEG/PNG -- re-exported here so callers only need one import
/// path for "the img-parts container helpers".
pub fn embed_exif_jpeg(jpeg_bytes: &[u8], exif_bytes: Vec<u8>) -> anyhow::Result<Vec<u8>> {
    let mut jpeg = Jpeg::from_bytes(Bytes::copy_from_slice(jpeg_bytes))
        .map_err(|e| anyhow::anyhow!("parsing JPEG for EXIF embed: {e}"))?;
    jpeg.set_exif(Some(Bytes::from(exif_bytes)));
    Ok(jpeg.encoder().bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_jpeg() -> Vec<u8> {
        let img = image::RgbImage::from_pixel(8, 8, image::Rgb([120, 130, 140]));
        let mut buf = Vec::new();
        let mut cursor = std::io::Cursor::new(&mut buf);
        img.write_to(&mut cursor, image::ImageFormat::Jpeg).unwrap();
        buf
    }

    fn tiny_png() -> Vec<u8> {
        let img = image::RgbImage::from_pixel(8, 8, image::Rgb([10, 20, 30]));
        let mut buf = Vec::new();
        let mut cursor = std::io::Cursor::new(&mut buf);
        img.write_to(&mut cursor, image::ImageFormat::Png).unwrap();
        buf
    }

    #[test]
    fn srgb_profile_encodes_nonempty_bytes() {
        let profile = srgb_icc_profile().unwrap();
        assert!(
            profile.len() > 128,
            "expected a real ICC header, got {} bytes",
            profile.len()
        );
        // ICC profiles start with a 4-byte size field followed by "acsp" at offset 36.
        assert_eq!(&profile[36..40], b"acsp");
    }

    #[test]
    fn jpeg_icc_roundtrips() {
        let icc = srgb_icc_profile().unwrap();
        let jpeg = tiny_jpeg();
        let embedded = embed_icc_jpeg(&jpeg, &icc).unwrap();
        let read_back = read_icc_jpeg(&embedded)
            .unwrap()
            .expect("icc profile present");
        assert_eq!(read_back, icc);
    }

    #[test]
    fn png_icc_roundtrips() {
        let icc = srgb_icc_profile().unwrap();
        let png = tiny_png();
        let embedded = embed_icc_png(&png, &icc).unwrap();
        let read_back = read_icc_png(&embedded)
            .unwrap()
            .expect("icc profile present");
        assert_eq!(read_back, icc);
        // The rewritten PNG must still decode as a valid image, not just carry the chunk.
        image::load_from_memory_with_format(&embedded, image::ImageFormat::Png)
            .expect("PNG with iCCP chunk must still decode");
    }

    #[test]
    fn jpeg_without_icc_reads_back_none() {
        let jpeg = tiny_jpeg();
        assert_eq!(read_icc_jpeg(&jpeg).unwrap(), None);
    }
}
