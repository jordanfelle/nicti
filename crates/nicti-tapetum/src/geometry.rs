//! Crop as a geometry-only affine sample pass (ADR-0044): reads only the live suffix's own
//! output, never a baked or live node's input directly. `Affine2D` maps an *output* pixel
//! coordinate to the *input* coordinate to sample -- the inverse of "where does this input pixel
//! end up," which is what a sampling pass actually needs.

use crate::color;

/// A 2D affine transform: `(x, y) -> (a*x + b*y + tx, c*x + d*y + ty)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Affine2D {
    pub a: f32,
    pub b: f32,
    pub c: f32,
    pub d: f32,
    pub tx: f32,
    pub ty: f32,
}

impl Affine2D {
    pub const IDENTITY: Affine2D = Affine2D {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        tx: 0.0,
        ty: 0.0,
    };

    /// The inverse-sample transform for a crop with top-left origin `(x, y)`: output pixel
    /// `(0,0)` samples input `(x, y)`, every other output pixel maps 1:1 (no rescale) -- a plain
    /// crop, not a crop+zoom. The output's own extent (how far the crop rect actually extends) is
    /// the caller's `RenderRequest::extent`, not a parameter here.
    pub fn crop(x: f32, y: f32) -> Affine2D {
        Affine2D {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            tx: x,
            ty: y,
        }
    }

    pub fn apply(&self, p: (f32, f32)) -> (f32, f32) {
        (
            self.a * p.0 + self.b * p.1 + self.tx,
            self.c * p.0 + self.d * p.1 + self.ty,
        )
    }
}

/// CPU reference for the geometry pass's bilinear sample -- what `present_sample.wgsl` computes
/// per pixel, used to prove the GPU kernel matches it. `src` is row-major RGBA f32, `src_extent`
/// is `(width, height)`. Samples outside `[0, width) x [0, height)` clamp to the nearest edge
/// texel (never wrap, never a transparent border) -- a crop is expected to only ever request
/// coordinates inside the source, so edge behavior here is a safety net, not a designed feature.
pub fn sample_bilinear(
    src: &[[f32; 4]],
    src_extent: (u32, u32),
    transform: &Affine2D,
    out_xy: (u32, u32),
) -> [f32; 4] {
    let (sx, sy) = transform.apply((out_xy.0 as f32 + 0.5, out_xy.1 as f32 + 0.5));
    // Texel-center convention: subtract 0.5 to convert from continuous sample position back to
    // an array index space where texel n's center is at n.0, matching `present_sample.wgsl`.
    let sx = (sx - 0.5).clamp(0.0, (src_extent.0 - 1) as f32);
    let sy = (sy - 0.5).clamp(0.0, (src_extent.1 - 1) as f32);
    let x0 = sx.floor() as u32;
    let y0 = sy.floor() as u32;
    let x1 = (x0 + 1).min(src_extent.0 - 1);
    let y1 = (y0 + 1).min(src_extent.1 - 1);
    let fx = sx - x0 as f32;
    let fy = sy - y0 as f32;

    let at = |x: u32, y: u32| -> [f32; 4] { src[(y * src_extent.0 + x) as usize] };
    let c00 = at(x0, y0);
    let c10 = at(x1, y0);
    let c01 = at(x0, y1);
    let c11 = at(x1, y1);

    let mut out = [0.0f32; 4];
    for i in 0..4 {
        let top = c00[i] * (1.0 - fx) + c10[i] * fx;
        let bottom = c01[i] * (1.0 - fx) + c11[i] * fx;
        out[i] = top * (1.0 - fy) + bottom * fy;
    }
    out
}

/// Linear ProPhoto RGB (the working space) -> linear sRGB -> sRGB OETF, for readback/golden-image
/// comparison only -- the live suffix and geometry pass themselves never touch this; a real
/// display/export color-managed path is #42's scope.
pub fn output_encode(prophoto_linear: [f32; 3]) -> [f32; 3] {
    let srgb_linear = color::mat3_apply(color::prophoto_to_srgb_linear_matrix(), prophoto_linear);
    srgb_linear.map(color::srgb_oetf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_transform_samples_the_same_pixel() {
        let src = vec![[1.0, 2.0, 3.0, 1.0]; 16];
        let out = sample_bilinear(&src, (4, 4), &Affine2D::IDENTITY, (2, 2));
        assert_eq!(out, [1.0, 2.0, 3.0, 1.0]);
    }

    #[test]
    fn crop_offset_shifts_the_sampled_pixel() {
        let mut src = vec![[0.0, 0.0, 0.0, 1.0]; 16];
        src[5] = [9.0, 9.0, 9.0, 1.0]; // (x=1, y=1) in a 4-wide row-major grid
        let transform = Affine2D::crop(1.0, 1.0);
        let out = sample_bilinear(&src, (4, 4), &transform, (0, 0));
        assert_eq!(out, [9.0, 9.0, 9.0, 1.0]);
    }

    #[test]
    fn out_of_bounds_sample_clamps_to_the_edge_not_black() {
        let src = vec![[5.0, 5.0, 5.0, 1.0]; 4]; // 2x2, all one color
        let transform = Affine2D::crop(-10.0, -10.0);
        let out = sample_bilinear(&src, (2, 2), &transform, (0, 0));
        assert_eq!(out, [5.0, 5.0, 5.0, 1.0]);
    }

    #[test]
    fn output_encode_maps_working_space_white_close_to_srgb_white() {
        let out = output_encode([1.0, 1.0, 1.0]);
        for c in out {
            assert!((c - 1.0).abs() < 0.01, "expected near-white, got {c}");
        }
    }

    #[test]
    fn output_encode_maps_black_to_black() {
        let out = output_encode([0.0, 0.0, 0.0]);
        for c in out {
            assert!(c.abs() < 1e-5);
        }
    }
}
