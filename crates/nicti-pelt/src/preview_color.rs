//! JPEG-sourced preview pixels (grid thumbnails, T0/T2 loupe and survey/compare tiles) -> the
//! monitor's color space (#319, ADR-0042).
//!
//! These never enter the render graph, so `image::load_from_memory` + an sRGB egui texture used to
//! show them as if every JPEG were sRGB on an sRGB monitor. This decodes keeping the embedded ICC
//! profile and converts through `nicti_calico::source_transform::SourceTransforms`.

use std::io::Cursor;

use image::{DynamicImage, ImageDecoder, ImageReader, RgbaImage};
use nicti_calico::source_transform::SourceTransforms;

/// A decoded JPEG and the ICC profile it carried, if any.
pub struct DecodedJpeg {
    pub image: DynamicImage,
    pub icc: Option<Vec<u8>>,
}

/// Decodes `bytes`, keeping the embedded ICC profile. EXIF orientation is deliberately not applied
/// (previews never were: T0/T2 are stored upright).
pub fn decode(bytes: &[u8]) -> Result<DecodedJpeg, String> {
    let mut decoder = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?
        .into_decoder()
        .map_err(|e| e.to_string())?;
    // A profile that can't be read is the same as none: sRGB, the pre-#319 behaviour.
    let icc = decoder.icc_profile().ok().flatten();
    let image = DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    Ok(DecodedJpeg { image, icc })
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
