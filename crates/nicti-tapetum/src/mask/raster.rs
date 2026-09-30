//! CPU rasterizers for geometry and range masks -- the reference the GPU kernels are checked
//! against, and the source of truth for what a stored mask *means*.
//!
//! Stored geometry is in normalized coordinates (see `params`); everything here works in pixels of
//! a given `width x height` extent, converting once at the edge: a point is `n * (width, height)`
//! and a length is `n * long_edge`. Pixel centres sit at `+0.5`.
//!
//! Brush dabs blend with `max` inside a stroke (overlapping dabs must not double-darken) and strokes
//! apply in order: an add stroke is `max(acc, s)`, an erase stroke is `max(acc - s, 0)`.

use super::params::{MaskSource, Stroke, MIN_RADIUS};
use super::Field;

/// Most dabs one stroke expands to. A stroke that would exceed it gets a wider dab spacing instead
/// of being truncated, so a very long stroke still covers its whole path.
pub const MAX_DABS_PER_STROKE: usize = 20_000;
/// Most dabs the whole brush component expands to (spent stroke by stroke, in order).
pub const MAX_DABS_TOTAL: usize = 400_000;
/// Dab spacing as a fraction of the radius (LRC-style overlap; 1/4 keeps an edge visibly smooth).
pub const SPACING_FRACTION: f32 = 0.25;
/// Never space dabs closer than this many pixels.
pub const MIN_SPACING_PX: f32 = 0.5;

/// One soft circle in pixel space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Dab {
    pub cx: f32,
    pub cy: f32,
    pub radius: f32,
    /// Soft edge width in pixels, already clamped to the radius.
    pub feather: f32,
    pub flow: f32,
}

impl Dab {
    /// Weight at pixel-space point `(x, y)`: 1 inside `radius - feather`, linear to 0 at `radius`,
    /// scaled by `flow`.
    pub fn weight(&self, x: f32, y: f32) -> f32 {
        let dist = ((x - self.cx).powi(2) + (y - self.cy).powi(2)).sqrt();
        if dist >= self.radius {
            return 0.0;
        }
        let inner = (self.radius - self.feather).max(0.0);
        let w = if dist <= inner {
            1.0
        } else {
            1.0 - (dist - inner) / (self.radius - inner).max(1e-6)
        };
        (w * self.flow).clamp(0.0, 1.0)
    }
}

fn long_edge(width: usize, height: usize) -> f32 {
    width.max(height) as f32
}

/// Expands a stroke's polyline into dabs, spaced `SPACING_FRACTION * radius` apart along the path
/// (never closer than [`MIN_SPACING_PX`]), at most `cap` of them. One point is one dab.
pub fn dabs_for_stroke(stroke: &Stroke, width: usize, height: usize, cap: usize) -> Vec<Dab> {
    let long = long_edge(width, height);
    let radius = stroke.radius * long;
    if stroke.points.is_empty() || radius < MIN_RADIUS * long || cap == 0 {
        return Vec::new();
    }
    let feather = (stroke.feather * long).clamp(0.0, radius);
    let to_px = |p: [f32; 2]| (p[0] * width as f32, p[1] * height as f32);
    let make = |(cx, cy): (f32, f32)| Dab {
        cx,
        cy,
        radius,
        feather,
        flow: stroke.flow,
    };

    let pts: Vec<(f32, f32)> = stroke.points.iter().copied().map(to_px).collect();
    if pts.len() == 1 {
        return vec![make(pts[0])];
    }
    let total: f32 = pts
        .windows(2)
        .map(|w| ((w[1].0 - w[0].0).powi(2) + (w[1].1 - w[0].1).powi(2)).sqrt())
        .sum();
    let cap = cap.min(MAX_DABS_PER_STROKE);
    let mut spacing = (radius * SPACING_FRACTION).max(MIN_SPACING_PX);
    // Widen rather than truncate, so the whole path is still covered at a coarser spacing.
    if total / spacing > (cap.saturating_sub(pts.len())) as f32 {
        spacing = total / (cap.saturating_sub(pts.len()).max(1)) as f32;
    }

    let mut dabs = vec![make(pts[0])];
    let mut carry = 0.0f32; // distance already walked since the last dab
    for w in pts.windows(2) {
        let (x0, y0) = w[0];
        let (x1, y1) = w[1];
        let seg = ((x1 - x0).powi(2) + (y1 - y0).powi(2)).sqrt();
        if seg <= f32::EPSILON {
            continue;
        }
        let mut at = spacing - carry;
        while at <= seg && dabs.len() < cap {
            let t = at / seg;
            dabs.push(make((x0 + (x1 - x0) * t, y0 + (y1 - y0) * t)));
            at += spacing;
        }
        carry = seg - (at - spacing);
    }
    dabs.truncate(cap);
    dabs
}

