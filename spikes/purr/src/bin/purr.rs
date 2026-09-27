//! CLI for #53's AI auto-tone spike. `dataset` (catalog -> sample manifest), `extract` (manifest ->
//! feature cache), `train` (one model/split combo), `report` (every model x split, the table
//! ADR-0053's Measured results section is built from). See `docs/research/purr-ai-auto-tone.md`.

use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand, ValueEnum};
use purr::dataset::{self, FeatureRow};
use purr::sliders::{Sliders, SLIDER_COUNT};
use purr::{baseline, catalog, eval, fit, mlp, sample, split};
use rand::seq::SliceRandom;
use rand::SeedableRng;

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Queries a `.lrcat` for keeper rows (picked/rated NEFs with a real non-default edit),
    /// samples down to a per-folder-capped working set, and writes a manifest.
    Dataset {
        #[arg(long)]
        lrcat: PathBuf,
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value_t = 300)]
        per_folder_cap: usize,
        #[arg(long, default_value_t = 5000)]
        target_total: usize,
    },
    /// Extracts embedded-JPEG histogram + thumbnail features for every manifest row.
    Extract {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    /// Trains and evaluates one model on one split.
    Train {
        #[arg(long)]
        features: PathBuf,
        #[arg(long, value_enum)]
        model: ModelKind,
        #[arg(long, value_enum, default_value_t = SplitKind::Event)]
        split: SplitKind,
        #[arg(long, default_value_t = 0.2)]
        holdout_fraction: f64,
        #[arg(long, default_value_t = 0.005)]
        learning_rate: f64,
    },
    /// Runs every model x split combination and prints a summary table.
    Report {
        #[arg(long)]
        features: PathBuf,
        #[arg(long, default_value_t = 0.2)]
        holdout_fraction: f64,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum ModelKind {
    B0,
    B1,
    M1,
    M2,
}

#[derive(Clone, Copy, ValueEnum)]
enum SplitKind {
    Event,
    Temporal,
}

fn split_rows(
    rows: &[FeatureRow],
    kind: SplitKind,
    holdout_fraction: f64,
) -> (Vec<FeatureRow>, Vec<FeatureRow>) {
    match kind {
        SplitKind::Event => split::by_folder(rows, |r| r.folder_id, holdout_fraction),
        SplitKind::Temporal => split::temporal(rows, |r| r.capture_time.clone(), holdout_fraction),
    }
}

fn hist_input(row: &FeatureRow) -> Vec<f32> {
    row.hist.to_vec()
}

fn thumb_input(row: &FeatureRow) -> Vec<f32> {
    let mut v = row.hist.to_vec();
    v.extend_from_slice(&row.thumb);
    v
}

fn targets_to_sliders(rows: &[FeatureRow]) -> Vec<Sliders> {
    rows.iter()
        .map(|r| Sliders::from_array(r.targets))
        .collect()
}

/// Deterministically shuffles `train` (fixed seed, so a re-run reproduces the same split) and
/// carves off a 15% validation slice for M1/M2's early stopping. **Every model (B0/B1/M1/M2) fits
/// on the same `fit` rows** -- an earlier version of this pipeline gave B0/B1 the full `train` set
/// while M1/M2 only saw the 85% `fit` slice, an undisclosed ~15%-more-data advantage for the
/// baselines that confounded the ADR's "M1 comes within 5% of B1" comparison (caught in adversarial
/// review). The shuffle itself also matters: `train` arrives folder/id_local-ordered (`catalog`'s
/// `ORDER BY i.id_local`, preserved through `sample`/`split::by_folder`'s stable partition), so a
/// positional tail-cut without shuffling would concentrate `val` in whichever folder(s) happen to
/// sort last rather than drawing a representative sample of the training distribution.
fn fit_val_split(train: &[FeatureRow]) -> (Vec<FeatureRow>, Vec<FeatureRow>) {
    let mut shuffled = train.to_vec();
    let mut rng = rand::rngs::StdRng::seed_from_u64(53);
    shuffled.shuffle(&mut rng);
    let val_at = ((shuffled.len() as f64 * 0.85) as usize)
        .max(1)
        .min(shuffled.len());
    let val = shuffled.split_off(val_at);
    (shuffled, val)
}

fn run_b0(fit_rows: &[FeatureRow], holdout: &[FeatureRow]) -> anyhow::Result<eval::EvalReport> {
    let model = baseline::MeanModel::fit(&targets_to_sliders(fit_rows))?;
    let predicted: Vec<_> = holdout.iter().map(|_| model.predict()).collect();
    eval::evaluate(&predicted, &targets_to_sliders(holdout))
}

fn run_b1(fit_rows: &[FeatureRow], holdout: &[FeatureRow]) -> anyhow::Result<eval::EvalReport> {
    let train_pairs: Vec<_> = fit_rows
        .iter()
        .map(|r| (r.hist, Sliders::from_array(r.targets)))
        .collect();
    let model = fit::fit(&train_pairs, 1e-3)?;
    let predicted: Vec<_> = holdout.iter().map(|r| model.predict(&r.hist)).collect();
    eval::evaluate(&predicted, &targets_to_sliders(holdout))
}

fn run_mlp(
    fit_rows: &[FeatureRow],
    val_rows: &[FeatureRow],
    holdout: &[FeatureRow],
    input_fn: impl Fn(&FeatureRow) -> Vec<f32>,
    hidden_dim: usize,
    config: mlp::TrainConfig,
) -> anyhow::Result<eval::EvalReport> {
    let fit_features: Vec<Vec<f32>> = fit_rows.iter().map(&input_fn).collect();
    let fit_targets: Vec<[f64; SLIDER_COUNT]> = fit_rows.iter().map(|r| r.targets).collect();
    let val_features: Vec<Vec<f32>> = val_rows.iter().map(&input_fn).collect();
    let val_targets: Vec<[f64; SLIDER_COUNT]> = val_rows.iter().map(|r| r.targets).collect();

    let input_dim = fit_features
        .first()
        .map(Vec::len)
        .ok_or_else(|| anyhow::anyhow!("cannot train an MLP on zero fit rows"))?;
    let mut model = mlp::Mlp::new(input_dim, hidden_dim)?;
    model.train(
        &fit_features,
        &fit_targets,
        &val_features,
        &val_targets,
        config,
    )?;

    let predicted: Vec<_> = holdout
        .iter()
        .map(|r| model.predict(&input_fn(r)).map(Sliders::from_array))
        .collect::<anyhow::Result<Vec<_>>>()?;
    eval::evaluate(&predicted, &targets_to_sliders(holdout))
}

fn print_report(label: &str, report: &eval::EvalReport) {
    println!(
        "{label} (mean normalized MAE = {:.4}):",
        report.mean_normalized_mae()
    );
    for (name, m) in report.per_slider {
        println!(
            "  {name:<16} MAE={:>8.3} p95={:>8.3} bias={:>8.3}",
            m.mae, m.p95_abs_error, m.bias
        );
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Dataset {
            lrcat,
            out,
            per_folder_cap,
            target_total,
        } => {
            let conn = catalog::open_readonly(&lrcat)?;
            let rows = catalog::keepers(&conn)?;
            println!("found {} keeper rows before sampling", rows.len());
            let sampled = sample::select(&rows, per_folder_cap, target_total);
            println!(
                "sampled {} rows (cap={per_folder_cap}, target={target_total})",
                sampled.len()
            );
            let manifest: Vec<_> = sampled
                .into_iter()
                .map(dataset::ManifestRow::from)
                .collect();
            dataset::write_manifest(&out, &manifest)
                .with_context(|| format!("writing manifest to {}", out.display()))?;
        }
        Command::Extract { manifest, out } => {
            let rows = dataset::read_manifest(&manifest)?;
            println!("extracting features for {} manifest rows", rows.len());
            let (features, report) = dataset::extract_all(&rows);
            println!(
                "attempted={} succeeded={} unreachable={} failed={}",
                report.attempted, report.succeeded, report.unreachable, report.failed
            );
            if !report.failure_kinds.is_empty() {
                let mut kinds: Vec<_> = report.failure_kinds.iter().collect();
                kinds.sort_by_key(|(name, _)| *name);
                for (kind, count) in kinds {
                    println!("  failed[{kind}]={count}");
                }
            }
            dataset::write_feature_cache(&out, &features)
                .with_context(|| format!("writing feature cache to {}", out.display()))?;
        }
        Command::Train {
            features,
            model,
            split,
            holdout_fraction,
            learning_rate,
        } => {
            let rows = dataset::read_feature_cache(&features)?;
            let (train, holdout) = split_rows(&rows, split, holdout_fraction);
            let (fit_rows, val_rows) = fit_val_split(&train);
            println!(
                "train={} (fit={} val={}) holdout={}",
                train.len(),
                fit_rows.len(),
                val_rows.len(),
                holdout.len()
            );
            let config = mlp::TrainConfig {
                epochs: 300,
                patience: 30,
                learning_rate,
            };
            let report = match model {
                ModelKind::B0 => run_b0(&fit_rows, &holdout)?,
                ModelKind::B1 => run_b1(&fit_rows, &holdout)?,
                ModelKind::M1 => run_mlp(&fit_rows, &val_rows, &holdout, hist_input, 32, config)?,
                ModelKind::M2 => run_mlp(&fit_rows, &val_rows, &holdout, thumb_input, 64, config)?,
            };
            print_report("result", &report);
        }
        Command::Report {
            features,
            holdout_fraction,
        } => {
            let rows = dataset::read_feature_cache(&features)?;
            for split_kind in [SplitKind::Event, SplitKind::Temporal] {
                let (train, holdout) = split_rows(&rows, split_kind, holdout_fraction);
                let (fit_rows, val_rows) = fit_val_split(&train);
                let split_name = match split_kind {
                    SplitKind::Event => "event",
                    SplitKind::Temporal => "temporal",
                };
                println!(
                    "\n=== split={split_name} train={} (fit={} val={}) holdout={} ===",
                    train.len(),
                    fit_rows.len(),
                    val_rows.len(),
                    holdout.len()
                );
                // Every model fits on the same `fit_rows` -- B0/B1 don't need a validation slice
                // for early stopping, but training them on `train` in full while M1/M2 only see
                // the 85% `fit` slice would give the baselines an undisclosed ~15%-more-data edge,
                // confounding the head-to-head comparison (see `fit_val_split`'s own doc comment).
                print_report("B0 (mean)", &run_b0(&fit_rows, &holdout)?);
                print_report("B1 (ridge)", &run_b1(&fit_rows, &holdout)?);
                let m1_config = mlp::TrainConfig {
                    epochs: 300,
                    patience: 30,
                    learning_rate: 0.005,
                };
                // M2's input is 236x wider than M1's (3085 histogram+thumbnail floats vs. 13) --
                // the same learning rate that trains M1 cleanly makes M2's first layer's gradient
                // norm large enough to collapse every output to a saturated tanh extreme within a
                // handful of epochs (confirmed: 0.005 diverges, 0.0005 does not -- see ADR-0053's
                // Measured results for the actual before/after numbers).
                let m2_config = mlp::TrainConfig {
                    epochs: 300,
                    patience: 30,
                    learning_rate: 0.0005,
                };
                print_report(
                    "M1 (MLP, histogram)",
                    &run_mlp(&fit_rows, &val_rows, &holdout, hist_input, 32, m1_config)?,
                );
                print_report(
                    "M2 (MLP, histogram+thumbnail)",
                    &run_mlp(&fit_rows, &val_rows, &holdout, thumb_input, 64, m2_config)?,
                );
            }
        }
    }
    Ok(())
}
