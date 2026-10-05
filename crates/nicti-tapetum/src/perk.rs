//! One-click "Auto" tone (#46), a production port of `spikes/pupil::heuristic`'s candidate A (a
//! histogram-percentile heuristic) onto this crate's own [`crate::histogram::Histogram`].
//!
//! **Provisional, not the final pick.** ADR-0099's decision rule between candidate A (this one)
//! and candidate B (`spikes/pupil::fit`, a ridge regression) is only resolved once #202's
//! reference-machine run measures both against real LRC "Auto Settings" output -- see
//! `docs/adr/0099-classic-auto-tone.md`. #46 ships with A now (the simpler, training-data-free
//! candidate) so the Auto button isn't blocked indefinitely on a research ticket with no fixed
//! timeline; if #202 picks B instead, only this module's `estimate` function needs to change --
//! its signature (a `Histogram` in, `ExposureParams`/`ToneParams` out) stays the same either way.
//! `spikes/pupil` itself is untouched and stays #202's own measurement tool, not depended on here
//! (this repo's spikes stay self-contained, the same convention `spikes/rods`/`spikes/pupil`
//! already follow for each other).

use crate::auto::{AutoOutcome, AutoReason};
use crate::coat::{ExposureParams, ToneParams};
use crate::histogram::Histogram;

/// Display-encoded sRGB luminance of 18% mid-gray (`srgb_oetf(0.18)`), the exposure target --
/// same constant `spikes/pupil::heuristic` uses.
const MID_GRAY: f64 = 0.4849;
/// A well-exposed, "normal contrast" image has roughly this wide an interquartile spread in
/// display-encoded luminance -- narrower reads as flat (needs +contrast), wider as already punchy
/// (needs -contrast). Same constant `spikes/pupil::heuristic` uses.
const REFERENCE_SPREAD: f64 = 0.35;
const NEAR_WHITE_TARGET: f64 = 0.99;
const NEAR_BLACK_TARGET: f64 = 0.01;
const CLIP_POINT_SCALE: f64 = 2000.0;
/// Share of pixels at either extreme at or above which a histogram counts as degenerate (ADR-0101).
const CLIPPED_FRACTION: f64 = 0.5;
/// Share of pixels in the single tallest luma bin at or above which a histogram counts as degenerate.
const SPIKE_FRACTION: f64 = 0.9;

/// Candidate A's six PV2012 Basic-panel targets, normalized into this crate's own [`ExposureParams`]/
/// [`ToneParams`] convention (`coat.rs`'s -1.0..=1.0 for every Contrast/Highlights/Shadows/
/// Whites/Blacks field; `ExposureParams::stops` stays raw EV, matching its own -5.0..=5.0 range).
/// `histogram` must be built from a **default-params render** (WB/exposure/tone all at their
/// stage defaults) -- the same thing ADR-0099 analyzes; running this against an already-edited
/// render would have Auto chase the user's own prior edits instead of the original image.
///
/// **Degradation contract (ADR-0101)**: always yields a value (matching LRC's Auto), never
/// `NoResult` -- `DecodeIncomplete` is the orchestration layer's call, this only sees a
/// `Histogram`. A degenerate histogram (see [`is_degenerate`]) is `LowConfidence`: still applied,
/// with a marker on the control.
pub fn estimate(histogram: &Histogram) -> AutoOutcome<(ExposureParams, ToneParams)> {
    let value = estimate_values(histogram);
    if is_degenerate(histogram) {
        AutoOutcome::LowConfidence(value, AutoReason::AtypicalInput)
    } else {
        AutoOutcome::Confident(value)
    }
}

/// Near-empty, heavily clipped (more than `CLIPPED_FRACTION` of pixels at either extreme) or
/// single-spike (one luma bin holds at least `SPIKE_FRACTION` of all pixels) -- a histogram the
/// percentile heuristic has little to say about. Untuned starting points, like the rest of this
/// module until #202.
fn is_degenerate(histogram: &Histogram) -> bool {
    let total = histogram.total();
    if total == 0 {
        return true;
    }
    let clipped = histogram.luma_fraction_above(0.98) + histogram.luma_fraction_below(0.02);
    let tallest_bin = histogram.luma.iter().copied().max().unwrap_or(0);
    clipped >= CLIPPED_FRACTION || f64::from(tallest_bin) >= SPIKE_FRACTION * f64::from(total)
}

