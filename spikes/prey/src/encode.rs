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
/// optional embedded ICC profile (auto-split across multiple APP2 segments if needed). Uses
/// `jpeg-encoder`'s own default chroma subsampling, which -- unlike mozjpeg's fixed libjpeg
/// default -- varies with `quality` (4:2:0 below quality 90, 4:4:4 at 90 and above; see
/// `jpeg-encoder` 0.6.1's `Encoder::new`). That quality-dependent switch is real pipeline
/// behavior worth keeping as the default here, but it means comparing two encoders at the same
/// nominal quality can silently also be comparing two different subsampling modes -- see
/// [`encode_jpeg_encoder_with_sampling`] for a comparison that pins subsampling explicitly.
pub fn encode_jpeg_encoder(
    img: &RgbImage,
    quality: u8,
    icc: Option<&[u8]>,
) -> anyhow::Result<Vec<u8>> {
    encode_jpeg_encoder_inner(img, quality, icc, None)
}

/// Like [`encode_jpeg_encoder`], but pins `sampling` explicitly instead of letting it vary with
/// `quality`. Exists so an encoder comparison can hold chroma subsampling constant on both sides
/// -- see [`find_matched_quality`]'s module docs for why nominal quality alone isn't
/// apples-to-apples between `jpeg-encoder` and mozjpeg.
pub fn encode_jpeg_encoder_with_sampling(
    img: &RgbImage,
    quality: u8,
    icc: Option<&[u8]>,
    sampling: jpeg_encoder::SamplingFactor,
) -> anyhow::Result<Vec<u8>> {
    encode_jpeg_encoder_inner(img, quality, icc, Some(sampling))
}

