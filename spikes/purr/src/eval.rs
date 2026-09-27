//! Per-slider MAE/p95/bias across all eight sliders -- a copy of `spikes/pupil::eval`, generalized
//! from six sliders to eight. ADR-0053's decision rule is read off this report.

use crate::histogram::Histogram;
use crate::sliders::{Sliders, SLIDER_COUNT};

#[derive(Debug, Clone, Copy)]
pub struct SliderMetric {
    pub mae: f64,
    pub p95_abs_error: f64,
    pub bias: f64,
}

#[derive(Debug, Clone)]
pub struct EvalReport {
    pub per_slider: [(&'static str, SliderMetric); SLIDER_COUNT],
}

impl EvalReport {
    /// Mean of the per-slider MAE, each normalized by that slider's documented range width --
    /// makes Exposure2012 (range width 10) and Contrast2012 (range width 200) comparable in one
    /// aggregate number, which ADR-0053's decision rule (1) needs to compare candidates.
    pub fn mean_normalized_mae(&self) -> f64 {
        let sum: f64 = self
            .per_slider
            .iter()
            .zip(Sliders::RANGES.iter())
            .map(|((_, m), (lo, hi))| m.mae / (hi - lo))
            .sum();
        sum / SLIDER_COUNT as f64
    }
}

/// Computes per-slider error metrics between paired predictions and ground truth. `predicted` and
/// `truth` must be the same, nonzero length and index-aligned.
pub fn evaluate(predicted: &[Sliders], truth: &[Sliders]) -> anyhow::Result<EvalReport> {
    anyhow::ensure!(!predicted.is_empty(), "cannot evaluate zero samples");
    anyhow::ensure!(
        predicted.len() == truth.len(),
        "predicted ({}) and truth ({}) must be the same length",
        predicted.len(),
        truth.len()
    );

    let per_slider = std::array::from_fn(|idx| {
        let signed_errors: Vec<f64> = predicted
            .iter()
            .zip(truth.iter())
            .map(|(p, t)| p.as_array()[idx] - t.as_array()[idx])
            .collect();
        let abs_errors: Vec<f32> = signed_errors.iter().map(|e| e.abs() as f32).collect();
        let mae = abs_errors.iter().map(|&e| e as f64).sum::<f64>() / abs_errors.len() as f64;
        let bias = signed_errors.iter().sum::<f64>() / signed_errors.len() as f64;
        let p95_abs_error = Histogram::from_samples(abs_errors).percentile(95.0) as f64;
        (
            Sliders::NAMES[idx],
            SliderMetric {
                mae,
                p95_abs_error,
                bias,
            },
        )
    });

    Ok(EvalReport { per_slider })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perfect_predictions_have_zero_error() {
        let truth = vec![
            Sliders {
                exposure2012: 0.5,
                ..Default::default()
            },
            Sliders {
                vibrance: -10.0,
                ..Default::default()
            },
        ];
        let report = evaluate(&truth, &truth).unwrap();
        for (_, m) in report.per_slider {
            assert_eq!(m.mae, 0.0);
        }
        assert_eq!(report.mean_normalized_mae(), 0.0);
    }

    #[test]
    fn detects_a_systematic_bias() {
        let truth = vec![Sliders::default(), Sliders::default()];
        let predicted = vec![
            Sliders {
                exposure2012: 1.0,
                ..Default::default()
            },
            Sliders {
                exposure2012: 1.0,
                ..Default::default()
            },
        ];
        let report = evaluate(&predicted, &truth).unwrap();
        let (_, exposure_metric) = report.per_slider[0];
        assert!((exposure_metric.mae - 1.0).abs() < 1e-9);
        assert!((exposure_metric.bias - 1.0).abs() < 1e-9);
    }

    #[test]
    fn rejects_mismatched_lengths() {
        let a = vec![Sliders::default()];
        let b = vec![Sliders::default(), Sliders::default()];
        assert!(evaluate(&a, &b).is_err());
    }

    #[test]
    fn rejects_empty_input() {
        assert!(evaluate(&[], &[]).is_err());
    }
}
