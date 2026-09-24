//! Frame-diff primitives for the #43 hero-scenario benchmark.
//!
//! `whisker`'s core logic never talks to ffmpeg directly — it consumes already-cropped,
//! already-decoded gray8 frame streams (produced upstream by ffmpeg's `crop` + `rawvideo`
//! filters, see `bin/whisker.rs` and `run-hero.ps1`) and turns them into the latency and
//! frame-interval numbers `docs/benchmarks/hero-scenario.md` defines. `analyze` is the one module
//! that does its own filesystem walking (a `run-hero-series.ps1` results tree), since pooling
//! samples across many runs/captures needs to read many `meta.json`/`*.raw` files itself rather
//! than being handed a single already-loaded pair like the rest of this crate.

pub mod analyze;
pub mod io;
pub mod stats;

use serde::Serialize;
use stats::{summarize, Stats};

/// A sequence of same-sized single-channel (gray8) frames.
#[derive(Debug, Clone)]
pub struct FrameStream {
    pub width: usize,
    pub height: usize,
    pub frames: Vec<Vec<u8>>,
}

impl FrameStream {
    pub fn frame_size(&self) -> usize {
        self.width * self.height
    }

    /// Mean pixel brightness (0.0-1.0) per frame.
    pub fn mean_brightness(&self) -> Vec<f64> {
        self.frames
            .iter()
            .map(|f| {
                if f.is_empty() {
                    0.0
                } else {
                    f.iter().map(|&b| b as f64).sum::<f64>() / f.len() as f64 / 255.0
                }
            })
            .collect()
    }

    /// Per-transition mean absolute pixel difference (0.0-1.0), normalized. `diffs[i]` is the
    /// difference between `frames[i]` and `frames[i+1]`; length is `frames.len() - 1`.
    pub fn diff_series(&self) -> Vec<f64> {
        self.frames
            .windows(2)
            .map(|pair| frame_diff(&pair[0], &pair[1]))
            .collect()
    }
}

/// Mean absolute difference between two same-length byte buffers, normalized to 0.0-1.0.
/// Mismatched lengths are treated as maximally different (1.0) rather than panicking, since a
/// malformed capture should show up as a loud outlier, not a crash mid-analysis.
pub fn frame_diff(a: &[u8], b: &[u8]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 1.0;
    }
    let sum: u64 = a
        .iter()
        .zip(b.iter())
        .map(|(&x, &y)| (x as i32 - y as i32).unsigned_abs() as u64)
        .sum();
    sum as f64 / a.len() as f64 / 255.0
}

/// Frame indices where `series` rises from below `threshold` to at or above it — used to find
/// each keypress-indicator flash's leading edge. A value must first drop back below `threshold`
/// before a later rise counts as a new edge, so a sustained-bright indicator only registers once.
pub fn rising_edges(series: &[f64], threshold: f64) -> Vec<usize> {
    let mut edges = Vec::new();
    let mut below = true;
    for (i, &v) in series.iter().enumerate() {
        if below && v >= threshold {
            edges.push(i);
            below = false;
        } else if v < threshold {
            below = true;
        }
    }
    edges
}

/// First frame index >= `start` where the ROI visibly differs from the previous frame (the
/// "first-change" metric — LRC's soft placeholder paint, not the settled result).
///
/// `diffs` is a transition series (see [`FrameStream::diff_series`]): `diffs[i]` describes the
/// change from frame `i` to frame `i + 1`, so a change detected at `diffs[i]` is first visible in
/// frame `i + 1`.
pub fn first_change_frame(diffs: &[f64], start: usize, change_threshold: f64) -> Option<usize> {
    (start..diffs.len())
        .find(|&i| diffs[i] >= change_threshold)
        .map(|i| i + 1)
}

/// First frame index >= `start` that begins a run of `min_quiet_frames` consecutive frames with
/// no further change (all transitions between them below `quiet_threshold`) — the "settled"
/// metric interactions A and C are judged against.
pub fn settled_frame(
    diffs: &[f64],
    start: usize,
    quiet_threshold: f64,
    min_quiet_frames: usize,
) -> Option<usize> {
    if min_quiet_frames < 2 {
        return None;
    }
    let needed_transitions = min_quiet_frames - 1;
    if needed_transitions > diffs.len() {
        return None;
    }
    (start..=diffs.len() - needed_transitions).find(|&f| {
        diffs[f..f + needed_transitions]
            .iter()
            .all(|&d| d < quiet_threshold)
    })
}

