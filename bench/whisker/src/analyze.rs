//! Multi-run pooling for the #43 hero-scenario results tree that `run-hero-series.ps1` produces.
//!
//! `docs/benchmarks/hero-scenario.md` judges p95 over samples pooled across all 5 measured runs
//! (and, for crop/zoom, across the 5 spread target images too) — not per-capture numbers eyeballed
//! one at a time. This module walks a results root, reads each capture's `meta.json`, runs the
//! same switch/drag pipeline `bin/whisker.rs`'s standalone subcommands use, and pools the raw
//! millisecond/interval samples per (config, interaction) before handing them to
//! [`crate::stats::summarize`] — pooling raw samples, not averaging each capture's own p95,
//! matches the spec's "pool all ... across those 5 images" wording.

use crate::io::{read_events_csv, read_frames_gray8, MixedEvent};
use crate::stats::{summarize, Stats};
use crate::{
    analyze_switch, distinct_change_frames, drag_window_from_edges, event_latencies_bounded,
    frame_intervals, frames_to_ms, rising_edges,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io as stdio;
use std::path::{Path, PathBuf};

/// The subset of `run-hero.ps1`'s `meta.json` fields `analyze` actually needs. Unknown fields
/// (hardware identity, capture-rate deviation notes, etc.) are ignored by serde's default
/// behavior, not an error.
#[derive(Debug, Clone, Deserialize)]
pub struct CaptureMeta {
    pub interaction: String,
    pub config: String,
    pub run_label: String,
    pub capture_fps: f64,
    pub indicator_w: usize,
    pub indicator_h: usize,
    pub roi_w: usize,
    pub roi_h: usize,
}

/// Recursively finds every directory under `root` that directly contains a `meta.json` — one
/// `run-hero.ps1` invocation's output.
pub fn find_capture_dirs(root: &Path) -> stdio::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if !root.is_dir() {
        return Ok(out);
    }
    let mut has_meta = false;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            out.extend(find_capture_dirs(&path)?);
        } else if path.file_name().is_some_and(|n| n == "meta.json") {
            has_meta = true;
        }
    }
    if has_meta {
        out.push(root.to_path_buf());
    }
    Ok(out)
}

/// Raw pooled samples for one (config, interaction) bucket, before summarizing. Kept as raw
/// values (not per-capture `Stats`) so pooling happens before percentiles are computed.
#[derive(Debug, Default)]
pub struct MetricSamples {
    pub settled_ms: Vec<f64>,
    pub first_change_ms: Vec<f64>,
    pub interval_ms: Vec<f64>,
    /// Count of interaction-D events whose settled latency was never found before the next
    /// event's own flash (or the capture's end) cut off the search — see
    /// `event_latencies_bounded`'s doc comment. Always 0 for switch/crop/zoom, which never bound
    /// the settle search. A nonzero count here is itself the finding #100 is measuring for, not
    /// noise to filter out — never dropped silently.
    pub unsettled: usize,
}

/// A capture directory that couldn't be parsed or analyzed — surfaced to the caller rather than
/// silently dropped, per this repo's "missing data must surface, not vanish" convention
/// (`event_latencies`'s own doc comment makes the same point about `None` vs. a bogus zero).
#[derive(Debug, Serialize)]
pub struct SkippedCapture {
    pub dir: PathBuf,
    pub reason: String,
}

pub struct AnalyzeOptions {
    pub indicator_threshold: f64,
    pub quiet_threshold: f64,
    pub min_quiet_frames: usize,
    pub change_threshold: f64,
    /// `run_label` value (case-insensitive) excluded from pooling — the discarded warm-up pass.
    pub warmup_label: String,
}

/// (config, interaction) -> pooled samples.
pub type SampleBuckets = BTreeMap<(String, String), MetricSamples>;

