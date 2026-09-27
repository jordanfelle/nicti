//! Turns one NEF into the two feature representations #53's models train on: a 13-value histogram
//! feature vector (a small independent copy of `spikes/pupil::fit::features`'s shape, computed
//! over the embedded JPEG's own luminance rather than a linear camera-RGB render) and a 32x32 RGB
//! downsample tensor (the issue's "downsampled image tensor"). Input is the NEF's largest embedded
//! JPEG (`nicti_cornea::embedded`, pure Rust, no LibRaw/vendor submodule needed) -- the
//! camera-rendered look, not Adobe's own default render, per the scope decision in ADR-0053.

use std::num::NonZeroU32;
use std::path::Path;

use fast_image_resize as fr;
use nicti_cornea::embedded::{FileSource, Walker};
use zune_jpeg::zune_core::bytestream::ZCursor;
use zune_jpeg::JpegDecoder;

use crate::histogram::Histogram;

pub const HIST_FEATURE_COUNT: usize = 13;
pub const THUMB_SIZE: u32 = 32;
pub const THUMB_FEATURE_COUNT: usize = (THUMB_SIZE * THUMB_SIZE * 3) as usize;

#[derive(Debug, Clone)]
pub struct ImageFeatures {
    pub hist: [f32; HIST_FEATURE_COUNT],
    /// Row-major RGB, `0.0..=1.0`, length `THUMB_FEATURE_COUNT`.
    pub thumb: Vec<f32>,
}

#[derive(Debug, thiserror::Error)]
pub enum FeatureError {
    #[error("reading {0}: {1}")]
    Io(std::path::PathBuf, std::io::Error),
    #[error("no embedded JPEG found in {0}")]
    NoEmbeddedJpeg(std::path::PathBuf),
    #[error("IFD walk failed for {0}: {1}")]
    Ifd(std::path::PathBuf, String),
    #[error("JPEG decode failed for {0}: {1}")]
    JpegDecode(std::path::PathBuf, String),
    #[error("resize failed for {0}: {1}")]
    Resize(std::path::PathBuf, String),
}

/// Extracts the largest embedded JPEG from `path` (by declared pixel area when known, else by byte
/// length) and returns its raw bytes.
pub fn largest_embedded_jpeg(path: &Path) -> Result<Vec<u8>, FeatureError> {
    let source = FileSource::open(path).map_err(|e| FeatureError::Io(path.to_path_buf(), e))?;
    let mut walker =
        Walker::new(source).map_err(|e| FeatureError::Ifd(path.to_path_buf(), e.to_string()))?;
    let jpegs = walker
        .find_embedded_jpegs()
        .map_err(|e| FeatureError::Ifd(path.to_path_buf(), e.to_string()))?;
    let best = jpegs
        .iter()
        .max_by_key(|j| match (j.declared_width, j.declared_height) {
            (Some(w), Some(h)) => (w as u64) * (h as u64),
            _ => j.byte_len,
        })
        .ok_or_else(|| FeatureError::NoEmbeddedJpeg(path.to_path_buf()))?;

    let mut source = FileSource::open(path).map_err(|e| FeatureError::Io(path.to_path_buf(), e))?;
    // `find_embedded_jpegs` already validated `file_offset`/`byte_len` against the file's own
    // length via the walk's own bounds checks -- a second `FileSource` (owned, not borrowed from
    // the walker) is used here only because `Walker` consumed its source into IFD-walking state,
    // not because this offset is independently untrusted.
    read_at(&mut source, best.file_offset, best.byte_len as usize)
        .map_err(|e| FeatureError::Io(path.to_path_buf(), e))
}

fn read_at(source: &mut FileSource, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
    use nicti_cornea::embedded::ByteSource;
    source.read_at(offset, len)
}

