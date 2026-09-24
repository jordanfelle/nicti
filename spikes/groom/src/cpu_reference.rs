//! Plain-`f32` CPU implementations of clone stamp, gradient-domain (Poisson) spot heal, and a
//! cheap SSD-over-a-ring auto-source-pick. `f32` here simulates linear RGBA16F (per the ticket's
//! "operate on f32/half-float linear RGBA data" instruction) -- half-float storage itself is a
//! render-pipeline concern (Tapetum, #44/#45), not this spike's.
//!
//! `poisson_jacobi_cpu` is checked against `gpu::run_poisson_jacobi`'s WGSL kernel in
//! `tests/correctness.rs` -- both implement the exact same Jacobi update rule so the two are
//! required to agree within float tolerance, not just "look plausible".

/// A simple RGBA `f32` image, row-major. `poisson_jacobi_step` solves the gradient-domain blend
/// over the RGB channels only and passes alpha through unchanged (from `input`, both for
/// boundary and interior pixels) -- alpha is opacity, not color, and gradient-domain blending it
/// as if it were a color channel would be semantically wrong even though every current test uses
/// uniform alpha and so can't observe the difference.
#[derive(Debug, Clone)]
pub struct Image {
    pub width: usize,
    pub height: usize,
    pub data: Vec<[f32; 4]>,
}

impl Image {
    pub fn new(width: usize, height: usize, fill: [f32; 4]) -> Self {
        Self {
            width,
            height,
            data: vec![fill; width * height],
        }
    }

    #[inline]
    pub fn index(&self, x: i32, y: i32) -> Option<usize> {
        if x < 0 || y < 0 || x as usize >= self.width || y as usize >= self.height {
            None
        } else {
            Some(y as usize * self.width + x as usize)
        }
    }

    pub fn get(&self, x: i32, y: i32) -> [f32; 4] {
        self.index(x, y).map(|i| self.data[i]).unwrap_or([0.0; 4])
    }

    pub fn set(&mut self, x: i32, y: i32, v: [f32; 4]) {
        if let Some(i) = self.index(x, y) {
            self.data[i] = v;
        }
    }
}

/// Radial feather weight: `1.0` for `dist <= radius - feather`, ramps linearly to `0.0` at
/// `dist == radius`, and `0.0` beyond. `feather <= 0.0` degenerates to a hard-edged circle.
pub fn feather_weight(dist: f32, radius: f32, feather: f32) -> f32 {
    if radius <= 0.0 {
        return 0.0;
    }
    if dist >= radius {
        return 0.0;
    }
    let feather = feather.max(0.0).min(radius);
    let inner = radius - feather;
    if dist <= inner {
        1.0
    } else {
        ((radius - dist) / (radius - inner)).clamp(0.0, 1.0)
    }
}

/// Clone stamp: copies a circular patch from `src` (offset by `(offset_x, offset_y)` relative to
/// `center`) onto `dst`, blended in with a feathered radial alpha edge so the patch boundary
/// doesn't leave a hard seam.
pub fn clone_stamp(
    dst: &mut Image,
    src: &Image,
    center: (i32, i32),
    offset: (i32, i32),
    radius: f32,
    feather: f32,
) {
    let r = radius.ceil() as i32;
    for dy in -r..=r {
        for dx in -r..=r {
            let dist = ((dx * dx + dy * dy) as f32).sqrt();
            let w = feather_weight(dist, radius, feather);
            if w <= 0.0 {
                continue;
            }
            let (dst_x, dst_y) = (center.0 + dx, center.1 + dy);
            let (src_x, src_y) = (dst_x + offset.0, dst_y + offset.1);
            let src_px = src.get(src_x, src_y);
            let dst_px = dst.get(dst_x, dst_y);
            let mut out = [0.0f32; 4];
            for c in 0..4 {
                out[c] = dst_px[c] + (src_px[c] - dst_px[c]) * w;
            }
            dst.set(dst_x, dst_y, out);
        }
    }
}

