//! JPEG-sourced preview pixels (grid thumbnails, T0/T2 loupe and survey/compare tiles) -> the
//! monitor's color space (#319, ADR-0042).
//!
//! These never enter the render graph, so `image::load_from_memory` + an sRGB egui texture used to
//! show them as if every JPEG were sRGB on an sRGB monitor. This decodes keeping the embedded ICC
//! profile and converts through `nicti_calico::source_transform::SourceTransforms`.

use std::io::Cursor;

use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageReader, RgbaImage};
use nicti_calico::source_transform::SourceTransforms;

/// A decoded JPEG and the ICC profile it carried, if any.
pub struct DecodedJpeg {
    pub image: DynamicImage,
    pub icc: Option<Vec<u8>>,
}

/// Decodes `bytes`, keeping the embedded ICC profile. A JPEG's EXIF orientation is applied: T2 is
/// stored upright (no tag), and ingest tags a T0 with its RAW container's IFD0 rotation (#476).
pub fn decode(bytes: &[u8]) -> Result<DecodedJpeg, String> {
    if is_avif(bytes) {
        return decode_avif(bytes);
    }
    let mut decoder = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?
        .into_decoder()
        .map_err(|e| e.to_string())?;
    // A profile that can't be read is the same as none: sRGB, the pre-#319 behaviour.
    let icc = decoder.icc_profile().ok().flatten();
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut image = DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    image.apply_orientation(orientation);
    Ok(DecodedJpeg { image, icc })
}

/// ISOBMFF `ftyp` box with an `avif` major brand: the `.thumb.avif` sidecars (#393).
fn is_avif(b: &[u8]) -> bool {
    b.len() > 12 && &b[4..12] == b"ftypavif"
}

/// Only untagged T0s are ever AVIF-encoded (`thumb_sidecar::export` keeps ICC-tagged ones JPEG),
/// so the result carries no profile and is treated as sRGB like any untagged JPEG. The decode is
/// panic-guarded in lair.
fn decode_avif(bytes: &[u8]) -> Result<DecodedJpeg, String> {
    let rgb = nicti_lair::thumb_sidecar::decode_avif_rgb8(bytes).ok_or("AVIF doesn't decode")?;
    Ok(DecodedJpeg {
        image: DynamicImage::ImageRgb8(rgb),
        icc: None,
    })
}

/// `decoded`'s pixels as 8-bit RGBA converted to the display `color` was built for.
pub fn to_display_rgba(decoded: DecodedJpeg, color: &SourceTransforms) -> RgbaImage {
    let DecodedJpeg { image, icc } = decoded;
    let mut rgba = image.into_rgba8();
    color.convert_rgba8(icc.as_deref(), &mut rgba);
    rgba
}

/// Decodes `bytes` and converts to the display in one go (no resize).
pub fn decode_to_display(bytes: &[u8], color: &SourceTransforms) -> Result<RgbaImage, String> {
    Ok(to_display_rgba(decode(bytes)?, color))
}

#[cfg(test)]
pub(crate) mod testutil {
    use image::codecs::jpeg::JpegEncoder;
    use image::ImageEncoder;