/// Linear gradient weight at pixel-space `(x, y)`: 1 at `p0`, 0 at `p1`, constant across.
pub fn linear_weight(p0: (f32, f32), p1: (f32, f32), x: f32, y: f32) -> f32 {
    let (dx, dy) = (p1.0 - p0.0, p1.1 - p0.1);
    let len_sq = dx * dx + dy * dy;
    let t = if len_sq <= f32::EPSILON {
        0.0
    } else {
        ((x - p0.0) * dx + (y - p0.1) * dy) / len_sq
    };
    (1.0 - t.clamp(0.0, 1.0)).clamp(0.0, 1.0)
}

/// Radial gradient weight at pixel-space `(x, y)`: 1 inside the (rotated) ellipse, linear to 0
/// over `feather` pixels beyond it. A feather is converted into normalized-ellipse units through
/// the mean radius, so one width works along both axes of an eccentric ellipse.
pub fn radial_weight(
    center: (f32, f32),
    radii: (f32, f32),
    angle_deg: f32,
    feather: f32,
    x: f32,
    y: f32,
) -> f32 {
    let (rx, ry) = radii;
    if rx <= 0.0 || ry <= 0.0 {
        return 0.0;
    }
    let (dx, dy) = (x - center.0, y - center.1);
    let angle = angle_deg.to_radians();
    let (cos_a, sin_a) = (angle.cos(), angle.sin());
    let rot_x = dx * cos_a + dy * sin_a;
    let rot_y = -dx * sin_a + dy * cos_a;
    let normalized = ((rot_x / rx).powi(2) + (rot_y / ry).powi(2)).sqrt();
    let mean_radius = (rx + ry) * 0.5;
    let feather_norm = (feather / mean_radius).max(1e-6);
    (1.0 - (normalized - 1.0) / feather_norm).clamp(0.0, 1.0)
}

/// Luminance-range weight: 1 inside `lo..=hi`, ramping to 0 over `smooth` either side (a hard edge
/// when `smooth` is 0).
pub fn luminance_range_weight(y: f32, lo: f32, hi: f32, smooth: f32) -> f32 {
    if y >= lo && y <= hi {
        return 1.0;
    }
    let outside = if y < lo { lo - y } else { y - hi };
    if smooth <= 0.0 {
        return 0.0;
    }
    (1.0 - outside / smooth).clamp(0.0, 1.0)
}

/// Colour-range weight: `1 - distance / tolerance` to the *nearest* sample, clamped. Lab distance
/// (ΔE76): cheap, and perceptually even enough for a select-by-colour tool.
pub fn color_range_weight(lab: [f32; 3], samples: &[[f32; 3]], tolerance: f32) -> f32 {
    if samples.is_empty() || tolerance <= 0.0 {
        return 0.0;
    }
    let nearest = samples
        .iter()
        .map(|s| {
            ((lab[0] - s[0]).powi(2) + (lab[1] - s[1]).powi(2) + (lab[2] - s[2]).powi(2)).sqrt()
        })
        .fold(f32::INFINITY, f32::min);
    (1.0 - nearest / tolerance).clamp(0.0, 1.0)
}

/// ProPhoto linear luminance coefficients (the Y row of ProPhoto -> XYZ D50).
pub const PROPHOTO_Y: [f32; 3] = [0.2880402, 0.7118741, 0.0000857];

/// Perceptual luma of a *working-space* (linear ProPhoto) pixel: `clamp(Y, 0, 1) ^ (1 / 2.2)`. This
/// is the value a luminance-range mask compares against `lo..hi`.
pub fn working_luma(working: [f32; 3]) -> f32 {
    let y = PROPHOTO_Y[0] * working[0] + PROPHOTO_Y[1] * working[1] + PROPHOTO_Y[2] * working[2];
    y.clamp(0.0, 1.0).powf(1.0 / 2.2)
}

