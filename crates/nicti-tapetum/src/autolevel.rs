//! Auto-level (#47): downscale -> Canny edge detection -> Hough line transform -> median angle ->
//! a rotation delta the caller feeds into `coat::CropParams::set_rotation`/
//! `geometry::affine_for_crop`, exactly like the manual Ctrl-drag-a-reference-line gesture writes
//! into the same field -- this button is a complementary, automatic way to reach the same value,
//! not a separate mechanism.
//!
//! Uses `imageproc` (`edges::canny` + `hough::detect_lines`), per this ticket's own preference:
//! it covers both primitives without pulling in the full opencv-rust binding surface (see
//! `docs/adr/0047-crop-straighten-autolevel.md`).

use image::{GrayImage, Luma};
use imageproc::edges::canny;
use imageproc::hough::{detect_lines, LineDetectionOptions, PolarLine};

use crate::auto::{AutoOutcome, AutoReason};
use crate::geometry::straighten_delta_degrees;

/// The long edge a source image is downscaled to before edge/line detection -- large enough to
/// preserve real architectural lines, small enough that Canny+Hough run in well under a second on
/// a full-res photo. Not derived from any measured benchmark (no real-photo harness exists yet for
/// this path, see this module's own doc comment on `detect_level_angle`'s test coverage) -- a
/// documented, reasonable starting point, tunable later without changing this function's contract.
pub const DOWNSCALE_LONG_EDGE: u32 = 800;

/// A detected line is only considered evidence for auto-level if it's within this many degrees of
/// being already horizontal or vertical -- a real diagonal feature in the photo (a fence rail
/// receding in perspective, a staircase) should never drag the median toward its own angle. LRC's
/// own auto-straighten has an equivalent "near-axis" gate; this value is a documented, reasonable
/// starting point.
const MAX_AXIS_DEVIATION_DEGREES: f32 = 30.0;

/// Fewer qualifying lines than this is too little evidence to trust the median (ADR-0101). Set to
/// 1 for now because Hough's `suppression_radius` merges a single thick synthetic line into one
/// detection (measured: that fixture yields exactly 1 line), and a lone dominant line (a horizon)
/// is legitimate evidence -- so today the disagreement check below is the live low-confidence
/// trigger. No real-photo measurement exists; #273 tunes this.
const MIN_SUPPORTING_LINES: usize = 1;

/// If any qualifying line's deviation differs from the median by more than this many degrees, the
/// lines disagree about which way is level (ADR-0101). A documented starting point -- tuned by #273.
const MAX_ANGLE_DISAGREEMENT_DEGREES: f32 = 3.0;

/// One Hough-detected line's deviation from level, in degrees, or `None` if it's not close enough
/// to horizontal/vertical to count as evidence (see `MAX_AXIS_DEVIATION_DEGREES`).
fn line_deviation_degrees(line: &PolarLine) -> Option<f32> {
    // `PolarLine::angle_in_degrees` is the clockwise angle between the x-axis and the line's own
    // *normal*, 0..180 (imageproc's own doc comment) -- the line's own direction is 90 degrees
    // from that, in this crate's same (x-right, y-down) convention `straighten_delta_degrees`
    // already uses.
    let direction_deg = (line.angle_in_degrees as f32) - 90.0;
    let direction_rad = direction_deg.to_radians();
    let (dy, dx) = direction_rad.sin_cos();
    let delta = straighten_delta_degrees(dx, dy);
    if delta.abs() <= MAX_AXIS_DEVIATION_DEGREES {
        Some(delta)
    } else {
        None
    }
}

/// The median of a non-empty slice of `f32`s (sorted copy, middle element -- or the mean of the
/// two middle elements for an even length). Returns `None` for an empty slice.
fn median(values: &mut [f32]) -> Option<f32> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.total_cmp(b));
    let mid = values.len() / 2;
    if values.len().is_multiple_of(2) {
        Some((values[mid - 1] + values[mid]) / 2.0)
    } else {
        Some(values[mid])
    }
}