fn estimate_values(histogram: &Histogram) -> (ExposureParams, ToneParams) {
    let median = histogram.luma_percentile(50.0) as f64;
    let exposure_stops = (MID_GRAY / median.max(1e-4)).log2().clamp(-5.0, 5.0);

    let p25 = histogram.luma_percentile(25.0) as f64;
    let p75 = histogram.luma_percentile(75.0) as f64;
    let spread = p75 - p25;
    let contrast = ((REFERENCE_SPREAD - spread) / REFERENCE_SPREAD).clamp(-1.0, 1.0);

    let highlight_clip = histogram.luma_fraction_above(0.98);
    let highlights = (-highlight_clip * 4.0).clamp(-1.0, 1.0);

    let shadow_clip = histogram.luma_fraction_below(0.02);
    let shadows = (shadow_clip * 4.0).clamp(-1.0, 1.0);

    let p995 = histogram.luma_percentile(99.5) as f64;
    let whites = ((NEAR_WHITE_TARGET - p995) * CLIP_POINT_SCALE / 100.0).clamp(-1.0, 1.0);

    let p05 = histogram.luma_percentile(0.5) as f64;
    let blacks = ((NEAR_BLACK_TARGET - p05) * CLIP_POINT_SCALE / 100.0).clamp(-1.0, 1.0);

    (
        ExposureParams {
            stops: exposure_stops as f32,
        },
        ToneParams {
            contrast: contrast as f32,
            highlights: highlights as f32,
            shadows: shadows as f32,
            whites: whites as f32,
            blacks: blacks as f32,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::histogram::from_display_pixels;

    fn uniform_histogram(min: f32, max: f32, n: usize) -> Histogram {
        let pixels: Vec<[f32; 4]> = (0..n)
            .map(|i| {
                let v = min + (max - min) * i as f32 / (n - 1).max(1) as f32;
                [v, v, v, 1.0]
            })
            .collect();
        from_display_pixels(&pixels)
    }

    /// The estimated values regardless of confidence (every histogram yields a value).
    fn est(histogram: &Histogram) -> (ExposureParams, ToneParams) {
        *estimate(histogram)
            .value()
            .expect("auto-tone always yields a value")
    }

    #[test]
    fn a_spread_out_histogram_is_confident() {
        let ramp = uniform_histogram(0.05, 0.95, 200);
        assert!(matches!(estimate(&ramp), AutoOutcome::Confident(_)));
    }

    #[test]
    fn an_empty_histogram_is_low_confidence_but_still_yields_a_value() {
        let empty = from_display_pixels(&[]);
        assert!(matches!(
            estimate(&empty),
            AutoOutcome::LowConfidence(_, AutoReason::AtypicalInput)
        ));
    }

    #[test]
    fn a_half_black_half_white_histogram_is_low_confidence() {
        let mut pixels = vec![[0.0, 0.0, 0.0, 1.0]; 500];
        pixels.extend(vec![[1.0, 1.0, 1.0, 1.0]; 500]);
        assert!(matches!(
            estimate(&from_display_pixels(&pixels)),
            AutoOutcome::LowConfidence(_, AutoReason::AtypicalInput)
        ));
    }

    #[test]
    fn a_single_spike_histogram_is_low_confidence() {
        let flat = from_display_pixels(&vec![[0.5, 0.5, 0.5, 1.0]; 400]);
        assert!(matches!(
            estimate(&flat),
            AutoOutcome::LowConfidence(_, AutoReason::AtypicalInput)
        ));
    }

    #[test]
    fn dark_image_gets_positive_exposure() {
        let dark = uniform_histogram(0.05, 0.25, 200);
        let (exposure, _) = est(&dark);
        assert!(exposure.stops > 0.0, "stops={}", exposure.stops);
    }

    #[test]
    fn bright_image_gets_negative_exposure() {
        let bright = uniform_histogram(0.7, 0.95, 200);
        let (exposure, _) = est(&bright);
        assert!(exposure.stops < 0.0, "stops={}", exposure.stops);
    }

    #[test]
    fn heavy_highlight_clipping_gives_negative_highlights_and_whites() {
        let mut pixels: Vec<[f32; 4]> = (0..100)
            .map(|i| {
                let v = i as f32 / 200.0;
                [v, v, v, 1.0]
            })
            .collect();
        pixels.extend(std::iter::repeat_n([1.0, 1.0, 1.0, 1.0], 100));
        let clipped = from_display_pixels(&pixels);
        let (_, tone) = est(&clipped);
        assert!(tone.highlights < 0.0, "highlights={}", tone.highlights);
        assert!(tone.whites < 0.0, "whites={}", tone.whites);
    }

    #[test]
    fn heavy_shadow_clipping_gives_positive_shadows_and_blacks() {
        let mut pixels: Vec<[f32; 4]> = (0..100)
            .map(|i| {
                let v = 0.5 + i as f32 / 200.0;
                [v, v, v, 1.0]
            })
            .collect();
        pixels.extend(std::iter::repeat_n([0.0, 0.0, 0.0, 1.0], 100));
        let crushed = from_display_pixels(&pixels);
        let (_, tone) = est(&crushed);
        assert!(tone.shadows > 0.0, "shadows={}", tone.shadows);
        assert!(tone.blacks > 0.0, "blacks={}", tone.blacks);
    }

    #[test]
    fn flat_low_contrast_image_gets_positive_contrast() {
        let flat = uniform_histogram(0.45, 0.55, 200);
        let (_, tone) = est(&flat);
        assert!(tone.contrast > 0.0, "contrast={}", tone.contrast);
    }

    #[test]
    fn every_output_stays_within_coat_rs_documented_range() {
        let mut pixels = vec![[0.0, 0.0, 0.0, 1.0]; 500];
        pixels.extend(vec![[1.0, 1.0, 1.0, 1.0]; 500]);
        let extreme = from_display_pixels(&pixels);
        let (exposure, tone) = est(&extreme);
        assert!((-5.0..=5.0).contains(&exposure.stops));
        for v in [
            tone.contrast,
            tone.highlights,
            tone.shadows,
            tone.whites,
            tone.blacks,
        ] {
            assert!((-1.0..=1.0).contains(&v), "out of range: {v}");
        }
    }
}