/// One Jacobi sweep of the discrete Poisson solve (Perez et al. 2003 "seamless cloning"), over a
/// flat `width * height` buffer. `mask[i] == true` marks an interior/unknown pixel being solved
/// for; `false` pixels are boundary conditions and always copy through unchanged. `guidance` is
/// the source patch supplying the gradient field. This exact update rule is mirrored bit-for-bit
/// (modulo float-op ordering) in `shaders/poisson_jacobi.wgsl`'s `poisson_jacobi` entry point --
/// keep the two in sync.
pub fn poisson_jacobi_step(
    input: &[[f32; 4]],
    guidance: &[[f32; 4]],
    mask: &[bool],
    width: usize,
    height: usize,
    output: &mut [[f32; 4]],
) {
    for y in 0..height {
        for x in 0..width {
            let i = y * width + x;
            if !mask[i] {
                output[i] = input[i];
                continue;
            }
            let mut sum_f = [0.0f32; 3];
            let mut sum_g = [0.0f32; 3];
            let mut n = 0.0f32;
            let mut accumulate = |j: usize| {
                for c in 0..3 {
                    sum_f[c] += input[j][c];
                    sum_g[c] += guidance[i][c] - guidance[j][c];
                }
                n += 1.0;
            };
            if x > 0 {
                accumulate(i - 1);
            }
            if x + 1 < width {
                accumulate(i + 1);
            }
            if y > 0 {
                accumulate(i - width);
            }
            if y + 1 < height {
                accumulate(i + width);
            }
            // Alpha always passes through from `input` unchanged -- it's never part of the
            // gradient-domain solve (see this fn's doc comment).
            let mut out = [0.0f32, 0.0, 0.0, input[i][3]];
            if n > 0.0 {
                for c in 0..3 {
                    out[c] = (sum_f[c] + sum_g[c]) / n;
                }
            } else {
                out = input[i];
            }
            output[i] = out;
        }
    }
}

/// Runs `iterations` Jacobi sweeps, ping-ponging between two buffers (mirroring the GPU kernel's
/// double-buffer dispatch pattern), starting from `initial`. Returns the final buffer.
pub fn poisson_jacobi_cpu(
    guidance: &[[f32; 4]],
    initial: &[[f32; 4]],
    mask: &[bool],
    width: usize,
    height: usize,
    iterations: u32,
) -> Vec<[f32; 4]> {
    let mut a = initial.to_vec();
    let mut b = vec![[0.0f32; 4]; initial.len()];
    for _ in 0..iterations {
        poisson_jacobi_step(&a, guidance, mask, width, height, &mut b);
        std::mem::swap(&mut a, &mut b);
    }
    a
}

/// Extracts a square bounding-box patch of `side` pixels centered on `center` from `image`,
/// clamped at the image edges (out-of-bounds reads return transparent black, matching
/// `Image::get`'s own clamp behavior).
fn extract_patch(image: &Image, center: (i32, i32), side: i32) -> Vec<[f32; 4]> {
    let half = side / 2;
    let mut out = Vec::with_capacity((side * side) as usize);
    for dy in -half..-half + side {
        for dx in -half..-half + side {
            out.push(image.get(center.0 + dx, center.1 + dy));
        }
    }
    out
}

/// Full spot-heal entry point: solves the Poisson blend of `src` (offset by `offset` from
/// `center`) into `dst` over a square patch sized to `2*ceil(radius)+1`, then composites the
/// result back onto `dst` with the same radial feather `clone_stamp` uses, so heal and clone
/// share one blending convention.
pub fn spot_heal(
    dst: &mut Image,
    src: &Image,
    center: (i32, i32),
    offset: (i32, i32),
    radius: f32,
    feather: f32,
    iterations: u32,
) {
    let r = radius.ceil() as i32;
    let side = 2 * r + 1;
    let half = side / 2;

    let initial = extract_patch(dst, center, side);
    let src_center = (center.0 + offset.0, center.1 + offset.1);
    let guidance = extract_patch(src, src_center, side);

    let mask: Vec<bool> = (0..side)
        .flat_map(|dy| {
            (0..side).map(move |dx| {
                let fx = (dx - half) as f32;
                let fy = (dy - half) as f32;
                (fx * fx + fy * fy).sqrt() < radius
            })
        })
        .collect();

    let solved = poisson_jacobi_cpu(
        &guidance,
        &initial,
        &mask,
        side as usize,
        side as usize,
        iterations,
    );

    for dy in -half..-half + side {
        for dx in -half..-half + side {
            let idx = ((dy + half) * side + (dx + half)) as usize;
            let dist = ((dx * dx + dy * dy) as f32).sqrt();
            let w = feather_weight(dist, radius, feather);
            if w <= 0.0 {
                continue;
            }
            let (px, py) = (center.0 + dx, center.1 + dy);
            let dst_px = dst.get(px, py);
            let solved_px = solved[idx];
            let mut out = [0.0f32; 4];
            for c in 0..4 {
                out[c] = dst_px[c] + (solved_px[c] - dst_px[c]) * w;
            }
            dst.set(px, py, out);
        }
    }
}

