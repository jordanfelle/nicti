//! Encoder candidates. Two pure-Rust JPEG encoders are always available:
//! [`encode_jpeg_encoder`] (the `jpeg-encoder` crate, already used by `spikes/sniff` for T2
//! previews) and [`encode_image_crate_jpeg`] (`image`'s own baseline encoder, no SIMD). A native
//! candidate, [`encode_mozjpeg`], sits behind the non-default `native` Cargo feature so ordinary
//! CI/dev builds never need a C compiler + nasm for `mozjpeg-sys`'s vendored mozjpeg build --
//! same pattern as `nicti-decode`'s `libraw` feature. 16-bit TIFF/PNG encode (for a non-JPEG
//! export target) go through `image` directly; no dedicated candidate comparison is needed there
//! since only one pure-Rust option exists in this workspace for either format.
//!
//! `turbojpeg` (libjpeg-turbo bindings) was not reached this pass -- see the ADR's "what wasn't
//! reachable" section and the follow-up issue.

use image::{ImageBuffer, Rgb, RgbImage};

/// Encodes `img` as a baseline JPEG via the `jpeg-encoder` crate, at `quality` (0-100), with an
/// optional embedded ICC profile (auto-split across multiple APP2 segments if needed).
pub fn encode_jpeg_encoder(
    img: &RgbImage,
    quality: u8,
    icc: Option<&[u8]>,
) -> anyhow::Result<Vec<u8>> {
    let (width, height) = img.dimensions();
    // jpeg-encoder's own `encode` takes u16 dimensions -- a silent `as u16` truncation on an
    // oversized image would pass the wrong (wrapped-around) width/height while `img.as_raw()`
    // still holds the full-size buffer, producing a corrupted encode rather than a clean error
    // (caught by CodeRabbit review on PR #221). JPEG's own format ceiling is 65535px per side
    // anyway, so a real caller past that limit needs a different container format regardless.
    let width_u16 = u16::try_from(width)
        .map_err(|_| anyhow::anyhow!("image width {width} exceeds JPEG's 65535px limit"))?;
    let height_u16 = u16::try_from(height)
        .map_err(|_| anyhow::anyhow!("image height {height} exceeds JPEG's 65535px limit"))?;
    let mut buf = Vec::new();
    let mut encoder = jpeg_encoder::Encoder::new(&mut buf, quality);
    if let Some(icc) = icc {
        encoder
            .add_icc_profile(icc)
            .map_err(|e| anyhow::anyhow!("jpeg-encoder add_icc_profile: {e}"))?;
    }
    encoder
        .encode(
            img.as_raw(),
            width_u16,
            height_u16,
            jpeg_encoder::ColorType::Rgb,
        )
        .map_err(|e| anyhow::anyhow!("jpeg-encoder encode: {e}"))?;
    Ok(buf)
}

/// Encodes `img` as a baseline JPEG via `image::codecs::jpeg::JpegEncoder` -- the naive baseline
/// every consumer of the `image` crate gets without picking a dedicated JPEG crate.
pub fn encode_image_crate_jpeg(img: &RgbImage, quality: u8) -> anyhow::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality);
    img.write_with_encoder(encoder)
        .map_err(|e| anyhow::anyhow!("image crate jpeg encode: {e}"))?;
    Ok(buf)
}

/// Encodes `img` as a 16-bit TIFF (deflate-compressed) via `image`.
pub fn encode_tiff_16bit(img: &ImageBuffer<Rgb<u16>, Vec<u16>>) -> anyhow::Result<Vec<u8>> {
    let mut cursor = std::io::Cursor::new(Vec::new());
    img.write_with_encoder(image::codecs::tiff::TiffEncoder::new(&mut cursor))
        .map_err(|e| anyhow::anyhow!("tiff encode: {e}"))?;
    Ok(cursor.into_inner())
}

/// Encodes `img` as a 16-bit PNG via `image`.
pub fn encode_png_16bit(img: &ImageBuffer<Rgb<u16>, Vec<u16>>) -> anyhow::Result<Vec<u8>> {
    let mut buf = Vec::new();
    img.write_with_encoder(image::codecs::png::PngEncoder::new(&mut buf))
        .map_err(|e| anyhow::anyhow!("png encode: {e}"))?;
    Ok(buf)
}

#[cfg(feature = "native")]
pub mod native {
    use super::*;
    use mozjpeg::{ColorSpace, Compress};