/// Walks `root`, analyzes every non-warm-up capture it finds, and pools the results into
/// per-(config, interaction) sample buckets. Returns the buckets plus every capture that was
/// skipped (unreadable meta, mismatched frame counts, a missing indicator edge, ...) so the
/// caller can report them instead of the pooled numbers silently under-counting.
///
/// Interaction handling (matches `docs/benchmarks/hero-scenario.md`):
/// - `switch` / `switch-cold`: every detected indicator edge is a switch event — pools
///   `settled_ms` and `first_change_ms`.
/// - `crop`: `hero.ahk`'s `ScriptedDrag` flashes the indicator at drag-start (edge 1) and
///   drag-end (edge 2); edge 0 is the `r` keypress that enters crop mode. Pools `interval_ms`
///   from the edge-1..edge-2 window.
/// - `zoom`: edge 0 is the `Z` keypress (the zoom-settled switch event, pools `settled_ms`);
///   edges 1/2 bracket the pan drag exactly like crop (pools `interval_ms`).
/// - `mixed` (interaction D, #100): reads the capture's `events.csv` sidecar for per-flash
///   `kind`/`step`/`step_index` attribution (positional edge conventions like crop/zoom's don't
///   apply — a mixed sequence's flash layout varies run to run). Each `switch`/`crop-enter`/
///   `auto-tone`/`straighten` kind pools `settled_ms`/`first_change_ms` via
///   [`event_latencies_bounded`], bounded to the *next* event's own flash so one event's settle
///   search never bleeds into the next; a `drag-start`/`drag-end` pair pools `interval_ms` the
///   same way crop/zoom do. Every metric is pooled twice: once under `mixed/<kind>` (the
///   aggregate) and once under `mixed/<kind>@after-<predecessor-step>`, where predecessor is the
///   step name immediately before this event's own step occurrence (`"start"` for the first) —
///   the per-predecessor buckets are what #100's cross-operation regression check compares.
///   `events.csv`'s row count must equal the capture's detected indicator-edge count, or the
///   whole capture is skipped (a mismatch means the sidecar and the capture disagree about what
///   happened, not something safe to guess through).
pub fn pool_results(
    root: &Path,
    opts: &AnalyzeOptions,
) -> stdio::Result<(SampleBuckets, Vec<SkippedCapture>)> {
    let mut buckets: SampleBuckets = BTreeMap::new();
    let mut skipped = Vec::new();

    for dir in find_capture_dirs(root)? {
        let meta_path = dir.join("meta.json");
        let meta: CaptureMeta = match fs::read_to_string(&meta_path)
            .map_err(|e| e.to_string())
            .and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string()))
        {
            Ok(m) => m,
            Err(e) => {
                skipped.push(SkippedCapture {
                    dir,
                    reason: format!("unreadable/invalid meta.json: {e}"),
                });
                continue;
            }
        };

        if meta.run_label.eq_ignore_ascii_case(&opts.warmup_label) {
            continue;
        }

        let indicator = match read_frames_gray8(
            &dir.join("indicator.raw"),
            meta.indicator_w,
            meta.indicator_h,
        ) {
            Ok(f) => f,
            Err(e) => {
                skipped.push(SkippedCapture {
                    dir,
                    reason: format!("indicator.raw: {e}"),
                });
                continue;
            }
        };
        let roi = match read_frames_gray8(&dir.join("roi.raw"), meta.roi_w, meta.roi_h) {
            Ok(f) => f,
            Err(e) => {
                skipped.push(SkippedCapture {
                    dir,
                    reason: format!("roi.raw: {e}"),
                });
                continue;
            }
        };

        // A `-Cold` run-hero.ps1 pass labels its meta.json interaction "$Interaction-cold" (e.g.
        // "crop-cold") so cold results pool separately from warm ones -- but which
        // switch/crop/zoom pipeline to run is the same regardless of warm/cold, so strip the
        // suffix only for that decision. `run-hero-series.ps1` passes `-Cold` uniformly for every
        // interaction (not just switch), so "crop-cold"/"zoom-cold" are real, reachable labels
        // that must be handled the same as "crop"/"zoom", not fall through to "unknown".
        let base_interaction = meta
            .interaction
            .strip_suffix("-cold")
            .unwrap_or(meta.interaction.as_str());

        // Computed before touching `buckets` so an unrecognized interaction never creates a
        // spurious empty bucket that would otherwise show up in the summary next to real results.
        let pan_interval_ms = |dir: &Path, skipped: &mut Vec<SkippedCapture>| -> Option<Vec<f64>> {
            match drag_window_from_edges(&indicator, opts.indicator_threshold, (1, 2)) {
                Ok((start, end)) => {
                    let diffs = roi.diff_series();
                    let distinct =
                        distinct_change_frames(&diffs, start, end, opts.change_threshold);
                    let intervals = frame_intervals(&distinct);
                    Some(
                        intervals
                            .iter()
                            .map(|&f| frames_to_ms(f, meta.capture_fps))
                            .collect(),
                    )
                }
                Err(e) => {
                    skipped.push(SkippedCapture {
                        dir: dir.to_path_buf(),
                        reason: e,
                    });
                    None
                }
            }
        };

        match base_interaction {
            "switch" => {
                match analyze_switch(
                    &indicator,
                    &roi,
                    meta.capture_fps,
                    opts.indicator_threshold,
                    opts.quiet_threshold,
                    opts.min_quiet_frames,
                    opts.change_threshold,
                    None,
                ) {
                    Ok(report) => {
                        let bucket = buckets
                            .entry((meta.config.clone(), meta.interaction.clone()))
                            .or_default();
                        bucket
                            .settled_ms
                            .extend(report.events.iter().filter_map(|e| e.settled_ms));
                        bucket
                            .first_change_ms
                            .extend(report.events.iter().filter_map(|e| e.first_change_ms));
                    }
                    Err(e) => skipped.push(SkippedCapture { dir, reason: e }),
                }
            }
            "crop" => {
                if let Some(samples) = pan_interval_ms(&dir, &mut skipped) {
                    buckets
                        .entry((meta.config.clone(), meta.interaction.clone()))
                        .or_default()
                        .interval_ms
                        .extend(samples);
                }
            }
            "zoom" => {
                let settled = match analyze_switch(
                    &indicator,
                    &roi,
                    meta.capture_fps,
                    opts.indicator_threshold,
                    opts.quiet_threshold,
                    opts.min_quiet_frames,
                    opts.change_threshold,
                    Some(&[0]),
                ) {
                    Ok(report) => Some(
                        report
                            .events
                            .iter()
                            .filter_map(|e| e.settled_ms)
                            .collect::<Vec<_>>(),
                    ),
                    Err(e) => {
                        skipped.push(SkippedCapture {
                            dir: dir.clone(),
                            reason: e,
                        });
                        None
                    }
                };
                let interval = pan_interval_ms(&dir, &mut skipped);
                if settled.is_some() || interval.is_some() {
                    let bucket = buckets
                        .entry((meta.config.clone(), meta.interaction.clone()))
                        .or_default();
                    if let Some(s) = settled {
                        bucket.settled_ms.extend(s);
                    }
                    if let Some(i) = interval {
                        bucket.interval_ms.extend(i);
                    }
                }
            }
            "mixed" => {
                // Same check `analyze_switch` does internally for switch/zoom -- the mixed arm
                // computes brightness/diffs directly instead of going through that helper, so it
                // doesn't get this for free. Without it, a capture whose indicator/roi crops came
                // from mismatched ffmpeg passes would silently produce edges that land past
                // `roi_diffs`'s end (clamped to `None`/empty by event_latencies_bounded/
                // distinct_change_frames, not a panic) instead of being skipped like every other
                // malformed-input case here.
                if indicator.frames.len() != roi.frames.len() {
                    skipped.push(SkippedCapture {
                        dir,
                        reason: format!(
                            "indicator ({} frames) and roi ({} frames) captures have different frame counts — \
                             they must come from the same capture window",
                            indicator.frames.len(),
                            roi.frames.len()
                        ),
                    });
                    continue;
                }

                let events_path = dir.join("events.csv");
                let events = match read_events_csv(&events_path) {
                    Ok(e) => e,
                    Err(e) => {
                        skipped.push(SkippedCapture {
                            dir,
                            reason: format!("events.csv: {e}"),
                        });
                        continue;
                    }
                };

                let brightness = indicator.mean_brightness();
                let all_edges = rising_edges(&brightness, opts.indicator_threshold);
                if all_edges.len() != events.len() {
                    skipped.push(SkippedCapture {
                        dir,
                        reason: format!(
                            "events.csv has {} row(s) but {} indicator edge(s) were detected — they must match 1:1",
                            events.len(),
                            all_edges.len()
                        ),
                    });
                    continue;
                }

                // predecessor_steps assumes step_index is gapless, non-decreasing, and each
                // step's flashes are contiguous -- true for well-formed hero.ahk output, but not
                // enforced by the two checks above. A hand-edited/corrupted events.csv that
                // violates it would otherwise get a silently wrong predecessor label (the
                // exact bucket #100's regression comparison reads) instead of being rejected.
                if events[0].step_index != 0 {
                    skipped.push(SkippedCapture {
                        dir,
                        reason: format!(
                            "events.csv row 0: step_index {} must be 0",
                            events[0].step_index
                        ),
                    });
                    continue;
                }
                if let Some((row, prev, cur)) = (1..events.len()).find_map(|i| {
                    let prev = events[i - 1].step_index;
                    let cur = events[i].step_index;
                    (cur != prev && cur != prev + 1).then_some((i, prev, cur))
                }) {
                    skipped.push(SkippedCapture {
                        dir,
                        reason: format!(
                            "events.csv row {row}: step_index {cur} is not {prev} (continuing the same step) or {} (the next step) — step_index must be gapless, non-decreasing, and each step's flashes contiguous",
                            prev + 1
                        ),
                    });
                    continue;
                }

                let roi_diffs = roi.diff_series();
                let predecessors = predecessor_steps(&events);
                let config = meta.config.clone();
                let interaction = meta.interaction.clone();

                let push_settled = |kind: &str,
                                         predecessor: &str,
                                         first_change_ms: Option<f64>,
                                         settled_ms: Option<f64>,
                                         buckets: &mut SampleBuckets| {
                    let unsettled = settled_ms.is_none();
                    for key in [
                        format!("{interaction}/{kind}"),
                        format!("{interaction}/{kind}@after-{predecessor}"),
                    ] {
                        let bucket = buckets.entry((config.clone(), key)).or_default();
                        if let Some(ms) = first_change_ms {
                            bucket.first_change_ms.push(ms);
                        }
                        if let Some(ms) = settled_ms {
                            bucket.settled_ms.push(ms);
                        }
                        if unsettled {
                            bucket.unsettled += 1;
                        }
                    }
                };
                let push_interval = |kind: &str,
                                          predecessor: &str,
                                          interval_ms: &[f64],
                                          buckets: &mut SampleBuckets| {
                    for key in [
                        format!("{interaction}/{kind}"),
                        format!("{interaction}/{kind}@after-{predecessor}"),
                    ] {
                        buckets
                            .entry((config.clone(), key))
                            .or_default()
                            .interval_ms
                            .extend_from_slice(interval_ms);
                    }
                };

                let mut i = 0;
                while i < events.len() {
                    let edge = all_edges[i];
                    let end = all_edges.get(i + 1).copied().unwrap_or(roi_diffs.len());
                    match events[i].kind.as_str() {
                        "drag-start" => match events.get(i + 1) {
                            Some(next) if next.kind == "drag-end" => {
                                let drag_end_edge = all_edges[i + 1];
                                let distinct = distinct_change_frames(
                                    &roi_diffs,
                                    edge,
                                    drag_end_edge,
                                    opts.change_threshold,
                                );
                                let intervals = frame_intervals(&distinct);
                                let interval_ms: Vec<f64> = intervals
                                    .iter()
                                    .map(|&f| frames_to_ms(f, meta.capture_fps))
                                    .collect();
                                push_interval("drag", &predecessors[i], &interval_ms, &mut buckets);
                                i += 2;
                            }
                            _ => {
                                skipped.push(SkippedCapture {
                                    dir: dir.clone(),
                                    reason: format!(
                                        "events.csv row {i}: 'drag-start' not immediately followed by 'drag-end'"
                                    ),
                                });
                                i += 1;
                            }
                        },
                        "drag-end" => {
                            // Only reached if a 'drag-end' appears without a preceding
                            // 'drag-start' -- already reported above; don't double-count it.
                            i += 1;
                        }
                        "end" => {
                            // Trailing bounding flash, no metric of its own.
                            i += 1;
                        }
                        kind @ ("switch" | "crop-enter" | "auto-tone" | "straighten") => {
                            let (first_change, settled) = event_latencies_bounded(
                                &roi_diffs,
                                edge,
                                end,
                                opts.change_threshold,
                                opts.quiet_threshold,
                                opts.min_quiet_frames,
                            );
                            let first_change_ms =
                                first_change.map(|f| frames_to_ms(f - edge, meta.capture_fps));
                            let settled_ms =
                                settled.map(|f| frames_to_ms(f - edge, meta.capture_fps));
                            push_settled(
                                kind,
                                &predecessors[i],
                                first_change_ms,
                                settled_ms,
                                &mut buckets,
                            );
                            i += 1;
                        }
                        other => {
                            skipped.push(SkippedCapture {
                                dir: dir.clone(),
                                reason: format!("events.csv row {i}: unknown event kind '{other}'"),
                            });
                            i += 1;
                        }
                    }
                }
            }
            other => skipped.push(SkippedCapture {
                dir,
                reason: format!(
                    "unknown interaction '{other}' (base of '{}', expected switch, crop, zoom, or mixed, each optionally suffixed '-cold')",
                    meta.interaction
                ),
            }),
        }
    }

    Ok((buckets, skipped))
}

