//! Upright (#427): automatic and guided perspective correction.
//!
//! Pipeline: line-segment detection (gradient-orientation region growing, an original LSD-style
//! detector -- no third-party LSD code) -> vanishing-point estimation (RANSAC over segment pairs,
//! then an eigenvector refinement over the inliers) -> a virtual camera rotation `K R K^-1`
//! expressed as a [`Homography`], with the focal length supplied by the caller (EXIF). Guided mode
//! skips detection and takes 1-4 user-drawn guide lines instead.
//!
//! Everything is CPU work on a downscaled proxy and resolution independent: segments are returned
//! in source-pixel coordinates and the homography is in source-pixel coordinates, so the same value
//! drives a preview and a full-resolution export.
//!
//! Coordinates are x-right, y-down pixels with the principal point at the image center. A
//! [`Homography`] maps an *output* pixel to the *input* pixel to sample (like
//! `geometry::Affine2D`), and is applied to the source image *before* the crop/straighten.
//!
//! The thresholds below (gradient cut, angle tolerance, inlier angle, minimum lengths) are
//! documented starting points, not tuned on real photos yet; see `docs/decisions/` for the tuning
//! pass.

use crate::auto::{AutoOutcome, AutoReason};

/// The long edge the detector works at.
pub const DETECT_LONG_EDGE: u32 = 800;
/// Region-growing angular tolerance between a pixel's line orientation and the region's mean.
const REGION_ANGLE_TOLERANCE_DEG: f64 = 22.5;
/// Gradient magnitude cut, as a fraction of the 90th percentile of the magnitudes.
const GRADIENT_P90_FRACTION: f64 = 0.35;
/// Absolute floor for the gradient cut (luma 0..1, Sobel / 8), so a flat image yields nothing.
const GRADIENT_FLOOR: f64 = 0.02;
/// A region needs at least this many pixels.
const MIN_REGION_PIXELS: usize = 15;
/// Minimum segment length as a fraction of the proxy's long edge.
const MIN_SEGMENT_FRACTION: f64 = 0.04;
/// Minimum length / width ratio for a region to count as a line.
const MIN_ELONGATION: f64 = 6.0;
/// A segment joins the vertical (horizontal) group when within this many degrees of vertical
/// (horizontal) in the image.
const GROUP_ANGLE_DEG: f64 = 40.0;
/// A segment is an inlier of a vanishing point within this angle (degrees) of its plane.
const INLIER_ANGLE_DEG: f64 = 2.0;
/// Minimum number of inlier segments for a vanishing point to be trusted at all.
const MIN_INLIERS: usize = 3;
/// Minimum fraction of a group's total segment length the inliers must carry for `Confident`.
const CONFIDENT_INLIER_FRACTION: f64 = 0.5;
/// Horizontal VP must be this close (degrees) to orthogonal to the vertical VP.
const ORTHOGONALITY_DEG: f64 = 10.0;
/// RANSAC hypotheses per group.
const RANSAC_ITERATIONS: usize = 300;
/// A correction needing more than this much of any rotation is refused as implausible.
pub const MAX_CORRECTION_DEG: f64 = 40.0;

/// A 3x3 projective transform, row-major, mapping an output pixel `(x, y, 1)` to the input pixel
/// to sample (before the perspective divide).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Homography(pub [[f64; 3]; 3]);

impl Homography {
    pub const IDENTITY: Homography =
        Homography([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]);

    pub fn is_identity(&self) -> bool {
        self.0 == Self::IDENTITY.0
    }

    /// Applies the transform; `None` when the point lands at or behind the camera plane.
    pub fn apply(&self, p: (f64, f64)) -> Option<(f64, f64)> {
        let m = &self.0;
        let w = m[2][0] * p.0 + m[2][1] * p.1 + m[2][2];
        if w <= 1e-9 {
            return None;
        }
        Some((
            (m[0][0] * p.0 + m[0][1] * p.1 + m[0][2]) / w,
            (m[1][0] * p.0 + m[1][1] * p.1 + m[1][2]) / w,
        ))
    }

    /// `self * rhs`: applies `rhs` first.
    pub fn then_after(&self, rhs: &Homography) -> Homography {
        Homography(mat_mul(&self.0, &rhs.0))
    }

    /// The row-major `f32` form a shader uniform wants.
    pub fn to_f32(&self) -> [f32; 9] {
        let m = &self.0;
        [
            m[0][0] as f32,
            m[0][1] as f32,
            m[0][2] as f32,
            m[1][0] as f32,
            m[1][1] as f32,
            m[1][2] as f32,
            m[2][0] as f32,
            m[2][1] as f32,
            m[2][2] as f32,
        ]
    }
}

type Mat3 = [[f64; 3]; 3];
type Vec3 = [f64; 3];

fn mat_mul(a: &Mat3, b: &Mat3) -> Mat3 {
    let mut out = [[0.0; 3]; 3];
    for (i, row) in out.iter_mut().enumerate() {
        for (j, cell) in row.iter_mut().enumerate() {
            *cell = (0..3).map(|k| a[i][k] * b[k][j]).sum();
        }
    }
    out
}

fn transpose(a: &Mat3) -> Mat3 {
    let mut out = [[0.0; 3]; 3];
    for (i, row) in out.iter_mut().enumerate() {
        for (j, cell) in row.iter_mut().enumerate() {
            *cell = a[j][i];
        }
    }
    out
}

fn dot(a: Vec3, b: Vec3) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: Vec3, b: Vec3) -> Vec3 {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn norm(a: Vec3) -> f64 {
    dot(a, a).sqrt()
}

fn normalized(a: Vec3) -> Option<Vec3> {
    let n = norm(a);
    (n > 1e-12).then(|| [a[0] / n, a[1] / n, a[2] / n])
}

fn rot_z(a: f64) -> Mat3 {
    let (s, c) = a.sin_cos();
    [[c, -s, 0.0], [s, c, 0.0], [0.0, 0.0, 1.0]]
}

fn rot_x(a: f64) -> Mat3 {
    let (s, c) = a.sin_cos();
    [[1.0, 0.0, 0.0], [0.0, c, -s], [0.0, s, c]]
}

fn rot_y(a: f64) -> Mat3 {
    let (s, c) = a.sin_cos();
    [[c, 0.0, s], [0.0, 1.0, 0.0], [-s, 0.0, c]]
}

fn mat_vec(m: &Mat3, v: Vec3) -> Vec3 {
    [dot(m[0], v), dot(m[1], v), dot(m[2], v)]
}

/// A detected or user-drawn line segment in source-pixel coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Segment {
    pub a: (f64, f64),
    pub b: (f64, f64),
}

