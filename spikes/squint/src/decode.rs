//! JPEG decode, copied from `spikes/litter/src/decode.rs` (itself adapted from
//! `spikes/sniff/src/decode.rs`, #28/#29) -- returns an `image::RgbImage` directly since every
//! downstream consumer here (`sharp`, `synth`, `label`) already speaks the `image` crate's types.

use image::RgbImage;
use zune_jpeg::zune_core::bytestream::ZCursor;
use zune_jpeg::JpegDecoder;

pub fn decode_jpeg(bytes: &[u8]) -> Result<RgbImage, String> {
    let mut decoder = JpegDecoder::new(ZCursor::new(bytes));
    let pixels = decoder.decode().map_err(|e| e.to_string())?;
    let info = decoder.info().ok_or("decoder produced no image info")?;
    RgbImage::from_raw(info.width as u32, info.height as u32, pixels)
        .ok_or_else(|| "decoded pixel buffer did not match declared dimensions".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TINY_JPEG: &[u8] = include_bytes!("../tests/fixtures/tiny_gray_8x8.jpg");

    #[test]
    fn decodes_tiny_fixture() {
        let img = decode_jpeg(TINY_JPEG).expect("decode");
        assert_eq!(img.dimensions(), (8, 8));
    }
}