/// Computes both the first-change and settled latency frames for one indicator-marked event,
/// given the event's indicator edge frame. Returns `(first_change, settled)`.
///
/// `settled` is searched starting from `first_change`, not `edge` -- searching from `edge` would
/// let a brief pre-change plateau (the ROI hasn't started transitioning yet) look like a spurious
/// "already settled" result. And when no change was ever detected (`first_change` is `None`),
/// `settled` is also `None` rather than falling back to `edge`: with equal quiet/change
/// thresholds, a plateau that never crosses `change_threshold` trivially never crosses
/// `quiet_threshold` either, so a fallback would report a falsely "instantly settled" result for
/// an event whose ROI never visibly changed at all (a dropped/late event, or the capture ending
/// mid-transition) -- missing data must surface as `None`, not a misleadingly clean zero.
pub fn event_latencies(
    diffs: &[f64],
    edge: usize,
    change_threshold: f64,
    quiet_threshold: f64,
    min_quiet_frames: usize,
) -> (Option<usize>, Option<usize>) {
    let first_change = first_change_frame(diffs, edge, change_threshold);
    let settled =
        first_change.and_then(|fc| settled_frame(diffs, fc, quiet_threshold, min_quiet_frames));
    (first_change, settled)
}

/// Frame indices in `[start, end)` where a visible change occurred — used for interaction B/C's
/// drag frame-interval metric (how often the loupe actually repainted during a scripted drag).
pub fn distinct_change_frames(
    diffs: &[f64],
    start: usize,
    end: usize,
    change_threshold: f64,
) -> Vec<usize> {
    let end = end.min(diffs.len());
    (start..end)
        .filter(|&i| diffs[i] >= change_threshold)
        .map(|i| i + 1)
        .collect()
}

/// Gaps (in frame counts) between consecutive entries of an already-sorted, already-distinct
/// frame-index sequence.
pub fn frame_intervals(frames: &[usize]) -> Vec<usize> {
    frames.windows(2).map(|w| w[1] - w[0]).collect()
}

/// Converts a frame count to milliseconds at the given capture rate.
pub fn frames_to_ms(frame_count: usize, fps: f64) -> f64 {
    frame_count as f64 / fps * 1000.0
}

/// One indicator-marked event's measured latencies, relative to that event's own indicator edge.
#[derive(Debug, Clone, Serialize)]
pub struct SwitchEventResult {
    pub indicator_frame: usize,
    pub first_change_ms: Option<f64>,
    pub settled_ms: Option<f64>,
}

/// Full report for one `switch`-shaped capture (interaction A, or interaction C's zoom-settled
/// half): every detected indicator event plus pooled p50/p95/max stats.
#[derive(Debug, Clone, Serialize)]
pub struct SwitchReport {
    pub fps: f64,
    /// Total indicator edges detected in this capture, before any `edge_indices` filter —
    /// the sanity-check number (e.g. "== 49") the calibration dry-run confirms against.
    pub events_detected: usize,
    /// Number of those edges actually analyzed (== `events_detected` unless `edge_indices` was
    /// used to select a subset, e.g. only the first edge of a zoom capture's 3 flashes).
    pub events_analyzed: usize,
    pub events: Vec<SwitchEventResult>,
    pub first_change_stats: Option<Stats>,
    pub settled_stats: Option<Stats>,
}

/// Full report for one `drag`-shaped capture (interaction B, or interaction C's pan half).
#[derive(Debug, Clone, Serialize)]
pub struct DragReport {
    pub fps: f64,
    pub distinct_frames: Vec<usize>,
    pub interval_frames: Vec<usize>,
    pub interval_ms_stats: Option<Stats>,
    pub effective_fps_p95: Option<f64>,
}

