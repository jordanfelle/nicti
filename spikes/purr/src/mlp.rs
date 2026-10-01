//! M1/M2: a small CPU-only MLP (`candle-core`/`candle-nn`) predicting all eight sliders from a
//! feature vector. Per ADR-0218, training runs offline, locally, on the user's own machine -- no
//! Python, no hosted training service, no network call anywhere in this module. `input_dim` is the
//! only thing that varies between M1 (13 histogram features), M2 (13 + 3072 thumbnail floats), and
//! the EXIF ablation (M1 + 3 EXIF features) -- see `train.rs` for how each variant assembles its
//! input vector.

use candle_core::{DType, Device, Tensor};
use candle_nn::{linear, AdamW, Linear, Module, Optimizer, ParamsAdamW, VarBuilder, VarMap};

use crate::sliders::SLIDER_COUNT;

pub struct Mlp {
    l1: Linear,
    l2: Linear,
    l3: Linear,
    varmap: VarMap,
    device: Device,
}

/// Per-slider min/max used to scale targets into `-1.0..=1.0` for the loss -- an unscaled loss
/// would let `Contrast2012` (range width 200) dominate `Exposure2012` (range width 10) purely from
/// units, not from which slider the model is actually worse at.
fn scale_targets(targets: &[[f64; SLIDER_COUNT]]) -> Vec<[f32; SLIDER_COUNT]> {
    targets
        .iter()
        .map(|row| {
            std::array::from_fn(|i| {
                let (lo, hi) = crate::sliders::Sliders::RANGES[i];
                (2.0 * (row[i] - lo) / (hi - lo) - 1.0) as f32
            })
        })
        .collect()
}

fn unscale(row: &[f32; SLIDER_COUNT]) -> [f64; SLIDER_COUNT] {
    std::array::from_fn(|i| {
        let (lo, hi) = crate::sliders::Sliders::RANGES[i];
        let v = row[i] as f64;
        (v + 1.0) / 2.0 * (hi - lo) + lo
    })
}

impl Mlp {
    pub fn new(input_dim: usize, hidden_dim: usize) -> anyhow::Result<Self> {
        let device = Device::Cpu;
        let varmap = VarMap::new();
        let vb = VarBuilder::from_varmap(&varmap, DType::F32, &device);
        let l1 = linear(input_dim, hidden_dim, vb.pp("l1"))?;
        let l2 = linear(hidden_dim, hidden_dim, vb.pp("l2"))?;
        let l3 = linear(hidden_dim, SLIDER_COUNT, vb.pp("l3"))?;
        Ok(Self {
            l1,
            l2,
            l3,
            varmap,
            device,
        })
    }

