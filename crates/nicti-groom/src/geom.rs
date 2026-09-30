//! Pure pixel-geometry helpers for the removal pipeline: bilinear resize, exact Euclidean distance
//! transform (the production replacement for the spike's O(r^2)-per-pixel `feather_mask`), mask
//! bounding boxes and mask growth/feathering. Model-independent, so all of it is tested without
//! any ONNX weights.

/// Half-open pixel rectangle `[x0, x1) x [y0, y1)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

impl Rect {
    pub fn width(&self) -> i32 {
        self.x1 - self.x0
    }
    pub fn height(&self) -> i32 {
        self.y1 - self.y0
    }
    pub fn center(&self) -> (i32, i32) {
        ((self.x0 + self.x1) / 2, (self.y0 + self.y1) / 2)
    }
}

/// The tightest rect containing every `true` cell of `mask` (`w * h`, row-major).
pub fn mask_bounds(mask: &[bool], w: usize, h: usize) -> Option<Rect> {
    debug_assert_eq!(mask.len(), w * h);
    let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0usize, 0usize);
    let mut any = false;
    for y in 0..h {
        for x in 0..w {
            if mask[y * w + x] {
                any = true;
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x + 1);
                y1 = y1.max(y + 1);
            }
        }
    }
    any.then_some(Rect {
        x0: x0 as i32,
        y0: y0 as i32,
        x1: x1 as i32,
        y1: y1 as i32,
    })
}

/// Bilinear resize of an `N`-channel image, pixel-center aligned, edge-clamped.
pub fn resize_bilinear<const N: usize>(
    src: &[[f32; N]],
    sw: usize,
    sh: usize,
    dw: usize,
    dh: usize,
) -> Vec<[f32; N]> {
    debug_assert_eq!(src.len(), sw * sh);
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
        return vec![[0.0; N]; dw * dh];
    }
    let (sx, sy) = (sw as f32 / dw as f32, sh as f32 / dh as f32);
    let mut out = Vec::with_capacity(dw * dh);
    for oy in 0..dh {
        let fy = ((oy as f32 + 0.5) * sy - 0.5).clamp(0.0, (sh - 1) as f32);
        let y0 = fy.floor() as usize;
        let y1 = (y0 + 1).min(sh - 1);
        let ty = fy - y0 as f32;
        for ox in 0..dw {
            let fx = ((ox as f32 + 0.5) * sx - 0.5).clamp(0.0, (sw - 1) as f32);
            let x0 = fx.floor() as usize;
            let x1 = (x0 + 1).min(sw - 1);
            let tx = fx - x0 as f32;
            let mut px = [0.0f32; N];
            for c in 0..N {
                let top = src[y0 * sw + x0][c] * (1.0 - tx) + src[y0 * sw + x1][c] * tx;
                let bot = src[y1 * sw + x0][c] * (1.0 - tx) + src[y1 * sw + x1][c] * tx;
                px[c] = top * (1.0 - ty) + bot * ty;
            }
            out.push(px);
        }
    }
    out
}

/// One row/column of the Felzenszwalb-Huttenlocher squared-distance transform: `f[i]` is the
/// input cost (0 at features, "infinity" elsewhere); returns `min_q ((i - q)^2 + f[q])`.
///
/// f32 is exact enough for the sizes used here (model frames of ~1024 px and crops of at most
/// 2049 px, so `q * q` stays under 2^23); it would start losing low bits somewhere past 4000 px.
fn edt_1d(f: &[f32], out: &mut [f32], v: &mut [usize], z: &mut [f32]) {
    let n = f.len();
    let mut k = 0usize;
    v[0] = 0;
    z[0] = f32::NEG_INFINITY;
    z[1] = f32::INFINITY;
    for q in 1..n {
        let s = loop {
            let p = v[k];
            let s =
                ((f[q] + (q * q) as f32) - (f[p] + (p * p) as f32)) / (2.0 * (q as f32 - p as f32));
            if s <= z[k] && k > 0 {
                k -= 1;
            } else {
                break s;
            }
        };
        k += 1;
        v[k] = q;
        z[k] = s;
        z[k + 1] = f32::INFINITY;
    }
    k = 0;
    for (q, o) in out.iter_mut().enumerate().take(n) {
        while z[k + 1] < q as f32 {
            k += 1;
        }
        let d = q as f32 - v[k] as f32;
        *o = d * d + f[v[k]];
    }
}

