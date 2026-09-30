//! Linear-light resize (#57, promoted from `spikes/prey`; ADR-0056).
//!
//! Resize happens on linear f32 RGB, *before* the output curve is applied -- averaging
//! gamma-encoded values darkens high-contrast edges. `fast_image_resize`'s Lanczos3 convolution is
//! the v1 pick; its `rayon` feature is deliberately off so a resize can't spawn its own thread pool
//! outside Pounce's CPU-lane throttle (each export encode job is one Pounce chunk).
//!
//! Unlike the spike, this takes and returns f32 straight from Tapetum's readback -- the spike's
//! sRGB-u8 in/out entry point would have quantized before resizing.

use fast_image_resize::images::{Image as FrImage, ImageRef as FrImageRef};
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};

use crate::spec::{ResizeMode, ResizeSpec};

#[derive(Debug, thiserror::Error)]
pub enum ResizeError {
    #[error("frame has {got} floats, expected {expected} for {width}x{height} RGB")]
    BadLength {
        got: usize,
        expected: usize,
        width: u32,
        height: u32,
    },
    #[error("zero-sized image ({0}x{1})")]
    Empty(u32, u32),
    #[error("resize failed: {0}")]
    Resize(String),
}

/// The exported size for a `src_w` x `src_h` image under `spec`. Never zero in either dimension;
/// with `dont_enlarge` the scale is capped at 1.0.
pub fn target_size(src_w: u32, src_h: u32, spec: &ResizeSpec) -> (u32, u32) {
    let (sw, sh) = (src_w.max(1) as f64, src_h.max(1) as f64);
    let scale = match spec.mode {
        ResizeMode::None => 1.0,
        ResizeMode::LongEdge(px) => px as f64 / sw.max(sh),
        ResizeMode::ShortEdge(px) => px as f64 / sw.min(sh),
        ResizeMode::Fit { width, height } => (width as f64 / sw).min(height as f64 / sh),
        ResizeMode::Megapixels(mp) => (mp as f64 * 1_000_000.0 / (sw * sh)).sqrt(),
    };
    let scale = if spec.dont_enlarge {
        scale.min(1.0)
    } else {
        scale
    };
    if (scale - 1.0).abs() < f64::EPSILON {
        return (src_w.max(1), src_h.max(1));
    }
    (
        ((sw * scale).round() as u32).max(1),
        ((sh * scale).round() as u32).max(1),
    )
}