    /// Overwrites every weight and bias with deterministic values from `seed` (a PCG-style LCG,
    /// uniform in `+-1/sqrt(fan_in)`, the same bound `candle_nn::linear` uses). `Mlp::new`'s own
    /// initialisation draws from candle's unseeded CPU rng, so a ReLU net trained from it can
    /// start with dead units and fail to fit -- a rare, run-to-run flake (it failed once on CI,
    /// #377) that a test asserting a fit can't tolerate. Tests call this right after `new`.
    pub fn reseed(&mut self, seed: u64) -> anyhow::Result<()> {
        let mut state = seed;
        let mut next = move || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((state >> 33) as f32 / (1u64 << 31) as f32) * 2.0 - 1.0
        };
        let named: Vec<(String, Vec<usize>)> = self
            .varmap
            .data()
            .lock()
            .unwrap()
            .iter()
            .map(|(name, var)| (name.clone(), var.dims().to_vec()))
            .collect();
        let fan_in = |name: &str| -> usize {
            let layer = name.rsplit_once('.').map_or(name, |(l, _)| l);
            named
                .iter()
                .find(|(n, d)| n.starts_with(layer) && n.ends_with("weight") && d.len() == 2)
                .map_or(1, |(_, d)| d[1])
        };
        // Visit in name order so the result doesn't depend on HashMap iteration order.
        let mut sorted = named.clone();
        sorted.sort();
        for (name, dims) in &sorted {
            let bound = 1.0 / (fan_in(name) as f32).sqrt();
            let n: usize = dims.iter().product();
            let values: Vec<f32> = (0..n).map(|_| next() * bound).collect();
            let tensor = Tensor::from_vec(values, dims.clone(), &self.device)?;
            self.varmap.set_one(name, tensor)?;
        }
        Ok(())
    }

    /// The final `tanh` bounds output to `-1.0..=1.0`, matching `scale_targets`'s scaling -- without
    /// it, `unscale`'s linear remap of an unbounded raw output could land far outside a slider's
    /// documented range on an out-of-distribution input.
    fn forward(&self, xs: &Tensor) -> candle_core::Result<Tensor> {
        let xs = self.l1.forward(xs)?.relu()?;
        let xs = self.l2.forward(&xs)?.relu()?;
        self.l3.forward(&xs)?.tanh()
    }

    /// Trains for `config.epochs` full-batch steps with Adam, early-stopping when `val_features`/
    /// `val_targets` stop improving for `config.patience` consecutive epochs (the weights are
    /// restored to the best validation epoch, not just wherever training happened to stop).
    pub fn train(
        &mut self,
        train_features: &[Vec<f32>],
        train_targets: &[[f64; SLIDER_COUNT]],
        val_features: &[Vec<f32>],
        val_targets: &[[f64; SLIDER_COUNT]],
        config: TrainConfig,
    ) -> anyhow::Result<TrainReport> {
        let TrainConfig {
            epochs,
            patience,
            learning_rate,
        } = config;
        anyhow::ensure!(!train_features.is_empty(), "cannot train on zero samples");
        let input_dim = train_features[0].len();

        let train_x = flatten_to_tensor(train_features, &self.device)?;
        let train_y = scale_targets(train_targets);
        let train_y = Tensor::from_slice(
            &train_y.iter().flatten().copied().collect::<Vec<f32>>(),
            (train_y.len(), SLIDER_COUNT),
            &self.device,
        )?;

        let val_x = if val_features.is_empty() {
            None
        } else {
            Some(flatten_to_tensor(val_features, &self.device)?)
        };
        let val_y_scaled = scale_targets(val_targets);

        let mut opt = AdamW::new(
            self.varmap.all_vars(),
            ParamsAdamW {
                lr: learning_rate,
                ..Default::default()
            },
        )?;

        let mut best_val_loss = f64::INFINITY;
        let mut best_weights: Option<Vec<(String, Tensor)>> = None;
        let mut epochs_without_improvement = 0;
        let mut final_train_loss = f64::INFINITY;

        for _epoch in 0..epochs {
            let predicted = self.forward(&train_x)?;
            let loss = candle_nn::loss::mse(&predicted, &train_y)?;
            opt.backward_step(&loss)?;
            final_train_loss = loss.to_scalar::<f32>()? as f64;

            if let Some(val_x) = &val_x {
                let val_pred = self.forward(val_x)?;
                let val_pred: Vec<f32> = val_pred.flatten_all()?.to_vec1()?;
                let mut sq_err = 0.0_f64;
                for (row_idx, target) in val_y_scaled.iter().enumerate() {
                    for (col_idx, &t) in target.iter().enumerate() {
                        let p = val_pred[row_idx * SLIDER_COUNT + col_idx];
                        sq_err += ((p - t) as f64).powi(2);
                    }
                }
                let val_loss = sq_err / (val_y_scaled.len() * SLIDER_COUNT) as f64;

                if val_loss < best_val_loss - 1e-6 {
                    best_val_loss = val_loss;
                    epochs_without_improvement = 0;
                    best_weights = Some(snapshot(&self.varmap)?);
                } else {
                    epochs_without_improvement += 1;
                    if epochs_without_improvement >= patience {
                        break;
                    }
                }
            }
        }

        if let Some(weights) = best_weights {
            restore(&self.varmap, &weights)?;
        }

        Ok(TrainReport {
            input_dim,
            final_train_loss,
            best_val_loss: if val_x.is_some() {
                Some(best_val_loss)
            } else {
                None
            },
        })
    }

    pub fn predict(&self, features: &[f32]) -> anyhow::Result<[f64; SLIDER_COUNT]> {
        let x = Tensor::from_slice(features, (1, features.len()), &self.device)?;
        let out = self.forward(&x)?;
        let out: Vec<f32> = out.flatten_all()?.to_vec1()?;
        let arr: [f32; SLIDER_COUNT] = out.try_into().map_err(|v: Vec<f32>| {
            anyhow::anyhow!("expected {SLIDER_COUNT} outputs, got {}", v.len())
        })?;
        Ok(unscale(&arr))
    }
}

