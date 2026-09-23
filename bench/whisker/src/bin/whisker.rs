//! CLI for the #43 hero-scenario frame analyzer. See `bench/whisker/README.md`.

use clap::{Parser, Subcommand};
use serde::Serialize;
use std::path::PathBuf;
use whisker::io::read_frames_gray8;
use whisker::stats::{summarize, Stats};
use whisker::{
    distinct_change_frames, first_change_frame, frame_intervals, frames_to_ms, rising_edges, settled_frame,
};

#[derive(Parser)]
#[command(name = "whisker", about = "Frame-diff analyzer for the #43 hero-scenario benchmark")]
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

        #[arg(long)]
        out: PathBuf,
    },

    /// Measures frame-interval fps during a scripted drag (interaction B's crop drag, or
    /// interaction C's pan drag) over an explicit frame window.
    Drag {
        #[arg(long)]
        roi_raw: PathBuf,
        #[arg(long)]
        roi_width: usize,
        #[arg(long)]
        roi_height: usize,

        #[arg(long)]
        fps: f64,

        /// First frame of the drag window (inclusive).
        #[arg(long)]
        start_frame: usize,
        /// Last frame of the drag window (exclusive).
        #[arg(long)]
        end_frame: usize,

        #[arg(long, default_value_t = 0.02)]
        change_threshold: f64,

        #[arg(long)]
        out: PathBuf,
    },
}

#[derive(Serialize)]
struct SwitchEventResult {
    indicator_frame: usize,
    first_change_ms: Option<f64>,
    settled_ms: Option<f64>,
}

#[derive(Serialize)]
struct SwitchReport {
    fps: f64,
    events_detected: usize,
    events: Vec<SwitchEventResult>,
    first_change_stats: Option<Stats>,
    settled_stats: Option<Stats>,
}

#[derive(Serialize)]
struct DragReport {
    fps: f64,
    distinct_frames: Vec<usize>,
    interval_frames: Vec<usize>,
    interval_ms_stats: Option<Stats>,
    effective_fps_p95: Option<f64>,
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
            out,
        } => {
            let indicator = read_frames_gray8(&indicator_raw, indicator_width, indicator_height)?;
            let roi = read_frames_gray8(&roi_raw, roi_width, roi_height)?;
            if indicator.frames.len() != roi.frames.len() {
                anyhow::bail!(
                    "indicator ({} frames) and roi ({} frames) captures have different frame counts \
                     — they must come from the same capture window",
                    indicator.frames.len(),
                    roi.frames.len()
                );
            }

            let brightness = indicator.mean_brightness();
            let edges = rising_edges(&brightness, indicator_threshold);
            let roi_diffs = roi.diff_series();

            let mut events = Vec::new();
            let mut first_change_samples = Vec::new();
            let mut settled_samples = Vec::new();
            for &edge in &edges {
                let first_change = first_change_frame(&roi_diffs, edge, change_threshold);
                // Search for the settled point starting from the first visible change, not the
                // raw indicator edge -- otherwise a brief pre-change plateau (the ROI hasn't
                // started transitioning yet) can itself look like a "quiet run" and produce a
                // bogus near-zero settled latency before the image has actually switched.
                let settled = settled_frame(&roi_diffs, first_change.unwrap_or(edge), quiet_threshold, min_quiet_frames);
                let first_change_ms = first_change.map(|f| frames_to_ms(f - edge, fps));
                let settled_ms = settled.map(|f| frames_to_ms(f - edge, fps));
                if let Some(ms) = first_change_ms {
                    first_change_samples.push(ms);
                }
                if let Some(ms) = settled_ms {
                    settled_samples.push(ms);
                }
                events.push(SwitchEventResult { indicator_frame: edge, first_change_ms, settled_ms });
            }

            let report = SwitchReport {
                fps,
                events_detected: edges.len(),
                first_change_stats: summarize(&first_change_samples),
                settled_stats: summarize(&settled_samples),
                events,
            };
            std::fs::write(&out, serde_json::to_string_pretty(&report)?)?;
            println!(
                "{} events detected; settled p95 = {:?} ms",
                report.events_detected,
                report.settled_stats.as_ref().map(|s| s.p95)
            );
        }

        Command::Drag { roi_raw, roi_width, roi_height, fps, start_frame, end_frame, change_threshold, out } => {
            let roi = read_frames_gray8(&roi_raw, roi_width, roi_height)?;
            let diffs = roi.diff_series();
            let distinct = distinct_change_frames(&diffs, start_frame, end_frame, change_threshold);
            let intervals = frame_intervals(&distinct);
            let interval_ms: Vec<f64> = intervals.iter().map(|&f| frames_to_ms(f, fps)).collect();
            let interval_ms_stats = summarize(&interval_ms);
            let effective_fps_p95 = interval_ms_stats.as_ref().map(|s| 1000.0 / s.p95);

            let report = DragReport {
                fps,
                distinct_frames: distinct,
                interval_frames: intervals,
                interval_ms_stats,
                effective_fps_p95,
            };
            std::fs::write(&out, serde_json::to_string_pretty(&report)?)?;
            println!("effective fps (p95 interval) = {:?}", report.effective_fps_p95);
        }
    }
    Ok(())
}
