//! Crop/rotate/zoom/pan as a geometry-only sample pass -- #44's own ticket body: "Crop as geometry
//! only: crop mode is a vertex transform over the already-rendered frame, nothing upstream
//! re-renders while dragging." This module is the CPU reference `gpu.rs`'s `present_sample.wgsl`
//! kernel is checked against; both read the same already-rendered live-suffix output and only
//! remap sample coordinates, never touching a Baked stage.

use serde::{Deserialize, Serialize};

/// A 2D affine transform from *output* pixel coordinates to *source* (already-rendered) pixel
/// coordinates -- i.e. `source = matrix * output`, the direction a sampler actually needs (given
/// an output pixel, where do I read from). Row-major 2x3 (no perspective; crop/rotate/zoom/pan are
/// all affine).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Affine2D {
    pub m: [[f32; 3]; 2],
}

impl Affine2D {
    pub fn identity() -> Self {
        Self {
            m: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
        }
    }

    /// Crop to `[x0, y0, x1, y1)` (source pixel coords) mapped onto an `out_w x out_h` output --
    /// the common case a crop-rectangle drag produces, expressed as scale + translate.
    pub fn crop(x0: f32, y0: f32, x1: f32, y1: f32, out_w: f32, out_h: f32) -> Self {
        let sx = (x1 - x0) / out_w;
        let sy = (y1 - y0) / out_h;
        Self {
            m: [[sx, 0.0, x0], [0.0, sy, y0]],
        }
    }

    /// Rotation by `radians` about `(cx, cy)` (source coords), composed after any existing
    /// transform -- straighten/rotate applied on top of a crop, matching how a real edit stack
    /// would compose the two (rotate is a develop-panel slider, crop is a separate tool, but both
    /// are geometry-only per this module's whole premise).
    pub fn then_rotate(self, radians: f32, cx: f32, cy: f32) -> Self {
        let (s, c) = radians.sin_cos();
        // Rotation matrix R applied to (source - center) + center, then composed with self:
        // new_source = R * (self * output - center) + center
        let r = [[c, -s], [s, c]];
        let mut out = [[0.0f32; 3]; 2];
        for (row, r_row) in r.iter().enumerate() {
            for (col, out_val) in out[row].iter_mut().take(2).enumerate() {
                *out_val = r_row[0] * self.m[0][col] + r_row[1] * self.m[1][col];
            }
            let t = r_row[0] * (self.m[0][2] - cx)
                + r_row[1] * (self.m[1][2] - cy)
                + if row == 0 { cx } else { cy };
            out[row][2] = t;
        }
        Self { m: out }
    }

    pub fn apply(&self, ox: f32, oy: f32) -> (f32, f32) {
        let sx = self.m[0][0] * ox + self.m[0][1] * oy + self.m[0][2];
        let sy = self.m[1][0] * ox + self.m[1][1] * oy + self.m[1][2];
        (sx, sy)
    }
}

/// A plain RGBA f32 plane, matching `spikes/siamese/src/image.rs::Image`'s shape closely enough
/// to share test-writing idioms, but defined locally since spikes don't depend on each other.
pub struct Plane {
    pub width: usize,
    pub height: usize,
    pub data: Vec<[f32; 4]>,
}

impl Plane {
    pub fn new(width: usize, height: usize, fill: [f32; 4]) -> Self {
        Self {
            width,
            height,
            data: vec![fill; width * height],
        }
    }

    fn get_clamped(&self, x: i32, y: i32) -> [f32; 4] {
        let xc = x.clamp(0, self.width as i32 - 1) as usize;
        let yc = y.clamp(0, self.height as i32 - 1) as usize;
        self.data[yc * self.width + xc]
    }

    fn sample_bilinear(&self, sx: f32, sy: f32) -> [f32; 4] {
        let x0 = sx.floor();
        let y0 = sy.floor();
        let (tx, ty) = (sx - x0, sy - y0);
        let (x0, y0) = (x0 as i32, y0 as i32);
        let p00 = self.get_clamped(x0, y0);
        let p10 = self.get_clamped(x0 + 1, y0);
        let p01 = self.get_clamped(x0, y0 + 1);
        let p11 = self.get_clamped(x0 + 1, y0 + 1);
        let mut out = [0.0f32; 4];
        for c in 0..4 {
            let top = p00[c] + (p10[c] - p00[c]) * tx;
            let bottom = p01[c] + (p11[c] - p01[c]) * tx;
            out[c] = top + (bottom - top) * ty;
        }
        out
    }
}

/// CPU reference for the present/sample pass: for every output pixel, map through `transform` to
/// find where to sample `source`, then bilinear-sample. This never reads from or writes to
/// anything Baked -- the whole point of "crop is geometry only."
pub fn sample(source: &Plane, transform: &Affine2D, out_w: usize, out_h: usize) -> Plane {
    let mut out = Plane::new(out_w, out_h, [0.0; 4]);
    for oy in 0..out_h {
        for ox in 0..out_w {
            let (sx, sy) = transform.apply(ox as f32 + 0.5, oy as f32 + 0.5);
            out.data[oy * out_w + ox] = source.sample_bilinear(sx - 0.5, sy - 0.5);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_transform_reproduces_the_source_at_matching_resolution() {
        let mut source = Plane::new(4, 4, [0.0; 4]);
        for (i, px) in source.data.iter_mut().enumerate() {
            *px = [i as f32, 0.0, 0.0, 1.0];
        }
        let out = sample(&source, &Affine2D::identity(), 4, 4);
        for i in 0..16 {
            assert!((out.data[i][0] - source.data[i][0]).abs() < 1e-3);
        }
    }

    #[test]
    fn crop_samples_only_the_cropped_region() {
        // 8x8 source, solid red in the top-left 4x4 quadrant, solid blue everywhere else.
        let mut source = Plane::new(8, 8, [0.0, 0.0, 1.0, 1.0]);
        for y in 0..4 {
            for x in 0..4 {
                source.data[y * 8 + x] = [1.0, 0.0, 0.0, 1.0];
            }
        }
        let transform = Affine2D::crop(0.0, 0.0, 4.0, 4.0, 4.0, 4.0);
        let out = sample(&source, &transform, 4, 4);
        for px in &out.data {
            assert!(px[0] > 0.9 && px[2] < 0.1, "expected red, got {px:?}");
        }
    }

    #[test]
    fn rotate_about_center_maps_a_known_corner_to_the_opposite_corner() {
        let mut source = Plane::new(4, 4, [0.0; 4]);
        source.data[0] = [1.0, 0.0, 0.0, 1.0]; // top-left corner is red
        let transform = Affine2D::identity().then_rotate(std::f32::consts::PI, 2.0, 2.0);
        let out = sample(&source, &transform, 4, 4);
        // A 180-degree rotation about the center should move the top-left content to roughly the
        // bottom-right corner.
        let br = out.data[3 * 4 + 3];
        assert!(br[0] > 0.3, "expected reddish bottom-right, got {br:?}");
    }
}