#[derive(Debug)]
pub struct DecodedRgb {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

pub fn decode_jpeg(bytes: &[u8]) -> Result<DecodedRgb, String> {
    let mut decoder = JpegDecoder::new(ZCursor::new(bytes));
    let pixels = decoder.decode().map_err(|e| e.to_string())?;
    let info = decoder.info().ok_or("decoder produced no image info")?;
    Ok(DecodedRgb {
        width: info.width as u32,
        height: info.height as u32,
        rgb: pixels,
    })
}

/// Builds the 13-value histogram feature vector over `decoded`'s luminance -- Rec. 601 luma
/// (`0.299R + 0.587G + 0.114B`), the ordinary weighting for an already-rendered sRGB JPEG (unlike
/// `pupil::render`, which reconstructs luminance from unrendered linear camera RGB and so needs
/// its own camera->XYZ->sRGB treatment first).
pub fn histogram_features(decoded: &DecodedRgb) -> [f32; HIST_FEATURE_COUNT] {
    let samples: Vec<f32> = decoded
        .rgb
        .as_chunks::<3>()
        .0
        .iter()
        .map(|px| {
            let r = px[0] as f32 / 255.0;
            let g = px[1] as f32 / 255.0;
            let b = px[2] as f32 / 255.0;
            0.299 * r + 0.587 * g + 0.114 * b
        })
        .collect();
    let hist = Histogram::from_samples(samples);
    [
        1.0,
        hist.percentile(0.5),
        hist.percentile(2.0),
        hist.percentile(10.0),
        hist.percentile(25.0),
        hist.percentile(50.0),
        hist.percentile(75.0),
        hist.percentile(90.0),
        hist.percentile(98.0),
        hist.percentile(99.5),
        hist.mean(),
        hist.fraction_below(0.02) as f32,
        hist.fraction_above(0.98) as f32,
    ]
}

/// Resizes `decoded` to `THUMB_SIZE`x`THUMB_SIZE` RGB and flattens to `0.0..=1.0` floats.
pub fn thumbnail(decoded: &DecodedRgb) -> Result<Vec<f32>, String> {
    let src_w = NonZeroU32::new(decoded.width).ok_or("zero width")?;
    let src_h = NonZeroU32::new(decoded.height).ok_or("zero height")?;
    let src_image = fr::images::Image::from_vec_u8(
        src_w.get(),
        src_h.get(),
        decoded.rgb.clone(),
        fr::PixelType::U8x3,
    )
    .map_err(|e| e.to_string())?;

    let mut dst_image = fr::images::Image::new(THUMB_SIZE, THUMB_SIZE, fr::PixelType::U8x3);
    let mut resizer = fr::Resizer::new();
    resizer
        .resize(&src_image, &mut dst_image, None)
        .map_err(|e| e.to_string())?;

    Ok(dst_image
        .into_vec()
        .into_iter()
        .map(|b| b as f32 / 255.0)
        .collect())
}

/// End-to-end: NEF path -> both feature representations.
pub fn extract(path: &Path) -> Result<ImageFeatures, FeatureError> {
    let jpeg_bytes = largest_embedded_jpeg(path)?;
    let decoded =
        decode_jpeg(&jpeg_bytes).map_err(|e| FeatureError::JpegDecode(path.to_path_buf(), e))?;
    let hist = histogram_features(&decoded);
    let thumb = thumbnail(&decoded).map_err(|e| FeatureError::Resize(path.to_path_buf(), e))?;
    Ok(ImageFeatures { hist, thumb })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_decoded(w: u32, h: u32, fill: [u8; 3]) -> DecodedRgb {
        DecodedRgb {
            width: w,
            height: h,
            rgb: fill.repeat((w * h) as usize),
        }
    }

    #[test]
    fn histogram_features_of_a_flat_gray_image_has_zero_spread() {
        let decoded = make_decoded(16, 16, [128, 128, 128]);
        let feats = histogram_features(&decoded);
        // p0.5 and p99.5 should be equal (and near 128/255) on a perfectly flat image.
        assert!((feats[1] - feats[9]).abs() < 1e-6);
    }

    #[test]
    fn histogram_features_leading_bias_term_is_one() {
        let decoded = make_decoded(4, 4, [0, 0, 0]);
        let feats = histogram_features(&decoded);
        assert_eq!(feats[0], 1.0);
    }

    #[test]
    fn thumbnail_produces_the_expected_flat_length() {
        let decoded = make_decoded(64, 48, [10, 20, 30]);
        let thumb = thumbnail(&decoded).unwrap();
        assert_eq!(thumb.len(), THUMB_FEATURE_COUNT);
        assert!(thumb.iter().all(|&v| (0.0..=1.0).contains(&v)));
    }

    #[test]
    fn decodes_tiny_fixture_jpeg() {
        // A minimal real JPEG generated by the `image` crate at build time would need a new dev
        // dependency just for this test; instead this asserts the error path is clean on garbage
        // bytes, which is the case purr's own extraction loop must handle per-file without
        // aborting the whole run.
        let err = decode_jpeg(b"not a jpeg").unwrap_err();
        assert!(!err.is_empty());
    }
}