impl Segment {
    pub fn length(&self) -> f64 {
        ((self.b.0 - self.a.0).powi(2) + (self.b.1 - self.a.1).powi(2)).sqrt()
    }
}

/// What a user guide marks: something that should end up vertical or horizontal.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GuideKind {
    Vertical,
    Horizontal,
}

/// One user-drawn guide (guided Upright takes one to four).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Guide {
    pub kind: GuideKind,
    pub segment: Segment,
}

/// Which corrections to apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UprightMode {
    /// Whatever the evidence supports: full when both vanishing points are found, else vertical,
    /// else level.
    Auto,
    /// Roll only.
    Level,
    /// Roll + vertical keystone.
    Vertical,
    /// Roll + vertical + horizontal keystone.
    Full,
}

/// The result of an Upright analysis: the correction plus the evidence behind it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UprightSolution {
    /// Output -> input sample homography in source pixels, already scaled to avoid empty corners.
    pub homography: Homography,
    /// Which corrections were actually applied (`Auto` resolves to one of the others).
    pub mode: UprightMode,
    /// The focal length (px) the solution was solved at.
    pub focal_px: f64,
    pub roll_degrees: f64,
    pub pitch_degrees: f64,
    pub yaw_degrees: f64,
}

// ---------------------------------------------------------------------------------------------
// Segment detection
// ---------------------------------------------------------------------------------------------

/// Box-downsamples a display-encoded RGBA buffer to luma at most [`DETECT_LONG_EDGE`] wide.
/// Returns `(luma, width, height, scale)` where `scale` is proxy pixels per source pixel.
fn proxy_luma(pixels: &[[f32; 4]], width: u32, height: u32) -> (Vec<f32>, usize, usize, f64) {
    let long = width.max(height) as f64;
    let scale = (DETECT_LONG_EDGE as f64 / long).min(1.0);
    let ow = ((width as f64 * scale).round() as usize).max(1);
    let oh = ((height as f64 * scale).round() as usize).max(1);
    let mut out = vec![0.0f32; ow * oh];
    for oy in 0..oh {
        let y0 = (oy as u64 * height as u64 / oh as u64) as usize;
        let y1 = (((oy as u64 + 1) * height as u64 / oh as u64) as usize).max(y0 + 1);
        for ox in 0..ow {
            let x0 = (ox as u64 * width as u64 / ow as u64) as usize;
            let x1 = (((ox as u64 + 1) * width as u64 / ow as u64) as usize).max(x0 + 1);
            let mut sum = 0.0f32;
            let mut n = 0.0f32;
            for y in y0..y1.min(height as usize) {
                for x in x0..x1.min(width as usize) {
                    let p = pixels[y * width as usize + x];
                    sum += (0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2]).clamp(0.0, 1.0);
                    n += 1.0;
                }
            }
            out[oy * ow + ox] = if n > 0.0 { sum / n } else { 0.0 };
        }
    }
    (out, ow, oh, scale)
}

/// Detects straight line segments in a display-encoded RGBA image, returned in source-pixel
/// coordinates. Gradient-orientation region growing: pixels are visited strongest-gradient first
/// and grown into regions of similar line orientation; each elongated region becomes a segment
/// along its principal axis.
pub fn detect_segments(pixels: &[[f32; 4]], width: u32, height: u32) -> Vec<Segment> {
    if width < 8 || height < 8 || pixels.len() != width as usize * height as usize {
        return Vec::new();
    }
    let (luma, w, h, scale) = proxy_luma(pixels, width, height);
    if w < 8 || h < 8 {
        return Vec::new();
    }

    // Sobel gradient on the interior; `orient` is the *line* orientation (perpendicular to the
    // gradient), folded into [0, pi).
    let mut mag = vec![0.0f64; w * h];
    let mut orient = vec![0.0f64; w * h];
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let at = |dx: isize, dy: isize| {
                luma[(y as isize + dy) as usize * w + (x as isize + dx) as usize] as f64
            };
            let gx = (at(1, -1) + 2.0 * at(1, 0) + at(1, 1))
                - (at(-1, -1) + 2.0 * at(-1, 0) + at(-1, 1));
            let gy = (at(-1, 1) + 2.0 * at(0, 1) + at(1, 1))
                - (at(-1, -1) + 2.0 * at(0, -1) + at(1, -1));
            mag[y * w + x] = (gx * gx + gy * gy).sqrt() / 8.0;
            let theta = (-gx).atan2(gy);
            orient[y * w + x] = theta.rem_euclid(std::f64::consts::PI);
        }
    }

    let mut sorted: Vec<f64> = mag.iter().copied().filter(|m| *m > 0.0).collect();
    if sorted.is_empty() {
        return Vec::new();
    }
    sorted.sort_by(f64::total_cmp);
    let p90 = sorted[(sorted.len() * 9 / 10).min(sorted.len() - 1)];
    let cut = (p90 * GRADIENT_P90_FRACTION).max(GRADIENT_FLOOR);

    let mut seeds: Vec<usize> = (0..w * h).filter(|&i| mag[i] > cut).collect();
    seeds.sort_by(|&a, &b| mag[b].total_cmp(&mag[a]));

    let tolerance = REGION_ANGLE_TOLERANCE_DEG.to_radians();
    let min_len = MIN_SEGMENT_FRACTION * w.max(h) as f64;
    let mut used = vec![false; w * h];
    let mut segments = Vec::new();
    let mut stack = Vec::new();
    let mut region = Vec::new();

    for &seed in &seeds {
        if used[seed] {
            continue;
        }
        region.clear();
        stack.clear();
        used[seed] = true;
        stack.push(seed);
        // Running mean orientation as a doubled-angle vector (orientation is mod pi).
        let (mut sx, mut sy) = ((2.0 * orient[seed]).cos(), (2.0 * orient[seed]).sin());
        while let Some(i) = stack.pop() {
            region.push(i);
            let (px, py) = (i % w, i / w);
            let mean = sy.atan2(sx) / 2.0;
            for dy in -1isize..=1 {
                for dx in -1isize..=1 {
                    if dx == 0 && dy == 0 {
                        continue;
                    }
                    let nx = px as isize + dx;
                    let ny = py as isize + dy;
                    if nx < 1 || ny < 1 || nx >= w as isize - 1 || ny >= h as isize - 1 {
                        continue;
                    }
                    let j = ny as usize * w + nx as usize;
                    if used[j] || mag[j] <= cut {
                        continue;
                    }
                    let mut diff = (orient[j] - mean).abs() % std::f64::consts::PI;
                    if diff > std::f64::consts::FRAC_PI_2 {
                        diff = std::f64::consts::PI - diff;
                    }
                    if diff <= tolerance {
                        used[j] = true;
                        sx += (2.0 * orient[j]).cos();
                        sy += (2.0 * orient[j]).sin();
                        stack.push(j);
                    }
                }
            }
        }
        if region.len() < MIN_REGION_PIXELS {
            continue;
        }
        if let Some(seg) = fit_region(&region, w, &mag, min_len) {
            segments.push(Segment {
                a: (seg.a.0 / scale, seg.a.1 / scale),
                b: (seg.b.0 / scale, seg.b.1 / scale),
            });
        }
    }
    segments
}