    /// A solid-colour JPEG of the given size, tagged with `icc` when given.
    pub fn tagged_jpeg(width: u32, height: u32, rgb: [u8; 3], icc: Option<&[u8]>) -> Vec<u8> {
        let mut buf = Vec::new();
        let pixels: Vec<u8> = (0..width * height).flat_map(|_| rgb).collect();
        let mut enc = JpegEncoder::new_with_quality(&mut buf, 95);
        if let Some(icc) = icc {
            enc.set_icc_profile(icc.to_vec()).unwrap();
        }
        enc.write_image(&pixels, width, height, image::ExtendedColorType::Rgb8)
            .unwrap();
        buf
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::tagged_jpeg;
    use super::*;
    use nicti_calico::icc::profile_bytes;
    use nicti_calico::space::OutputSpace;
    use nicti_calico::transform::DisplayProfile;

    fn srgb_display() -> SourceTransforms {
        SourceTransforms::new(&DisplayProfile::Space(OutputSpace::Srgb))
    }

    #[test]
    fn a_jpegs_exif_orientation_is_applied_on_decode() {
        let jpeg = tagged_jpeg(64, 32, [9, 9, 9], None);
        let mut tiff = vec![b'I', b'I', 42, 0, 8, 0, 0, 0, 1, 0];
        tiff.extend_from_slice(&0x0112u16.to_le_bytes());
        tiff.extend_from_slice(&3u16.to_le_bytes());
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&6u32.to_le_bytes());
        tiff.extend_from_slice(&0u32.to_le_bytes());
        let mut seg = b"Exif\0\0".to_vec();
        seg.extend_from_slice(&tiff);
        let mut tagged = jpeg[..2].to_vec();
        tagged.extend_from_slice(&[0xFF, 0xE1]);
        tagged.extend_from_slice(&((seg.len() + 2) as u16).to_be_bytes());
        tagged.extend_from_slice(&seg);
        tagged.extend_from_slice(&jpeg[2..]);
        let plain = decode(&jpeg).unwrap().image;
        assert_eq!((plain.width(), plain.height()), (64, 32));
        let rotated = decode(&tagged).unwrap().image;
        assert_eq!((rotated.width(), rotated.height()), (32, 64));
    }

    /// The `.thumb.avif` sidecar a real archive move writes decodes back to about the same pixels.
    #[test]
    fn decodes_an_avif_sidecar_like_the_jpeg_it_came_from() {
        let jpeg = tagged_jpeg(64, 48, [200, 40, 40], None);
        let d = tempfile::tempdir().unwrap();
        let raw = d.path().join("a.NEF");
        let pv = nicti_lair::Preview {
            width: Some(64),
            height: Some(48),
            bytes: jpeg.clone(),
        };
        nicti_lair::thumb_sidecar::export(&raw, &pv, nicti_lair::thumb_sidecar::SidecarCodec::Avif)
            .unwrap();
        let avif = std::fs::read(nicti_lair::thumb_sidecar::sidecar_path(
            &raw,
            nicti_lair::thumb_sidecar::SidecarCodec::Avif,
        ))
        .unwrap();
        let got = decode_to_display(&avif, &srgb_display()).unwrap();
        let want = decode_to_display(&jpeg, &srgb_display()).unwrap();
        assert_eq!(got.dimensions(), want.dimensions());
        let (g, w) = (got.get_pixel(32, 24), want.get_pixel(32, 24));
        for i in 0..3 {
            assert!((g[i] as i32 - w[i] as i32).abs() < 8, "{g:?} vs {w:?}");
        }
        assert!(decode(b"\0\0\0\x1cftypavif garbage").is_err());
    }

    #[test]
    fn the_embedded_profile_survives_decoding() {
        let p3 = profile_bytes(OutputSpace::DisplayP3).unwrap();
        let d = decode(&tagged_jpeg(8, 8, [200, 120, 60], Some(&p3))).unwrap();
        assert_eq!(d.icc.as_deref(), Some(p3.as_slice()));
        assert!(decode(&tagged_jpeg(8, 8, [1, 2, 3], None))
            .unwrap()
            .icc
            .is_none());
    }

    #[test]
    fn an_untagged_jpeg_on_an_srgb_display_is_untouched() {
        let jpeg = tagged_jpeg(8, 8, [200, 120, 60], None);
        let plain = image::load_from_memory(&jpeg).unwrap().to_rgba8();
        assert_eq!(decode_to_display(&jpeg, &srgb_display()).unwrap(), plain);
    }

    #[test]
    fn a_p3_jpeg_is_converted_for_an_srgb_display() {
        let p3 = profile_bytes(OutputSpace::DisplayP3).unwrap();
        let jpeg = tagged_jpeg(8, 8, [200, 120, 60], Some(&p3));
        let raw = image::load_from_memory(&jpeg).unwrap().to_rgba8();
        let out = decode_to_display(&jpeg, &srgb_display()).unwrap();
        assert_ne!(out.get_pixel(3, 3), raw.get_pixel(3, 3));
    }
}
