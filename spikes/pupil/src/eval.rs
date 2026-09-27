//! Per-slider MAE/p95/bias, the primary accuracy metric ADR-0099 picks a candidate on -- not a
//! golden-image comparison, since nothing can render PV2012 sliders to pixels yet (that's #46,
//! which owns the rendered-image comparison the issue originally asked for).

use crate::histogram::Histogram;
use crate::sliders::Sliders;

#[derive(Debug, Clone, Copy)]
pub struct SliderMetric {
    pub mae: f64,
    pub p95_abs_error: f64,
    /// Mean signed error (predicted - truth); a nonzero bias means the candidate is
    /// systematically over/under-shooting, not just noisily wrong.
    pub bias: f64,
}

#[derive(Debug, Clone)]
pub struct EvalReport {
    /// One entry per slider, in `Sliders::NAMES` order.
    pub per_slider: [(&'static str, SliderMetric); 6],
}

/// Computes per-slider error metrics between paired predictions and ground truth. `predicted` and
/// `truth` must be the same, nonzero length and index-aligned (same file at the same position).
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
                contrast2012: 10.0,
                ..Default::default()
            },
            Sliders {
                exposure2012: -0.5,
                contrast2012: -10.0,
                ..Default::default()
            },
        ];
        let report = evaluate(&truth, &truth).unwrap();
        for (_, m) in report.per_slider {
            assert_eq!(m.mae, 0.0);
            assert_eq!(m.p95_abs_error, 0.0);
            assert_eq!(m.bias, 0.0);
        }
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