/// Fits a segment to a pixel region by magnitude-weighted PCA; `None` if it isn't a line.
fn fit_region(region: &[usize], w: usize, mag: &[f64], min_len: f64) -> Option<Segment> {
    let (mut sw, mut mx, mut my) = (0.0, 0.0, 0.0);
    for &i in region {
        let m = mag[i];
        sw += m;
        mx += m * ((i % w) as f64 + 0.5);
        my += m * ((i / w) as f64 + 0.5);
    }
    if sw <= 0.0 {
        return None;
    }
    mx /= sw;
    my /= sw;
    let (mut cxx, mut cxy, mut cyy) = (0.0, 0.0, 0.0);
    for &i in region {
        let m = mag[i];
        let dx = (i % w) as f64 + 0.5 - mx;
        let dy = (i / w) as f64 + 0.5 - my;
        cxx += m * dx * dx;
        cxy += m * dx * dy;
        cyy += m * dy * dy;
    }
    cxx /= sw;
    cxy /= sw;
    cyy /= sw;
    let tr = cxx + cyy;
    let det = cxx * cyy - cxy * cxy;
    let disc = (tr * tr / 4.0 - det).max(0.0).sqrt();
    let (l1, l2) = (tr / 2.0 + disc, (tr / 2.0 - disc).max(0.0));
    // Uniform-density extent of a line: variance = len^2 / 12.
    let len = (12.0 * l1).sqrt();
    let width = (12.0 * l2).sqrt().max(1.0);
    if len < min_len || len / width < MIN_ELONGATION {
        return None;
    }
    let angle = 0.5 * (2.0 * cxy).atan2(cxx - cyy);
    let (dy, dx) = angle.sin_cos();
    Some(Segment {
        a: (mx - dx * len / 2.0, my - dy * len / 2.0),
        b: (mx + dx * len / 2.0, my + dy * len / 2.0),
    })
}

// ---------------------------------------------------------------------------------------------
// Vanishing points
// ---------------------------------------------------------------------------------------------

/// A segment as the unit normal of the plane through the camera center and the segment, in ray
/// space (`(x - cx) / f, (y - cy) / f, 1`), plus its pixel length as a weight.
#[derive(Debug, Clone, Copy)]
struct RayLine {
    normal: Vec3,
    weight: f64,
}

fn ray_line(seg: &Segment, center: (f64, f64), focal: f64) -> Option<RayLine> {
    let ray = |p: (f64, f64)| [(p.0 - center.0) / focal, (p.1 - center.1) / focal, 1.0];
    let normal = normalized(cross(ray(seg.a), ray(seg.b)))?;
    Some(RayLine {
        normal,
        weight: seg.length(),
    })
}

/// A vanishing direction (unit, ray space) and the evidence behind it.
#[derive(Debug, Clone)]
struct VanishingPoint {
    dir: Vec3,
    inliers: usize,
    /// Share of the group's total segment length carried by the inliers.
    support: f64,
}

/// The unit vector minimizing `sum w (n . v)^2`: the eigenvector of the smallest eigenvalue of
/// `sum w n n^T`, found by power iteration on `lambda_max * I - M`.
fn least_squares_direction(lines: &[&RayLine]) -> Option<Vec3> {
    let mut m = [[0.0; 3]; 3];
    for l in lines {
        for (i, row) in m.iter_mut().enumerate() {
            for (j, cell) in row.iter_mut().enumerate() {
                *cell += l.weight * l.normal[i] * l.normal[j];
            }
        }
    }
    let shift = m[0][0] + m[1][1] + m[2][2]; // trace >= lambda_max
    let mut b = [[0.0; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            b[i][j] = if i == j { shift } else { 0.0 } - m[i][j];
        }
    }
    let mut v = [0.577_350_269, 0.577_350_269, 0.577_350_269];
    for _ in 0..200 {
        v = normalized(mat_vec(&b, v))?;
    }
    Some(v)
}

/// xorshift64*: deterministic hypotheses so a given image always yields the same answer.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn score(dir: Vec3, lines: &[RayLine], sin_thr: f64) -> (usize, f64) {
    let mut count = 0;
    let mut weight = 0.0;
    for l in lines {
        if dot(l.normal, dir).abs() < sin_thr {
            count += 1;
            weight += l.weight;
        }
    }
    (count, weight)
}