/// Runs the full switch-measurement pipeline (rising-edge detection -> first-change/settled
/// latency -> ms conversion -> pooled stats) for one already-loaded indicator/ROI capture pair.
///
/// `edge_indices`, when given, restricts analysis to the listed 0-based indices into the *full*
/// set of detected indicator edges (e.g. `&[0]` for a zoom capture, where only the first of its 3
/// flashes — the `Z` keypress itself — is the zoom-settled event; the other two mark the pan
/// drag's window and would otherwise be mis-scored as switch events). `events_detected` in the
/// returned report always reports the full unfiltered count, so a filtered call still supports
/// the "confirm events_detected == N" calibration check.
#[allow(clippy::too_many_arguments)]
pub fn analyze_switch(
    indicator: &FrameStream,
    roi: &FrameStream,
    fps: f64,
    indicator_threshold: f64,
    quiet_threshold: f64,
    min_quiet_frames: usize,
    change_threshold: f64,
    edge_indices: Option<&[usize]>,
) -> Result<SwitchReport, String> {
    if indicator.frames.len() != roi.frames.len() {
        return Err(format!(
            "indicator ({} frames) and roi ({} frames) captures have different frame counts — \
             they must come from the same capture window",
            indicator.frames.len(),
            roi.frames.len()
        ));
    }

    let brightness = indicator.mean_brightness();
    let all_edges = rising_edges(&brightness, indicator_threshold);
    let edges: Vec<usize> = match edge_indices {
        Some(idxs) => idxs
            .iter()
            .filter_map(|&i| all_edges.get(i).copied())
            .collect(),
        None => all_edges.clone(),
    };
    let roi_diffs = roi.diff_series();

    let mut events = Vec::new();
    let mut first_change_samples = Vec::new();
    let mut settled_samples = Vec::new();
    for &edge in &edges {
        let (first_change, settled) = event_latencies(
            &roi_diffs,
            edge,
            change_threshold,
            quiet_threshold,
            min_quiet_frames,
        );
        let first_change_ms = first_change.map(|f| frames_to_ms(f - edge, fps));
        let settled_ms = settled.map(|f| frames_to_ms(f - edge, fps));
        if let Some(ms) = first_change_ms {
            first_change_samples.push(ms);
        }
        if let Some(ms) = settled_ms {
            settled_samples.push(ms);
        }
        events.push(SwitchEventResult {
            indicator_frame: edge,
            first_change_ms,
            settled_ms,
        });
    }

    Ok(SwitchReport {
        fps,
        events_detected: all_edges.len(),
        events_analyzed: edges.len(),
        first_change_stats: summarize(&first_change_samples),
        settled_stats: summarize(&settled_samples),
        events,
    })
}

/// Runs the full drag-measurement pipeline (distinct-repaint detection -> frame-interval ->
/// effective fps) for one already-loaded ROI capture, over an explicit transition-index window.
pub fn analyze_drag(
    roi: &FrameStream,
    fps: f64,
    start_frame: usize,
    end_frame: usize,
    change_threshold: f64,
) -> DragReport {
    let diffs = roi.diff_series();
    let distinct = distinct_change_frames(&diffs, start_frame, end_frame, change_threshold);
    let intervals = frame_intervals(&distinct);
    let interval_ms: Vec<f64> = intervals.iter().map(|&f| frames_to_ms(f, fps)).collect();
    let interval_ms_stats = summarize(&interval_ms);
    let effective_fps_p95 = interval_ms_stats.as_ref().map(|s| 1000.0 / s.p95);

    DragReport {
        fps,
        distinct_frames: distinct,
        interval_frames: intervals,
        interval_ms_stats,
        effective_fps_p95,
    }
}

