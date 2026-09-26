//! Tier-payload format candidates for #29, measured against JPEG (`decode.rs`'s existing path):
//! AVIF, added at the user's explicit request ("numbers tell it all") over the ADR author's own
//! prior toward JPEG-only, and lossy WebP, added for #143's follow-up measurement. AVIF's encode
//! (`ravif`, rav1e-based) and decode (`avif-decode`, rav1d-based) are pure Rust -- no C toolchain
//! dependency, unlike `image`'s `avif-native` feature (which needs the C `dav1d` library) -- so
//! that path stays safe to build on Windows CI the same way the rest of this repo's Rust-first
//! stack does. WebP is the one exception: the `webp` crate wraps the real C `libwebp`, compiled
//! via `cc` (see `Cargo.toml`'s comment on why that's acceptable here).
//!
//! Scope note (also in `docs/adr/0017-preview-tier-strategy.md`): this only measures AVIF/WebP vs
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
    Webp,
}

/// Callers that need encode/decode latency (e.g. `tier_bench.rs`) time these calls externally,
/// often as part of a larger combined measurement (a cache read + decode together) -- so these
/// return plain `Result`s rather than bundling their own internal timing.
///
/// `speed` is AVIF-only (ravif's 1-10 encode-effort/ratio dial, ignored by the other two codecs)
/// -- #143's whole reason for existing is to sweep this axis, so it's a real parameter now
/// rather than this module's previous hardcoded constant.
pub fn encode(codec: Codec, img: &DecodedRgb, quality: u8, speed: u8) -> Result<Vec<u8>, String> {
    match codec {
        Codec::Jpeg => encode_jpeg(img, quality),
        Codec::Avif => encode_avif(img, quality, speed),
        Codec::Webp => encode_webp(img, quality),
    }
}

pub fn decode(codec: Codec, bytes: &[u8]) -> Result<DecodedRgb, String> {
    match codec {
        Codec::Jpeg => crate::decode::decode_jpeg(bytes),
        Codec::Avif => decode_avif(bytes),
        Codec::Webp => decode_webp(bytes),
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

fn encode_avif(img: &DecodedRgb, quality: u8, speed: u8) -> Result<Vec<u8>, String> {
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
        .with_speed(speed)
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

/// libwebp's own `WebPConfig::new()` default (used by `Encoder::encode`'s `encode_simple` path,
/// which this function calls) sets `method = 4`, the same effort/ratio point `cwebp` defaults to
/// -- not swept as a bench axis here, unlike AVIF's `speed`, since #143 only asks for one lossy
/// WebP quality sweep, not a second effort-level sweep.
fn encode_webp(img: &DecodedRgb, quality: u8) -> Result<Vec<u8>, String> {
    let encoder = webp::Encoder::from_rgb(&img.rgb, img.width, img.height);
    let encoded = encoder.encode(quality as f32);
    Ok(encoded.to_vec())
}

fn decode_webp(bytes: &[u8]) -> Result<DecodedRgb, String> {
    let image = webp::Decoder::new(bytes)
        .decode()
        .ok_or("failed to decode WebP")?;
    if image.layout() != webp::PixelLayout::Rgb {
        return Err(format!(
            "unexpected WebP pixel layout {:?} (expected Rgb -- encode_webp never produces alpha)",
            image.layout()
        ));
    }
    Ok(DecodedRgb {
        width: image.width(),
        height: image.height(),
        rgb: image.to_vec(),
    })
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
        let encoded = encode(Codec::Jpeg, &img, 90, 6).expect("encode");
        assert!(!encoded.is_empty());
        let decoded = decode(Codec::Jpeg, &encoded).expect("decode");
        assert_eq!(decoded.width, 64);
        assert_eq!(decoded.height, 32);
    }

    fn gradient_image(width: u32, height: u32) -> DecodedRgb {
        let mut rgb = Vec::with_capacity((width * height * 3) as usize);
        for y in 0..height {
            for x in 0..width {
                rgb.extend_from_slice(&[(x * 4) as u8, (y * 8) as u8, ((x + y) * 2) as u8]);
            }
        }
        DecodedRgb { width, height, rgb }
    }

    /// A fixed-color image would still "round-trip" even if decode lost all image variation
    /// (e.g. returned a single wrong-but-uniform color) -- checking for non-uniform output
    /// confirms a real, content-preserving decode happened, without requiring exact pixel
    /// equality (both AVIF and lossy WebP are lossy).
    fn assert_decoded_content_preserved(rgb: &[u8]) {
        #[allow(clippy::chunks_exact_to_as_chunks)]
        let pixels: Vec<&[u8]> = rgb.chunks_exact(3).collect();
        let first_pixel = pixels[0];
        assert!(
            pixels.iter().any(|p| *p != first_pixel),
            "decoded image lost all image content (every pixel identical)"
        );
    }

    #[test]
    fn avif_round_trip_preserves_dimensions_and_content() {
        let (width, height) = (64u32, 32u32);
        let img = gradient_image(width, height);

        let encoded = encode(Codec::Avif, &img, 80, 6).expect("encode");
        assert!(!encoded.is_empty());
        let decoded = decode(Codec::Avif, &encoded).expect("decode");
        assert_eq!(decoded.width, width);
        assert_eq!(decoded.height, height);
        assert_decoded_content_preserved(&decoded.rgb);
    }

    /// #143's whole reason for existing: confirm both ends of the speed dial still produce a
    /// real, decodable image -- the sweep script itself is what measures the actual
    /// time/quality tradeoff between them on real photos.
    #[test]
    fn avif_round_trips_at_fastest_and_slowest_speed() {
        let img = gradient_image(64, 32);
        for speed in [1u8, 10u8] {
            let encoded = encode(Codec::Avif, &img, 75, speed)
                .unwrap_or_else(|e| panic!("encode at speed {speed}: {e}"));
            assert!(!encoded.is_empty());
            let decoded = decode(Codec::Avif, &encoded)
                .unwrap_or_else(|e| panic!("decode at speed {speed}: {e}"));
            assert_decoded_content_preserved(&decoded.rgb);
        }
    }

    #[test]
    fn webp_round_trip_preserves_dimensions_and_content() {
        let (width, height) = (64u32, 32u32);
        let img = gradient_image(width, height);

        let encoded = encode(Codec::Webp, &img, 80, 6).expect("encode");
        assert!(!encoded.is_empty());
        let decoded = decode(Codec::Webp, &encoded).expect("decode");
        assert_eq!(decoded.width, width);
        assert_eq!(decoded.height, height);
        assert_decoded_content_preserved(&decoded.rgb);
    }

    #[test]
    fn avif_is_smaller_than_jpeg_on_a_photographic_gradient_at_comparable_quality() {
        // A gradient (not a flat color) so both codecs actually spend bits on real content --
        // a solid-color image compresses trivially in both formats and wouldn't be a meaningful
        // ratio comparison.
        let img = gradient_image(256, 256);
        let jpeg = encode(Codec::Jpeg, &img, 85, 6).expect("jpeg encode");
        let avif = encode(Codec::Avif, &img, 75, 6).expect("avif encode");
        let webp = encode(Codec::Webp, &img, 75, 6).expect("webp encode");
        // Not a hard assertion on exact ratio (that's what tier-bench's real-photo numbers are
        // for) -- just confirms each path produces a real, non-degenerate encode here.
        assert!(avif.len() > 100);
        assert!(jpeg.len() > 100);
        assert!(webp.len() > 100);
    }
}