/// RANSAC + refinement for the dominant vanishing direction of `lines`. `accept` filters
/// hypotheses (e.g. orthogonality to a known vertical VP).
fn estimate_vp(
    lines: &[RayLine],
    accept: impl Fn(Vec3) -> bool,
    seed: u64,
) -> Option<VanishingPoint> {
    if lines.len() < 2 {
        return None;
    }
    let sin_thr = INLIER_ANGLE_DEG.to_radians().sin();
    let min_cross = 0.5f64.to_radians().sin();
    let total: f64 = lines.iter().map(|l| l.weight).sum();
    let mut rng = Rng(seed | 1);
    let mut best: Option<(Vec3, f64)> = None;
    for _ in 0..RANSAC_ITERATIONS {
        let (i, j) = (rng.below(lines.len()), rng.below(lines.len()));
        if i == j {
            continue;
        }
        let c = cross(lines[i].normal, lines[j].normal);
        if norm(c) < min_cross {
            continue;
        }
        let Some(dir) = normalized(c) else { continue };
        if !accept(dir) {
            continue;
        }
        let (_, weight) = score(dir, lines, sin_thr);
        if best.is_none_or(|(_, w)| weight > w) {
            best = Some((dir, weight));
        }
    }
    let (mut dir, _) = best?;
    // Refine on the inliers, twice (the inlier set can grow once the direction improves).
    for _ in 0..2 {
        let inliers: Vec<&RayLine> = lines
            .iter()
            .filter(|l| dot(l.normal, dir).abs() < sin_thr)
            .collect();
        if inliers.len() < 2 {
            break;
        }
        let Some(refined) = least_squares_direction(&inliers) else {
            break;
        };
        // The eigenvector's sign is arbitrary; keep it on the hypothesis' side.
        dir = if dot(refined, dir) < 0.0 {
            [-refined[0], -refined[1], -refined[2]]
        } else {
            refined
        };
        if !accept(dir) {
            return None;
        }
    }
    let (count, weight) = score(dir, lines, sin_thr);
    (count >= MIN_INLIERS).then_some(VanishingPoint {
        dir,
        inliers: count,
        support: if total > 0.0 { weight / total } else { 0.0 },
    })
}

/// Splits segments into near-vertical and near-horizontal groups (by image direction).
fn group_lines(
    segments: &[Segment],
    center: (f64, f64),
    focal: f64,
) -> (Vec<RayLine>, Vec<RayLine>) {
    let limit = GROUP_ANGLE_DEG.to_radians();
    let (mut vertical, mut horizontal) = (Vec::new(), Vec::new());
    for seg in segments {
        let (dx, dy) = (seg.b.0 - seg.a.0, seg.b.1 - seg.a.1);
        if dx == 0.0 && dy == 0.0 {
            continue;
        }
        let from_horizontal = dy.abs().atan2(dx.abs());
        let Some(line) = ray_line(seg, center, focal) else {
            continue;
        };
        if from_horizontal >= std::f64::consts::FRAC_PI_2 - limit {
            vertical.push(line);
        } else if from_horizontal <= limit {
            horizontal.push(line);
        }
    }
    (vertical, horizontal)
}

/// Orients a vertical direction to point down the image (+y) so angles read consistently.
fn orient_vertical(v: Vec3) -> Vec3 {
    if v[1] < 0.0 {
        [-v[0], -v[1], -v[2]]
    } else {
        v
    }
}

fn orient_horizontal(h: Vec3) -> Vec3 {
    if h[0] < 0.0 {
        [-h[0], -h[1], -h[2]]
    } else {
        h
    }
}

/// A vertical VP must lie within [`GROUP_ANGLE_DEG`] + slack of the image's y axis.
fn plausible_vertical(v: Vec3) -> bool {
    v[1].abs() > (60.0f64).to_radians().cos() * norm(v)
}

fn plausible_horizontal(h: Vec3) -> bool {
    h[0].abs() > (60.0f64).to_radians().cos() * norm(h)
}

// ---------------------------------------------------------------------------------------------
// Rotation and homography
// ---------------------------------------------------------------------------------------------

/// Roll that brings a vertical direction's image-plane projection onto the y axis.
fn roll_for_vertical(v: Vec3) -> f64 {
    v[0].atan2(v[1])
}

/// Roll that brings a horizontal direction's image-plane projection onto the x axis.
fn roll_for_horizontal(h: Vec3) -> f64 {
    (-h[1]).atan2(h[0])
}

fn pitch_for(rolled_vertical: Vec3) -> f64 {
    (-rolled_vertical[2]).atan2(rolled_vertical[1])
}

fn yaw_for(h: Vec3) -> f64 {
    h[2].atan2(h[0])
}

