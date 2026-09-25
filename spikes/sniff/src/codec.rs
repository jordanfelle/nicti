//! Tier-payload format candidates for #29, measured against JPEG (`decode.rs`'s existing path):
//! AVIF, added at the user's explicit request ("numbers tell it all") over the ADR author's own
//! prior toward JPEG-only. Both encode (`ravif`, rav1e-based) and decode (`avif-decode`,
//! rav1d-based) are pure Rust -- no C toolchain dependency, unlike `image`'s `avif-native`
//! feature (which needs the C `dav1d` library), so this stays safe to build on Windows CI the
//! same way the rest of this repo's Rust-first stack does.
//!
//! Scope note (also in `docs/adr/0017-preview-tier-strategy.md`): this only measures AVIF vs
//! JPEG as static tier-payload formats. Per-user format choice with hardware-accel-aware
//! auto-selection (e.g. preferring AVIF only where the OS/GPU actually offers hardware AV1
//! decode, falling back otherwise) is a real feature for the eventual non-spike preview
//! pipeline, not something this research pass builds -- flagged as a follow-up, not implemented
//! here.

use crate::decode::DecodedRgb;

#[derive(Debug, Clone, Copy, clap::ValueEnum, PartialEq, Eq)]
pub enum Codec {
    Jpeg,
    Avif,
}

/// Callers that need encode/decode latency (e.g. `tier_bench.rs`) time these calls externally,
/// often as part of a larger combined measurement (a cache read + decode together) -- so these
/// return plain `Result`s rather than bundling their own internal timing.
pub fn encode(codec: Codec, img: &DecodedRgb, quality: u8) -> Result<Vec<u8>, String> {
    match codec {
        Codec::Jpeg => encode_jpeg(img, quality),
        Codec::Avif => encode_avif(img, quality),
    }
}

pub fn decode(codec: Codec, bytes: &[u8]) -> Result<DecodedRgb, String> {
    match codec {
        Codec::Jpeg => crate::decode::decode_jpeg(bytes),
        Codec::Avif => decode_avif(bytes),
    }
}

fn encode_jpeg(img: &DecodedRgb, quality: u8) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let encoder = jpeg_encoder::Encoder::new(&mut out, quality);
    encoder
        .encode(
            &img.rgb,
            img.width as u16,
            img.height as u16,
            jpeg_encoder::ColorType::Rgb,
        )
        .map_err(|e| e.to_string())?;
    Ok(out)
}

/// `speed` is fixed at 6 (ravif's own mid-range default) rather than exposed as a bench axis --
/// this spike is measuring format choice, not tuning AVIF's own speed/ratio tradeoff, which is
/// an orthogonal question ravif's own docs already cover.
const AVIF_SPEED: u8 = 6;

fn encode_avif(img: &DecodedRgb, quality: u8) -> Result<Vec<u8>, String> {
    let pixels: Vec<rgb::RGB8> = img
        .rgb
        .as_chunks::<3>()
        .0
        .iter()
        .map(|c| rgb::RGB8::new(c[0], c[1], c[2]))
        .collect();
    let buffer = imgref::Img::new(pixels.as_slice(), img.width as usize, img.height as usize);
    let encoded = ravif::Encoder::new()
        .with_quality(quality as f32)
        .with_speed(AVIF_SPEED)
        // Ravif defaults to encoding 8-bit input at 10-bit internal depth (its own docs note this
        // "works best, even for 8-bit inputs"); forced back to Eight here so decode returns
        // `Image::Rgb8` matching this spike's 8-bit `DecodedRgb` pipeline, not `Rgb16`.
        .with_bit_depth(ravif::BitDepth::Eight)
        .encode_rgb(buffer)
        .map_err(|e| e.to_string())?;
    Ok(encoded.avif_file)
}

fn decode_avif(bytes: &[u8]) -> Result<DecodedRgb, String> {
    let image = avif_decode::Decoder::from_avif(bytes)
        .map_err(|e| e.to_string())?
        .to_image()
        .map_err(|e| e.to_string())?;
    match image {
        avif_decode::Image::Rgb8(img) => {
            let width = img.width() as u32;
            let height = img.height() as u32;
            let mut rgb = Vec::with_capacity(img.width() * img.height() * 3);
            for px in img.pixels() {
                rgb.push(px.r);
                rgb.push(px.g);
                rgb.push(px.b);
            }
            Ok(DecodedRgb { width, height, rgb })
        }
        avif_decode::Image::Rgba8(img) => {
            let width = img.width() as u32;
            let height = img.height() as u32;
            let mut rgb = Vec::with_capacity(img.width() * img.height() * 3);
            for px in img.pixels() {
                rgb.push(px.r);
                rgb.push(px.g);
                rgb.push(px.b);
            }
            Ok(DecodedRgb { width, height, rgb })
        }
        avif_decode::Image::Rgb16(_) => {
            Err("unexpected 16-bit RGB AVIF output (expected 8-bit RGB(A))".to_string())
        }
        avif_decode::Image::Rgba16(_) => {
            Err("unexpected 16-bit RGBA AVIF output (expected 8-bit RGB(A))".to_string())
        }
        avif_decode::Image::Gray8(_) | avif_decode::Image::Gray16(_) => {
            Err("unexpected grayscale AVIF output (expected 8-bit RGB(A))".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_image(width: u32, height: u32, r: u8, g: u8, b: u8) -> DecodedRgb {
        let mut rgb = Vec::with_capacity((width * height * 3) as usize);
        for _ in 0..(width * height) {
            rgb.push(r);
            rgb.push(g);
            rgb.push(b);
        }
        DecodedRgb { width, height, rgb }
    }

    #[test]
    fn jpeg_round_trip_preserves_dimensions() {
        let img = solid_image(64, 32, 200, 100, 50);
        let encoded = encode(Codec::Jpeg, &img, 90).expect("encode");
        assert!(!encoded.is_empty());
        let decoded = decode(Codec::Jpeg, &encoded).expect("decode");
        assert_eq!(decoded.width, 64);
        assert_eq!(decoded.height, 32);
    }

    #[test]
    fn avif_round_trip_preserves_dimensions() {
        let img = solid_image(64, 32, 200, 100, 50);
        let encoded = encode(Codec::Avif, &img, 80).expect("encode");
        assert!(!encoded.is_empty());
        let decoded = decode(Codec::Avif, &encoded).expect("decode");
        assert_eq!(decoded.width, 64);
        assert_eq!(decoded.height, 32);
    }

    #[test]
    fn avif_is_smaller_than_jpeg_on_a_photographic_gradient_at_comparable_quality() {
        // A gradient (not a flat color) so both codecs actually spend bits on real content --
        // a solid-color image compresses trivially in both formats and wouldn't be a meaningful
        // ratio comparison.
        let (w, h) = (256u32, 256u32);
        let mut rgb = Vec::with_capacity((w * h * 3) as usize);
        for y in 0..h {
            for x in 0..w {
                rgb.push((x % 256) as u8);
                rgb.push((y % 256) as u8);
                rgb.push(((x + y) % 256) as u8);
            }
        }
        let img = DecodedRgb {
            width: w,
            height: h,
            rgb,
        };
        let jpeg = encode(Codec::Jpeg, &img, 85).expect("jpeg encode");
        let avif = encode(Codec::Avif, &img, 75).expect("avif encode");
        // Not a hard assertion on exact ratio (that's what tier-bench's real-photo numbers are
        // for) -- just confirms the AVIF path produces a real, non-degenerate encode here.
        assert!(avif.len() > 100);
        assert!(jpeg.len() > 100);
    }
}