/// CIE Lab (D50) of a working-space (linear ProPhoto) pixel -- what a colour-range mask measures.
pub fn working_lab(working: [f32; 3]) -> [f32; 3] {
    let x = 0.7976749 * working[0] + 0.1351917 * working[1] + 0.0313534 * working[2];
    let y = PROPHOTO_Y[0] * working[0] + PROPHOTO_Y[1] * working[1] + PROPHOTO_Y[2] * working[2];
    let z = 0.82521 * working[2];
    let f = |t: f32| {
        if t > 0.008856 {
            t.cbrt()
        } else {
            7.787 * t + 16.0 / 116.0
        }
    };
    let (fx, fy, fz) = (f(x / 0.9642), f(y), f(z / 0.8251));
    [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)]
}

/// A range source's weight for one camera-linear pixel: the pixel goes through `matrix` (camera ->
/// working) and is then measured by luma or Lab. `None` for a non-range source.
pub fn range_weight_at(
    source: &MaskSource,
    cam_rgb: [f32; 3],
    matrix: crate::color::Mat3,
) -> Option<f32> {
    let working = crate::color::mat3_apply(matrix, cam_rgb);
    match source {
        MaskSource::LuminanceRange { lo, hi, smooth } => Some(luminance_range_weight(
            working_luma(working),
            *lo,
            *hi,
            *smooth,
        )),
        MaskSource::ColorRange { samples, tolerance } => Some(color_range_weight(
            working_lab(working),
            samples,
            *tolerance,
        )),
        _ => None,
    }
}

/// A range source rasterized at `mask_w x mask_h` from an RGBA frame: the weight is computed per
/// source tap and bilinearly combined at each mask pixel (so a smaller mask is smooth, not
/// aliased) -- the CPU twin of `mask_range.wgsl`.
pub fn range_field(
    source: &MaskSource,
    frame: &[[f32; 4]],
    frame_w: usize,
    frame_h: usize,
    mask_w: usize,
    mask_h: usize,
    matrix: crate::color::Mat3,
) -> Option<Field> {
    let taps = Field {
        width: frame_w,
        height: frame_h,
        data: frame
            .iter()
            .map(|p| range_weight_at(source, [p[0], p[1], p[2]], matrix))
            .collect::<Option<Vec<f32>>>()?,
    };
    let mut out = Field::new(mask_w, mask_h, 0.0);
    for y in 0..mask_h {
        for x in 0..mask_w {
            out.data[y * mask_w + x] = super::guided::sample_mapped(&taps, x, y, mask_w, mask_h);
        }
    }
    Some(out)
}

/// The pixel box `[lo_x, hi_x) x [lo_y, hi_y)` a dab can touch, clamped to the frame; `None` when it
/// is entirely outside. Shared by the CPU splat and the GPU tile binning so both visit exactly the
/// same pixels.
pub fn dab_box(d: &Dab, width: usize, height: usize) -> Option<(usize, usize, usize, usize)> {
    let lo_x = (d.cx - d.radius).floor().max(0.0) as usize;
    let lo_y = (d.cy - d.radius).floor().max(0.0) as usize;
    let hi_x = ((d.cx + d.radius).ceil().max(0.0) as usize).min(width);
    let hi_y = ((d.cy + d.radius).ceil().max(0.0) as usize).min(height);
    (hi_x > lo_x && hi_y > lo_y).then_some((lo_x, lo_y, hi_x, hi_y))
}

/// The union of a stroke's dab boxes, `(x0, y0, x1, y1)`; `None` if nothing is on the frame.
pub fn stroke_box(
    dabs: &[Dab],
    width: usize,
    height: usize,
) -> Option<(usize, usize, usize, usize)> {
    dabs.iter()
        .filter_map(|d| dab_box(d, width, height))
        .reduce(|a, b| (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3)))
}