/// Builds the output -> input homography for the camera rotation `r` (old rays -> new rays) at
/// focal length `focal` px, then zooms the output (about the center) just enough that no output
/// pixel samples outside the source.
fn homography_for_rotation(r: &Mat3, width: f64, height: f64, focal: f64) -> Homography {
    let (cx, cy) = (width / 2.0, height / 2.0);
    // output (centered px) -> ray -> old ray (R^T) -> centered px.
    let rt = transpose(r);
    let k = [[focal, 0.0, 0.0], [0.0, focal, 0.0], [0.0, 0.0, 1.0]];
    let k_inv = [
        [1.0 / focal, 0.0, 0.0],
        [0.0, 1.0 / focal, 0.0],
        [0.0, 0.0, 1.0],
    ];
    let centered = mat_mul(&k, &mat_mul(&rt, &k_inv));
    let to_center = [[1.0, 0.0, -cx], [0.0, 1.0, -cy], [0.0, 0.0, 1.0]];
    let from_center = [[1.0, 0.0, cx], [0.0, 1.0, cy], [0.0, 0.0, 1.0]];
    let base = Homography(mat_mul(&from_center, &mat_mul(&centered, &to_center)));

    // Largest output window (scale s <= 1 about the center) whose corners all sample inside the
    // source. Corners suffice: the map is a homography and the source is convex.
    let covers = |s: f64| {
        [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)]
            .iter()
            .all(
                |(sx, sy)| match base.apply((cx + sx * cx * s, cy + sy * cy * s)) {
                    Some((x, y)) => x >= 0.0 && y >= 0.0 && x <= width && y <= height,
                    None => false,
                },
            )
    };
    let s = if covers(1.0) {
        1.0
    } else {
        let (mut lo, mut hi) = (0.0, 1.0);
        for _ in 0..40 {
            let mid = (lo + hi) / 2.0;
            if covers(mid) {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        lo
    };
    let zoom = [
        [s, 0.0, cx * (1.0 - s)],
        [0.0, s, cy * (1.0 - s)],
        [0.0, 0.0, 1.0],
    ];
    Homography(mat_mul(&base.0, &zoom))
}

/// The output -> input homography for a stored correction (angles in degrees, as in
/// [`UprightSolution`]) at an image size and focal length in pixels. Because it is rebuilt from
/// angles, the same stored correction serves any render resolution.
pub fn homography_for_angles(
    roll_degrees: f64,
    pitch_degrees: f64,
    yaw_degrees: f64,
    width: f64,
    height: f64,
    focal_px: f64,
) -> Homography {
    let r = mat_mul(
        &rot_y(yaw_degrees.to_radians()),
        &mat_mul(
            &rot_x(pitch_degrees.to_radians()),
            &rot_z(roll_degrees.to_radians()),
        ),
    );
    homography_for_rotation(&r, width, height, focal_px)
}

fn solution(
    mode: UprightMode,
    roll: f64,
    pitch: f64,
    yaw: f64,
    width: f64,
    height: f64,
    focal: f64,
) -> Option<UprightSolution> {
    let limit = MAX_CORRECTION_DEG.to_radians();
    if roll.abs() > limit || pitch.abs() > limit || yaw.abs() > limit {
        return None;
    }
    Some(UprightSolution {
        homography: homography_for_angles(
            roll.to_degrees(),
            pitch.to_degrees(),
            yaw.to_degrees(),
            width,
            height,
            focal,
        ),
        mode,
        focal_px: focal,
        roll_degrees: roll.to_degrees(),
        pitch_degrees: pitch.to_degrees(),
        yaw_degrees: yaw.to_degrees(),
    })
}

/// The rotation angles a mode derives from the vanishing points found.
fn angles_for(
    mode: UprightMode,
    v: Option<&VanishingPoint>,
    h: Option<&VanishingPoint>,
) -> Option<(UprightMode, f64, f64, f64)> {
    let v_dir = v.map(|v| orient_vertical(v.dir));
    let h_dir = h.map(|h| orient_horizontal(h.dir));
    // Roll from whichever family carries more weight; vertical wins ties (it's the tighter cue).
    let roll_only = || match (v, h) {
        (Some(v), Some(h)) if h.support * h.inliers as f64 > v.support * v.inliers as f64 => {
            h_dir.map(roll_for_horizontal)
        }
        (Some(_), _) => v_dir.map(roll_for_vertical),
        (None, Some(_)) => h_dir.map(roll_for_horizontal),
        (None, None) => None,
    };
    let vertical_angles = || {
        let vd = v_dir?;
        let roll = roll_for_vertical(vd);
        let rolled = mat_vec(&rot_z(roll), vd);
        Some((roll, pitch_for(rolled)))
    };
    match mode {
        UprightMode::Level => roll_only().map(|r| (UprightMode::Level, r, 0.0, 0.0)),
        UprightMode::Vertical => {
            let (roll, pitch) = vertical_angles()?;
            Some((UprightMode::Vertical, roll, pitch, 0.0))
        }
        UprightMode::Full => {
            let (roll, pitch) = vertical_angles()?;
            let hd = h_dir?;
            let rotated = mat_vec(&rot_x(pitch), mat_vec(&rot_z(roll), hd));
            Some((UprightMode::Full, roll, pitch, yaw_for(rotated)))
        }
        UprightMode::Auto => angles_for(UprightMode::Full, v, h)
            .or_else(|| angles_for(UprightMode::Vertical, v, h))
            .or_else(|| angles_for(UprightMode::Level, v, h)),
    }
}

/// Confidence of a result given the vanishing points the chosen mode relied on.
fn outcome_for(
    sol: UprightSolution,
    v: Option<&VanishingPoint>,
    h: Option<&VanishingPoint>,
) -> AutoOutcome<UprightSolution> {
    let relied: Vec<&VanishingPoint> = match sol.mode {
        UprightMode::Full => v.into_iter().chain(h).collect(),
        UprightMode::Vertical => v.into_iter().collect(),
        UprightMode::Level => match (v, h) {
            (Some(v), _) => vec![v],
            (None, Some(h)) => vec![h],
            _ => vec![],
        },
        UprightMode::Auto => vec![],
    };
    if relied
        .iter()
        .all(|vp| vp.support >= CONFIDENT_INLIER_FRACTION)
    {
        AutoOutcome::Confident(sol)
    } else {
        AutoOutcome::LowConfidence(sol, AutoReason::WeakEvidence)
    }
}

/// Auto Upright: detects segments in the image and derives the correction for `mode`.
///
/// `focal_px` is the focal length in pixels of the source image (35mm-equivalent focal length /
/// 43.27 * the image diagonal; see [`focal_px_from_35mm`]). `NoResult(NoFeatures)` when too few
/// lines support the requested correction, `LowConfidence` when the lines found carry less than
/// half of their group's length (a wrong keystone is worse than none, so callers skip those).
pub fn analyze(
    pixels: &[[f32; 4]],
    width: u32,
    height: u32,
    focal_px: f64,
    mode: UprightMode,
) -> AutoOutcome<UprightSolution> {
    if width == 0 || height == 0 || pixels.len() != width as usize * height as usize {
        return AutoOutcome::NoResult(AutoReason::AtypicalInput);
    }
    if !(focal_px.is_finite() && focal_px > 0.0) {
        return AutoOutcome::NoResult(AutoReason::AtypicalInput);
    }
    let segments = detect_segments(pixels, width, height);
    analyze_segments(&segments, width, height, focal_px, mode)
}

/// [`analyze`] on already-detected (or synthetic) segments.
pub fn analyze_segments(
    segments: &[Segment],
    width: u32,
    height: u32,
    focal_px: f64,
    mode: UprightMode,
) -> AutoOutcome<UprightSolution> {
    let (w, h) = (width as f64, height as f64);
    let center = (w / 2.0, h / 2.0);
    let (vertical, horizontal) = group_lines(segments, center, focal_px);

    let v = estimate_vp(&vertical, plausible_vertical, 0x9E37_79B9_7F4A_7C15);
    let wants_h = matches!(
        mode,
        UprightMode::Full | UprightMode::Auto | UprightMode::Level
    );
    let h_vp = if wants_h {
        let v_dir = v.as_ref().map(|v| v.dir);
        let ortho = ORTHOGONALITY_DEG.to_radians().sin();
        estimate_vp(
            &horizontal,
            |d| plausible_horizontal(d) && v_dir.is_none_or(|vd| dot(vd, d).abs() < ortho),
            0xD1B5_4A32_D192_ED03,
        )
    } else {
        None
    };

    let Some((resolved, roll, pitch, yaw)) = angles_for(mode, v.as_ref(), h_vp.as_ref()) else {
        return AutoOutcome::NoResult(AutoReason::NoFeatures);
    };
    let Some(sol) = solution(resolved, roll, pitch, yaw, w, h, focal_px) else {
        return AutoOutcome::NoResult(AutoReason::AtypicalInput);
    };
    outcome_for(sol, v.as_ref(), h_vp.as_ref())
}

/// Guided Upright from one to four user guides. Two or more guides of a kind give that kind's
/// vanishing point (least squares); a single guide gives roll only. Always `Confident`: the user
/// said what is vertical/horizontal.
pub fn analyze_guides(
    guides: &[Guide],
    width: u32,
    height: u32,
    focal_px: f64,
    mode: UprightMode,
) -> AutoOutcome<UprightSolution> {
    if width == 0 || height == 0 || guides.is_empty() || guides.len() > 4 {
        return AutoOutcome::NoResult(AutoReason::AtypicalInput);
    }
    if !(focal_px.is_finite() && focal_px > 0.0) {
        return AutoOutcome::NoResult(AutoReason::AtypicalInput);
    }
    let (w, h) = (width as f64, height as f64);
    let center = (w / 2.0, h / 2.0);
    let family = |kind: GuideKind| -> Vec<RayLine> {
        guides
            .iter()
            .filter(|g| g.kind == kind)
            .filter_map(|g| ray_line(&g.segment, center, focal_px))
            .collect()
    };
    let to_vp = |lines: Vec<RayLine>| -> Option<VanishingPoint> {
        let dir = match lines.len() {
            0 => return None,
            // One guide: its own in-plane direction (the guide's image direction, z = 0).
            1 => return None,
            _ => least_squares_direction(&lines.iter().collect::<Vec<_>>())?,
        };
        Some(VanishingPoint {
            dir,
            inliers: lines.len(),
            support: 1.0,
        })
    };
    // A lone guide cannot define a vanishing point but does define a roll direction.
    let lone_direction = |kind: GuideKind| -> Option<Vec3> {
        let mut it = guides.iter().filter(|g| g.kind == kind);
        let first = it.next()?;
        if it.next().is_some() {
            return None;
        }
        let (dx, dy) = (
            first.segment.b.0 - first.segment.a.0,
            first.segment.b.1 - first.segment.a.1,
        );
        normalized([dx, dy, 0.0])
    };
    let v_lines = family(GuideKind::Vertical);
    let h_lines = family(GuideKind::Horizontal);
    let mut v = to_vp(v_lines);
    let mut hv = to_vp(h_lines);
    if v.is_none() {
        v = lone_direction(GuideKind::Vertical).map(|dir| VanishingPoint {
            dir,
            inliers: 1,
            support: 1.0,
        });
    }
    if hv.is_none() {
        hv = lone_direction(GuideKind::Horizontal).map(|dir| VanishingPoint {
            dir,
            inliers: 1,
            support: 1.0,
        });
    }
    // A lone guide can only drive roll, whatever mode was asked for.
    let effective = match mode {
        UprightMode::Vertical if v.as_ref().is_some_and(|v| v.inliers < 2) => UprightMode::Level,
        UprightMode::Full
            if v.as_ref().is_none_or(|v| v.inliers < 2)
                || hv.as_ref().is_none_or(|h| h.inliers < 2) =>
        {
            if v.as_ref().is_some_and(|v| v.inliers >= 2) {
                UprightMode::Vertical
            } else {
                UprightMode::Level
            }
        }
        UprightMode::Auto => {
            let two = |vp: &Option<VanishingPoint>| vp.as_ref().is_some_and(|v| v.inliers >= 2);
            match (two(&v), two(&hv)) {
                (true, true) => UprightMode::Full,
                (true, false) => UprightMode::Vertical,
                _ => UprightMode::Level,
            }
        }
        other => other,
    };
    let Some((resolved, roll, pitch, yaw)) = angles_for(effective, v.as_ref(), hv.as_ref()) else {
        return AutoOutcome::NoResult(AutoReason::NoFeatures);
    };
    match solution(resolved, roll, pitch, yaw, w, h, focal_px) {
        Some(sol) => AutoOutcome::Confident(sol),
        None => AutoOutcome::NoResult(AutoReason::AtypicalInput),
    }
}

/// Focal length in pixels from a 35mm-equivalent focal length (mm) and the image size:
/// `f_px = f35 / 43.27 * diagonal_px`. `None` for a missing or non-positive focal length.
pub fn focal_px_from_35mm(focal_35mm: f64, width: u32, height: u32) -> Option<f64> {
    if !(focal_35mm.is_finite() && focal_35mm > 0.0) {
        return None;
    }
    let diag = ((width as f64).powi(2) + (height as f64).powi(2)).sqrt();
    Some(focal_35mm / 43.27 * diag)
}

/// Fallback when EXIF has no focal length: a 28mm-equivalent, a typical architectural wide angle.
pub const FALLBACK_FOCAL_35MM: f64 = 28.0;

#[cfg(test)]
mod tests {
    use super::*;

    const W: u32 = 1200;
    const H: u32 = 800;
    const F: f64 = 1000.0;

    /// Projects a world point through camera rotation `r` (world -> camera) to image pixels.
    fn project(r: &Mat3, p: Vec3) -> (f64, f64) {
        let c = mat_vec(r, p);
        (
            W as f64 / 2.0 + F * c[0] / c[2],
            H as f64 / 2.0 + F * c[1] / c[2],
        )
    }

    /// A synthetic building facade: vertical edges along world-y, horizontal edges along
    /// world-x, seen by a camera rotated by `r`. World y points down, matching the image.
    fn facade_segments(r: &Mat3) -> Vec<Segment> {
        let mut segs = Vec::new();
        for i in -3..=3 {
            let x = i as f64 * 0.6;
            segs.push(Segment {
                a: project(r, [x, -1.0, 4.0]),
                b: project(r, [x, 1.0, 4.0]),
            });
        }
        for j in -2..=2 {
            let y = j as f64 * 0.45;
            segs.push(Segment {
                a: project(r, [-2.0, y, 4.0]),
                b: project(r, [2.0, y, 4.0]),
            });
        }
        segs
    }

    /// Residual tilt of the corrected facade: maps its edges through the solution's inverse and
    /// reports the worst deviation from vertical / horizontal in degrees.
    fn corrected_deviation(sol: &UprightSolution, segs: &[Segment]) -> (f64, f64) {
        // The solution maps output -> input; invert it to push input points to the output.
        let inv = invert(&sol.homography.0);
        let h = Homography(inv);
        let (mut worst_v, mut worst_h) = (0.0f64, 0.0f64);
        for (i, s) in segs.iter().enumerate() {
            let a = h.apply(s.a).unwrap();
            let b = h.apply(s.b).unwrap();
            let (dx, dy) = (b.0 - a.0, b.1 - a.1);
            if i < 7 {
                worst_v = worst_v.max(dx.atan2(dy.abs()).abs().to_degrees());
            } else {
                worst_h = worst_h.max(dy.atan2(dx.abs()).abs().to_degrees());
            }
        }
        (worst_v, worst_h)
    }

    fn invert(m: &Mat3) -> Mat3 {
        let c = [cross(m[1], m[2]), cross(m[2], m[0]), cross(m[0], m[1])];
        let det = dot(m[0], c[0]);
        let mut out = [[0.0; 3]; 3];
        for i in 0..3 {
            for j in 0..3 {
                out[i][j] = c[j][i] / det;
            }
        }
        out
    }

    fn camera(pitch_deg: f64, yaw_deg: f64, roll_deg: f64) -> Mat3 {
        mat_mul(
            &rot_z(roll_deg.to_radians()),
            &mat_mul(&rot_x(pitch_deg.to_radians()), &rot_y(yaw_deg.to_radians())),
        )
    }

    #[test]
    fn full_upright_flattens_a_keystoned_rolled_facade() {
        let segs = facade_segments(&camera(12.0, 15.0, 3.0));
        let AutoOutcome::Confident(sol) = analyze_segments(&segs, W, H, F, UprightMode::Full)
        else {
            panic!("a clean facade should be confident");
        };
        let (v, h) = corrected_deviation(&sol, &segs);
        assert!(v < 0.2 && h < 0.2, "residual v={v} h={h}");
        assert_eq!(sol.mode, UprightMode::Full);
    }

    #[test]
    fn vertical_upright_straightens_verticals_only() {
        let segs = facade_segments(&camera(12.0, 15.0, 3.0));
        let AutoOutcome::Confident(sol) = analyze_segments(&segs, W, H, F, UprightMode::Vertical)
        else {
            panic!("expected a result");
        };
        let (v, h) = corrected_deviation(&sol, &segs);
        assert!(v < 0.2, "verticals still lean {v} degrees");
        assert!(
            h > 2.0,
            "vertical mode must leave the horizontal keystone, got {h}"
        );
        assert_eq!(sol.yaw_degrees, 0.0);
    }

    #[test]
    fn level_upright_is_roll_only() {
        let segs = facade_segments(&camera(0.0, 0.0, 4.0));
        let AutoOutcome::Confident(sol) = analyze_segments(&segs, W, H, F, UprightMode::Level)
        else {
            panic!("expected a result");
        };
        assert!(sol.pitch_degrees == 0.0 && sol.yaw_degrees == 0.0);
        let (v, h) = corrected_deviation(&sol, &segs);
        assert!(v < 0.2 && h < 0.2, "residual v={v} h={h}");
    }

    #[test]
    fn auto_picks_vertical_when_only_verticals_exist() {
        let all = facade_segments(&camera(10.0, 0.0, 2.0));
        let verticals = &all[..7];
        let AutoOutcome::Confident(sol) = analyze_segments(verticals, W, H, F, UprightMode::Auto)
        else {
            panic!("expected a result");
        };
        assert_eq!(sol.mode, UprightMode::Vertical);
    }

    #[test]
    fn a_level_facade_gets_an_identity_like_correction() {
        let segs = facade_segments(&camera(0.0, 0.0, 0.0));
        let AutoOutcome::Confident(sol) = analyze_segments(&segs, W, H, F, UprightMode::Full)
        else {
            panic!("expected a result");
        };
        assert!(sol.roll_degrees.abs() < 0.1);
        assert!(sol.pitch_degrees.abs() < 0.1);
        // Parallel horizontals have their VP at infinity, so yaw is the worst-conditioned angle.
        assert!(sol.yaw_degrees.abs() < 1.0);
    }

    #[test]
    fn too_few_lines_is_no_result() {
        let segs = vec![Segment {
            a: (100.0, 100.0),
            b: (110.0, 500.0),
        }];
        assert_eq!(
            analyze_segments(&segs, W, H, F, UprightMode::Auto),
            AutoOutcome::NoResult(AutoReason::NoFeatures)
        );
    }

    #[test]
    fn correction_never_samples_outside_the_source() {
        let segs = facade_segments(&camera(15.0, 12.0, 3.0));
        let AutoOutcome::Confident(sol) = analyze_segments(&segs, W, H, F, UprightMode::Full)
        else {
            panic!("expected a result");
        };
        for (x, y) in [
            (0.0, 0.0),
            (W as f64, 0.0),
            (W as f64, H as f64),
            (0.0, H as f64),
        ] {
            let (sx, sy) = sol.homography.apply((x, y)).unwrap();
            let eps = 1e-3;
            assert!(
                sx >= -eps && sy >= -eps && sx <= W as f64 + eps && sy <= H as f64 + eps,
                "corner ({x},{y}) samples ({sx},{sy})"
            );
        }
    }

    #[test]
    fn guided_two_vertical_two_horizontal_matches_full() {
        let r = camera(10.0, 12.0, 2.0);
        let segs = facade_segments(&r);
        let guides = [
            Guide {
                kind: GuideKind::Vertical,
                segment: segs[0],
            },
            Guide {
                kind: GuideKind::Vertical,
                segment: segs[6],
            },
            Guide {
                kind: GuideKind::Horizontal,
                segment: segs[7],
            },
            Guide {
                kind: GuideKind::Horizontal,
                segment: segs[11],
            },
        ];
        let AutoOutcome::Confident(sol) = analyze_guides(&guides, W, H, F, UprightMode::Full)
        else {
            panic!("expected a result");
        };
        let (v, h) = corrected_deviation(&sol, &segs);
        assert!(v < 0.2 && h < 0.2, "residual v={v} h={h}");
    }

    #[test]
    fn a_single_guide_only_rolls() {
        let segs = facade_segments(&camera(0.0, 0.0, 5.0));
        let guides = [Guide {
            kind: GuideKind::Vertical,
            segment: segs[3],
        }];
        let AutoOutcome::Confident(sol) = analyze_guides(&guides, W, H, F, UprightMode::Full)
        else {
            panic!("expected a result");
        };
        assert_eq!(sol.mode, UprightMode::Level);
        let (v, _) = corrected_deviation(&sol, &segs);
        assert!(v < 0.2, "v={v}");
    }

    #[test]
    fn guides_are_limited_to_four() {
        let g = Guide {
            kind: GuideKind::Vertical,
            segment: Segment {
                a: (0.0, 0.0),
                b: (0.0, 100.0),
            },
        };
        assert_eq!(
            analyze_guides(&[g; 5], W, H, F, UprightMode::Auto),
            AutoOutcome::NoResult(AutoReason::AtypicalInput)
        );
        assert_eq!(
            analyze_guides(&[], W, H, F, UprightMode::Auto),
            AutoOutcome::NoResult(AutoReason::AtypicalInput)
        );
    }

    #[test]
    fn homography_is_resolution_independent() {
        let r = camera(10.0, 8.0, 2.0);
        let small = {
            let segs: Vec<Segment> = facade_segments(&r);
            analyze_segments(&segs, W, H, F, UprightMode::Full)
        };
        // Same scene at 2x: segments and focal scale together.
        let scale = 2.0;
        let segs2: Vec<Segment> = facade_segments(&r)
            .iter()
            .map(|s| Segment {
                a: (s.a.0 * scale, s.a.1 * scale),
                b: (s.b.0 * scale, s.b.1 * scale),
            })
            .collect();
        let big = analyze_segments(&segs2, W * 2, H * 2, F * scale, UprightMode::Full);
        let (AutoOutcome::Confident(a), AutoOutcome::Confident(b)) = (small, big) else {
            panic!("expected results");
        };
        assert!((a.roll_degrees - b.roll_degrees).abs() < 1e-6);
        assert!((a.pitch_degrees - b.pitch_degrees).abs() < 1e-6);
        assert!((a.yaw_degrees - b.yaw_degrees).abs() < 1e-6);
    }

    /// Renders a synthetic facade (bright grid lines on dark) through the pixel buffer, so the
    /// whole detect -> estimate -> solve path is exercised, not just the geometry.
    fn render_facade(r: &Mat3) -> Vec<[f32; 4]> {
        let mut img = vec![[0.1f32, 0.1, 0.1, 1.0]; (W * H) as usize];
        let mut draw = |a: (f64, f64), b: (f64, f64)| {
            let n = (Segment { a, b }.length() * 2.0) as usize + 1;
            for t in 0..=n {
                let f = t as f64 / n as f64;
                let (x, y) = (a.0 + (b.0 - a.0) * f, a.1 + (b.1 - a.1) * f);
                for oy in -1..=1i32 {
                    for ox in -1..=1i32 {
                        let (px, py) = (x as i32 + ox, y as i32 + oy);
                        if px >= 0 && py >= 0 && (px as u32) < W && (py as u32) < H {
                            img[(py as u32 * W + px as u32) as usize] = [0.95, 0.95, 0.95, 1.0];
                        }
                    }
                }
            }
        };
        for s in facade_segments(r) {
            draw(s.a, s.b);
        }
        img
    }

    #[test]
    fn detects_and_corrects_a_rendered_keystoned_facade() {
        let r = camera(8.0, 6.0, 2.0);
        let img = render_facade(&r);
        let truth = facade_segments(&r);
        let out = analyze(&img, W, H, F, UprightMode::Auto);
        let sol = *out.value().unwrap_or_else(|| panic!("no result: {out:?}"));
        let (v, h) = corrected_deviation(&sol, &truth);
        assert!(
            v < 0.6,
            "verticals still lean {v} degrees (mode {:?})",
            sol.mode
        );
        assert!(
            h < 1.5,
            "horizontals still lean {h} degrees (mode {:?})",
            sol.mode
        );
    }

    #[test]
    fn a_featureless_image_has_no_result() {
        let img = vec![[0.5f32, 0.5, 0.5, 1.0]; 100 * 100];
        assert_eq!(
            analyze(&img, 100, 100, 80.0, UprightMode::Auto),
            AutoOutcome::NoResult(AutoReason::NoFeatures)
        );
    }

    #[test]
    fn bad_input_is_atypical() {
        assert_eq!(
            analyze(&[], 0, 0, 1000.0, UprightMode::Auto),
            AutoOutcome::NoResult(AutoReason::AtypicalInput)
        );
        let img = vec![[0.5f32, 0.5, 0.5, 1.0]; 100 * 100];
        assert_eq!(
            analyze(&img, 100, 100, f64::NAN, UprightMode::Auto),
            AutoOutcome::NoResult(AutoReason::AtypicalInput)
        );
    }

    #[test]
    fn focal_from_35mm_uses_the_diagonal() {
        let f = focal_px_from_35mm(43.27, 3000, 4000).unwrap();
        assert!((f - 5000.0).abs() < 1e-6);
        assert_eq!(focal_px_from_35mm(0.0, 10, 10), None);
        assert_eq!(focal_px_from_35mm(f64::NAN, 10, 10), None);
    }

    #[test]
    fn identity_homography_round_trips() {
        assert_eq!(Homography::IDENTITY.apply((3.0, 4.0)), Some((3.0, 4.0)));
        assert!(Homography::IDENTITY.is_identity());
    }
}