fn encode_jpeg_encoder_inner(
    img: &RgbImage,
    quality: u8,
    icc: Option<&[u8]>,
    sampling: Option<jpeg_encoder::SamplingFactor>,
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
    if let Some(sampling) = sampling {
        encoder.set_sampling_factor(sampling);
    }
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

/// The result of [`find_matched_quality`]: the smallest encoder-native `quality` parameter that
/// reached `target_ssim` against the source image, plus that encode's output size and actual
/// score.
#[derive(Debug, Clone, Copy)]
pub struct MatchedQuality {
    pub quality: u8,
    pub size_bytes: usize,
    pub ssim: f64,
}

/// Finds the smallest `quality` in `1..=100` for which `encode(quality)` (decoded via `decode`)
/// scores at least `target_ssim` against `src` -- the quality-matching step ADR-0056's own
/// decision rule requires before comparing encoder candidates' output size or speed (nominal
/// "quality 90" isn't comparable across encoders; see the ADR's Consequences section). A linear
/// ascending scan, not a binary search: JPEG quantization tables aren't guaranteed strictly
/// monotonic in SSIM as quality rises (a higher nominal quality can occasionally score a hair
/// lower on a specific image due to how chroma subsampling or rounding falls at that quantizer),
/// so a binary search could converge on a quality that looks like it clears the bar by luck at
/// the midpoint while a lower one actually would too -- a full scan avoids picking a
/// non-representative crossing point. 100 encodes is cheap for a one-shot research measurement
/// even at mozjpeg's ~70ms per encode (roughly 7s worst case), and this function only runs once
/// per encoder candidate, not in a hot path.
pub fn find_matched_quality(
    src: &RgbImage,
    target_ssim: f64,
    mut encode: impl FnMut(u8) -> anyhow::Result<Vec<u8>>,
    mut decode: impl FnMut(&[u8]) -> anyhow::Result<RgbImage>,
) -> anyhow::Result<MatchedQuality> {
    for quality in 1..=100u8 {
        let bytes = encode(quality)?;
        let decoded = decode(&bytes)?;
        if decoded.dimensions() != src.dimensions() {
            anyhow::bail!(
                "decoded image is {:?}, source is {:?} -- dimensions must match to score SSIM",
                decoded.dimensions(),
                src.dimensions(),
            );
        }
        let score = nicti_prowl::golden::ssim(src, &decoded);
        if score >= target_ssim {
            return Ok(MatchedQuality {
                quality,
                size_bytes: bytes.len(),
                ssim: score,
            });
        }
    }
    anyhow::bail!("no quality in 1..=100 reached target SSIM {target_ssim} against this source")
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
    /// build), at `quality` (0.0-100.0), with an optional embedded ICC profile. Uses libjpeg's
    /// own default chroma subsampling (2x2/1x1, i.e. 4:2:0, set by `jpeg_set_defaults` and left
    /// unchanged regardless of `quality`) -- unlike `jpeg-encoder`'s default, which switches to
    /// 4:4:4 at quality 90+ (see [`super::encode_jpeg_encoder`]'s doc comment). See
    /// [`encode_mozjpeg_with_sampling`] for a comparison that pins subsampling explicitly.
    pub fn encode_mozjpeg(
        img: &RgbImage,
        quality: f32,
        icc: Option<&[u8]>,
    ) -> anyhow::Result<Vec<u8>> {
        encode_mozjpeg_inner(img, quality, icc, None)
    }

    /// Like [`encode_mozjpeg`], but pins chroma subsampling explicitly (`(1,1)` == 4:4:4, `(2,2)`
    /// == 4:2:0 -- see `Compress::set_chroma_sampling_pixel_sizes`'s own doc comment for the
    /// full mapping) instead of relying on libjpeg's fixed 4:2:0 default. Exists for the same
    /// reason as [`super::encode_jpeg_encoder_with_sampling`]: holding subsampling constant on
    /// both encoder candidates is what makes a quality-matched comparison apples-to-apples.
    pub fn encode_mozjpeg_with_sampling(
        img: &RgbImage,
        quality: f32,
        icc: Option<&[u8]>,
        cb: (u8, u8),
        cr: (u8, u8),
    ) -> anyhow::Result<Vec<u8>> {
        encode_mozjpeg_inner(img, quality, icc, Some((cb, cr)))
    }

    fn encode_mozjpeg_inner(
        img: &RgbImage,
        quality: f32,
        icc: Option<&[u8]>,
        sampling: Option<((u8, u8), (u8, u8))>,
    ) -> anyhow::Result<Vec<u8>> {
        let (width, height) = img.dimensions();
        let mut compress = Compress::new(ColorSpace::JCS_RGB);
        compress.set_size(width as usize, height as usize);
        compress.set_quality(quality);
        if let Some((cb, cr)) = sampling {
            compress.set_chroma_sampling_pixel_sizes(cb, cr);
        }
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
    fn jpeg_encoder_default_sampling_switches_at_quality_90() {
        // Regression test for a real finding made while building #223's quality-matched sweep:
        // jpeg-encoder 0.6.1's own default (Encoder::new) switches chroma subsampling from 4:2:0
        // to 4:4:4 exactly at quality 90, which produces a much bigger size jump across that one
        // quality step than quantization-table refinement alone would -- an unannounced confound
        // in any encoder comparison run at nominal "quality 90". Pinning subsampling explicitly
        // (encode_jpeg_encoder_with_sampling) removes the artificial jump.
        let src = gradient(128, 128);
        let default_89 = encode_jpeg_encoder(&src, 89, None).unwrap();
        let default_90 = encode_jpeg_encoder(&src, 90, None).unwrap();
        let default_jump = default_90.len() as f64 / default_89.len() as f64;

        let pinned_89 =
            encode_jpeg_encoder_with_sampling(&src, 89, None, jpeg_encoder::SamplingFactor::F_2_2)
                .unwrap();
        let pinned_90 =
            encode_jpeg_encoder_with_sampling(&src, 90, None, jpeg_encoder::SamplingFactor::F_2_2)
                .unwrap();
        let pinned_jump = pinned_90.len() as f64 / pinned_89.len() as f64;

        assert!(
            pinned_jump < default_jump,
            "pinning subsampling should remove the artificial q90 jump: default={default_jump:.2}x pinned={pinned_jump:.2}x"
        );
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
    fn find_matched_quality_returns_smallest_quality_reaching_target() {
        let src = gradient(64, 64);
        let matched = find_matched_quality(
            &src,
            0.9,
            |q| encode_jpeg_encoder(&src, q, None),
            |bytes| Ok(decode_jpeg(bytes)),
        )
        .unwrap();
        assert!(matched.ssim >= 0.9, "matched quality must clear the target");
        assert!(matched.size_bytes > 0);
        // The quality immediately below the match must score under the target -- otherwise the
        // scan didn't actually find the *smallest* one, just *a* one.
        if matched.quality > 1 {
            let lower = encode_jpeg_encoder(&src, matched.quality - 1, None).unwrap();
            let lower_score = ssim(&src, &decode_jpeg(&lower));
            assert!(
                lower_score < 0.9,
                "quality {} scored {lower_score} >= target, so it wasn't actually the smallest match",
                matched.quality - 1
            );
        }
    }

    #[test]
    fn find_matched_quality_errors_when_target_unreachable() {
        let src = gradient(64, 64);
        // No JPEG quality reaches a perfect 1.0 SSIM against a real gradient -- lossy quantization
        // always introduces at least a little error.
        let result = find_matched_quality(
            &src,
            1.0,
            |q| encode_jpeg_encoder(&src, q, None),
            |bytes| Ok(decode_jpeg(bytes)),
        );
        assert!(result.is_err());
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

    #[cfg(feature = "native")]
    #[test]
    fn mozjpeg_444_produces_larger_output_than_default_420_at_same_quality() {
        // mozjpeg's own default (jpeg_set_defaults, unlike jpeg-encoder's) stays at 4:2:0
        // regardless of quality -- confirms encode_mozjpeg's doc claim, and that
        // encode_mozjpeg_with_sampling actually changes the encoder's behavior rather than being
        // a no-op wrapper.
        let src = gradient(128, 128);
        let default = native::encode_mozjpeg(&src, 90.0, None).unwrap();
        let pinned_444 =
            native::encode_mozjpeg_with_sampling(&src, 90.0, None, (1, 1), (1, 1)).unwrap();
        assert!(
            pinned_444.len() > default.len(),
            "4:4:4 ({} bytes) must be larger than the 4:2:0 default ({} bytes) at the same quality",
            pinned_444.len(),
            default.len()
        );
    }
}