/// For each event, the step name of the step occurrence immediately before it — used to bucket
/// interaction D's per-(kind, predecessor) regression comparison (#100). All events sharing one
/// `step_index` (e.g. a crop step's `crop-enter` + `drag-start` + `drag-end` flashes) share the
/// same predecessor: the step name of `step_index - 1`, or `"start"` for the sequence's first
/// step.
fn predecessor_steps(events: &[MixedEvent]) -> Vec<String> {
    let mut step_names: BTreeMap<usize, String> = BTreeMap::new();
    for e in events {
        step_names
            .entry(e.step_index)
            .or_insert_with(|| e.step.clone());
    }
    events
        .iter()
        .map(|e| {
            if e.step_index == 0 {
                "start".to_string()
            } else {
                step_names
                    .get(&(e.step_index - 1))
                    .cloned()
                    .unwrap_or_else(|| "start".to_string())
            }
        })
        .collect()
}

/// Summarized stats for one pooled (config, interaction) bucket. Fields are `None` when that
/// interaction doesn't produce that metric (e.g. `crop` never has `settled_ms`).
#[derive(Debug, Serialize)]
pub struct MetricStats {
    pub settled_ms: Option<Stats>,
    pub first_change_ms: Option<Stats>,
    pub interval_ms: Option<Stats>,
    /// `1000 / interval_ms.p95`, matching hero-scenario.md's "effective fps (p95 interval)".
    pub effective_fps_p95: Option<f64>,
    /// See [`MetricSamples::unsettled`]. Always 0 outside interaction `mixed`.
    pub unsettled: usize,
}