const INF: f32 = 1e20;

/// Exact Euclidean distance from every cell to the nearest `true` cell of `feature` (0 on the
/// features themselves; a huge value everywhere if there are none).
pub fn distance_to_feature(feature: &[bool], w: usize, h: usize) -> Vec<f32> {
    debug_assert_eq!(feature.len(), w * h);
    if w == 0 || h == 0 {
        return Vec::new();
    }
    let mut grid: Vec<f32> = feature.iter().map(|&b| if b { 0.0 } else { INF }).collect();
    let m = w.max(h);
    let (mut v, mut z, mut col, mut res) = (
        vec![0usize; m],
        vec![0.0f32; m + 1],
        vec![0.0f32; m],
        vec![0.0f32; m],
    );
    for x in 0..w {
        for y in 0..h {
            col[y] = grid[y * w + x];
        }
        edt_1d(&col[..h], &mut res[..h], &mut v, &mut z);
        for y in 0..h {
            grid[y * w + x] = res[y];
        }
    }
    for y in 0..h {
        col[..w].copy_from_slice(&grid[y * w..(y + 1) * w]);
        edt_1d(&col[..w], &mut res[..w], &mut v, &mut z);
        grid[y * w..(y + 1) * w].copy_from_slice(&res[..w]);
    }
    grid.into_iter().map(|d2| d2.sqrt()).collect()
}

/// Grows `mask` outward by `radius` pixels (Euclidean).
pub fn dilate(mask: &[bool], w: usize, h: usize, radius: f32) -> Vec<bool> {
    if radius <= 0.0 {
        return mask.to_vec();
    }
    distance_to_feature(mask, w, h)
        .into_iter()
        .map(|d| d <= radius)
        .collect()
}