/// Resizes interleaved linear RGB f32 to `dst_w` x `dst_h` (a no-op copy-free return when the
/// size is unchanged).
pub fn resize_linear_f32(
    mut pixels: Vec<f32>,
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
) -> Result<Vec<f32>, ResizeError> {
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return Err(ResizeError::Empty(src_w.min(dst_w), src_h.min(dst_h)));
    }
    let expected = src_w as usize * src_h as usize * 3;
    if pixels.len() != expected {
        return Err(ResizeError::BadLength {
            got: pixels.len(),
            expected,
            width: src_w,
            height: src_h,
        });
    }
    if (src_w, src_h) == (dst_w, dst_h) {
        return Ok(pixels);
    }

    let bytes: &mut [u8] = bytemuck::cast_slice_mut(&mut pixels);
    let src = FrImageRef::new(src_w, src_h, bytes, PixelType::F32x3)
        .map_err(|e| ResizeError::Resize(e.to_string()))?;
    let mut dst = FrImage::new(dst_w, dst_h, PixelType::F32x3);
    let options = ResizeOptions {
        algorithm: ResizeAlg::Convolution(FilterType::Lanczos3),
        // No alpha channel, and the data is already linear: premultiplication is irrelevant.
        mul_div_alpha: false,
        ..Default::default()
    };
    Resizer::new()
        .resize(&src, &mut dst, &options)
        .map_err(|e| ResizeError::Resize(e.to_string()))?;
    let out: &[f32] = bytemuck::cast_slice(dst.buffer());
    Ok(out.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(mode: ResizeMode, dont_enlarge: bool) -> ResizeSpec {
        ResizeSpec { mode, dont_enlarge }
    }

    #[test]
    fn target_size_for_every_mode() {
        assert_eq!(
            target_size(6000, 4000, &spec(ResizeMode::None, true)),
            (6000, 4000)
        );
        assert_eq!(
            target_size(6000, 4000, &spec(ResizeMode::LongEdge(2048), true)),
            (2048, 1365)
        );
        assert_eq!(
            target_size(4000, 6000, &spec(ResizeMode::LongEdge(2048), true)),
            (1365, 2048)
        );
        assert_eq!(
            target_size(6000, 4000, &spec(ResizeMode::ShortEdge(1000), true)),
            (1500, 1000)
        );
        assert_eq!(
            target_size(
                6000,
                4000,
                &spec(
                    ResizeMode::Fit {
                        width: 1200,
                        height: 1200
                    },
                    true
                )
            ),
            (1200, 800)
        );
        assert_eq!(
            target_size(
                6000,
                4000,
                &spec(
                    ResizeMode::Fit {
                        width: 5000,
                        height: 500
                    },
                    true
                )
            ),
            (750, 500)
        );
        // 24 MP -> 6 MP is half each dimension.
        assert_eq!(
            target_size(6000, 4000, &spec(ResizeMode::Megapixels(6.0), true)),
            (3000, 2000)
        );
    }

    #[test]
    fn dont_enlarge_caps_at_the_source_size_but_enlarge_allows_growth() {
        assert_eq!(
            target_size(1000, 800, &spec(ResizeMode::LongEdge(4000), true)),
            (1000, 800)
        );
        assert_eq!(
            target_size(1000, 800, &spec(ResizeMode::LongEdge(2000), false)),
            (2000, 1600)
        );
        assert_eq!(
            target_size(1000, 800, &spec(ResizeMode::Megapixels(100.0), true)),
            (1000, 800)
        );
    }

    #[test]
    fn extreme_aspect_ratios_never_produce_a_zero_dimension() {
        assert_eq!(
            target_size(10_000, 1, &spec(ResizeMode::LongEdge(100), true)).1,
            1
        );
    }

    #[test]
    fn a_flat_image_stays_flat_and_has_the_requested_size() {
        let src = vec![0.25f32; 8 * 6 * 3];
        let out = resize_linear_f32(src, 8, 6, 4, 3).unwrap();
        assert_eq!(out.len(), 4 * 3 * 3);
        assert!(out.iter().all(|v| (v - 0.25).abs() < 1e-4), "{out:?}");
    }

    #[test]
    fn unchanged_size_returns_the_input_untouched() {
        let src: Vec<f32> = (0..12).map(|i| i as f32).collect();
        assert_eq!(resize_linear_f32(src.clone(), 2, 2, 2, 2).unwrap(), src);
    }

    #[test]
    fn resizing_is_linear_light_not_gamma_space() {
        // A 2x1 black/white checker averaged to 1x1: linear light gives 0.5 (which encodes to
        // ~0.735 sRGB); a gamma-space average of the *encoded* values would give 0.5 encoded,
        // i.e. ~0.214 linear. Ours must be the former.
        let src = vec![0.0, 0.0, 0.0, 1.0, 1.0, 1.0];
        let out = resize_linear_f32(src, 2, 1, 1, 1).unwrap();
        assert!((out[0] - 0.5).abs() < 0.02, "{out:?}");
    }

    #[test]
    fn bad_input_is_an_error_not_a_panic() {
        assert!(matches!(
            resize_linear_f32(vec![0.0; 5], 2, 2, 1, 1),
            Err(ResizeError::BadLength { .. })
        ));
        assert!(matches!(
            resize_linear_f32(vec![], 0, 2, 1, 1),
            Err(ResizeError::Empty(..))
        ));
    }
}