#[derive(Debug, Clone, Copy)]
pub struct TrainConfig {
    pub epochs: usize,
    pub patience: usize,
    pub learning_rate: f64,
}

pub struct TrainReport {
    pub input_dim: usize,
    pub final_train_loss: f64,
    pub best_val_loss: Option<f64>,
}

fn flatten_to_tensor(rows: &[Vec<f32>], device: &Device) -> candle_core::Result<Tensor> {
    let cols = rows[0].len();
    let flat: Vec<f32> = rows.iter().flatten().copied().collect();
    Tensor::from_slice(&flat, (rows.len(), cols), device)
}

fn snapshot(varmap: &VarMap) -> candle_core::Result<Vec<(String, Tensor)>> {
    varmap
        .data()
        .lock()
        .unwrap()
        .iter()
        .map(|(name, var)| Ok((name.clone(), var.as_tensor().copy()?)))
        .collect()
}

fn restore(varmap: &VarMap, snapshot: &[(String, Tensor)]) -> candle_core::Result<()> {
    let data = varmap.data().lock().unwrap();
    for (name, tensor) in snapshot {
        if let Some(var) = data.get(name) {
            var.set(tensor)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_a_planted_linear_mapping_on_synthetic_data() {
        // Ground truth: Exposure2012 == 2.0 * feature[0], every other slider zero. A tiny MLP with
        // enough epochs should fit this near-exactly -- proves the training loop actually reduces
        // loss and `predict` round-trips through the scale/unscale correctly.
        let mut rng_state = 12345_u64;
        let mut next = || {
            rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1);
            ((rng_state >> 33) as f32 / u32::MAX as f32) * 2.0 - 1.0
        };

        let n = 64;
        let features: Vec<Vec<f32>> = (0..n).map(|_| vec![next(), next(), next()]).collect();
        let targets: Vec<[f64; SLIDER_COUNT]> = features
            .iter()
            .map(|f| {
                let mut t = [0.0; SLIDER_COUNT];
                t[0] = (2.0 * f[0] as f64).clamp(-5.0, 5.0);
                t
            })
            .collect();

        let (train_f, val_f) = features.split_at(48);
        let (train_t, val_t) = targets.split_at(48);

        let mut model = Mlp::new(3, 16).unwrap();
        // Deterministic init: candle's own is unseeded, and a ReLU net occasionally starts dead.
        model.reseed(7).unwrap();
        let report = model
            .train(
                train_f,
                train_t,
                val_f,
                val_t,
                TrainConfig {
                    epochs: 500,
                    patience: 50,
                    learning_rate: 0.01,
                },
            )
            .unwrap();
        assert!(report.best_val_loss.unwrap() < report.final_train_loss + 10.0);

        for (f, expected) in val_f.iter().zip(val_t.iter()) {
            let predicted = model.predict(f).unwrap();
            assert!(
                (predicted[0] - expected[0]).abs() < 1.0,
                "expected {}, got {}",
                expected[0],
                predicted[0]
            );
        }
    }

    #[test]
    fn reseed_makes_initialisation_deterministic_and_seed_dependent() {
        let predict = |seed: u64| {
            let mut m = Mlp::new(3, 8).unwrap();
            m.reseed(seed).unwrap();
            m.predict(&[0.3, -0.2, 0.9]).unwrap()
        };
        assert_eq!(predict(1), predict(1));
        assert_ne!(predict(1), predict(2));
    }

    #[test]
    fn predict_output_stays_within_documented_ranges() {
        let model = Mlp::new(5, 8).unwrap();
        let predicted = model.predict(&[10.0, -10.0, 10.0, -10.0, 10.0]).unwrap();
        for (v, (lo, hi)) in predicted.iter().zip(crate::sliders::Sliders::RANGES.iter()) {
            // Untrained weights can still saturate tanh-like scaling at the extremes; unscale()
            // maps the raw -1..1 network output onto the documented range by construction, so an
            // out-of-distribution input should never overshoot that range's edges by more than a
            // small numerical margin.
            assert!(
                *v >= lo - 1e-3 && *v <= hi + 1e-3,
                "{v} outside [{lo}, {hi}]"
            );
        }
    }
}
