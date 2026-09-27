//! B1: ridge regression from the 13-value histogram feature vector to all eight sliders -- a small
//! independent copy of `spikes/pupil::fit`'s hand-rolled normal-equations solve, generalized from
//! six outputs to eight. No linear-algebra crate dependency, same reasoning as `pupil::fit`: a
//! dozen features, eight outputs, not worth the license-review surface (ADR-0018).

use crate::features::HIST_FEATURE_COUNT;
use crate::sliders::{Sliders, SLIDER_COUNT};

#[derive(Debug, Clone)]
pub struct RidgeModel {
    weights: [[f64; HIST_FEATURE_COUNT]; SLIDER_COUNT],
}

impl RidgeModel {
    pub fn predict(&self, features: &[f32; HIST_FEATURE_COUNT]) -> Sliders {
        let mut out = [0.0; SLIDER_COUNT];
        for (slider, w) in out.iter_mut().zip(self.weights.iter()) {
            *slider = w
                .iter()
                .zip(features.iter())
                .map(|(wi, fi)| wi * (*fi as f64))
                .sum();
        }
        Sliders::from_array(out).clamped()
    }
}

/// Fits one independent ridge regression per slider: `w = (X^T X + lambda*I)^-1 X^T y`.
pub fn fit(
    samples: &[([f32; HIST_FEATURE_COUNT], Sliders)],
    lambda: f64,
) -> anyhow::Result<RidgeModel> {
    anyhow::ensure!(!samples.is_empty(), "cannot fit on zero samples");
    anyhow::ensure!(
        lambda > 0.0,
        "lambda must be positive for a well-posed solve"
    );

    let n = HIST_FEATURE_COUNT;
    let mut gram = vec![vec![0.0_f64; n]; n];
    for (features, _) in samples {
        for i in 0..n {
            for j in 0..n {
                gram[i][j] += features[i] as f64 * features[j] as f64;
            }
        }
    }
    for (i, row) in gram.iter_mut().enumerate() {
        row[i] += lambda;
    }

    let mut weights = [[0.0; HIST_FEATURE_COUNT]; SLIDER_COUNT];
    for (slider_idx, w) in weights.iter_mut().enumerate() {
        let mut xty = vec![0.0_f64; n];
        for (features, target) in samples {
            let y = target.as_array()[slider_idx];
            for i in 0..n {
                xty[i] += features[i] as f64 * y;
            }
        }
        let solved = solve_linear_system(gram.clone(), xty)
            .ok_or_else(|| anyhow::anyhow!("ridge normal-equations matrix is singular"))?;
        w.copy_from_slice(&solved);
    }

    Ok(RidgeModel { weights })
}

/// Solves `a * x = b` via Gaussian elimination with partial pivoting. A small, one-off spike solve,
/// not a hot path worth optimizing for reuse -- a copy of `pupil::fit::solve_linear_system`.
fn solve_linear_system(mut a: Vec<Vec<f64>>, mut b: Vec<f64>) -> Option<Vec<f64>> {
    let n = b.len();
    for col in 0..n {
        let pivot_row =
            (col..n).max_by(|&r1, &r2| a[r1][col].abs().total_cmp(&a[r2][col].abs()))?;
        if a[pivot_row][col].abs() < 1e-12 {
            return None;
        }
        a.swap(col, pivot_row);
        b.swap(col, pivot_row);

        let pivot = a[col][col];
        for v in &mut a[col][col..] {
            *v /= pivot;
        }
        b[col] /= pivot;

        let pivot_row_tail = a[col][col..].to_vec();
        for row in 0..n {
            if row == col {
                continue;
            }
            let factor = a[row][col];
            if factor == 0.0 {
                continue;
            }
            for (v, &pivot_v) in a[row][col..].iter_mut().zip(pivot_row_tail.iter()) {
                *v -= factor * pivot_v;
            }
            b[row] -= factor * b[col];
        }
    }
    Some(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_features(mean: f32) -> [f32; HIST_FEATURE_COUNT] {
        let mut f = [0.0; HIST_FEATURE_COUNT];
        f[0] = 1.0;
        f[10] = mean; // mean sits at index 10, same layout as pupil::fit::features.
        f
    }

    #[test]
    fn recovers_a_planted_linear_mapping() {
        let samples: Vec<_> = (0..30)
            .map(|i| {
                let mean = 0.05 + i as f32 * 0.02;
                let feats = make_features(mean);
                let target = Sliders {
                    exposure2012: (2.0 - 4.0 * mean as f64).clamp(-5.0, 5.0),
                    ..Sliders::default()
                };
                (feats, target)
            })
            .collect();

        let model = fit(&samples, 1e-6).expect("fit should succeed");
        for (feats, expected) in &samples {
            let predicted = model.predict(feats);
            assert!(
                (predicted.exposure2012 - expected.exposure2012).abs() < 0.05,
                "expected {}, got {}",
                expected.exposure2012,
                predicted.exposure2012
            );
        }
    }

    #[test]
    fn predictions_stay_within_documented_range() {
        let samples: Vec<_> = (0..10)
            .map(|i| (make_features(i as f32 * 0.1), Sliders::default()))
            .collect();
        let model = fit(&samples, 1e-3).expect("fit should succeed");
        let out_of_distribution = [1.0; HIST_FEATURE_COUNT];
        let predicted = model.predict(&out_of_distribution);
        for (v, range) in predicted.as_array().iter().zip(Sliders::RANGES.iter()) {
            assert!((range.0..=range.1).contains(v));
        }
    }

    #[test]
    fn fit_rejects_zero_samples() {
        assert!(fit(&[], 1e-3).is_err());
    }

    #[test]
    fn fit_rejects_non_positive_lambda() {
        let samples = vec![(make_features(0.2), Sliders::default())];
        assert!(fit(&samples, 0.0).is_err());
    }
}
