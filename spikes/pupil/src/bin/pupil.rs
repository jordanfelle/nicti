//! CLI for #99's classic auto-tone spike. Three subcommands: `auto` (candidate A on one image),
//! `fit` (train candidate B and report holdout error), `eval` (compare a candidate against real
//! LRC ground truth from a `.lrcat`). See `docs/research/pupil-auto-tone.md` for the deferred
//! reference-machine capture workflow `fit`/`eval` are meant to be run against once it lands.

use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::{Parser, Subcommand};
use pupil::fit;
use pupil::histogram::Histogram;
use pupil::sliders::Sliders;
use pupil::{eval, heuristic, input, render, truth};

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Prints candidate A's (percentile heuristic) sliders for one `retina dump-linear` output.
    Auto {
        tiff: PathBuf,
        json: PathBuf,
        /// Sample every Nth pixel in each dimension when building the histogram.
        #[arg(long, default_value_t = 4)]
        stride: u32,
    },
    /// Fits candidate B (ridge regression) on a 50/50 train/holdout split and reports the
    /// holdout error, the number ADR-0099's decision rule is based on.
    Fit {
        /// `.lrcat` holding LRC's own Auto Settings ground truth.
        #[arg(long)]
        truth: PathBuf,
        /// Directory of `<stem>.tiff`/`<stem>.json` pairs (`retina dump-linear` output), where
        /// `<stem>` matches the source NEF's `AgLibraryFile.baseName`.
        #[arg(long)]
        inputs: PathBuf,
        #[arg(long, default_value = "nef")]
        extension: String,
        #[arg(long, default_value_t = 4)]
        stride: u32,
        #[arg(long, default_value_t = 1e-3)]
        lambda: f64,
    },
    /// Evaluates one candidate against real LRC ground truth. `a` needs no training; `b` fits on
    /// half the inputs and reports holdout error on the other half, same split as `fit`.
    Eval {
        #[arg(long)]
        truth: PathBuf,
        #[arg(long)]
        inputs: PathBuf,
        #[arg(long, default_value = "nef")]
        extension: String,
        #[arg(long, value_parser = ["a", "b"])]
        candidate: String,
        #[arg(long, default_value_t = 4)]
        stride: u32,
        #[arg(long, default_value_t = 1e-3)]
        lambda: f64,
    },
}

/// One `retina dump-linear` output pair plus its ground truth, keyed by file stem.
struct Sample {
    #[allow(dead_code)] // kept for future per-file diagnostics; not read today
    stem: String,
    histogram: Histogram,
    truth: Sliders,
}

fn collect_samples(
    inputs_dir: &Path,
    extension: &str,
    stride: u32,
    truth_conn: &rusqlite::Connection,
) -> anyhow::Result<Vec<Sample>> {
    let mut stems: Vec<String> = std::fs::read_dir(inputs_dir)
        .with_context(|| format!("reading {}", inputs_dir.display()))?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            if path.extension()?.to_str()? != "tiff" {
                return None;
            }
            path.file_stem()?.to_str().map(String::from)
        })
        .collect();
    stems.sort();

    let mut samples = Vec::new();
    for stem in stems {
        let tiff = inputs_dir.join(format!("{stem}.tiff"));
        let json = inputs_dir.join(format!("{stem}.json"));
        if !json.exists() {
            continue;
        }
        let Some(ground_truth) = truth::truth_for_file(truth_conn, &stem, extension)? else {
            continue;
        };
        let linear = input::load(&tiff, &json)?;
        let samples_luma = render::default_render_luminance(&linear, stride);
        let histogram = Histogram::from_samples(samples_luma);
        samples.push(Sample {
            stem,
            histogram,
            truth: ground_truth,
        });
    }
    Ok(samples)
}

/// Deterministic 50/50 split by sorted stem name -- even indices train, odd indices hold out.
fn split(samples: &[Sample]) -> (Vec<&Sample>, Vec<&Sample>) {
    samples.iter().enumerate().fold(
        (Vec::new(), Vec::new()),
        |(mut train, mut holdout), (i, s)| {
            if i % 2 == 0 {
                train.push(s);
            } else {
                holdout.push(s);
            }
            (train, holdout)
        },
    )
}

fn print_report(label: &str, report: &eval::EvalReport) {
    println!("{label}:");
    for (name, m) in report.per_slider {
        println!(
            "  {name:<16} MAE={:>7.3} p95={:>7.3} bias={:>7.3}",
            m.mae, m.p95_abs_error, m.bias
        );
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Auto { tiff, json, stride } => {
            let linear = input::load(&tiff, &json)?;
            let samples = render::default_render_luminance(&linear, stride);
            let hist = Histogram::from_samples(samples);
            let sliders = heuristic::estimate(&hist);
            println!("{}", serde_json::to_string_pretty(&sliders)?);
        }
        Command::Fit {
            truth: truth_path,
            inputs,
            extension,
            stride,
            lambda,
        } => {
            let conn = truth::open_readonly(&truth_path)?;
            let samples = collect_samples(&inputs, &extension, stride, &conn)?;
            anyhow::ensure!(
                samples.len() >= 4,
                "need at least 4 matched samples to fit+holdout"
            );
            let (train, holdout) = split(&samples);
            let train_pairs: Vec<_> = train
                .iter()
                .map(|s| (fit::features(&s.histogram), s.truth))
                .collect();
            let model = fit::fit(&train_pairs, lambda)?;
            let predicted: Vec<_> = holdout
                .iter()
                .map(|s| model.predict(&fit::features(&s.histogram)))
                .collect();
            let truths: Vec<_> = holdout.iter().map(|s| s.truth).collect();
            let report = eval::evaluate(&predicted, &truths)?;
            println!("train={} holdout={}", train.len(), holdout.len());
            print_report("candidate B (fit), holdout", &report);
        }
        Command::Eval {
            truth: truth_path,
            inputs,
            extension,
            candidate,
            stride,
            lambda,
        } => {
            let conn = truth::open_readonly(&truth_path)?;
            let samples = collect_samples(&inputs, &extension, stride, &conn)?;
            anyhow::ensure!(!samples.is_empty(), "no matched input/truth pairs found");
            match candidate.as_str() {
                "a" => {
                    let predicted: Vec<_> = samples
                        .iter()
                        .map(|s| heuristic::estimate(&s.histogram))
                        .collect();
                    let truths: Vec<_> = samples.iter().map(|s| s.truth).collect();
                    let report = eval::evaluate(&predicted, &truths)?;
                    print_report("candidate A (heuristic), all samples", &report);
                }
                "b" => {
                    anyhow::ensure!(
                        samples.len() >= 4,
                        "need at least 4 matched samples to fit+holdout"
                    );
                    let (train, holdout) = split(&samples);
                    let train_pairs: Vec<_> = train
                        .iter()
                        .map(|s| (fit::features(&s.histogram), s.truth))
                        .collect();
                    let model = fit::fit(&train_pairs, lambda)?;
                    let predicted: Vec<_> = holdout
                        .iter()
                        .map(|s| model.predict(&fit::features(&s.histogram)))
                        .collect();
                    let truths: Vec<_> = holdout.iter().map(|s| s.truth).collect();
                    let report = eval::evaluate(&predicted, &truths)?;
                    print_report("candidate B (fit), holdout", &report);
                }
                _ => unreachable!("clap value_parser restricts this to a|b"),
            }
        }
    }
    Ok(())
}
