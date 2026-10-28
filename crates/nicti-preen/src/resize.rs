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

/// Lanczos3 support (taps each side at scale 1).
const LANCZOS_A: f64 = 3.0;

fn lanczos3(x: f64) -> f64 {
    if x == 0.0 {
        return 1.0;
    }
    if x.abs() >= LANCZOS_A {
        return 0.0;
    }
    let px = std::f64::consts::PI * x;
    LANCZOS_A * px.sin() * (px / LANCZOS_A).sin() / (px * px)
}

/// The size, in the *sensor frame's* orientation, a `w` x `h` frame is resized to for `spec` (what
/// [`crate::export_frame`] computes), or `None` when it is not a plain shrink: same size, any
/// enlargement, or a mode where resizing the result again could move it a pixel. Only a
/// `Some` target is safe to produce ahead of `export_frame` (which then finds nothing to resize).
pub fn shrink_target(
    w: u32,
    h: u32,
    orientation: crate::orient::Orientation,
    spec: &ResizeSpec,
) -> Option<(u32, u32)> {
    let (ow, oh) = orientation.oriented_size(w, h);
    let (tw, th) = target_size(ow, oh, spec);
    let (rw, rh) = orientation.oriented_size(tw, th);
    if rw > w || rh > h || (rw, rh) == (w, h) {
        return None;
    }
    // Idempotent: a second pass over the result must change nothing, or `export_frame` would
    // resize again (a 1 px Lanczos re-blur).
    let (ow2, oh2) = orientation.oriented_size(rw, rh);
    (target_size(ow2, oh2, spec) == (ow2, oh2)).then_some((rw, rh))
}

/// Per destination index: the first source index and its normalised weights, computed like
/// `fast_image_resize`'s convolution (support widens by the scale when shrinking).
fn axis_weights(src: u32, dst: u32) -> Vec<(u32, Vec<f32>)> {
    let scale = f64::from(src) / f64::from(dst);
    let filter_scale = scale.max(1.0);
    let support = LANCZOS_A * filter_scale;
    (0..dst)
        .map(|i| {
            let center = (f64::from(i) + 0.5) * scale;
            let lo = ((center - support).floor().max(0.0)) as u32;
            let hi = (((center + support).ceil()).min(f64::from(src))) as u32;
            let hi = hi.max(lo + 1).min(src);
            let mut w: Vec<f64> = (lo..hi)
                .map(|j| lanczos3((f64::from(j) + 0.5 - center) / filter_scale))
                .collect();
            let sum: f64 = w.iter().sum();
            if sum.abs() > 1e-12 {
                w.iter_mut().for_each(|v| *v /= sum);
            } else {
                // Degenerate (cannot happen for Lanczos3 inside its support): nearest sample.
                w.iter_mut().for_each(|v| *v = 0.0);
                w[(center as usize).clamp(lo as usize, hi as usize - 1) - lo as usize] = 1.0;
            }
            (lo, w.into_iter().map(|v| v as f32).collect())
        })
        .collect()
}

/// Per source index: the destination indices it contributes to and with what weight.
fn invert(weights: &[(u32, Vec<f32>)], src: u32) -> Vec<Vec<(u32, f32)>> {
    let mut inv = vec![Vec::new(); src as usize];
    for (dst, (lo, ws)) in weights.iter().enumerate() {
        for (k, &w) in ws.iter().enumerate() {
            inv[*lo as usize + k].push((dst as u32, w));
        }
    }
    inv
}

/// The same Lanczos3 resize as [`resize_linear_f32`], fed one tile at a time so the full-size
/// frame is never resident (#336): memory is the *output* size, not the source's. The convolution
/// is separable and linear, so each source pixel is added into the destination pixels it
/// contributes to, in any tile order. Every source pixel must be added exactly once before
/// [`Self::finish`] (weights are normalised per destination pixel, so a gap would darken it).
pub struct StreamResizer {
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
    h_inv: Vec<Vec<(u32, f32)>>,
    v_inv: Vec<Vec<(u32, f32)>>,
    /// First and last destination column each source column touches.
    h_span: Vec<(u32, u32)>,
    acc: Vec<f32>,
    tmp: Vec<f32>,
}

impl StreamResizer {
    /// `acc` is `dst_w * dst_h * 3` f32, reserved fallibly: too big fails this photo, not the app.
    pub fn try_new(src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Result<Self, ResizeError> {
        if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
            return Err(ResizeError::Empty(src_w.min(dst_w), src_h.min(dst_h)));
        }
        let len = (dst_w as usize)
            .checked_mul(dst_h as usize)
            .and_then(|n| n.checked_mul(3))
            .ok_or_else(|| ResizeError::Resize(format!("{dst_w}x{dst_h} is too large")))?;
        let mut acc = Vec::new();
        acc.try_reserve_exact(len).map_err(|_| {
            ResizeError::Resize(format!(
                "not enough memory for a {dst_w}x{dst_h} export ({} MB needed)",
                len * 4 / 1_000_000
            ))
        })?;
        acc.resize(len, 0.0);
        let h_inv = invert(&axis_weights(src_w, dst_w), src_w);
        let v_inv = invert(&axis_weights(src_h, dst_h), src_h);
        let h_span = h_inv
            .iter()
            .map(|l| {
                let lo = l.iter().map(|&(d, _)| d).min().unwrap_or(0);
                let hi = l.iter().map(|&(d, _)| d).max().unwrap_or(0);
                (lo, hi)
            })
            .collect();
        Ok(Self {
            src_w,
            src_h,
            dst_w,
            dst_h,
            h_inv,
            v_inv,
            h_span,
            acc,
            tmp: vec![0.0; dst_w as usize * 3],
        })
    }