/// Cheap baseline auto-source-pick: searches `num_candidates` offsets evenly spaced around a
/// ring of radius `search_radius` centered on `center`, comparing each candidate's own
/// surrounding annulus (radius `radius..radius + border`) against `center`'s annulus via sum of
/// squared differences (SSD), and returns the offset with the lowest SSD. Candidates whose own
/// annulus would overlap `center`'s heal disc are skipped, so the search doesn't pick a source
/// that's contaminated by the region being healed. Returns `None` if every candidate overlaps
/// (degenerate case: `search_radius` too small relative to `radius`).
pub fn auto_source_pick(
    image: &Image,
    center: (i32, i32),
    radius: f32,
    search_radius: f32,
    num_candidates: usize,
) -> Option<(i32, i32)> {
    let border = (radius * 0.5).max(2.0);
    let ring_points = annulus_points(radius, radius + border);

    let mut best: Option<((i32, i32), f32)> = None;
    for k in 0..num_candidates {
        let theta = 2.0 * std::f32::consts::PI * (k as f32) / (num_candidates as f32);
        let ox = (search_radius * theta.cos()).round() as i32;
        let oy = (search_radius * theta.sin()).round() as i32;

        // Skip candidates whose annulus would overlap the source disc being healed.
        if (ox * ox + oy * oy) < ((2.0 * (radius + border)) as i32).pow(2) {
            continue;
        }

        let mut ssd = 0.0f32;
        for &(rx, ry) in &ring_points {
            let a = image.get(center.0 + rx, center.1 + ry);
            let b = image.get(center.0 + ox + rx, center.1 + oy + ry);
            for c in 0..3 {
                let d = a[c] - b[c];
                ssd += d * d;
            }
        }

        if best.is_none_or(|(_, best_ssd)| ssd < best_ssd) {
            best = Some(((ox, oy), ssd));
        }
    }
    best.map(|(offset, _)| offset)
}

