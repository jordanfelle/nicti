//! CLI for #53's AI auto-tone spike. `dataset` (catalog -> sample manifest), `extract` (manifest ->
//! feature cache), `train` (one model/split combo), `report` (every model x split, the table
//! ADR-0053's Measured results section is built from). See `docs/research/purr-ai-auto-tone.md`.

use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand, ValueEnum};
use purr::dataset::{self, FeatureRow};
use purr::sliders::{Sliders, SLIDER_COUNT};
use purr::{baseline, catalog, eval, fit, mlp, sample, split};

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

fn run_b0(train: &[FeatureRow], holdout: &[FeatureRow]) -> anyhow::Result<eval::EvalReport> {
    let model = baseline::MeanModel::fit(&targets_to_sliders(train))?;
    let predicted: Vec<_> = holdout.iter().map(|_| model.predict()).collect();
    eval::evaluate(&predicted, &targets_to_sliders(holdout))
}

fn run_b1(train: &[FeatureRow], holdout: &[FeatureRow]) -> anyhow::Result<eval::EvalReport> {
    let train_pairs: Vec<_> = train
        .iter()
        .map(|r| (r.hist, Sliders::from_array(r.targets)))
        .collect();
    let model = fit::fit(&train_pairs, 1e-3)?;
    let predicted: Vec<_> = holdout.iter().map(|r| model.predict(&r.hist)).collect();
    eval::evaluate(&predicted, &targets_to_sliders(holdout))
}

fn run_mlp(
    train: &[FeatureRow],
    holdout: &[FeatureRow],
    input_fn: impl Fn(&FeatureRow) -> Vec<f32>,
    hidden_dim: usize,
    config: mlp::TrainConfig,
) -> anyhow::Result<eval::EvalReport> {
    let train_features: Vec<Vec<f32>> = train.iter().map(&input_fn).collect();
    let train_targets: Vec<[f64; SLIDER_COUNT]> = train.iter().map(|r| r.targets).collect();
    // A validation slice carved from the training rows only -- the holdout set stays untouched
    // until final evaluation, matching ADR-0053's decision rule (holdout error is the reported
    // number, early-stopping must not see it).
    let val_at = (train_features.len() as f64 * 0.85) as usize;
    let (fit_features, val_features) =
        train_features.split_at(val_at.max(1).min(train_features.len()));
    let (fit_targets, val_targets) = train_targets.split_at(val_at.max(1).min(train_targets.len()));

    let input_dim = train_features[0].len();
    let mut model = mlp::Mlp::new(input_dim, hidden_dim)?;
    model.train(fit_features, fit_targets, val_features, val_targets, config)?;

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
            println!("train={} holdout={}", train.len(), holdout.len());
            let config = mlp::TrainConfig {
                epochs: 300,
                patience: 30,
                learning_rate,
            };
            let report = match model {
                ModelKind::B0 => run_b0(&train, &holdout)?,
                ModelKind::B1 => run_b1(&train, &holdout)?,
                ModelKind::M1 => run_mlp(&train, &holdout, hist_input, 32, config)?,
                ModelKind::M2 => run_mlp(&train, &holdout, thumb_input, 64, config)?,
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
                let split_name = match split_kind {
                    SplitKind::Event => "event",
                    SplitKind::Temporal => "temporal",
                };
                println!(
                    "\n=== split={split_name} train={} holdout={} ===",
                    train.len(),
                    holdout.len()
                );
                print_report("B0 (mean)", &run_b0(&train, &holdout)?);
                print_report("B1 (ridge)", &run_b1(&train, &holdout)?);
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
                    &run_mlp(&train, &holdout, hist_input, 32, m1_config)?,
                );
                print_report(
                    "M2 (MLP, histogram+thumbnail)",
                    &run_mlp(&train, &holdout, thumb_input, 64, m2_config)?,
                );
            }
        }
    }
    Ok(())
}
