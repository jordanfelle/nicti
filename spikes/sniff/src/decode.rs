//! Decode + resize helpers for the two preview tiers Sniff benchmarks: a "grid" thumbnail
//! (~512px long edge) and a "screen" preview (~2560px long edge, matching a typical loupe view).

use fast_image_resize as fr;
use std::num::NonZeroU32;
use zune_jpeg::JpegDecoder;

pub const GRID_TIER_LONG_EDGE: u32 = 512;
pub const SCREEN_TIER_LONG_EDGE: u32 = 2560;

pub struct DecodedRgb {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

pub fn decode_jpeg(bytes: &[u8]) -> Result<DecodedRgb, String> {
    let mut decoder = JpegDecoder::new(bytes);
    let pixels = decoder.decode().map_err(|e| e.to_string())?;
    let info = decoder.info().ok_or("decoder produced no image info")?;
    Ok(DecodedRgb {
        width: info.width as u32,
        height: info.height as u32,
        rgb: pixels,
    })
}

/// Resizes so the long edge matches `target_long_edge`, preserving aspect ratio. No-op (clones)
/// if the source is already at or below the target.
pub fn resize_to_long_edge(src: &DecodedRgb, target_long_edge: u32) -> Result<DecodedRgb, String> {
    let long_edge = src.width.max(src.height);
    if long_edge <= target_long_edge {
        return Ok(DecodedRgb {
            width: src.width,
            height: src.height,
            rgb: src.rgb.clone(),
        });
    }
    let scale = target_long_edge as f64 / long_edge as f64;
    let dst_w = ((src.width as f64 * scale).round() as u32).max(1);
    let dst_h = ((src.height as f64 * scale).round() as u32).max(1);

    let src_w = NonZeroU32::new(src.width).ok_or("zero width")?;
    let src_h = NonZeroU32::new(src.height).ok_or("zero height")?;
    let src_image = fr::images::Image::from_vec_u8(
        src_w.get(),
        src_h.get(),
        src.rgb.clone(),
        fr::PixelType::U8x3,
    )
    .map_err(|e| e.to_string())?;

    let mut dst_image = fr::images::Image::new(dst_w, dst_h, fr::PixelType::U8x3);
    let mut resizer = fr::Resizer::new();
    resizer
        .resize(&src_image, &mut dst_image, None)
        .map_err(|e| e.to_string())?;

    Ok(DecodedRgb {
        width: dst_w,
        height: dst_h,
        rgb: dst_image.into_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real 8x8 solid-color baseline JPEG, pre-generated (not produced by this repo's own
    /// tooling, to keep the decode test independent of any encoder we might add later).
    const TINY_JPEG: &[u8] = include_bytes!("../tests/fixtures/tiny_gray_8x8.jpg");

    #[test]
    fn decodes_tiny_fixture() {
        let decoded = decode_jpeg(TINY_JPEG).expect("decode");
        assert_eq!(decoded.width, 8);
        assert_eq!(decoded.height, 8);
        assert_eq!(decoded.rgb.len(), 8 * 8 * 3);
    }

    #[test]
    fn resize_no_op_when_already_small() {
        let decoded = DecodedRgb {
            width: 8,
            height: 8,
            rgb: vec![128u8; 8 * 8 * 3],
        };
        let out = resize_to_long_edge(&decoded, 512).expect("resize");
        assert_eq!((out.width, out.height), (8, 8));
    }
}