/// Derives a drag transition-index window from a pair of indicator edges, instead of requiring
/// hand-picked frame numbers — used when `hero.ahk`'s `ScriptedDrag` flashes the indicator at the
/// drag's start and end (see its own doc comment) so the window is machine-detectable.
///
/// `window_edges` is `(start_index, end_index)`, 0-based into the full set of detected indicator
/// edges — e.g. `(1, 2)` for crop/zoom-pan, whose edge 0 is the mode-entry keypress (`r`/`z`).
pub fn drag_window_from_edges(
    indicator: &FrameStream,
    indicator_threshold: f64,
    window_edges: (usize, usize),
) -> Result<(usize, usize), String> {
    let brightness = indicator.mean_brightness();
    let edges = rising_edges(&brightness, indicator_threshold);
    let start = edges.get(window_edges.0).copied().ok_or_else(|| {
        format!(
            "indicator edge index {} not found (only {} edge(s) detected)",
            window_edges.0,
            edges.len()
        )
    })?;
    let end = edges.get(window_edges.1).copied().ok_or_else(|| {
        format!(
            "indicator edge index {} not found (only {} edge(s) detected)",
            window_edges.1,
            edges.len()
        )
    })?;
    Ok((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(value: u8, size: usize) -> Vec<u8> {
        vec![value; size]
    }

    #[test]
    fn frame_diff_identical_is_zero() {
        let a = solid(100, 16);
        assert_eq!(frame_diff(&a, &a), 0.0);
    }

    #[test]
    fn frame_diff_black_white_is_one() {
        let a = solid(0, 16);
        let b = solid(255, 16);
        assert!((frame_diff(&a, &b) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn frame_diff_mismatched_lengths_is_max() {
        assert_eq!(frame_diff(&[1, 2, 3], &[1, 2]), 1.0);
    }

    #[test]
    fn rising_edges_debounces_sustained_high() {
        // low, HIGH, high, high, low, HIGH, low
        let series = vec![0.0, 0.9, 0.9, 0.9, 0.1, 0.9, 0.1];
        assert_eq!(rising_edges(&series, 0.5), vec![1, 5]);
    }

    #[test]
    fn rising_edges_empty_on_flat_low() {
        let series = vec![0.0, 0.1, 0.05];
        assert!(rising_edges(&series, 0.5).is_empty());
    }

    /// Frame 0 = pre-switch (unchanged from a prior state), frames 1-2 a blurry placeholder
    /// paint, frames 3-6 settled on the final image.
    fn switch_frames() -> FrameStream {
        let size = 4;
        FrameStream {
            width: 2,
            height: 2,
            frames: vec![
                solid(10, size),  // 0: stable pre-switch
                solid(80, size),  // 1: first change (placeholder)
                solid(90, size),  // 2: still settling
                solid(200, size), // 3: final image
                solid(200, size), // 4: settled
                solid(200, size), // 5: settled
                solid(200, size), // 6: settled
            ],
        }
    }

    #[test]
    fn first_change_frame_finds_placeholder_paint() {
        let s = switch_frames();
        let diffs = s.diff_series();
        assert_eq!(first_change_frame(&diffs, 0, 0.05), Some(1));
    }

    #[test]
    fn settled_frame_finds_final_stable_run() {
        let s = switch_frames();
        let diffs = s.diff_series();
        // 3 consecutive frames with < 0.02 change between them, starting at frame 4
        // (frames 3,4,5,6 are all equal, so the first 3-in-a-row quiet run starts at frame 3).
        assert_eq!(settled_frame(&diffs, 0, 0.02, 3), Some(3));
    }

    #[test]
    fn settled_frame_none_if_never_quiets() {
        let mut frames = Vec::new();
        for i in 0..10u8 {
            frames.push(solid(i * 20, 4));
        }
        let s = FrameStream {
            width: 2,
            height: 2,
            frames,
        };
        let diffs = s.diff_series();
        assert_eq!(settled_frame(&diffs, 0, 0.01, 3), None);
    }

    #[test]
    fn distinct_change_frames_and_intervals_for_a_drag() {
        // Simulates a drag repainting every other frame: 0 same, 1 change, 2 same, 3 change, ...
        let mut frames = Vec::new();
        for i in 0..8u8 {
            let v = if i % 2 == 0 { 50 } else { 200 };
            frames.push(solid(v, 4));
        }
        let s = FrameStream {
            width: 2,
            height: 2,
            frames,
        };
        let diffs = s.diff_series();
        let changes = distinct_change_frames(&diffs, 0, diffs.len(), 0.2);
        assert_eq!(changes, vec![1, 2, 3, 4, 5, 6, 7]);
        let intervals = frame_intervals(&changes);
        assert!(intervals.iter().all(|&i| i == 1));
    }

    #[test]
    fn distinct_change_frames_window_respects_end() {
        let mut frames = Vec::new();
        for i in 0..6u8 {
            frames.push(solid(if i % 2 == 0 { 0 } else { 255 }, 4));
        }
        let s = FrameStream {
            width: 2,
            height: 2,
            frames,
        };
        let diffs = s.diff_series();
        // window only covers transitions [0, 2), i.e. frames 0->1 and 1->2
        let changes = distinct_change_frames(&diffs, 0, 2, 0.5);
        assert_eq!(changes, vec![1, 2]);
    }

    #[test]
    fn frames_to_ms_conversion() {
        assert!((frames_to_ms(6, 60.0) - 100.0).abs() < 1e-9);
        assert!((frames_to_ms(1, 120.0) - (1000.0 / 120.0)).abs() < 1e-9);
    }

    #[test]
    fn event_latencies_reports_settled_relative_to_first_change() {
        let s = switch_frames();
        let diffs = s.diff_series();
        let (first_change, settled) = event_latencies(&diffs, 0, 0.05, 0.02, 3);
        assert_eq!(first_change, Some(1));
        assert_eq!(settled, Some(3));
    }

    #[test]
    fn event_latencies_settled_is_none_when_no_change_ever_detected() {
        // The ROI never changes at all after the edge -- with equal change/quiet thresholds, a
        // buggy "fall back to edge" implementation would report Some(edge) (falsely "instantly
        // settled") instead of the correct "we never even saw a change" None.
        let s = FrameStream {
            width: 2,
            height: 2,
            frames: vec![solid(50, 4); 10],
        };
        let diffs = s.diff_series();
        let (first_change, settled) = event_latencies(&diffs, 0, 0.02, 0.02, 3);
        assert_eq!(first_change, None);
        assert_eq!(
            settled, None,
            "must not fall back to reporting a bogus near-zero settled latency"
        );
    }

    #[test]
    fn dropped_frames_show_up_as_a_large_single_diff() {
        // Two ffmpeg-adjacent frames that actually differ a lot (as if a frame were dropped
        // mid-transition) should register as a clean single change, not be silently smoothed.
        let s = FrameStream {
            width: 2,
            height: 2,
            frames: vec![solid(0, 4), solid(0, 4), solid(255, 4), solid(255, 4)],
        };
        let diffs = s.diff_series();
        assert_eq!(distinct_change_frames(&diffs, 0, diffs.len(), 0.5), vec![2]);
    }

    /// A 3-flash capture shaped like `hero.ahk`'s zoom interaction: edge 0 is the `Z` keypress
    /// (zoom-settled event), edges 1/2 bracket a pan drag with 3 distinct repaints in between.
    fn zoom_shaped_capture() -> (FrameStream, FrameStream) {
        let size = 4;
        let mut indicator = Vec::new();
        let mut roi = Vec::new();
        let mut push = |ind: u8, r: u8| {
            indicator.push(solid(ind, size));
            roi.push(solid(r, size));
        };
        push(0, 10); // pre-roll
        push(255, 10); // edge 0: Z keypress
        push(0, 10);
        push(0, 90); // settles here (offset 2 from edge 0)
        push(0, 90);
        push(0, 90);
        push(255, 90); // edge 1: drag start
        push(0, 90);
        push(0, 150); // repaint 1
        push(0, 150);
        push(0, 210); // repaint 2
        push(255, 210); // edge 2: drag end
        push(0, 210);
        (
            FrameStream {
                width: 2,
                height: 2,
                frames: indicator,
            },
            FrameStream {
                width: 2,
                height: 2,
                frames: roi,
            },
        )
    }

    #[test]
    fn analyze_switch_edge_filter_keeps_only_selected_edges() {
        let (indicator, roi) = zoom_shaped_capture();
        let unfiltered = analyze_switch(&indicator, &roi, 60.0, 0.5, 0.02, 2, 0.05, None).unwrap();
        assert_eq!(unfiltered.events_detected, 3);
        assert_eq!(unfiltered.events_analyzed, 3);

        let filtered =
            analyze_switch(&indicator, &roi, 60.0, 0.5, 0.02, 2, 0.05, Some(&[0])).unwrap();
        // events_detected still reports the full raw count -- useful for the calibration
        // "confirm events_detected == N" check even when only analyzing a subset.
        assert_eq!(filtered.events_detected, 3);
        assert_eq!(filtered.events_analyzed, 1);
        assert_eq!(filtered.events.len(), 1);
        assert!(filtered.events[0].settled_ms.is_some());
    }

    #[test]
    fn analyze_switch_mismatched_frame_counts_is_an_error() {
        let indicator = FrameStream {
            width: 2,
            height: 2,
            frames: vec![solid(0, 4); 5],
        };
        let roi = FrameStream {
            width: 2,
            height: 2,
            frames: vec![solid(0, 4); 4],
        };
        let result = analyze_switch(&indicator, &roi, 60.0, 0.5, 0.02, 3, 0.02, None);
        assert!(result.is_err());
    }

    #[test]
    fn drag_window_from_edges_derives_start_and_end() {
        let (indicator, roi) = zoom_shaped_capture();
        let (start, end) = drag_window_from_edges(&indicator, 0.5, (1, 2)).unwrap();
        let report = analyze_drag(&roi, 60.0, start, end, 0.05);
        // Two distinct repaints (150, then 210) inside the drag-start..drag-end window.
        assert_eq!(report.distinct_frames.len(), 2);
    }

    #[test]
    fn drag_window_from_edges_missing_edge_is_an_error() {
        let (indicator, _roi) = zoom_shaped_capture();
        let result = drag_window_from_edges(&indicator, 0.5, (1, 5));
        assert!(result.is_err());
    }
}