/// Downscales a display-encoded RGBA buffer (row-major, `width * height` entries, channel values
/// roughly in `0.0..=1.0`) to `GrayImage` at (at most) `DOWNSCALE_LONG_EDGE` on its long edge,
/// converting to 8-bit luma. Nearest-neighbor (not bilinear) -- edge detection doesn't need
/// smooth resampling, and this keeps the implementation independent of `geometry::sample_bilinear`
/// (a genuinely different resize, not a repeat of that logic).
fn downscale_to_gray(pixels: &[[f32; 4]], width: u32, height: u32) -> GrayImage {
    let long_edge = width.max(height).max(1);
    let scale = (DOWNSCALE_LONG_EDGE as f32 / long_edge as f32).min(1.0);
    let out_width = ((width as f32 * scale).round() as u32).max(1);
    let out_height = ((height as f32 * scale).round() as u32).max(1);

    let mut gray = GrayImage::new(out_width, out_height);
    for oy in 0..out_height {
        for ox in 0..out_width {
            let sx = ((ox as f32 / out_width as f32) * width as f32) as u32;
            let sy = ((oy as f32 / out_height as f32) * height as f32) as u32;
            let sx = sx.min(width - 1);
            let sy = sy.min(height - 1);
            let p = pixels[(sy * width + sx) as usize];
            // Rec. 709 luma, matching `histogram.rs`'s own channel convention (R=0, G=1, B=2) --
            // display-encoded (roughly sRGB-gamma) input, so this is a perceptual luma, not a
            // linear one, which is what an edge detector wants for photographic content anyway.
            let luma = (0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2]).clamp(0.0, 1.0);
            gray.put_pixel(ox, oy, Luma([(luma * 255.0).round() as u8]));
        }
    }
    gray
}