pub fn summarize_buckets(
    buckets: &SampleBuckets,
) -> BTreeMap<String, BTreeMap<String, MetricStats>> {
    let mut out: BTreeMap<String, BTreeMap<String, MetricStats>> = BTreeMap::new();
    for ((config, interaction), samples) in buckets {
        let interval_stats = summarize(&samples.interval_ms);
        let effective_fps_p95 = interval_stats.as_ref().map(|s| 1000.0 / s.p95);
        let stats = MetricStats {
            settled_ms: summarize(&samples.settled_ms),
            first_change_ms: summarize(&samples.first_change_ms),
            interval_ms: interval_stats,
            effective_fps_p95,
            unsettled: samples.unsettled,
        };
        out.entry(config.clone())
            .or_default()
            .insert(interaction.clone(), stats);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_dir() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("whisker-analyze-test-{}-{}", std::process::id(), n));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Writes a synthetic switch capture (3 events, each settling 4 frames after its flash, same
    /// shape as `tests/switch_pipeline.rs`'s ground truth) into `dir` as `meta.json` +
    /// `indicator.raw` + `roi.raw`.
    fn write_switch_capture(dir: &Path, config: &str, run_label: &str) {
        fs::create_dir_all(dir).unwrap();
        let mut indicator = Vec::new();
        let mut roi = Vec::new();
        let mut roi_value: u8 = 10;
        for _ in 0..5 {
            indicator.push(0u8);
            roi.push(roi_value);
        }
        for _event in 0..3 {
            indicator.extend([255, 255]);
            roi.extend([roi_value, roi_value]);
            indicator.extend([0, 0]);
            roi.extend([roi_value, roi_value]);
            roi_value = roi_value.wrapping_add(80);
            for _ in 0..6 {
                indicator.push(0);
                roi.push(roi_value);
            }
        }
        fs::write(dir.join("indicator.raw"), &indicator).unwrap();
        fs::write(dir.join("roi.raw"), &roi).unwrap();
        let meta = serde_json::json!({
            "interaction": "switch",
            "config": config,
            "run_label": run_label,
            "capture_fps": 60.0,
            "indicator_w": 1,
            "indicator_h": 1,
            "roi_w": 1,
            "roi_h": 1,
        });
        fs::write(dir.join("meta.json"), serde_json::to_string(&meta).unwrap()).unwrap();
    }

    #[test]
    fn find_capture_dirs_locates_nested_meta_json_only() {
        let root = temp_dir();
        write_switch_capture(&root.join("originals/switch/warmup"), "originals", "warmup");
        write_switch_capture(&root.join("originals/switch/run-1"), "originals", "run-1");
        fs::create_dir_all(root.join("originals/switch/empty-dir")).unwrap();

        let mut found = find_capture_dirs(&root).unwrap();
        found.sort();
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|p| p.join("meta.json").exists()));

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn pool_results_excludes_warmup_and_pools_across_runs() {
        let root = temp_dir();
        write_switch_capture(&root.join("originals/switch/warmup"), "originals", "warmup");
        write_switch_capture(&root.join("originals/switch/run-1"), "originals", "run-1");
        write_switch_capture(&root.join("originals/switch/run-2"), "originals", "run-2");

        let opts = AnalyzeOptions {
            indicator_threshold: 0.5,
            quiet_threshold: 0.01,
            min_quiet_frames: 3,
            change_threshold: 0.1,
            warmup_label: "warmup".to_string(),
        };
        let (buckets, skipped) = pool_results(&root, &opts).unwrap();
        assert!(skipped.is_empty(), "unexpected skips: {skipped:?}");

        let key = ("originals".to_string(), "switch".to_string());
        let samples = buckets.get(&key).expect("bucket present");
        // 3 events per run-1/run-2 capture (warmup excluded) = 6 pooled settled samples.
        assert_eq!(samples.settled_ms.len(), 6);
        assert_eq!(samples.first_change_ms.len(), 6);

        let summary = summarize_buckets(&buckets);
        let stats = &summary["originals"]["switch"];
        assert_eq!(stats.settled_ms.as_ref().unwrap().n, 6);

        fs::remove_dir_all(&root).unwrap();
    }

    /// Writes a zoom-shaped capture (edge 0 = Z keypress/zoom-settled, edges 1/2 bracket a pan
    /// drag with 2 distinct repaints) into `dir`.
    fn write_zoom_capture(dir: &Path, config: &str, run_label: &str, interaction: &str) {
        fs::create_dir_all(dir).unwrap();
        let mut indicator = Vec::new();
        let mut roi = Vec::new();
        let mut push = |ind: u8, r: u8| {
            indicator.push(ind);
            roi.push(r);
        };
        push(0, 10);
        push(255, 10); // edge 0: Z keypress
        push(0, 10);
        push(0, 90); // zoom settles
        push(0, 90);
        push(0, 90);
        push(255, 90); // edge 1: drag start
        push(0, 90);
        push(0, 150); // repaint 1
        push(0, 150);
        push(0, 210); // repaint 2
        push(255, 210); // edge 2: drag end
        push(0, 210);
        fs::write(dir.join("indicator.raw"), &indicator).unwrap();
        fs::write(dir.join("roi.raw"), &roi).unwrap();
        let meta = serde_json::json!({
            "interaction": interaction,
            "config": config,
            "run_label": run_label,
            "capture_fps": 60.0,
            "indicator_w": 1,
            "indicator_h": 1,
            "roi_w": 1,
            "roi_h": 1,
        });
        fs::write(dir.join("meta.json"), serde_json::to_string(&meta).unwrap()).unwrap();
    }

    #[test]
    fn pool_results_zoom_pools_both_settled_and_pan_interval() {
        let root = temp_dir();
        write_zoom_capture(
            &root.join("originals/zoom/run-1/img-1"),
            "originals",
            "run-1",
            "zoom",
        );
        write_zoom_capture(
            &root.join("originals/zoom/run-2/img-1"),
            "originals",
            "run-2",
            "zoom",
        );

        let opts = AnalyzeOptions {
            indicator_threshold: 0.5,
            quiet_threshold: 0.02,
            min_quiet_frames: 2,
            change_threshold: 0.05,
            warmup_label: "warmup".to_string(),
        };
        let (buckets, skipped) = pool_results(&root, &opts).unwrap();
        assert!(skipped.is_empty(), "unexpected skips: {skipped:?}");

        let key = ("originals".to_string(), "zoom".to_string());
        let samples = buckets.get(&key).expect("bucket present");
        // 1 settled sample per capture (only edge 0 counts), 2 captures.
        assert_eq!(samples.settled_ms.len(), 2);
        // 2 distinct repaints -> 1 interval between them, per capture; 2 captures.
        assert_eq!(samples.interval_ms.len(), 2);

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn pool_results_crop_pools_pan_interval_only() {
        let root = temp_dir();
        write_zoom_capture(
            &root.join("originals/crop/run-1/img-1"),
            "originals",
            "run-1",
            "crop",
        );

        let opts = AnalyzeOptions {
            indicator_threshold: 0.5,
            quiet_threshold: 0.02,
            min_quiet_frames: 2,
            change_threshold: 0.05,
            warmup_label: "warmup".to_string(),
        };
        let (buckets, skipped) = pool_results(&root, &opts).unwrap();
        assert!(skipped.is_empty(), "unexpected skips: {skipped:?}");

        let key = ("originals".to_string(), "crop".to_string());
        let samples = buckets.get(&key).expect("bucket present");
        assert!(samples.settled_ms.is_empty(), "crop has no settled metric");
        // 1 capture, 2 distinct repaints in its drag-start..drag-end window -> 1 interval.
        assert_eq!(samples.interval_ms.len(), 1);

        fs::remove_dir_all(&root).unwrap();
    }

    /// A `run-hero.ps1 -Cold` pass on crop/zoom records `interaction: "crop-cold"`/`"zoom-cold"`
    /// (run-hero-series.ps1 passes `-Cold` uniformly, not just for switch) -- these must pool
    /// using the same crop/zoom pipeline as their warm counterparts, into their own separate
    /// bucket, not fall through to "unknown interaction" and get silently dropped.
    #[test]
    fn pool_results_handles_cold_crop_and_zoom_interactions() {
        let root = temp_dir();
        write_zoom_capture(
            &root.join("originals/crop-cold/run-1/img-1"),
            "originals",
            "run-1",
            "crop-cold",
        );
        write_zoom_capture(
            &root.join("originals/zoom-cold/run-1/img-1"),
            "originals",
            "run-1",
            "zoom-cold",
        );

        let opts = AnalyzeOptions {
            indicator_threshold: 0.5,
            quiet_threshold: 0.02,
            min_quiet_frames: 2,
            change_threshold: 0.05,
            warmup_label: "warmup".to_string(),
        };
        let (buckets, skipped) = pool_results(&root, &opts).unwrap();
        assert!(skipped.is_empty(), "unexpected skips: {skipped:?}");

        let crop_cold = buckets
            .get(&("originals".to_string(), "crop-cold".to_string()))
            .expect("crop-cold bucket present, distinct from warm crop");
        assert_eq!(crop_cold.interval_ms.len(), 1);

        let zoom_cold = buckets
            .get(&("originals".to_string(), "zoom-cold".to_string()))
            .expect("zoom-cold bucket present, distinct from warm zoom");
        assert_eq!(zoom_cold.settled_ms.len(), 1);
        assert_eq!(zoom_cold.interval_ms.len(), 1);

        // Warm "crop"/"zoom" buckets must not exist just because a cold capture was seen.
        assert!(!buckets.contains_key(&("originals".to_string(), "crop".to_string())));
        assert!(!buckets.contains_key(&("originals".to_string(), "zoom".to_string())));

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn pool_results_unrecognized_interaction_is_skipped_without_creating_a_bucket() {
        let root = temp_dir();
        write_zoom_capture(
            &root.join("originals/bogus/run-1/img-1"),
            "originals",
            "run-1",
            "bogus",
        );

        let opts = AnalyzeOptions {
            indicator_threshold: 0.5,
            quiet_threshold: 0.02,
            min_quiet_frames: 2,
            change_threshold: 0.05,
            warmup_label: "warmup".to_string(),
        };
        let (buckets, skipped) = pool_results(&root, &opts).unwrap();
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0].reason.contains("unknown interaction"));
        assert!(
            buckets.is_empty(),
            "an unrecognized interaction must not create an empty bucket in the summary"
        );

        fs::remove_dir_all(&root).unwrap();
    }

    /// Writes a synthetic interaction-D capture (#100): switch -> crop (crop-enter + drag) ->
    /// auto-tone -> switch -> straighten (crop-enter + straighten) -> end, 9 flashes total. Each
    /// non-drag event settles instantly on the frame after its own change (min_quiet_frames=2,
    /// so a single repeated frame counts as settled) so the expected settled/predecessor buckets
    /// are easy to hand-verify. ROI steps through 9 widely separated plateaus (30/255 apart, well
    /// above the 0.05 change threshold) so every transition is unambiguous.
    fn write_mixed_capture(dir: &Path, config: &str, run_label: &str) {
        fs::create_dir_all(dir).unwrap();
        let levels: [u8; 9] = [10, 40, 70, 100, 130, 160, 190, 220, 250];
        let mut indicator = Vec::new();
        let mut roi = Vec::new();
        let mut push = |ind: u8, r: u8| {
            indicator.push(ind);
            roi.push(r);
        };
        push(0, levels[0]); // pre-roll
        push(255, levels[0]); // edge 0: switch flash
        push(0, levels[1]); // switch change
        push(0, levels[1]); // switch settles
        push(255, levels[1]); // edge 1: crop-enter flash
        push(0, levels[2]); // crop-enter change
        push(0, levels[2]); // crop-enter settles
        push(255, levels[2]); // edge 2: drag-start flash
        push(0, levels[3]); // drag repaint 1
        push(0, levels[3]); // (no new distinct change)
        push(0, levels[4]); // drag repaint 2
        push(255, levels[4]); // edge 3: drag-end flash
        push(0, levels[4]); // indicator low again, no roi change yet
        push(255, levels[4]); // edge 4: auto-tone flash
        push(0, levels[5]); // auto-tone change
        push(0, levels[5]); // auto-tone settles
        push(255, levels[5]); // edge 5: switch (2nd) flash
        push(0, levels[6]); // switch change
        push(0, levels[6]); // switch settles
        push(255, levels[6]); // edge 6: crop-enter (straighten) flash
        push(0, levels[7]); // change
        push(0, levels[7]); // settles
        push(255, levels[7]); // edge 7: straighten flash
        push(0, levels[8]); // change
        push(0, levels[8]); // settles
        push(255, levels[8]); // edge 8: end flash
        push(0, levels[8]); // trailing

        fs::write(dir.join("indicator.raw"), &indicator).unwrap();
        fs::write(dir.join("roi.raw"), &roi).unwrap();
        fs::write(
            dir.join("events.csv"),
            "edge,kind,step,step_index\n\
             0,switch,switch,0\n\
             1,crop-enter,crop,1\n\
             2,drag-start,crop,1\n\
             3,drag-end,crop,1\n\
             4,auto-tone,auto-tone,2\n\
             5,switch,switch,3\n\
             6,crop-enter,straighten,4\n\
             7,straighten,straighten,4\n\
             8,end,end,5\n",
        )
        .unwrap();
        let meta = serde_json::json!({
            "interaction": "mixed",
            "config": config,
            "run_label": run_label,
            "capture_fps": 60.0,
            "indicator_w": 1,
            "indicator_h": 1,
            "roi_w": 1,
            "roi_h": 1,
        });
        fs::write(dir.join("meta.json"), serde_json::to_string(&meta).unwrap()).unwrap();
    }

    fn mixed_opts() -> AnalyzeOptions {
        AnalyzeOptions {
            indicator_threshold: 0.5,
            quiet_threshold: 0.02,
            min_quiet_frames: 2,
            change_threshold: 0.05,
            warmup_label: "warmup".to_string(),
        }
    }

    #[test]
    fn pool_results_mixed_buckets_settled_events_by_kind_and_predecessor() {
        let root = temp_dir();
        write_mixed_capture(&root.join("originals/mixed/run-1"), "originals", "run-1");

        let (buckets, skipped) = pool_results(&root, &mixed_opts()).unwrap();
        assert!(skipped.is_empty(), "unexpected skips: {skipped:?}");

        let get = |key: &str| {
            buckets
                .get(&("originals".to_string(), key.to_string()))
                .unwrap_or_else(|| panic!("expected bucket '{key}'"))
        };

        // Aggregate bucket: crop-enter appears twice (once for `crop`, once for `straighten`).
        assert_eq!(get("mixed/crop-enter").settled_ms.len(), 2);
        // Per-predecessor split: both crop-enter occurrences happen to follow a `switch` step.
        assert_eq!(get("mixed/crop-enter@after-switch").settled_ms.len(), 2);

        // The two `switch` events have different predecessors ("start" for the first, "auto-tone"
        // for the second) -- this is exactly the cross-operation comparison #100 is after.
        assert_eq!(get("mixed/switch").settled_ms.len(), 2);
        assert_eq!(get("mixed/switch@after-start").settled_ms.len(), 1);
        assert_eq!(get("mixed/switch@after-auto-tone").settled_ms.len(), 1);

        assert_eq!(get("mixed/auto-tone@after-crop").settled_ms.len(), 1);
        assert_eq!(get("mixed/straighten@after-switch").settled_ms.len(), 1);

        // The crop step's drag-start/drag-end pair pools an interval, not a settled latency.
        assert_eq!(get("mixed/drag").interval_ms.len(), 1);
        assert_eq!(get("mixed/drag@after-switch").interval_ms.len(), 1);

        // Every settled event in this fixture actually settles before the next flash.
        for key in [
            "mixed/switch",
            "mixed/crop-enter",
            "mixed/auto-tone",
            "mixed/straighten",
        ] {
            assert_eq!(
                get(key).unsettled,
                0,
                "unexpected unsettled count for {key}"
            );
        }

        // The trailing "end" bounding flash contributes no metric of its own.
        assert!(!buckets.keys().any(|(_, k)| k.starts_with("mixed/end")));

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn pool_results_mixed_counts_unsettled_when_next_flash_cuts_off_the_search() {
        let root = temp_dir();
        let dir = root.join("originals/mixed/run-1");
        fs::create_dir_all(&dir).unwrap();
        // A 2-event capture: `switch` flashes, the ROI keeps changing every frame (never two
        // consecutive equal values) until the very next `switch` flash fires -- must count as
        // unsettled, not silently pass.
        let indicator = vec![0u8, 255, 0, 0, 255, 0, 0];
        let roi = vec![10u8, 10, 90, 170, 250, 250, 250];
        fs::write(dir.join("indicator.raw"), &indicator).unwrap();
        fs::write(dir.join("roi.raw"), &roi).unwrap();
        fs::write(
            dir.join("events.csv"),
            "edge,kind,step,step_index\n0,switch,switch,0\n1,switch,switch,1\n",
        )
        .unwrap();
        let meta = serde_json::json!({
            "interaction": "mixed",
            "config": "originals",
            "run_label": "run-1",
            "capture_fps": 60.0,
            "indicator_w": 1,
            "indicator_h": 1,
            "roi_w": 1,
            "roi_h": 1,
        });
        fs::write(dir.join("meta.json"), serde_json::to_string(&meta).unwrap()).unwrap();

        let (buckets, skipped) = pool_results(&root, &mixed_opts()).unwrap();
        assert!(skipped.is_empty(), "unexpected skips: {skipped:?}");

        let first = buckets
            .get(&(
                "originals".to_string(),
                "mixed/switch@after-start".to_string(),
            ))
            .unwrap();
        assert!(first.settled_ms.is_empty());
        assert_eq!(first.unsettled, 1);

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn pool_results_mixed_skips_capture_when_indicator_and_roi_frame_counts_differ() {
        let root = temp_dir();
        let dir = root.join("originals/mixed/run-1");
        fs::create_dir_all(&dir).unwrap();
        // roi.raw has one fewer frame than indicator.raw -- simulates the two crop-extraction
        // ffmpeg passes disagreeing, same failure mode `analyze_switch` already guards against.
        let indicator = vec![0u8, 255, 0, 255, 0];
        let roi = vec![10u8, 10, 90, 90];
        fs::write(dir.join("indicator.raw"), &indicator).unwrap();
        fs::write(dir.join("roi.raw"), &roi).unwrap();
        fs::write(
            dir.join("events.csv"),
            "edge,kind,step,step_index\n0,switch,switch,0\n1,switch,switch,1\n",
        )
        .unwrap();
        let meta = serde_json::json!({
            "interaction": "mixed",
            "config": "originals",
            "run_label": "run-1",
            "capture_fps": 60.0,
            "indicator_w": 1,
            "indicator_h": 1,
            "roi_w": 1,
            "roi_h": 1,
        });
        fs::write(dir.join("meta.json"), serde_json::to_string(&meta).unwrap()).unwrap();

        let (buckets, skipped) = pool_results(&root, &mixed_opts()).unwrap();
        assert!(buckets.is_empty());
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0].reason.contains("different frame counts"));

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn pool_results_mixed_skips_capture_when_step_index_is_not_gapless_or_contiguous() {
        let root = temp_dir();
        let dir = root.join("originals/mixed/run-1");
        fs::create_dir_all(&dir).unwrap();
        // step_index jumps from 0 straight to 2 -- a malformed/hand-edited events.csv that
        // predecessor_steps must not silently misinterpret.
        let indicator = vec![0u8, 255, 0, 255, 0];
        let roi = vec![10u8, 10, 90, 90, 90];
        fs::write(dir.join("indicator.raw"), &indicator).unwrap();
        fs::write(dir.join("roi.raw"), &roi).unwrap();
        fs::write(
            dir.join("events.csv"),
            "edge,kind,step,step_index\n0,switch,switch,0\n1,switch,switch,2\n",
        )
        .unwrap();
        let meta = serde_json::json!({
            "interaction": "mixed",
            "config": "originals",
            "run_label": "run-1",
            "capture_fps": 60.0,
            "indicator_w": 1,
            "indicator_h": 1,
            "roi_w": 1,
            "roi_h": 1,
        });
        fs::write(dir.join("meta.json"), serde_json::to_string(&meta).unwrap()).unwrap();

        let (buckets, skipped) = pool_results(&root, &mixed_opts()).unwrap();
        assert!(buckets.is_empty());
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0].reason.contains("gapless"));

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn pool_results_mixed_skips_capture_when_first_step_index_is_not_zero() {
        let root = temp_dir();
        let dir = root.join("originals/mixed/run-1");
        fs::create_dir_all(&dir).unwrap();
        let indicator = vec![0u8, 255, 0];
        let roi = vec![10u8, 10, 90];
        fs::write(dir.join("indicator.raw"), &indicator).unwrap();
        fs::write(dir.join("roi.raw"), &roi).unwrap();
        fs::write(
            dir.join("events.csv"),
            "edge,kind,step,step_index\n0,switch,switch,1\n",
        )
        .unwrap();
        let meta = serde_json::json!({
            "interaction": "mixed",
            "config": "originals",
            "run_label": "run-1",
            "capture_fps": 60.0,
            "indicator_w": 1,
            "indicator_h": 1,
            "roi_w": 1,
            "roi_h": 1,
        });
        fs::write(dir.join("meta.json"), serde_json::to_string(&meta).unwrap()).unwrap();

        let (buckets, skipped) = pool_results(&root, &mixed_opts()).unwrap();
        assert!(buckets.is_empty());
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0].reason.contains("must be 0"));

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn pool_results_mixed_skips_capture_when_events_csv_edge_count_mismatches() {
        let root = temp_dir();
        let dir = root.join("originals/mixed/run-1");
        fs::create_dir_all(&dir).unwrap();
        // 2 indicator flashes in the capture, but events.csv only describes 1.
        let indicator = vec![0u8, 255, 0, 255, 0];
        let roi = vec![10u8, 10, 90, 90, 90];
        fs::write(dir.join("indicator.raw"), &indicator).unwrap();
        fs::write(dir.join("roi.raw"), &roi).unwrap();
        fs::write(
            dir.join("events.csv"),
            "edge,kind,step,step_index\n0,switch,switch,0\n",
        )
        .unwrap();
        let meta = serde_json::json!({
            "interaction": "mixed",
            "config": "originals",
            "run_label": "run-1",
            "capture_fps": 60.0,
            "indicator_w": 1,
            "indicator_h": 1,
            "roi_w": 1,
            "roi_h": 1,
        });
        fs::write(dir.join("meta.json"), serde_json::to_string(&meta).unwrap()).unwrap();

        let (buckets, skipped) = pool_results(&root, &mixed_opts()).unwrap();
        assert!(buckets.is_empty());
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0].reason.contains("must match 1:1"));

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn pool_results_mixed_reports_malformed_drag_pair_instead_of_dropping_silently() {
        let root = temp_dir();
        let dir = root.join("originals/mixed/run-1");
        fs::create_dir_all(&dir).unwrap();
        // "drag-start" immediately followed by another "switch" instead of "drag-end".
        let indicator = vec![0u8, 255, 0, 255, 0];
        let roi = vec![10u8, 10, 90, 90, 130];
        fs::write(dir.join("indicator.raw"), &indicator).unwrap();
        fs::write(dir.join("roi.raw"), &roi).unwrap();
        fs::write(
            dir.join("events.csv"),
            "edge,kind,step,step_index\n0,drag-start,crop,0\n1,switch,switch,1\n",
        )
        .unwrap();
        let meta = serde_json::json!({
            "interaction": "mixed",
            "config": "originals",
            "run_label": "run-1",
            "capture_fps": 60.0,
            "indicator_w": 1,
            "indicator_h": 1,
            "roi_w": 1,
            "roi_h": 1,
        });
        fs::write(dir.join("meta.json"), serde_json::to_string(&meta).unwrap()).unwrap();

        let (buckets, skipped) = pool_results(&root, &mixed_opts()).unwrap();
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0]
            .reason
            .contains("not immediately followed by 'drag-end'"));
        // The trailing "switch" is still processed even though the drag pair was malformed.
        assert!(buckets.contains_key(&("originals".to_string(), "mixed/switch".to_string())));

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn pool_results_reports_unreadable_meta_instead_of_dropping_silently() {
        let root = temp_dir();
        let bad = root.join("originals/switch/run-1");
        fs::create_dir_all(&bad).unwrap();
        fs::write(bad.join("meta.json"), "not json").unwrap();

        let opts = AnalyzeOptions {
            indicator_threshold: 0.5,
            quiet_threshold: 0.01,
            min_quiet_frames: 3,
            change_threshold: 0.1,
            warmup_label: "warmup".to_string(),
        };
        let (buckets, skipped) = pool_results(&root, &opts).unwrap();
        assert!(buckets.is_empty());
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0].reason.contains("meta.json"));

        fs::remove_dir_all(&root).unwrap();
    }
}