/// Rasterizes a brush's strokes to a field. Each stroke is splatted by dab bounding box (cost is
/// the painted area, not `pixels x dabs`), then folded in order: add = `max`, erase = subtract.
pub fn rasterize_brush(strokes: &[Stroke], width: usize, height: usize) -> Field {
    let mut acc = Field::new(width, height, 0.0);
    let mut budget = MAX_DABS_TOTAL;
    for stroke in strokes {
        let dabs = dabs_for_stroke(stroke, width, height, budget);
        budget = budget.saturating_sub(dabs.len());
        let Some((x0, y0, x1, y1)) = stroke_box(&dabs, width, height) else {
            continue;
        };
        let (bw, bh) = (x1 - x0, y1 - y0);
        // The stroke's own field, only over the box its dabs touch.
        let mut scratch = vec![0.0f32; bw * bh];
        for d in &dabs {
            let Some((lo_x, lo_y, hi_x, hi_y)) = dab_box(d, width, height) else {
                continue;
            };
            for y in lo_y..hi_y {
                for x in lo_x..hi_x {
                    let w = d.weight(x as f32 + 0.5, y as f32 + 0.5);
                    let s = &mut scratch[(y - y0) * bw + (x - x0)];
                    *s = s.max(w);
                }
            }
        }
        for y in 0..bh {
            for x in 0..bw {
                let s = scratch[y * bw + x];
                let a = &mut acc.data[(y + y0) * width + (x + x0)];
                *a = if stroke.erase {
                    (*a - s).max(0.0)
                } else {
                    a.max(s)
                };
            }
        }
    }
    acc
}

/// Rasterizes a *geometry* source (gradient or brush) to a field at `width x height`. Returns
/// `None` for an AI or range source -- those need an alpha or the image, not just an extent.
pub fn rasterize_source(source: &MaskSource, width: usize, height: usize) -> Option<Field> {
    let (w, h) = (width as f32, height as f32);
    let long = long_edge(width, height);
    match source {
        MaskSource::LinearGradient { p0, p1 } => {
            let (p0, p1) = ((p0[0] * w, p0[1] * h), (p1[0] * w, p1[1] * h));
            let mut f = Field::new(width, height, 0.0);
            for y in 0..height {
                for x in 0..width {
                    f.data[y * width + x] = linear_weight(p0, p1, x as f32 + 0.5, y as f32 + 0.5);
                }
            }
            Some(f)
        }
        MaskSource::RadialGradient {
            center,
            radii,
            angle_deg,
            feather,
        } => {
            let center = (center[0] * w, center[1] * h);
            let radii = (radii[0] * long, radii[1] * long);
            let feather = feather * long;
            let mut f = Field::new(width, height, 0.0);
            for y in 0..height {
                for x in 0..width {
                    f.data[y * width + x] = radial_weight(
                        center,
                        radii,
                        *angle_deg,
                        feather,
                        x as f32 + 0.5,
                        y as f32 + 0.5,
                    );
                }
            }
            Some(f)
        }
        MaskSource::Brush { strokes } => Some(rasterize_brush(strokes, width, height)),
        MaskSource::Ai(_) | MaskSource::LuminanceRange { .. } | MaskSource::ColorRange { .. } => {
            None
        }
    }
}

