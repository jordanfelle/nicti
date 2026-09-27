//! B0: the training-set mean, predicted for every input regardless of features -- the floor every
//! real model (B1/M1/M2) must beat. Not "a model" in any interesting sense; its only job is to
//! catch a fit that's actually worse than doing nothing.

use crate::sliders::{Sliders, SLIDER_COUNT};

pub struct MeanModel {
    mean: Sliders,
}

impl MeanModel {
    pub fn fit(targets: &[Sliders]) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !targets.is_empty(),
            "cannot fit a mean model on zero samples"
        );
        let mut sum = [0.0; SLIDER_COUNT];
        for t in targets {
            for (s, v) in sum.iter_mut().zip(t.as_array().iter()) {
                *s += v;
            }
        }
        let n = targets.len() as f64;
        for s in sum.iter_mut() {
            *s /= n;
        }
        Ok(Self {
            mean: Sliders::from_array(sum),
        })
    }

    pub fn predict(&self) -> Sliders {
        self.mean
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predicts_the_arithmetic_mean() {
        let targets = vec![
            Sliders {
                exposure2012: 1.0,
                ..Default::default()
            },
            Sliders {
                exposure2012: 3.0,
                ..Default::default()
            },
        ];
        let model = MeanModel::fit(&targets).unwrap();
        assert!((model.predict().exposure2012 - 2.0).abs() < 1e-9);
    }

    #[test]
    fn rejects_zero_samples() {
        assert!(MeanModel::fit(&[]).is_err());
    }
}