/// Fill weight for a (possibly already grown) mask: `1.0` inside it, falling linearly to `0.0`
/// over `feather` pixels outside it. `feather <= 0` gives a hard `0/1` mask.
pub fn feathered_weight(mask: &[bool], w: usize, h: usize, feather: f32) -> Vec<f32> {
    if feather <= 0.0 {
        return mask.iter().map(|&m| f32::from(m)).collect();
    }
    distance_to_feature(mask, w, h)
        .into_iter()
        .map(|d| (1.0 - d / feather).clamp(0.0, 1.0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brute_force(feature: &[bool], w: usize, h: usize) -> Vec<f32> {
        (0..w * h)
            .map(|i| {
                let (x, y) = ((i % w) as f32, (i / w) as f32);
                feature
                    .iter()
                    .enumerate()
                    .filter(|(_, &f)| f)
                    .map(|(j, _)| {
                        let (fx, fy) = ((j % w) as f32, (j / w) as f32);
                        ((x - fx).powi(2) + (y - fy).powi(2)).sqrt()
                    })
                    .fold(f32::INFINITY, f32::min)
            })
            .collect()
    }

    /// Deterministic pseudo-random mask (no rand dependency).
    fn scattered(w: usize, h: usize, seed: u32, density: u32) -> Vec<bool> {
        let mut s = seed;
        (0..w * h)
            .map(|_| {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                (s >> 24) % 100 < density
            })
            .collect()
    }

    #[test]
    fn distance_transform_matches_brute_force() {
        for (w, h, seed, density) in [
            (17, 11, 1, 5),
            (32, 32, 7, 2),
            (5, 40, 3, 10),
            (1, 9, 9, 30),
        ] {
            let mask = scattered(w, h, seed, density);
            if !mask.iter().any(|&m| m) {
                continue;
            }
            let fast = distance_to_feature(&mask, w, h);
            let slow = brute_force(&mask, w, h);
            for (i, (a, b)) in fast.iter().zip(&slow).enumerate() {
                assert!(
                    (a - b).abs() < 1e-3,
                    "{w}x{h} cell {i}: fast {a} vs brute {b}"
                );
            }
        }
    }

    #[test]
    fn distance_transform_handles_a_single_feature_and_no_features() {
        let mut m = vec![false; 25];
        m[12] = true; // center of 5x5
        let d = distance_to_feature(&m, 5, 5);
        assert_eq!(d[12], 0.0);
        assert!((d[0] - (8.0f32).sqrt()).abs() < 1e-4);
        assert!(distance_to_feature(&[false; 9], 3, 3)
            .iter()
            .all(|&d| d > 1e9));
    }

    #[test]
    fn dilate_grows_by_euclidean_radius() {
        let mut m = vec![false; 81];
        m[40] = true; // center of 9x9
        let d = dilate(&m, 9, 9, 2.0);
        assert!(d[40] && d[40 + 2] && d[40 - 9 * 2]);
        assert!(!d[40 + 3]);
        // (2,2) away has distance sqrt(8) > 2.
        assert!(!d[40 + 9 * 2 + 2]);
        assert_eq!(dilate(&m, 9, 9, 0.0), m);
    }

    #[test]
    fn feather_is_one_inside_zero_far_and_monotone_between() {
        let mut m = vec![false; 21 * 3];
        for x in 8..13 {
            for y in 0..3 {
                m[y * 21 + x] = true;
            }
        }
        let w = feathered_weight(&m, 21, 3, 4.0);
        assert_eq!(w[21 + 10], 1.0);
        assert_eq!(w[21 + 20], 0.0);
        let row: Vec<f32> = (13..21).map(|x| w[21 + x]).collect();
        assert!(row.windows(2).all(|p| p[0] >= p[1]), "{row:?}");
        // Mask columns are 8..=12, so column 13 is 1 px outside it: 1 - 1/4 of the feather.
        assert!((w[21 + 13] - 0.75).abs() < 1e-5, "{}", w[21 + 13]);
        assert!((w[21 + 14] - 0.5).abs() < 1e-5, "{}", w[21 + 14]);
        let hard = feathered_weight(&m, 21, 3, 0.0);
        assert_eq!(hard[21 + 10], 1.0);
        assert_eq!(hard[21 + 13], 0.0);
    }

    #[test]
    fn mask_bounds_are_tight_and_empty_is_none() {
        let mut m = vec![false; 10 * 8];
        m[2 * 10 + 3] = true;
        m[5 * 10 + 7] = true;
        assert_eq!(
            mask_bounds(&m, 10, 8),
            Some(Rect {
                x0: 3,
                y0: 2,
                x1: 8,
                y1: 6
            })
        );
        assert_eq!(mask_bounds(&[false; 4], 2, 2), None);
    }

    #[test]
    fn resize_preserves_flat_fields_and_identity_size() {
        let flat = vec![[0.25f32, 0.5, 0.75]; 6 * 4];
        for px in resize_bilinear(&flat, 6, 4, 13, 9) {
            assert!((px[0] - 0.25).abs() < 1e-6 && (px[2] - 0.75).abs() < 1e-6);
        }
        let ramp: Vec<[f32; 1]> = (0..12).map(|i| [i as f32]).collect();
        assert_eq!(resize_bilinear(&ramp, 4, 3, 4, 3), ramp);
    }

    #[test]
    fn resize_interpolates_a_ramp_linearly() {
        let ramp: Vec<[f32; 1]> = (0..4).map(|x| [x as f32]).collect(); // 4x1: 0,1,2,3
        let up = resize_bilinear(&ramp, 4, 1, 8, 1);
        assert!(up.windows(2).all(|p| p[1][0] >= p[0][0]));
        assert!((up[0][0] - 0.0).abs() < 1e-6 && (up[7][0] - 3.0).abs() < 1e-6);
    }
}
