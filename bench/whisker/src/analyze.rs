//! Multi-run pooling for the #43 hero-scenario results tree that `run-hero-series.ps1` produces.
//!
//! `docs/benchmarks/hero-scenario.md` judges p95 over samples pooled across all 5 measured runs
//! (and, for crop/zoom, across the 5 spread target images too) — not per-capture numbers eyeballed
//! one at a time. This module walks a results root, reads each capture's `meta.json`, runs the
//! same switch/drag pipeline `bin/whisker.rs`'s standalone subcommands use, and pools the raw
//! millisecond/interval samples per (config, interaction) before handing them to
//! [`crate::stats::summarize`] — pooling raw samples, not averaging each capture's own p95,
//! matches the spec's "pool all ... across those 5 images" wording.

use crate::io::read_frames_gray8;
use crate::stats::{summarize, Stats};
use crate::{
    analyze_switch, distinct_change_frames, drag_window_from_edges, frame_intervals, frames_to_ms,
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
            other => skipped.push(SkippedCapture {
                dir,
                reason: format!(
                    "unknown interaction '{other}' (base of '{}', expected switch, crop, or zoom, each optionally suffixed '-cold')",
                    meta.interaction
                ),
            }),
        }
    }

    Ok((buckets, skipped))
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
