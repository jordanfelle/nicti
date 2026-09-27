//! The synthetic + keeper-false-flag measurement pass (#34/ADR-0034's Measured results, the part
//! that's real this pass -- see the ADR's Context for why the real-reject accuracy pass itself is
//! deferred to a follow-up issue against the user's next unculled con card).
//!
//! **Methodology, stated before any numbers exist (same discipline ADR-0033 followed):**
//! 1. For each candidate, pick a decision threshold from a *reference severity*
//!    (`REFERENCE_DEFOCUS`, a defocus radius chosen to be unambiguously reject-worthy by eye): the
//!    median candidate score across every keeper image degraded at that severity.
//! 2. **Detection rate** for every other degradation in `synth::default_sweep()`: the fraction of
//!    degraded images whose score falls below that same threshold.
//! 3. **Keeper false-flag rate** (ADR-0034's headline number): the fraction of *undegraded* keeper
//!    images whose score falls below that same threshold -- these are real photos the user actually
//!    kept, so any flag here is a real false reject, not a synthetic one.
//!
//! This ties the threshold to one concrete, inspectable severity rather than an arbitrary
//! percentile pick, so a reader can sanity-check "is this candidate's threshold reasonable" against
//! an image they can look at.

use std::path::Path;

use image::RgbImage;
use serde::Serialize;

use crate::sharp::{
    fft_high_freq_ratio_tiled, laplacian_variance_tiled, tenengrad_tiled, GrayFrame,
};
use crate::synth::{self, Degradation};

/// The reference severity every candidate's threshold is calibrated against -- see this module's
/// doc comment. A disk-kernel radius of 6px on a decoded preview is well past "gentle softness,"
/// chosen to look unambiguously out-of-focus in a manual spot-check during this pass.
pub const REFERENCE_DEGRADATION: Degradation = Degradation::Defocus { radius: 6 };

pub const DEFAULT_TILE: u32 = 64;
pub const FFT_CUTOFF_FRAC: f32 = 0.25;