/// Detects the dominant near-horizontal/near-vertical tilt in a display-encoded RGBA image and
/// returns the rotation delta (degrees) that would level it -- `None` if no line met the
/// `MAX_AXIS_DEVIATION_DEGREES` gate (e.g. a featureless image, or every detected line is a real
/// diagonal). The returned delta uses the exact same sign convention as
/// `geometry::straighten_delta_degrees`, so a caller adds it to the current
/// `CropParams::rotation_degrees` exactly the way the manual gesture does.
///
/// **Sandbox note**: this is proven against synthetic test images with known, exact tilt angles
/// (see this module's tests) -- there is no real-photo golden-image harness for this path yet (no
/// such harness exists anywhere in this crate for a genuinely photographic image; `bench/knead`'s
/// own real-NEF goldens are #45's scope, not this one). Real-photo quality/threshold tuning is
/// deliberately left as a follow-up rather than blocking this ticket on it.
///
/// **Degradation contract (ADR-0101)**: the result is an [`AutoOutcome`]. `NoResult` means no line
/// qualified (or the buffer was unusable); `LowConfidence` means lines qualified but there were too
/// few of them (`MIN_SUPPORTING_LINES`) or their angles disagree by more than
/// `MAX_ANGLE_DISAGREEMENT_DEGREES` -- the caller skips, not applies, a low-confidence angle (a
/// wrong rotation is worse than none). Both thresholds are untuned starting points (#273).
pub fn detect_level_angle(pixels: &[[f32; 4]], width: u32, height: u32) -> AutoOutcome<f32> {
    if width == 0 || height == 0 || pixels.len() != (width as usize * height as usize) {
        return AutoOutcome::NoResult(AutoReason::AtypicalInput);
    }
    let gray = downscale_to_gray(pixels, width, height);
    let edges = canny(&gray, 20.0, 50.0);
    let lines = detect_lines(
        &edges,
        LineDetectionOptions {
            vote_threshold: (gray.width().min(gray.height()) / 8).max(10),
            suppression_radius: 8,
        },
    );

    let mut deviations: Vec<f32> = lines.iter().filter_map(line_deviation_degrees).collect();
    let Some(angle) = median(&mut deviations) else {
        return AutoOutcome::NoResult(AutoReason::NoFeatures);
    };
    let max_disagreement = deviations
        .iter()
        .map(|d| (d - angle).abs())
        .fold(0.0_f32, f32::max);
    if deviations.len() < MIN_SUPPORTING_LINES || max_disagreement > MAX_ANGLE_DISAGREEMENT_DEGREES
    {
        AutoOutcome::LowConfidence(angle, AutoReason::WeakEvidence)
    } else {
        AutoOutcome::Confident(angle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Renders a synthetic RGBA buffer containing a single straight bright line on a dark
    /// background, tilted by `tilt_degrees` from horizontal, long enough and thick enough for
    /// Canny+Hough to reliably pick up at this module's downscale target.
    fn line_image(size: u32, tilt_degrees: f32) -> Vec<[f32; 4]> {
        let mut pixels = vec![[0.05, 0.05, 0.05, 1.0]; (size * size) as usize];
        let theta = tilt_degrees.to_radians();
        let (sin_t, cos_t) = theta.sin_cos();
        let center = size as f32 / 2.0;
        let half_len = size as f32 * 0.4;
        let steps = (half_len * 4.0) as i32;
        for step in -steps..=steps {
            let t = step as f32 / 4.0;
            let x = center + t * cos_t;
            let y = center + t * sin_t;
            // A few pixels thick so downscaling/Canny don't lose it entirely.
            for oy in -1..=1i32 {
                for ox in -1..=1i32 {
                    let px = x as i32 + ox;
                    let py = y as i32 + oy;
                    if px >= 0 && py >= 0 && (px as u32) < size && (py as u32) < size {
                        pixels[(py as u32 * size + px as u32) as usize] = [0.95, 0.95, 0.95, 1.0];
                    }
                }
            }
        }
        pixels
    }

    #[test]
    fn detects_an_exactly_level_horizontal_line_as_already_level() {
        let size = 200;
        let pixels = line_image(size, 0.0);
        let AutoOutcome::Confident(angle) = detect_level_angle(&pixels, size, size) else {
            panic!("a clear horizontal line should be detected confidently");
        };
        assert!(angle.abs() < 1.0, "angle = {angle}");
    }

    #[test]
    fn detects_a_tilted_line_and_returns_the_correcting_delta() {
        let size = 200;
        let tilt = 8.0;
        let pixels = line_image(size, tilt);
        let AutoOutcome::Confident(angle) = detect_level_angle(&pixels, size, size) else {
            panic!("a clear tilted line should be detected confidently");
        };
        // The correcting delta should be close to -tilt (matching
        // `straighten_delta_degrees`'s own sign convention: a line tilted +8 degrees below
        // horizontal needs a -8 degree correction).
        assert!(
            (angle - (-tilt)).abs() < 2.0,
            "angle = {angle}, tilt = {tilt}"
        );
    }

    #[test]
    fn a_featureless_image_has_no_result() {
        let size = 64;
        let pixels = vec![[0.5, 0.5, 0.5, 1.0]; (size * size) as usize];
        assert_eq!(
            detect_level_angle(&pixels, size, size),
            AutoOutcome::NoResult(AutoReason::NoFeatures)
        );
    }

    #[test]
    fn a_zero_sized_image_has_no_result() {
        assert_eq!(
            detect_level_angle(&[], 0, 0),
            AutoOutcome::NoResult(AutoReason::AtypicalInput)
        );
    }

    /// Two thick lines at clearly different tilts (2 and 12 degrees): the median lands between
    /// them and each is well over `MAX_ANGLE_DISAGREEMENT_DEGREES` from it.
    fn two_line_image(size: u32, tilt_a: f32, tilt_b: f32) -> Vec<[f32; 4]> {
        let (a, b) = (line_image(size, tilt_a), line_image(size, tilt_b));
        a.iter()
            .zip(&b)
            .map(|(pa, pb)| if pa[0] > pb[0] { *pa } else { *pb })
            .collect()
    }

    #[test]
    fn lines_that_disagree_about_level_are_low_confidence() {
        let size = 200;
        let pixels = two_line_image(size, 2.0, 12.0);
        assert!(
            matches!(
                detect_level_angle(&pixels, size, size),
                AutoOutcome::LowConfidence(_, AutoReason::WeakEvidence)
            ),
            "{:?}",
            detect_level_angle(&pixels, size, size)
        );
    }

    #[test]
    fn a_diagonal_line_far_from_either_axis_is_not_counted_as_evidence() {
        let size = 200;
        let pixels = line_image(size, 45.0);
        // A perfect 45-degree line is equidistant from both axes and should be gated out by
        // `MAX_AXIS_DEVIATION_DEGREES` (30 degrees) -- no confident axis to snap to.
        assert_eq!(
            detect_level_angle(&pixels, size, size),
            AutoOutcome::NoResult(AutoReason::NoFeatures)
        );
    }

    #[test]
    fn median_of_empty_slice_is_none() {
        assert_eq!(median(&mut []), None);
    }

    #[test]
    fn median_of_odd_length_is_the_middle_element() {
        let mut values = [3.0, 1.0, 2.0];
        assert_eq!(median(&mut values), Some(2.0));
    }

    #[test]
    fn median_of_even_length_is_the_average_of_the_two_middle_elements() {
        let mut values = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(median(&mut values), Some(2.5));
    }
}