    /// Encodes `img` as a baseline JPEG via real mozjpeg (through `mozjpeg-sys`'s vendored C
    /// build), at `quality` (0.0-100.0), with an optional embedded ICC profile.
    pub fn encode_mozjpeg(
        img: &RgbImage,
        quality: f32,
        icc: Option<&[u8]>,
    ) -> anyhow::Result<Vec<u8>> {
        let (width, height) = img.dimensions();
        let mut compress = Compress::new(ColorSpace::JCS_RGB);
        compress.set_size(width as usize, height as usize);
        compress.set_quality(quality);
        let mut compress = compress
            .start_compress(Vec::new())
            .map_err(|e| anyhow::anyhow!("mozjpeg start_compress: {e}"))?;
        if let Some(icc) = icc {
            compress.write_icc_profile(icc);
        }
        compress
            .write_scanlines(img.as_raw())
            .map_err(|e| anyhow::anyhow!("mozjpeg write_scanlines: {e}"))?;
        compress
            .finish()
            .map_err(|e| anyhow::anyhow!("mozjpeg finish: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_prowl::golden::ssim;

    fn gradient(width: u32, height: u32) -> RgbImage {
        RgbImage::from_fn(width, height, |x, y| {
            Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
        })
    }

    fn decode_jpeg(bytes: &[u8]) -> RgbImage {
        image::load_from_memory_with_format(bytes, image::ImageFormat::Jpeg)
            .expect("must decode as JPEG")
            .to_rgb8()
    }

    #[test]
    fn jpeg_encoder_roundtrips_at_high_quality() {
        let src = gradient(64, 64);
        let bytes = encode_jpeg_encoder(&src, 95, None).unwrap();
        let decoded = decode_jpeg(&bytes);
        assert_eq!(decoded.dimensions(), src.dimensions());
        assert!(ssim(&src, &decoded) > 0.95);
    }

    #[test]
    fn jpeg_encoder_embeds_icc_profile() {
        let src = gradient(32, 32);
        let icc = crate::icc::srgb_icc_profile().unwrap();
        let bytes = encode_jpeg_encoder(&src, 90, Some(&icc)).unwrap();
        let read_back = crate::icc::read_icc_jpeg(&bytes).unwrap();
        assert_eq!(read_back.as_deref(), Some(icc.as_slice()));
    }

    #[test]
    fn image_crate_jpeg_roundtrips_at_high_quality() {
        let src = gradient(64, 64);
        let bytes = encode_image_crate_jpeg(&src, 95).unwrap();
        let decoded = decode_jpeg(&bytes);
        assert_eq!(decoded.dimensions(), src.dimensions());
        assert!(ssim(&src, &decoded) > 0.95);
    }

    #[test]
    fn jpeg_encoder_rejects_dimension_past_u16_range_instead_of_corrupting() {
        // Regression test for a real CodeRabbit finding on PR #221: an unchecked `as u16` cast
        // on an oversized dimension used to silently wrap around and pass the wrong width to the
        // encoder while img.as_raw() still held the full-size buffer -- a corrupted encode, not
        // a clean error. A 1-pixel-tall image keeps the test's own memory footprint small.
        let oversized = RgbImage::new(u16::MAX as u32 + 1, 1);
        let result = encode_jpeg_encoder(&oversized, 90, None);
        assert!(
            result.is_err(),
            "a width past u16::MAX must error, not silently truncate"
        );
    }

    #[test]
    fn lower_quality_produces_smaller_and_lower_fidelity_output() {
        let src = gradient(128, 128);
        let high = encode_jpeg_encoder(&src, 95, None).unwrap();
        let low = encode_jpeg_encoder(&src, 20, None).unwrap();
        assert!(low.len() < high.len(), "q20 should be smaller than q95");

        let high_score = ssim(&src, &decode_jpeg(&high));
        let low_score = ssim(&src, &decode_jpeg(&low));
        assert!(low_score < high_score, "q20 should score lower than q95");
    }

    #[test]
    fn tiff_16bit_roundtrips() {
        let img: ImageBuffer<Rgb<u16>, Vec<u16>> = ImageBuffer::from_fn(16, 16, |x, y| {
            Rgb([x as u16 * 1000, y as u16 * 1000, 30000])
        });
        let bytes = encode_tiff_16bit(&img).unwrap();
        let decoded = image::load_from_memory_with_format(&bytes, image::ImageFormat::Tiff)
            .unwrap()
            .to_rgb16();
        assert_eq!(decoded.dimensions(), (16, 16));
        assert_eq!(decoded.get_pixel(1, 1), img.get_pixel(1, 1));
    }

    #[test]
    fn png_16bit_roundtrips() {
        let img: ImageBuffer<Rgb<u16>, Vec<u16>> = ImageBuffer::from_fn(16, 16, |x, y| {
            Rgb([x as u16 * 1000, y as u16 * 1000, 30000])
        });
        let bytes = encode_png_16bit(&img).unwrap();
        let decoded = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)
            .unwrap()
            .to_rgb16();
        assert_eq!(decoded.get_pixel(1, 1), img.get_pixel(1, 1));
    }

    #[cfg(feature = "native")]
    #[test]
    fn mozjpeg_roundtrips_and_embeds_icc() {
        let src = gradient(64, 64);
        let icc = crate::icc::srgb_icc_profile().unwrap();
        let bytes = native::encode_mozjpeg(&src, 90.0, Some(&icc)).unwrap();
        let decoded = decode_jpeg(&bytes);
        assert_eq!(decoded.dimensions(), src.dimensions());
        assert!(ssim(&src, &decoded) > 0.95);
        let read_back = crate::icc::read_icc_jpeg(&bytes).unwrap();
        assert_eq!(read_back.as_deref(), Some(icc.as_slice()));
    }
}
