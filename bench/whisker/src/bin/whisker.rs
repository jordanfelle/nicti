//! CLI for the #43 hero-scenario frame analyzer. See `bench/whisker/README.md`.

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use whisker::analyze::{pool_results, summarize_buckets, AnalyzeOptions};
use whisker::io::read_frames_gray8;
use whisker::{analyze_drag, analyze_switch, drag_window_from_edges};

#[derive(Parser)]
#[command(
    name = "whisker",
    about = "Frame-diff analyzer for the #43 hero-scenario benchmark"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Measures first-change and settled latency for a series of indicator-marked events
    /// (interaction A's switches, or a single zoom-settled event for interaction C).
    Switch {
        /// Raw gray8 capture of the keypress-indicator crop.
        #[arg(long)]
        indicator_raw: PathBuf,
        #[arg(long)]
        indicator_width: usize,
        #[arg(long)]
        indicator_height: usize,

        /// Raw gray8 capture of the loupe ROI crop, same frame count/timing as `indicator_raw`.
        #[arg(long)]
        roi_raw: PathBuf,
        #[arg(long)]
        roi_width: usize,
        #[arg(long)]
        roi_height: usize,

        #[arg(long)]
        fps: f64,

        /// Mean-brightness threshold (0.0-1.0) above which the indicator counts as "on".
        #[arg(long, default_value_t = 0.5)]
        indicator_threshold: f64,

        /// Per-transition diff threshold (0.0-1.0) below which the ROI counts as unchanged.
        #[arg(long, default_value_t = 0.02)]
        quiet_threshold: f64,

        /// Consecutive quiet frames required to call the ROI settled.
        #[arg(long, default_value_t = 3)]
        min_quiet_frames: usize,

        /// Per-transition diff threshold (0.0-1.0) above which the ROI counts as changed.
        #[arg(long, default_value_t = 0.02)]
        change_threshold: f64,

        /// Restrict analysis to these 0-based indicator-edge indices (comma-separated), e.g.
        /// `--edges 0` for a zoom capture whose 3 flashes are [Z keypress, pan-drag-start,
        /// pan-drag-end] and only the first is a switch-shaped event. Defaults to every detected
        /// edge.
        #[arg(long, value_delimiter = ',')]
        edges: Option<Vec<usize>>,

        #[arg(long)]
        out: PathBuf,
    },

    /// Measures frame-interval fps during a scripted drag (interaction B's crop drag, or
    /// interaction C's pan drag), over either an explicit frame window or a window derived from
    /// a pair of indicator edges (see `--indicator-raw`/`--window-edges`).
    Drag {
        #[arg(long)]
        roi_raw: PathBuf,
        #[arg(long)]
        roi_width: usize,
        #[arg(long)]
        roi_height: usize,

        #[arg(long)]
        fps: f64,

        /// Start of the drag window, as an index into the frame-to-frame transition series (not
        /// a raw frame number): transition `i` covers frames `i` -> `i+1`, so the earliest change
        /// this can report is frame `start_frame + 1`. Mutually exclusive with
        /// `--indicator-raw`/`--window-edges`.
        #[arg(long, requires = "end_frame", conflicts_with_all = ["indicator_raw", "window_edges"])]
        start_frame: Option<usize>,
        /// End of the drag window (exclusive), same transition-index space as `start_frame`.
        #[arg(long, requires = "start_frame")]
        end_frame: Option<usize>,

        /// Raw gray8 capture of the keypress-indicator crop, used with `--window-edges` to derive
        /// the drag window from a pair of indicator flashes instead of hand-picked frame numbers
        /// — see `hero.ahk`'s `ScriptedDrag`, which flashes at the drag's start and end.
        #[arg(long, requires = "window_edges")]
        indicator_raw: Option<PathBuf>,
        #[arg(long, requires = "indicator_raw")]
        indicator_width: Option<usize>,
        #[arg(long, requires = "indicator_raw")]
        indicator_height: Option<usize>,
        #[arg(long, default_value_t = 0.5)]
        indicator_threshold: f64,
        /// Two comma-separated 0-based indicator-edge indices, `start,end` — e.g. `1,2` for a
        /// crop/zoom capture where edge 0 is the mode-entry keypress (`r`/`z`).
        #[arg(long, value_delimiter = ',', num_args = 2, requires = "indicator_raw")]
        window_edges: Option<Vec<usize>>,

        #[arg(long, default_value_t = 0.02)]
        change_threshold: f64,

        #[arg(long)]
        out: PathBuf,
    },

    /// Walks a `run-hero-series.ps1` results tree, analyzes every non-warm-up capture, and pools
    /// samples per (config, interaction) into the p50/p95/max stats hero-scenario.md's Results
    /// section wants — instead of hand-annotating and eyeballing dozens of individual captures.
    Analyze {
        #[arg(long)]
        results_root: PathBuf,

        #[arg(long, default_value_t = 0.5)]
        indicator_threshold: f64,
        #[arg(long, default_value_t = 0.02)]
        quiet_threshold: f64,
        #[arg(long, default_value_t = 3)]
        min_quiet_frames: usize,
        #[arg(long, default_value_t = 0.02)]
        change_threshold: f64,
        #[arg(long, default_value = "warmup")]
        warmup_label: String,

        /// Output directory for `summary.json`/`summary.md`. Defaults to `results_root` itself.
        #[arg(long)]
        out_dir: Option<PathBuf>,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Switch {
            indicator_raw,
            indicator_width,
            indicator_height,
            roi_raw,
            roi_width,
            roi_height,
            fps,
            indicator_threshold,
            quiet_threshold,
            min_quiet_frames,
            change_threshold,
            edges,
            out,
        } => {
            let indicator = read_frames_gray8(&indicator_raw, indicator_width, indicator_height)?;
            let roi = read_frames_gray8(&roi_raw, roi_width, roi_height)?;
            let report = analyze_switch(
                &indicator,
                &roi,
                fps,
                indicator_threshold,
                quiet_threshold,
                min_quiet_frames,
                change_threshold,
                edges.as_deref(),
            )
            .map_err(|e| anyhow::anyhow!(e))?;
            std::fs::write(&out, serde_json::to_string_pretty(&report)?)?;
            println!(
                "{} events detected ({} analyzed); settled p95 = {:?} ms",
                report.events_detected,
                report.events_analyzed,
                report.settled_stats.as_ref().map(|s| s.p95)
            );
        }

        Command::Drag {
            roi_raw,
            roi_width,
            roi_height,
            fps,
            start_frame,
            end_frame,
            indicator_raw,
            indicator_width,
            indicator_height,
            indicator_threshold,
            window_edges,
            change_threshold,
            out,
        } => {
            let roi = read_frames_gray8(&roi_raw, roi_width, roi_height)?;
            let (start, end) = match (start_frame, end_frame) {
                (Some(s), Some(e)) => (s, e),
                _ => {
                    let indicator_raw = indicator_raw
                        .ok_or_else(|| anyhow::anyhow!("either --start-frame/--end-frame or --indicator-raw/--window-edges is required"))?;
                    let indicator = read_frames_gray8(
                        &indicator_raw,
                        indicator_width.expect("clap requires indicator_width with indicator_raw"),
                        indicator_height
                            .expect("clap requires indicator_height with indicator_raw"),
                    )?;
                    let edges =
                        window_edges.expect("clap requires window_edges with indicator_raw");
                    drag_window_from_edges(&indicator, indicator_threshold, (edges[0], edges[1]))
                        .map_err(|e| anyhow::anyhow!(e))?
                }
            };

            let report = analyze_drag(&roi, fps, start, end, change_threshold);
            std::fs::write(&out, serde_json::to_string_pretty(&report)?)?;
            println!(
                "effective fps (p95 interval) = {:?}",
                report.effective_fps_p95
            );
        }

        Command::Analyze {
            results_root,
            indicator_threshold,
            quiet_threshold,
            min_quiet_frames,
            change_threshold,
            warmup_label,
            out_dir,
        } => {
            let opts = AnalyzeOptions {
                indicator_threshold,
                quiet_threshold,
                min_quiet_frames,
                change_threshold,
                warmup_label,
            };
            let (buckets, skipped) = pool_results(&results_root, &opts)?;
            if !skipped.is_empty() {
                eprintln!("WARNING: {} capture(s) skipped:", skipped.len());
                for s in &skipped {
                    eprintln!("  {} — {}", s.dir.display(), s.reason);
                }
            }
            let summary = summarize_buckets(&buckets);

            let out_dir = out_dir.unwrap_or_else(|| results_root.clone());
            std::fs::create_dir_all(&out_dir)?;

            #[derive(serde::Serialize)]
            struct AnalyzeOutput<'a> {
                results_root: &'a std::path::Path,
                skipped: &'a [whisker::analyze::SkippedCapture],
                configs: &'a std::collections::BTreeMap<
                    String,
                    std::collections::BTreeMap<String, whisker::analyze::MetricStats>,
                >,
            }
            let output = AnalyzeOutput {
                results_root: &results_root,
                skipped: &skipped,
                configs: &summary,
            };
            std::fs::write(
                out_dir.join("summary.json"),
                serde_json::to_string_pretty(&output)?,
            )?;

            let mut md = String::new();
            md.push_str("# Hero-scenario pooled results\n\n");
            md.push_str(&format!("Source: `{}`\n\n", results_root.display()));
            if !skipped.is_empty() {
                md.push_str(&format!(
                    "**{} capture(s) skipped — see summary.json.**\n\n",
                    skipped.len()
                ));
            }
            md.push_str("| Config | Interaction | Metric | n | p50 | p95 | max |\n");
            md.push_str("|---|---|---|---|---|---|---|\n");
            for (config, interactions) in &summary {
                for (interaction, stats) in interactions {
                    let mut row = |metric: &str, s: &Option<whisker::stats::Stats>| {
                        if let Some(s) = s {
                            md.push_str(&format!(
                                "| {config} | {interaction} | {metric} | {} | {:.2} | {:.2} | {:.2} |\n",
                                s.n, s.p50, s.p95, s.max
                            ));
                        }
                    };
                    row("settled_ms", &stats.settled_ms);
                    row("first_change_ms", &stats.first_change_ms);
                    row("interval_ms", &stats.interval_ms);
                    if let Some(fps) = stats.effective_fps_p95 {
                        md.push_str(&format!(
                            "| {config} | {interaction} | effective_fps_p95 | — | — | {fps:.1} | — |\n"
                        ));
                    }
                }
            }
            std::fs::write(out_dir.join("summary.md"), &md)?;

            println!(
                "Analyzed {} config/interaction bucket(s), {} capture(s) skipped. Wrote {}/summary.{{json,md}}",
                summary.values().map(|m| m.len()).sum::<usize>(),
                skipped.len(),
                out_dir.display()
            );
        }
    }
    Ok(())
}