    pub fn dst_size(&self) -> (u32, u32) {
        (self.dst_w, self.dst_h)
    }

    /// Adds the `w` x `h` RGBA tile whose top-left is `(x0, y0)` in the source (alpha ignored).
    /// Parts outside the source are ignored.
    pub fn add_tile_rgba(&mut self, x0: u32, y0: u32, w: u32, h: u32, px: &[[f32; 4]]) {
        debug_assert_eq!(px.len(), w as usize * h as usize);
        let w_eff = w.min(self.src_w.saturating_sub(x0));
        if w_eff == 0 {
            return;
        }
        let (lo, _) = self.h_span[x0 as usize];
        let (_, hi) = self.h_span[(x0 + w_eff - 1) as usize];
        for ty in 0..h {
            let y = y0 + ty;
            if y >= self.src_h {
                break;
            }
            self.tmp[lo as usize * 3..(hi as usize + 1) * 3].fill(0.0);
            let row = &px[(ty * w) as usize..(ty * w + w_eff) as usize];
            for (i, p) in row.iter().enumerate() {
                for &(dx, wt) in &self.h_inv[x0 as usize + i] {
                    let t = dx as usize * 3;
                    self.tmp[t] += wt * p[0];
                    self.tmp[t + 1] += wt * p[1];
                    self.tmp[t + 2] += wt * p[2];
                }
            }
            for &(dy, wy) in &self.v_inv[y as usize] {
                let base = dy as usize * self.dst_w as usize * 3;
                let dst = &mut self.acc[base + lo as usize * 3..base + (hi as usize + 1) * 3];
                let src = &self.tmp[lo as usize * 3..(hi as usize + 1) * 3];
                for (d, s) in dst.iter_mut().zip(src) {
                    *d += wy * s;
                }
            }
        }
    }

    /// The resized interleaved linear RGB frame.
    pub fn finish(self) -> Vec<f32> {
        self.acc
    }
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

    fn test_image(w: u32, h: u32) -> Vec<f32> {
        // Deterministic pseudo-random, with hard edges so a wrong kernel shows.
        let mut state = 0x9E3779B9u32;
        (0..w as usize * h as usize * 3)
            .map(|i| {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                let noise = (state >> 8) as f32 / 16_777_216.0;
                let edge = if (i / 3 % w as usize / 7).is_multiple_of(2) {
                    0.0
                } else {
                    0.8
                };
                edge + 0.2 * noise
            })
            .collect()
    }

    fn stream(
        src: &[f32],
        sw: u32,
        sh: u32,
        dw: u32,
        dh: u32,
        tile: u32,
        shuffle: bool,
    ) -> Vec<f32> {
        let mut r = StreamResizer::try_new(sw, sh, dw, dh).unwrap();
        let mut tiles = Vec::new();
        for y in (0..sh).step_by(tile as usize) {
            for x in (0..sw).step_by(tile as usize) {
                tiles.push((x, y));
            }
        }
        if shuffle {
            tiles.reverse();
        }
        for (x, y) in tiles {
            let (w, h) = (tile.min(sw - x), tile.min(sh - y));
            let mut px = Vec::new();
            for ty in 0..h {
                for tx in 0..w {
                    let i = (((y + ty) * sw + x + tx) * 3) as usize;
                    px.push([src[i], src[i + 1], src[i + 2], 1.0]);
                }
            }
            r.add_tile_rgba(x, y, w, h, &px);
        }
        r.finish()
    }

    #[test]
    fn streaming_matches_the_one_shot_resize_for_any_tiling_and_order() {
        for (sw, sh, dw, dh, tile) in [
            (300, 200, 100, 67, 64),
            (257, 129, 31, 17, 50),
            (200, 300, 200, 150, 77),
            (96, 64, 400, 267, 32),
            (1000, 20, 7, 3, 256),
        ] {
            let src = test_image(sw, sh);
            let one = resize_linear_f32(src.clone(), sw, sh, dw, dh).unwrap();
            for shuffle in [false, true] {
                let got = stream(&src, sw, sh, dw, dh, tile, shuffle);
                assert_eq!(got.len(), one.len());
                let worst = got
                    .iter()
                    .zip(&one)
                    .map(|(a, b)| (a - b).abs())
                    .fold(0.0f32, f32::max);
                assert!(
                    worst < 2e-3,
                    "{sw}x{sh}->{dw}x{dh} tile {tile} shuffle {shuffle}: worst {worst}"
                );
            }
        }
    }