/// Sample points on a discrete annulus between `inner` and `outer` radius, used as the
/// "known context" ring `auto_source_pick` compares between candidate source locations.
fn annulus_points(inner: f32, outer: f32) -> Vec<(i32, i32)> {
    let r = outer.ceil() as i32;
    let mut points = Vec::new();
    for dy in -r..=r {
        for dx in -r..=r {
            let dist = ((dx * dx + dy * dy) as f32).sqrt();
            if dist >= inner && dist <= outer {
                points.push((dx, dy));
            }
        }
    }
    points
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkerboard(width: usize, height: usize) -> Image {
        let mut img = Image::new(width, height, [0.0, 0.0, 0.0, 1.0]);
        for y in 0..height {
            for x in 0..width {
                let v = if (x / 4 + y / 4) % 2 == 0 { 0.9 } else { 0.1 };
                img.set(x as i32, y as i32, [v, v, v, 1.0]);
            }
        }
        img
    }

    #[test]
    fn feather_weight_is_one_inside_and_zero_outside() {
        assert_eq!(feather_weight(0.0, 5.0, 1.0), 1.0);
        assert_eq!(feather_weight(6.0, 5.0, 1.0), 0.0);
        let mid = feather_weight(4.5, 5.0, 1.0);
        assert!(mid > 0.0 && mid < 1.0);
    }

    #[test]
    fn clone_stamp_copies_offset_source_inside_radius() {
        let src = checkerboard(32, 32);
        let mut dst = Image::new(32, 32, [0.0, 0.0, 0.0, 1.0]);
        clone_stamp(&mut dst, &src, (16, 16), (4, 4), 5.0, 0.0);
        // Center pixel should now match src's pixel at the offset location exactly (hard edge).
        let expected = src.get(20, 20);
        let actual = dst.get(16, 16);
        for c in 0..3 {
            assert!((expected[c] - actual[c]).abs() < 1e-6);
        }
        // Far outside the radius, dst is untouched.
        assert_eq!(dst.get(0, 0), [0.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn poisson_jacobi_boundary_pixels_never_change() {
        let width = 8;
        let height = 8;
        let guidance = vec![[0.5, 0.5, 0.5, 1.0]; width * height];
        let mut initial = vec![[0.2, 0.2, 0.2, 1.0]; width * height];
        // Mark a 4x4 interior block as unknown; everything else is a fixed boundary.
        let mask: Vec<bool> = (0..height)
            .flat_map(|y| (0..width).map(move |x| (2..6).contains(&x) && (2..6).contains(&y)))
            .collect();
        initial[0] = [0.77, 0.77, 0.77, 1.0]; // a distinctive boundary value
        let out = poisson_jacobi_cpu(&guidance, &initial, &mask, width, height, 20);
        assert_eq!(out[0], [0.77, 0.77, 0.77, 1.0]);
    }

    #[test]
    fn spot_heal_and_clone_stamp_are_deterministic() {
        let src = checkerboard(48, 48);
        let mut dst1 = checkerboard(48, 48);
        let mut dst2 = checkerboard(48, 48);
        // Corrupt a region in both copies identically before healing it.
        for img in [&mut dst1, &mut dst2] {
            for y in 20..28 {
                for x in 20..28 {
                    img.set(x, y, [1.0, 0.0, 0.0, 1.0]);
                }
            }
        }
        spot_heal(&mut dst1, &src, (24, 24), (10, 10), 6.0, 1.0, 30);
        spot_heal(&mut dst2, &src, (24, 24), (10, 10), 6.0, 1.0, 30);
        assert_eq!(
            dst1.data, dst2.data,
            "spot_heal must be bit-identical across runs"
        );

        let mut c1 = checkerboard(48, 48);
        let mut c2 = checkerboard(48, 48);
        clone_stamp(&mut c1, &src, (24, 24), (10, 10), 6.0, 1.0);
        clone_stamp(&mut c2, &src, (24, 24), (10, 10), 6.0, 1.0);
        assert_eq!(
            c1.data, c2.data,
            "clone_stamp must be bit-identical across runs"
        );
    }

    #[test]
    fn auto_source_pick_prefers_a_matching_region_over_a_mismatched_one() {
        // A checkerboard is locally self-similar, so build an image with one clearly distinct
        // "scar" far from the search ring and confirm auto_source_pick avoids steering toward it
        // by construction of a controlled two-region image instead of relying on the
        // checkerboard's own repetition (which could coincidentally match many offsets).
        let mut img = Image::new(64, 64, [0.2, 0.2, 0.2, 1.0]);
        // A uniform light region on the left half, matching the area around `center`.
        for y in 0..64 {
            for x in 0..32 {
                img.set(x, y, [0.8, 0.8, 0.8, 1.0]);
            }
        }
        // Force the near-left candidate offset to actually be excluded from ring overlap; place
        // center near the boundary so left offsets land in the light region and right offsets
        // land in the dark region -- then the SSD should prefer whichever side annulus-matches.
        let center = (30, 32);
        let picked = auto_source_pick(&img, center, 4.0, 16.0, 16)
            .expect("some candidate should be selected");
        // The picked offset's annulus SSD should be the minimum found -- re-derive it here to
        // assert the function actually returns the argmin, not just "some" tuple.
        let border = (4.0f32 * 0.5).max(2.0);
        let ring_points: Vec<(i32, i32)> = (-9..=9)
            .flat_map(|dy| {
                (-9..=9).filter_map(move |dx| {
                    let dist = ((dx * dx + dy * dy) as f32).sqrt();
                    (dist >= 4.0 && dist <= 4.0 + border).then_some((dx, dy))
                })
            })
            .collect();
        let ssd_at = |offset: (i32, i32)| -> f32 {
            ring_points
                .iter()
                .map(|&(rx, ry)| {
                    let a = img.get(center.0 + rx, center.1 + ry);
                    let b = img.get(center.0 + offset.0 + rx, center.1 + offset.1 + ry);
                    (0..3).map(|c| (a[c] - b[c]).powi(2)).sum::<f32>()
                })
                .sum()
        };
        let picked_ssd = ssd_at(picked);
        for k in 0..16 {
            let theta = 2.0 * std::f32::consts::PI * (k as f32) / 16.0;
            let ox = (16.0 * theta.cos()).round() as i32;
            let oy = (16.0 * theta.sin()).round() as i32;
            if (ox * ox + oy * oy) < (2.0 * (4.0 + border)).powi(2) as i32 {
                continue;
            }
            assert!(
                ssd_at((ox, oy)) >= picked_ssd - 1e-4,
                "picked offset {picked:?} (ssd={picked_ssd}) is not the argmin over candidate ({ox},{oy})"
            );
        }
    }
}