/// A named scoring function -- `(candidate name, score fn)`.
pub type NamedCandidate = (&'static str, fn(&GrayFrame) -> f64);

/// Every scoring function, named for reporting. Each candidate here is a *global* sharpness score
/// (tile-max) -- the AF-aware misfocus candidate is measured separately in
/// `af_region_misfocus_ratio`'s own tests, since it needs a real AF-area rectangle per file rather
/// than a single sweep-wide score.
pub fn candidates() -> Vec<NamedCandidate> {
    vec![
        ("laplacian_variance", |f| {
            laplacian_variance_tiled(f, DEFAULT_TILE).0
        }),
        ("tenengrad", |f| tenengrad_tiled(f, DEFAULT_TILE).0),
        ("fft_high_freq_ratio", |f| {
            fft_high_freq_ratio_tiled(f, DEFAULT_TILE, FFT_CUTOFF_FRAC).0
        }),
    ]
}

#[derive(Debug, Clone, Serialize)]
pub struct DegradationResult {
    pub degradation: String,
    pub detection_rate: f64,
    pub mean_score_ratio: f64,
    pub n: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct CandidateReport {
    pub candidate: String,
    pub threshold: f64,
    pub keeper_false_flag_rate: f64,
    pub keeper_count: usize,
    pub by_degradation: Vec<DegradationResult>,
}

/// Loads every image at `paths`, decoding (not resizing) so scores are comparable across a run.
/// Callers are expected to point this at a real keeper folder -- see `main.rs`'s `eval` subcommand.
pub fn load_keepers(paths: &[impl AsRef<Path>]) -> anyhow::Result<Vec<RgbImage>> {
    let mut out = Vec::with_capacity(paths.len());
    for p in paths {
        let img = image::open(p.as_ref())
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", p.as_ref().display()))?
            .to_rgb8();
        out.push(img);
    }
    Ok(out)
}

/// Runs the full methodology above for every registered candidate. `keepers` should be real,
/// already-culled (kept) images -- their *undegraded* scores are what the false-flag rate is
/// measured against.
pub fn run(keepers: &[RgbImage]) -> Vec<CandidateReport> {
    let sweep = synth::default_sweep();
    let mut reports = Vec::new();

    for (name, score_fn) in candidates() {
        let baseline_scores: Vec<f64> = keepers
            .iter()
            .map(|img| score_fn(&GrayFrame::from_rgb(img)))
            .collect();

        let reference_scores: Vec<f64> = keepers
            .iter()
            .map(|img| {
                let degraded = REFERENCE_DEGRADATION.apply(img);
                score_fn(&GrayFrame::from_rgb(&degraded))
            })
            .collect();
        let threshold = median(&reference_scores);

        let false_positives = baseline_scores.iter().filter(|&&s| s < threshold).count();
        let keeper_false_flag_rate = if baseline_scores.is_empty() {
            0.0
        } else {
            false_positives as f64 / baseline_scores.len() as f64
        };

        let mut by_degradation = Vec::with_capacity(sweep.len());
        for degradation in &sweep {
            let mut ratios = Vec::with_capacity(keepers.len());
            let mut below = 0usize;
            for (img, &baseline) in keepers.iter().zip(baseline_scores.iter()) {
                let degraded_img = degradation.apply(img);
                let score = score_fn(&GrayFrame::from_rgb(&degraded_img));
                if score < threshold {
                    below += 1;
                }
                if baseline > 0.0 {
                    ratios.push(score / baseline);
                }
            }
            let n = keepers.len();
            by_degradation.push(DegradationResult {
                degradation: degradation.label(),
                detection_rate: if n == 0 { 0.0 } else { below as f64 / n as f64 },
                mean_score_ratio: if ratios.is_empty() {
                    0.0
                } else {
                    ratios.iter().sum::<f64>() / ratios.len() as f64
                },
                n,
            });
        }

        reports.push(CandidateReport {
            candidate: name.to_string(),
            threshold,
            keeper_false_flag_rate,
            keeper_count: keepers.len(),
            by_degradation,
        });
    }

    reports
}

fn median(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    let mut sorted = xs.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mid = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    } else {
        sorted[mid]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    fn checkerboard(width: u32, height: u32, period: u32) -> RgbImage {
        RgbImage::from_fn(width, height, |x, y| {
            if (x / period + y / period).is_multiple_of(2) {
                Rgb([230, 230, 230])
            } else {
                Rgb([20, 20, 20])
            }
        })
    }

    #[test]
    fn median_of_known_values() {
        assert!((median(&[1.0, 2.0, 3.0]) - 2.0).abs() < 1e-9);
        assert!((median(&[1.0, 2.0, 3.0, 4.0]) - 2.5).abs() < 1e-9);
        assert_eq!(median(&[]), 0.0);
    }

    #[test]
    fn run_detects_more_severe_defocus_more_often_than_gentle_defocus() {
        // A handful of sharp synthetic "keepers" -- real signal (checkerboards at varying
        // periods/offsets so they aren't all identical), not photographs, but enough to exercise
        // the pipeline end-to-end and check its monotonicity property.
        let keepers: Vec<RgbImage> = (0..6).map(|i| checkerboard(64, 64, 4 + (i % 3))).collect();

        let reports = run(&keepers);
        for report in &reports {
            let gentle = report
                .by_degradation
                .iter()
                .find(|d| d.degradation == "defocus_r1")
                .unwrap();
            let severe = report
                .by_degradation
                .iter()
                .find(|d| d.degradation == "defocus_r10")
                .unwrap();
            assert!(
                severe.detection_rate >= gentle.detection_rate,
                "{}: severe defocus should be detected at least as often as gentle defocus (severe={}, gentle={})",
                report.candidate,
                severe.detection_rate,
                gentle.detection_rate,
            );
        }
    }

    #[test]
    fn run_reports_a_false_flag_rate_for_every_candidate() {
        let keepers: Vec<RgbImage> = (0..4).map(|i| checkerboard(64, 64, 4 + i)).collect();
        let reports = run(&keepers);
        assert_eq!(reports.len(), candidates().len());
        for report in &reports {
            assert!(report.keeper_false_flag_rate >= 0.0 && report.keeper_false_flag_rate <= 1.0);
        }
    }
}