/// Rasterizes a luminance-range source over a luminance field.
pub fn rasterize_luminance(luma: &Field, lo: f32, hi: f32, smooth: f32) -> Field {
    Field {
        width: luma.width,
        height: luma.height,
        data: luma
            .data
            .iter()
            .map(|&y| luminance_range_weight(y, lo, hi, smooth))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stroke(points: &[[f32; 2]], radius: f32, feather: f32, erase: bool) -> Stroke {
        Stroke {
            points: points.to_vec(),
            radius,
            feather,
            flow: 1.0,
            erase,
        }
    }

    #[test]
    fn linear_gradient_is_full_at_p0_and_zero_at_p1() {
        let src = MaskSource::LinearGradient {
            p0: [0.0, 0.5],
            p1: [1.0, 0.5],
        };
        let f = rasterize_source(&src, 101, 101).unwrap();
        assert!((f.get(0, 50) - 1.0).abs() < 1e-2);
        assert!(f.get(100, 50) < 1e-2);
        assert!((f.get(50, 50) - 0.5).abs() < 0.05);
        // Constant across the gradient direction.
        assert_eq!(f.get(30, 0), f.get(30, 100));
    }

    #[test]
    fn a_degenerate_gradient_does_not_nan() {
        let src = MaskSource::LinearGradient {
            p0: [0.5, 0.5],
            p1: [0.5, 0.5],
        };
        let f = rasterize_source(&src, 8, 8).unwrap();
        assert!(f.data.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn radial_gradient_is_full_inside_and_zero_far_outside() {
        let src = MaskSource::RadialGradient {
            center: [0.5, 0.5],
            radii: [0.2, 0.2],
            angle_deg: 0.0,
            feather: 0.05,
        };
        let f = rasterize_source(&src, 101, 101).unwrap();
        assert!((f.get(50, 50) - 1.0).abs() < 1e-3);
        assert!(f.get(0, 0) < 1e-3);
        // Just past the radius sits in the feather: strictly between.
        let edge = f.get(50 + 22, 50);
        assert!(edge > 0.0 && edge < 1.0, "{edge}");
    }

    #[test]
    fn a_rotated_ellipse_follows_its_angle() {
        let mk = |angle| {
            rasterize_source(
                &MaskSource::RadialGradient {
                    center: [0.5, 0.5],
                    radii: [0.3, 0.05],
                    angle_deg: angle,
                    feather: 0.01,
                },
                101,
                101,
            )
            .unwrap()
        };
        let flat = mk(0.0);
        let tall = mk(90.0);
        assert!(flat.get(75, 50) > 0.9 && flat.get(50, 75) < 0.1);
        assert!(tall.get(50, 75) > 0.9 && tall.get(75, 50) < 0.1);
    }

    #[test]
    fn masks_are_resolution_independent() {
        let src = MaskSource::RadialGradient {
            center: [0.4, 0.6],
            radii: [0.25, 0.15],
            angle_deg: 20.0,
            feather: 0.1,
        };
        let small = rasterize_source(&src, 100, 100).unwrap();
        let big = rasterize_source(&src, 400, 400).unwrap();
        let mut worst = 0.0f32;
        for y in 0..100 {
            for x in 0..100 {
                // The centre of small pixel (x, y) is the centre of big pixels 4x..4x+3's block.
                let b = big.get(x * 4 + 2, y * 4 + 2);
                worst = worst.max((small.get(x, y) - b).abs());
            }
        }
        assert!(worst < 0.08, "same normalized geometry drifted by {worst}");
    }

    #[test]
    fn overlapping_dabs_blend_with_max_not_addition() {
        let f = rasterize_brush(
            &[stroke(&[[0.4, 0.5], [0.45, 0.5]], 0.15, 0.05, false)],
            100,
            100,
        );
        assert!(f.data.iter().all(|&v| v <= 1.0 + 1e-6));
        assert!((f.get(42, 50) - 1.0).abs() < 1e-3);
    }

    #[test]
    fn an_erase_stroke_removes_what_an_add_stroke_painted() {
        let add = stroke(&[[0.5, 0.5]], 0.2, 0.0, false);
        let erase = stroke(&[[0.5, 0.5]], 0.2, 0.0, true);
        let painted = rasterize_brush(std::slice::from_ref(&add), 50, 50);
        assert!(painted.get(25, 25) > 0.99);
        let erased = rasterize_brush(&[add, erase], 50, 50);
        assert!(erased.data.iter().all(|&v| v < 1e-6));
    }

    #[test]
    fn a_stroke_paints_a_continuous_path_between_distant_points() {
        let f = rasterize_brush(
            &[stroke(&[[0.1, 0.5], [0.9, 0.5]], 0.03, 0.0, false)],
            200,
            100,
        );
        for x in 25..175 {
            assert!(f.get(x, 50) > 0.99, "gap in the stroke at x={x}");
        }
        assert!(f.get(100, 10) < 1e-6, "paint leaked away from the path");
    }

    #[test]
    fn a_single_point_stroke_is_one_dab_and_empty_strokes_are_nothing() {
        let one = stroke(&[[0.5, 0.5]], 0.1, 0.0, false);
        assert_eq!(dabs_for_stroke(&one, 100, 100, 1000).len(), 1);
        let none = stroke(&[], 0.1, 0.0, false);
        assert!(dabs_for_stroke(&none, 100, 100, 1000).is_empty());
        let zero_radius = stroke(&[[0.5, 0.5]], 0.0, 0.0, false);
        assert!(dabs_for_stroke(&zero_radius, 100, 100, 1000).is_empty());
    }

    #[test]
    fn a_huge_stroke_is_capped_but_still_covers_its_whole_path() {
        let long = stroke(&[[0.0, 0.5], [1.0, 0.5]], 0.0005, 0.0, false);
        let dabs = dabs_for_stroke(&long, 8000, 100, 1000);
        assert!(dabs.len() <= 1000, "{}", dabs.len());
        let last = dabs.last().unwrap();
        assert!(last.cx > 7000.0, "coverage stops at {}", last.cx);
    }

    #[test]
    fn dab_spacing_follows_the_radius() {
        let s = stroke(&[[0.0, 0.5], [1.0, 0.5]], 0.05, 0.0, false);
        let dabs = dabs_for_stroke(&s, 1000, 1000, MAX_DABS_PER_STROKE);
        // radius 50px -> spacing 12.5px -> ~80 dabs across 1000px.
        assert!((70..=90).contains(&dabs.len()), "{}", dabs.len());
    }

    #[test]
    fn luminance_range_is_full_inside_and_ramps_outside() {
        assert_eq!(luminance_range_weight(0.5, 0.4, 0.6, 0.1), 1.0);
        assert_eq!(luminance_range_weight(0.4, 0.4, 0.6, 0.1), 1.0);
        assert!((luminance_range_weight(0.35, 0.4, 0.6, 0.1) - 0.5).abs() < 1e-5);
        assert_eq!(luminance_range_weight(0.2, 0.4, 0.6, 0.1), 0.0);
        assert_eq!(
            luminance_range_weight(0.39, 0.4, 0.6, 0.0),
            0.0,
            "hard edge"
        );
    }

    #[test]
    fn color_range_uses_the_nearest_sample() {
        let samples = [[50.0, 0.0, 0.0], [80.0, 40.0, 0.0]];
        assert_eq!(color_range_weight([50.0, 0.0, 0.0], &samples, 10.0), 1.0);
        assert_eq!(color_range_weight([80.0, 40.0, 0.0], &samples, 10.0), 1.0);
        assert!((color_range_weight([55.0, 0.0, 0.0], &samples, 10.0) - 0.5).abs() < 1e-5);
        assert_eq!(color_range_weight([0.0, 0.0, 0.0], &samples, 10.0), 0.0);
        assert_eq!(color_range_weight([50.0, 0.0, 0.0], &[], 10.0), 0.0);
        assert_eq!(color_range_weight([50.0, 0.0, 0.0], &samples, 0.0), 0.0);
    }

    #[test]
    fn ai_and_range_sources_have_no_extent_only_raster() {
        assert!(rasterize_source(
            &MaskSource::LuminanceRange {
                lo: 0.0,
                hi: 1.0,
                smooth: 0.0
            },
            4,
            4
        )
        .is_none());
    }

    #[test]
    fn working_luma_and_lab_are_sane_reference_points() {
        assert_eq!(working_luma([0.0; 3]), 0.0);
        assert!((working_luma([1.0; 3]) - 1.0).abs() < 1e-3);
        assert!(working_luma([0.2; 3]) < working_luma([0.5; 3]));
        // A neutral grey has no chroma; white is L*=100; black is L*=0.
        let grey = working_lab([0.18; 3]);
        assert!(grey[1].abs() < 1.5 && grey[2].abs() < 1.5, "{grey:?}");
        assert!((working_lab([1.0; 3])[0] - 100.0).abs() < 0.5);
        assert!(working_lab([0.0; 3])[0].abs() < 1e-3);
        // A saturated red has a strongly positive a*.
        assert!(working_lab([0.6, 0.02, 0.02])[1] > 40.0);
    }

    #[test]
    fn range_field_is_none_for_a_non_range_source_and_follows_the_frame() {
        let frame: Vec<[f32; 4]> = (0..16)
            .map(|i| {
                let v = i as f32 / 15.0;
                [v, v, v, 1.0]
            })
            .collect();
        let id = crate::color::mat3_identity();
        assert!(range_field(
            &MaskSource::Brush { strokes: vec![] },
            &frame,
            4,
            4,
            4,
            4,
            id
        )
        .is_none());
        let bright = range_field(
            &MaskSource::LuminanceRange {
                lo: 0.6,
                hi: 1.0,
                smooth: 0.0,
            },
            &frame,
            4,
            4,
            4,
            4,
            id,
        )
        .unwrap();
        // The last (brightest) pixel is selected, the first (darkest) is not.
        assert!(bright.get(3, 3) > 0.99 && bright.get(0, 0) < 0.01);
    }
}
