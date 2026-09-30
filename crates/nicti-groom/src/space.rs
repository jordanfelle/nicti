//! Mapping between the heal stage's color space and what the models expect.
//!
//! The heal stage sees linear *camera* RGB -- before white balance and before the camera matrix
//! (ADR-0044), typically a green-heavy, dark image. MobileSAM and LaMa were trained on ordinary
//! display-referred sRGB photos, so an object mask or inpaint on the raw camera values would work
//! badly. [`SpaceMap`] is a per-image, invertible-per-channel stand-in for "what the photo looks
//! like": as-shot white-balance gains, one exposure scale chosen from the image's own highlights,
//! then the sRGB transfer curve. The inverse maps a model's output back into camera space so the GPU
//! can blend it in directly.
//!
//! It is deliberately *not* the render pipeline's real color transform (a DCP profile, tone curves
//! and so on): the models only need a natural-looking image, and the mapping must be exactly
//! invertible so unmasked pixels round-trip unchanged.

use crate::PixelSource;

/// Percentile of the (white-balanced) per-pixel max channel mapped to model white.
const HIGHLIGHT_PERCENTILE: f64 = 0.99;
/// Cap on samples used to find that percentile, so it costs the same on a thumbnail and a 45 MP
/// frame.
const MAX_SAMPLES: usize = 1 << 16;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpaceMap {
    /// Per-channel white-balance multipliers, green fixed at 1.
    pub gain: [f32; 3],
    /// Single exposure scale applied after `gain`.
    pub scale: f32,
}

fn srgb_oetf(v: f32) -> f32 {
    if v <= 0.003_130_8 {
        12.92 * v
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

fn srgb_eotf(v: f32) -> f32 {
    if v <= 0.040_45 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

impl SpaceMap {
    /// Builds the mapping for `source` from the camera's as-shot multipliers (`cam_mul`, R/G/B/G2).
    /// A missing or non-finite multiplier falls back to 1, i.e. no white balance on that channel.
    pub fn for_source(cam_mul: [f32; 4], source: &dyn PixelSource) -> Self {
        let g = cam_mul[1];
        let ok = |m: f32| {
            if m.is_finite() && m > 0.0 && g.is_finite() && g > 0.0 {
                m / g
            } else {
                1.0
            }
        };
        let gain = [ok(cam_mul[0]), 1.0, ok(cam_mul[2])];

        let (w, h) = (source.width() as usize, source.height() as usize);
        let total = w * h;
        let stride = (total / MAX_SAMPLES).max(1);
        let mut maxes: Vec<f32> = (0..total)
            .step_by(stride)
            .map(|i| {
                let p = source.pixel((i % w) as u32, (i / w) as u32);
                (p[0] * gain[0]).max(p[1] * gain[1]).max(p[2] * gain[2])
            })
            .filter(|v| v.is_finite())
            .collect();
        maxes.sort_by(|a, b| a.total_cmp(b));
        let white = maxes
            .get(
                ((maxes.len() as f64 * HIGHLIGHT_PERCENTILE) as usize)
                    .min(maxes.len().saturating_sub(1)),
            )
            .copied()
            .unwrap_or(1.0)
            .max(1e-3);
        Self {
            gain,
            scale: 1.0 / white,
        }
    }

    /// Camera-linear RGB -> display-referred model RGB in [0, 1] (clipping above model white).
    pub fn to_model(&self, cam: [f32; 3]) -> [f32; 3] {
        let f = |c: usize| srgb_oetf((cam[c] * self.gain[c] * self.scale).clamp(0.0, 1.0));
        [f(0), f(1), f(2)]
    }

    /// The inverse of [`Self::to_model`] (for values that did not clip).
    pub fn to_camera(&self, model: [f32; 3]) -> [f32; 3] {
        let f = |c: usize| srgb_eotf(model[c].clamp(0.0, 1.0)) / (self.gain[c] * self.scale);
        [f(0), f(1), f(2)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RgbBuffer;

    fn buffer(w: u32, h: u32, f: impl Fn(u32, u32) -> [f32; 3]) -> RgbBuffer {
        RgbBuffer {
            width: w,
            height: h,
            data: (0..w * h).map(|i| f(i % w, i / w)).collect(),
        }
    }

    #[test]
    fn round_trips_unclipped_camera_values() {
        // A dark, green-heavy "camera" image with a bright-ish corner.
        let img = buffer(32, 32, |x, y| {
            let t = (x + y) as f32 / 62.0;
            [0.05 + 0.2 * t, 0.2 + 0.5 * t, 0.03 + 0.15 * t]
        });
        let map = SpaceMap::for_source([2.1, 1.0, 1.6, 1.0], &img);
        for &(x, y) in &[(0u32, 0u32), (5, 9), (16, 16), (20, 3)] {
            let cam = img.pixel(x, y);
            let back = map.to_camera(map.to_model(cam));
            for c in 0..3 {
                assert!(
                    (back[c] - cam[c]).abs() < 1e-4 * (1.0 + cam[c]),
                    "({x},{y}) c{c}: {} -> {}",
                    cam[c],
                    back[c]
                );
            }
        }
    }

    #[test]
    fn model_values_stay_in_unit_range_and_bright_pixels_clip() {
        let img = buffer(16, 16, |x, _| {
            let v = if x == 15 { 50.0 } else { 0.1 };
            [v, v, v]
        });
        let map = SpaceMap::for_source([1.0; 4], &img);
        for y in 0..16 {
            for x in 0..16 {
                let m = map.to_model(img.pixel(x, y));
                assert!(m.iter().all(|c| (0.0..=1.0).contains(c)), "{m:?}");
            }
        }
        // Above model white clips to (numerically) 1.0; below black to exactly 0.
        for v in map.to_model([100.0, 100.0, 100.0]) {
            assert!((v - 1.0).abs() < 1e-6, "{v}");
        }
        assert_eq!(map.to_model([-1.0, -1.0, -1.0]), [0.0, 0.0, 0.0]);
    }

    #[test]
    fn white_balance_lifts_the_camera_red_and_blue_toward_green() {
        // A neutral gray under a typical camera WB reads red/blue-dark in raw.
        let raw = [0.25f32, 0.5, 0.31];
        let img = buffer(8, 8, |_, _| raw);
        let map = SpaceMap::for_source([2.0, 1.0, 1.6129, 1.0], &img);
        let m = map.to_model(raw);
        assert!(
            (m[0] - m[1]).abs() < 0.01 && (m[2] - m[1]).abs() < 0.01,
            "{m:?}"
        );
    }

    #[test]
    fn bad_multipliers_fall_back_to_no_white_balance() {
        let img = buffer(4, 4, |_, _| [0.3, 0.3, 0.3]);
        let map = SpaceMap::for_source([f32::NAN, 0.0, -1.0, 1.0], &img);
        assert_eq!(map.gain, [1.0, 1.0, 1.0]);
        assert!(map.scale.is_finite() && map.scale > 0.0);
    }

    #[test]
    fn an_all_black_image_still_gives_a_finite_mapping() {
        let img = buffer(4, 4, |_, _| [0.0; 3]);
        let map = SpaceMap::for_source([1.0; 4], &img);
        assert!(map.scale.is_finite());
        assert_eq!(map.to_model([0.0; 3]), [0.0; 3]);
    }
}