    #[test]
    fn streaming_a_flat_image_stays_flat_and_a_gap_is_visible() {
        let flat = vec![0.4f32; 120 * 80 * 3];
        let out = stream(&flat, 120, 80, 40, 27, 32, false);
        assert!(out.iter().all(|v| (v - 0.4).abs() < 1e-4));
        // Skipping a tile darkens the result (weights are normalised per destination pixel), which
        // is why the contract says every pixel exactly once.
        let mut r = StreamResizer::try_new(64, 64, 16, 16).unwrap();
        let px = vec![[1.0f32, 1.0, 1.0, 1.0]; 32 * 64];
        r.add_tile_rgba(0, 0, 32, 64, &px);
        assert!(r.finish().iter().any(|&v| v < 0.9));
        assert!(StreamResizer::try_new(0, 4, 2, 2).is_err());
    }

    #[test]
    fn shrink_target_is_only_a_plain_idempotent_shrink() {
        use crate::orient::Orientation;
        let spec = |mode| ResizeSpec {
            mode,
            dont_enlarge: true,
        };
        let long = spec(ResizeMode::LongEdge(2048));
        assert_eq!(
            shrink_target(6000, 4000, Orientation::Normal, &long),
            Some((2048, 1365))
        );
        // Not a shrink: smaller than the target, or exactly the size.
        assert_eq!(shrink_target(1000, 800, Orientation::Normal, &long), None);
        assert_eq!(shrink_target(2048, 1365, Orientation::Normal, &long), None);
        assert_eq!(
            shrink_target(6000, 4000, Orientation::Normal, &spec(ResizeMode::None)),
            None
        );
        // Idempotent for every mode over many sizes, or it returns None.
        let modes = [
            ResizeMode::LongEdge(1999),
            ResizeMode::ShortEdge(777),
            ResizeMode::Fit {
                width: 1234,
                height: 987,
            },
            ResizeMode::Megapixels(2.5),
        ];
        for mode in modes {
            for (w, h) in [
                (6000, 4000),
                (4000, 6000),
                (8256, 5504),
                (5999, 4001),
                (7000, 7000),
            ] {
                if let Some((rw, rh)) = shrink_target(w, h, Orientation::Normal, &spec(mode)) {
                    assert_eq!(
                        target_size(rw, rh, &spec(mode)),
                        (rw, rh),
                        "{mode:?} {w}x{h}"
                    );
                    assert!(rw <= w && rh <= h);
                }
            }
        }
    }

    #[test]
    fn shrink_target_handles_rotation_and_never_enlarges() {
        use crate::orient::Orientation;
        let long = ResizeSpec {
            mode: ResizeMode::LongEdge(2048),
            dont_enlarge: true,
        };
        // A portrait shot stored as a 6000x4000 sensor frame: the target is chosen upright
        // (1365x2048) and swapped back to the sensor frame (2048x1365).
        let (rw, rh) = shrink_target(6000, 4000, Orientation::Rotate90Cw, &long).unwrap();
        assert_eq!((rw, rh), (2048, 1365));
        assert_eq!(
            target_size(
                Orientation::Rotate90Cw.oriented_size(rw, rh).0,
                Orientation::Rotate90Cw.oriented_size(rw, rh).1,
                &long
            ),
            Orientation::Rotate90Cw.oriented_size(rw, rh)
        );
        // Enlargement is never a shrink, even when the spec allows enlarging.
        let up = ResizeSpec {
            mode: ResizeMode::LongEdge(4000),
            dont_enlarge: false,
        };
        assert_eq!(shrink_target(1000, 800, Orientation::Normal, &up), None);
    }

    #[test]
    fn a_tile_overhanging_the_frame_is_clipped_not_added_twice() {
        let (sw, sh, dw, dh) = (50u32, 40u32, 10u32, 8u32);
        let src = test_image(sw, sh);
        let want = resize_linear_f32(src.clone(), sw, sh, dw, dh).unwrap();
        let mut r = StreamResizer::try_new(sw, sh, dw, dh).unwrap();
        // 64x64 tiles starting inside the frame, each claiming more than the frame has left.
        for (x0, y0) in [(0u32, 0u32), (32, 0), (0, 32), (32, 32)] {
            let (w, h) = (32u32, 32u32);
            let px: Vec<[f32; 4]> = (0..h)
                .flat_map(|ty| (0..w).map(move |tx| (tx, ty)))
                .map(|(tx, ty)| {
                    let (x, y) = ((x0 + tx).min(sw - 1), (y0 + ty).min(sh - 1));
                    let i = ((y * sw + x) * 3) as usize;
                    [src[i], src[i + 1], src[i + 2], 1.0]
                })
                .collect();
            r.add_tile_rgba(x0, y0, w, h, &px);
        }
        let got = r.finish();
        let worst = got
            .iter()
            .zip(&want)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(worst < 2e-3, "worst {worst}");
    }
}
