//! Resize candidates for export long-edge downsizing. Decision rule (`docs/adr/0056`): resize
//! happens in **linear light**, before the sRGB OETF is reapplied, so a downsample doesn't darken
//! high-contrast edges the way resizing post-gamma does. Two CPU candidates are compared:
//!
//! - [`resize_fast_linear`] -- `fast_image_resize` over an `F32x3` buffer converted to/from linear
//!   light by hand (sRGB has no dedicated linear pixel type in that crate).
//! - [`resize_image_crate_srgb`] -- `image::imageops::resize`, which operates directly on the
//!   gamma-encoded `u8` buffer (no linear conversion) -- the naive baseline every other RAW
//!   pipeline that doesn't bother with linear-light resize actually ships.
//!
//! A GPU candidate lives in [`crate::gpu_resize`].

use fast_image_resize::images::Image as FrImage;
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
use image::{ImageBuffer, Rgb, RgbImage};

/// sRGB (IEC 61966-2-1) electro-optical transfer function: linear -> gamma-encoded, `[0,1]`.
pub fn srgb_oetf(linear: f32) -> f32 {
    if linear <= 0.003_130_8 {
        linear * 12.92
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    }
}

/// sRGB inverse transfer function: gamma-encoded -> linear, `[0,1]`.
pub fn srgb_eotf(encoded: f32) -> f32 {
    if encoded <= 0.040_45 {
        encoded / 12.92
    } else {
        ((encoded + 0.055) / 1.055).powf(2.4)
    }
}

fn to_linear_f32(img: &RgbImage) -> Vec<f32> {
    img.pixels()
        .flat_map(|p| p.0.iter().map(|&c| srgb_eotf(c as f32 / 255.0)))
        .collect()
}

fn from_linear_f32(buf: &[f32], width: u32, height: u32) -> RgbImage {
    ImageBuffer::from_fn(width, height, |x, y| {
        let idx = ((y * width + x) * 3) as usize;
        Rgb([
            (srgb_oetf(buf[idx]) * 255.0).round().clamp(0.0, 255.0) as u8,
            (srgb_oetf(buf[idx + 1]) * 255.0).round().clamp(0.0, 255.0) as u8,
            (srgb_oetf(buf[idx + 2]) * 255.0).round().clamp(0.0, 255.0) as u8,
        ])
    })
}

/// Resizes `src` to `dst_width`x`dst_height` via `fast_image_resize`'s Lanczos3 convolution over
/// a linear-light `f32` buffer, then re-encodes to sRGB `u8`.
pub fn resize_fast_linear(
    src: &RgbImage,
    dst_width: u32,
    dst_height: u32,
) -> anyhow::Result<RgbImage> {
    let (src_width, src_height) = src.dimensions();
    let linear = to_linear_f32(src);
    let linear_bytes = bytemuck::cast_slice(&linear).to_vec();

    let src_image = FrImage::from_vec_u8(src_width, src_height, linear_bytes, PixelType::F32x3)
        .map_err(|e| anyhow::anyhow!("building src fast_image_resize buffer: {e}"))?;
    let mut dst_image = FrImage::new(dst_width, dst_height, PixelType::F32x3);

    let mut resizer = Resizer::new();
    let options = ResizeOptions {
        algorithm: ResizeAlg::Convolution(FilterType::Lanczos3),
        // sRGB EOTF/OETF already applied by hand above -- resizing here is genuinely linear, so
        // fast_image_resize's own alpha premultiplication step (which assumes gamma-space RGB) is
        // irrelevant; there's no alpha channel in this f32x3 buffer at all.
        mul_div_alpha: false,
        ..Default::default()
    };
    resizer
        .resize(&src_image, &mut dst_image, &options)
        .map_err(|e| anyhow::anyhow!("fast_image_resize resize: {e}"))?;

    let dst_linear: &[f32] = bytemuck::cast_slice(dst_image.buffer());
    Ok(from_linear_f32(dst_linear, dst_width, dst_height))
}

/// Naive baseline: `image::imageops::resize` (Lanczos3) directly over gamma-encoded `u8` pixels,
/// no linear-light conversion. This is what a pipeline gets "for free" without the extra
/// conversion `resize_fast_linear` does.
pub fn resize_image_crate_srgb(src: &RgbImage, dst_width: u32, dst_height: u32) -> RgbImage {
    image::imageops::resize(
        src,
        dst_width,
        dst_height,
        image::imageops::FilterType::Lanczos3,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_prowl::golden::ssim;

    fn high_contrast_checkerboard(width: u32, height: u32) -> RgbImage {
        RgbImage::from_fn(width, height, |x, y| {
            if (x / 2 + y / 2) % 2 == 0 {
                Rgb([255, 255, 255])
            } else {
                Rgb([0, 0, 0])
            }
        })
    }

    #[test]
    fn srgb_oetf_eotf_roundtrip() {
        for i in 0..=255u16 {
            let encoded = i as f32 / 255.0;
            let linear = srgb_eotf(encoded);
            let back = srgb_oetf(linear);
            assert!(
                (back - encoded).abs() < 1e-4,
                "roundtrip failed at {encoded}: got {back}"
            );
        }
    }

    #[test]
    fn linear_resize_produces_correct_dimensions() {
        let src = high_contrast_checkerboard(256, 256);
        let out = resize_fast_linear(&src, 64, 64).unwrap();
        assert_eq!(out.dimensions(), (64, 64));
    }

    #[test]
    fn linear_resize_agrees_with_srgb_resize_on_flat_image() {
        // A flat-color image has no gamma-vs-linear averaging difference to expose -- both
        // candidates must reduce to the same output color, modulo rounding.
        let src = RgbImage::from_pixel(64, 64, Rgb([128, 128, 128]));
        let linear_out = resize_fast_linear(&src, 16, 16).unwrap();
        let srgb_out = resize_image_crate_srgb(&src, 16, 16);
        let score = ssim(&linear_out, &srgb_out);
        assert!(
            score > 0.99,
            "expected near-identical flat resize, got {score}"
        );
    }

    #[test]
    fn linear_resize_differs_from_naive_srgb_resize_on_high_contrast_edges() {
        // This is the whole point of the decision rule: resizing in linear light vs. gamma space
        // gives measurably different results on a high-contrast image, because averaging
        // gamma-encoded values is not the same as averaging light. If this test ever starts
        // failing (scores converge), the linear conversion above has a bug -- silently resizing
        // in gamma space anyway.
        let src = high_contrast_checkerboard(256, 256);
        let linear_out = resize_fast_linear(&src, 32, 32).unwrap();
        let srgb_out = resize_image_crate_srgb(&src, 32, 32);
        let score = ssim(&linear_out, &srgb_out);
        assert!(
            score < 0.95,
            "expected linear vs. gamma-space resize to diverge on high-contrast content, got {score}"
        );
    }
}
