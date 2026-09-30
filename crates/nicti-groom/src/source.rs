//! Auto source-pick for clone/heal spots: given a spot to fix, guess a good place to copy from.
//!
//! Promoted from `spikes/groom`'s SSD-over-a-ring baseline (ADR-0050 calls it a deliberately simple
//! baseline, not PatchMatch). Two changes for production: it reads lazily through a
//! [`PixelSource`] (a full-resolution frame is never copied), and it searches several rings rather
//! than one, skipping any candidate whose annulus would leave the image or overlap the region being
//! fixed -- so it never picks a source that is off-frame or contaminated by the blemish itself.

use crate::PixelSource;

/// Ring radii searched, as multiples of the spot radius.
const RING_FACTORS: [f32; 3] = [2.5, 4.0, 6.0];
const CANDIDATES_PER_RING: usize = 24;

/// Integer offsets of a discrete annulus between `inner` and `outer` (exclusive of the disc).
fn annulus(inner: f32, outer: f32) -> Vec<(i32, i32)> {
    let r = outer.ceil() as i32;
    let mut pts = Vec::new();
    for dy in -r..=r {
        for dx in -r..=r {
            let d = ((dx * dx + dy * dy) as f32).sqrt();
            if d >= inner && d <= outer {
                pts.push((dx, dy));
            }
        }
    }
    pts
}

/// Returns the `(dx, dy)` offset from `center` to the best-matching source location, or `None` if
/// no candidate fits inside the image without touching the spot (a spot that fills most of a tiny
/// image, say).
///
/// A candidate scores by the sum of squared RGB differences between the ring of pixels just outside
/// the spot and the same ring around the candidate -- "does the surrounding texture look alike".
pub fn auto_source_pick(
    src: &dyn PixelSource,
    center: (i32, i32),
    radius: f32,
) -> Option<(i32, i32)> {
    if !(radius.is_finite() && radius > 0.0) {
        return None;
    }
    let (w, h) = (src.width() as i32, src.height() as i32);
    let border = (radius * 0.5).max(2.0);
    let ring = annulus(radius, radius + border);
    let reach = (radius + border).ceil() as i32;
    let inside = |x: i32, y: i32| x >= 0 && y >= 0 && x < w && y < h;
    let px = |x: i32, y: i32| src.pixel(x as u32, y as u32);

    // The spot's own annulus must be fully readable to compare against.
    if !ring
        .iter()
        .all(|&(dx, dy)| inside(center.0 + dx, center.1 + dy))
    {
        return None;
    }

    let mut best: Option<((i32, i32), f32)> = None;
    for factor in RING_FACTORS {
        let search = radius * factor;
        for k in 0..CANDIDATES_PER_RING {
            let theta = std::f32::consts::TAU * k as f32 / CANDIDATES_PER_RING as f32;
            let (ox, oy) = (
                (search * theta.cos()).round() as i32,
                (search * theta.sin()).round() as i32,
            );
            // The candidate's whole neighbourhood must clear the spot's own, and lie in the image.
            if ((ox * ox + oy * oy) as f32).sqrt() < 2.0 * (radius + border) {
                continue;
            }
            let (sx, sy) = (center.0 + ox, center.1 + oy);
            if !inside(sx - reach, sy - reach) || !inside(sx + reach, sy + reach) {
                continue;
            }
            let ssd: f32 = ring
                .iter()
                .map(|&(dx, dy)| {
                    let a = px(center.0 + dx, center.1 + dy);
                    let b = px(sx + dx, sy + dy);
                    (0..3).map(|c| (a[c] - b[c]).powi(2)).sum::<f32>()
                })
                .sum();
            if best.is_none_or(|(_, s)| ssd < s) {
                best = Some(((ox, oy), ssd));
            }
        }
    }
    best.map(|(o, _)| o)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RgbBuffer;

    /// Vertical stripes with a 16-px period: the "texture" a good source must match in phase.
    fn stripes(w: u32, h: u32) -> RgbBuffer {
        RgbBuffer {
            width: w,
            height: h,
            data: (0..w * h)
                .map(|i| {
                    let x = i % w;
                    let v = if (x / 8).is_multiple_of(2) { 0.2 } else { 0.8 };
                    [v, v, v]
                })
                .collect(),
        }
    }

    #[test]
    fn picks_a_source_whose_surroundings_match_the_spot() {
        let img = stripes(200, 120);
        let (dx, dy) = auto_source_pick(&img, (100, 60), 6.0).expect("plenty of room");
        // Stripes repeat every 16 px in x and are constant in y: a matching source sits at an
        // x-offset that is a multiple of 16 (any y works).
        assert_eq!(
            dx.rem_euclid(16),
            0,
            "offset ({dx},{dy}) must keep the stripe phase"
        );
    }

    #[test]
    fn candidates_never_leave_the_image_or_touch_the_spot() {
        let img = stripes(200, 120);
        for center in [(100, 60), (30, 30), (170, 100), (8, 60)] {
            if let Some((dx, dy)) = auto_source_pick(&img, center, 6.0) {
                let (sx, sy) = (center.0 + dx, center.1 + dy);
                let reach = 6 + 3;
                assert!(
                    sx - reach >= 0 && sy - reach >= 0,
                    "{center:?} -> ({sx},{sy})"
                );
                assert!(
                    sx + reach < 200 && sy + reach < 120,
                    "{center:?} -> ({sx},{sy})"
                );
                assert!(((dx * dx + dy * dy) as f32).sqrt() >= 2.0 * 9.0);
            }
        }
    }

    #[test]
    fn a_spot_whose_own_ring_leaves_the_image_has_no_answer() {
        let img = stripes(200, 120);
        assert_eq!(auto_source_pick(&img, (2, 60), 6.0), None);
    }

    #[test]
    fn a_tiny_image_or_bad_radius_has_no_answer() {
        assert_eq!(auto_source_pick(&stripes(20, 20), (10, 10), 6.0), None);
        for r in [0.0, -3.0, f32::NAN, f32::INFINITY] {
            assert_eq!(auto_source_pick(&stripes(200, 120), (100, 60), r), None);
        }
    }

    #[test]
    fn is_deterministic() {
        let img = stripes(200, 120);
        assert_eq!(
            auto_source_pick(&img, (100, 60), 6.0),
            auto_source_pick(&img, (100, 60), 6.0)
        );
    }
}
