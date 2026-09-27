//! Candidate B: an empirical fit -- ridge regression from a fixed histogram feature vector to the
//! six PV2012 sliders, matched against real LRC "Auto Settings" output (once the deferred
//! reference-machine run in the follow-up issue supplies training data; tested here against
//! synthetic planted-linear-mapping ground truth). Hand-rolled normal-equations solve rather than
//! a linear-algebra crate: six outputs, about a dozen features, no need for the dependency or
//! license-review surface a crate like `nalgebra`/`ndarray` would add (ADR-0018).

use crate::histogram::Histogram;
use crate::sliders::Sliders;

/// Number of features `features()` returns, including the leading bias term.
pub const FEATURE_COUNT: usize = 13;

/// A fixed-length feature vector describing a luminance histogram's shape: percentiles at a
/// spread of ranks, the mean, and the two clip-mass fractions `heuristic` also uses -- plus a
/// leading `1.0` bias term so the fit needs no separate intercept handling.
pub fn features(hist: &Histogram) -> [f64; FEATURE_COUNT] {
    [
        1.0,
        hist.percentile(0.5) as f64,
        hist.percentile(2.0) as f64,
        hist.percentile(10.0) as f64,
        hist.percentile(25.0) as f64,
        hist.percentile(50.0) as f64,
        hist.percentile(75.0) as f64,
        hist.percentile(90.0) as f64,
        hist.percentile(98.0) as f64,
        hist.percentile(99.5) as f64,
        hist.mean() as f64,
        hist.fraction_below(0.02),
        hist.fraction_above(0.98),
    ]
}

#[derive(Debug, Clone)]
pub struct RidgeModel {
    /// `weights[slider_index]` is a `FEATURE_COUNT`-long weight vector for that slider.
    weights: [[f64; FEATURE_COUNT]; 6],
}

impl RidgeModel {
    pub fn predict(&self, features: &[f64; FEATURE_COUNT]) -> Sliders {
        let mut out = [0.0; 6];
        for (slider, w) in out.iter_mut().zip(self.weights.iter()) {
            *slider = w.iter().zip(features.iter()).map(|(wi, fi)| wi * fi).sum();
        }
        Sliders::from_array(out).clamped()
    }
}

/// Fits one independent ridge regression per slider: `w = (X^T X + lambda*I)^-1 X^T y`.
/// `lambda` must be > 0 to guarantee the normal-equations matrix is invertible even with fewer
/// samples than features.
pub fn fit(samples: &[([f64; FEATURE_COUNT], Sliders)], lambda: f64) -> anyhow::Result<RidgeModel> {
    anyhow::ensure!(!samples.is_empty(), "cannot fit on zero samples");
    anyhow::ensure!(
        lambda > 0.0,
        "lambda must be positive for a well-posed solve"
    );

    let n = FEATURE_COUNT;
    // X^T X, an n x n Gram matrix.
    let mut gram = vec![vec![0.0_f64; n]; n];
    for (features, _) in samples {
        for i in 0..n {
            for j in 0..n {
                gram[i][j] += features[i] * features[j];
            }
        }
    }
    for (i, row) in gram.iter_mut().enumerate() {
        row[i] += lambda;
    }

    let mut weights = [[0.0; FEATURE_COUNT]; 6];
    for (slider_idx, w) in weights.iter_mut().enumerate() {
        // X^T y for this one slider's labels.
        let mut xty = vec![0.0_f64; n];
        for (features, target) in samples {
            let y = target.as_array()[slider_idx];
            for i in 0..n {
                xty[i] += features[i] * y;
            }
        }
        let solved = solve_linear_system(gram.clone(), xty)
            .ok_or_else(|| anyhow::anyhow!("ridge normal-equations matrix is singular"))?;
        w.copy_from_slice(&solved);
    }

    Ok(RidgeModel { weights })
}

/// Solves `a * x = b` via Gaussian elimination with partial pivoting. `a` is consumed (this is a
/// throwaway spike's small, one-off solve, not a hot path worth optimizing for reuse).
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

    fn make_hist(seed: f32) -> Histogram {
        Histogram::from_samples(
            (0..200)
                .map(|i| (seed + i as f32 / 400.0).clamp(0.0, 1.0))
                .collect(),
        )
    }

    #[test]
    fn recovers_a_planted_linear_mapping() {
        // Ground truth: Exposure2012 is exactly `2.0 - 4.0 * mean`, every other slider zero.
        let samples: Vec<_> = (0..30)
            .map(|i| {
                let hist = make_hist(0.05 + i as f32 * 0.02);
                let feats = features(&hist);
                let mean = feats[10];
                let target = Sliders {
                    exposure2012: (2.0 - 4.0 * mean).clamp(-5.0, 5.0),
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
            .map(|i| (features(&make_hist(i as f32 * 0.1)), Sliders::default()))
            .collect();
        let model = fit(&samples, 1e-3).expect("fit should succeed");
        // A wildly out-of-distribution input the fit never saw during training.
        let out_of_distribution = [1.0; FEATURE_COUNT];
        let predicted = model.predict(&out_of_distribution);
        assert!((-5.0..=5.0).contains(&predicted.exposure2012));
        for v in [
            predicted.contrast2012,
            predicted.highlights2012,
            predicted.shadows2012,
            predicted.whites2012,
            predicted.blacks2012,
        ] {
            assert!((-100.0..=100.0).contains(&v));
        }
    }

    #[test]
    fn fit_rejects_zero_samples() {
        assert!(fit(&[], 1e-3).is_err());
    }

    #[test]
    fn fit_rejects_non_positive_lambda() {
        let samples = vec![(features(&make_hist(0.2)), Sliders::default())];
        assert!(fit(&samples, 0.0).is_err());
    }

    #[test]
    fn solve_linear_system_solves_a_known_2x2() {
        // [[2, 1], [1, 3]] x = [5, 10] -> x = [1, 3]
        let a = vec![vec![2.0, 1.0], vec![1.0, 3.0]];
        let b = vec![5.0, 10.0];
        let x = solve_linear_system(a, b).expect("solvable system");
        assert!((x[0] - 1.0).abs() < 1e-9);
        assert!((x[1] - 3.0).abs() < 1e-9);
    }

    #[test]
    fn solve_linear_system_detects_singular_matrix() {
        let a = vec![vec![1.0, 2.0], vec![2.0, 4.0]];
        let b = vec![1.0, 2.0];
        assert!(solve_linear_system(a, b).is_none());
    }
}
