//! Candidate A: a histogram-percentile heuristic, the commonly-documented approximation of
//! Adobe's undisclosed auto-tone algorithm (see #99's issue body). Clip points at fixed
//! percentiles map to Exposure/Contrast/Highlights/Shadows/Whites/Blacks estimates. This is a
//! deliberately simple, explainable starting point -- ADR-0099's decision rule picks between this
//! and `fit`'s empirical candidate B based on measured error against real LRC output.

use crate::histogram::Histogram;
use crate::sliders::Sliders;

/// Display-encoded sRGB luminance of 18% mid-gray (`srgb_oetf(0.18)`), the exposure target.
const MID_GRAY: f64 = 0.4849;

pub fn estimate(hist: &Histogram) -> Sliders {
    let median = hist.percentile(50.0) as f64;
    // log2 ratio, clamped to the slider's own range before the final `clamped()` pass so a
    // near-zero median doesn't blow up toward infinity.
    let exposure2012 = (MID_GRAY / median.max(1e-4)).log2().clamp(-5.0, 5.0);

    let p25 = hist.percentile(25.0) as f64;
    let p75 = hist.percentile(75.0) as f64;
    let spread = p75 - p25;
    // A well-exposed, "normal contrast" image has roughly a 0.35-wide interquartile spread in
    // display-encoded luminance; narrower reads as flat (needs +contrast), wider as already
    // punchy (needs -contrast).
    const REFERENCE_SPREAD: f64 = 0.35;
    let contrast2012 =
        ((REFERENCE_SPREAD - spread) / REFERENCE_SPREAD * 100.0).clamp(-100.0, 100.0);

    let highlight_clip = hist.fraction_above(0.98);
    let highlights2012 = (-highlight_clip * 400.0).clamp(-100.0, 100.0);

    let shadow_clip = hist.fraction_below(0.02);
    let shadows2012 = (shadow_clip * 400.0).clamp(-100.0, 100.0);

    let p995 = hist.percentile(99.5) as f64;
    let whites2012 = ((1.0 - p995) * -200.0).clamp(-100.0, 100.0);

    let p05 = hist.percentile(0.5) as f64;
    let blacks2012 = (p05 * 200.0).clamp(-100.0, 100.0);

    Sliders {
        exposure2012,
        contrast2012,
        highlights2012,
        shadows2012,
        whites2012,
        blacks2012,
    }
    .clamped()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uniform(min: f32, max: f32, n: usize) -> Histogram {
        Histogram::from_samples(
            (0..n)
                .map(|i| min + (max - min) * i as f32 / (n - 1) as f32)
                .collect(),
        )
    }

    #[test]
    fn dark_image_gets_positive_exposure() {
        let dark = uniform(0.05, 0.25, 200);
        assert!(estimate(&dark).exposure2012 > 0.0);
    }

    #[test]
    fn bright_image_gets_negative_exposure() {
        let bright = uniform(0.7, 0.95, 200);
        assert!(estimate(&bright).exposure2012 < 0.0);
    }

    #[test]
    fn darker_median_gives_more_positive_exposure_monotonically() {
        let darker = uniform(0.05, 0.20, 200);
        let less_dark = uniform(0.15, 0.30, 200);
        assert!(estimate(&darker).exposure2012 > estimate(&less_dark).exposure2012);
    }

    #[test]
    fn heavy_highlight_clipping_gives_negative_highlights_and_whites() {
        // Half the samples pinned at 1.0 -- a badly blown-out image.
        let mut samples: Vec<f32> = (0..100).map(|i| i as f32 / 200.0).collect();
        samples.extend(std::iter::repeat_n(1.0, 100));
        let clipped = Histogram::from_samples(samples);
        let s = estimate(&clipped);
        assert!(s.highlights2012 < 0.0);
        assert!(s.whites2012 <= 0.0);
    }

    #[test]
    fn heavy_shadow_clipping_gives_positive_shadows_and_blacks() {
        let mut samples: Vec<f32> = (0..100).map(|i| 0.5 + i as f32 / 200.0).collect();
        samples.extend(std::iter::repeat_n(0.0, 100));
        let crushed = Histogram::from_samples(samples);
        let s = estimate(&crushed);
        assert!(s.shadows2012 > 0.0);
        assert!(s.blacks2012 >= 0.0);
    }

    #[test]
    fn flat_low_contrast_image_gets_positive_contrast() {
        let flat = uniform(0.45, 0.55, 200);
        assert!(estimate(&flat).contrast2012 > 0.0);
    }

    #[test]
    fn every_slider_stays_within_documented_range() {
        // An adversarial histogram: everything pinned at the extremes.
        let extreme =
            Histogram::from_samples(vec![0.0; 500].into_iter().chain(vec![1.0; 500]).collect());
        let s = estimate(&extreme);
        assert!((-5.0..=5.0).contains(&s.exposure2012));
        for v in [
            s.contrast2012,
            s.highlights2012,
            s.shadows2012,
            s.whites2012,
            s.blacks2012,
        ] {
            assert!((-100.0..=100.0).contains(&v), "slider out of range: {v}");
        }
    }
}
