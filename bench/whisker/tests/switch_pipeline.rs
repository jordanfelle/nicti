//! End-to-end check of the full "switch" measurement pipeline (rising-edge detection ->
//! first-change/settled latency -> ms conversion), the same sequence `bin/whisker.rs`'s
//! `Command::Switch` runs. Stands in for the physical calibration pass described in
//! `../README.md` (a real capture with a known, scripted input timer) until that's been run on
//! actual hardware: here the "known timer" is a synthetic frame sequence instead of a real
//! capture, but the assertion is the same one the real calibration pass makes — the reported
//! latency should land within one frame of the known ground truth.

use whisker::stats::summarize;
use whisker::{first_change_frame, frames_to_ms, rising_edges, settled_frame, FrameStream};

fn solid(value: u8, size: usize) -> Vec<u8> {
    vec![value; size]
}

/// Builds a synthetic capture at 60fps for 3 switch events, each with a known, fixed timing:
/// indicator flashes for 2 frames (33ms), the ROI starts changing 2 frames after the flash
/// (33ms first-change latency) and settles 4 frames after the flash (67ms settled latency).
fn synthetic_capture() -> (FrameStream, FrameStream, f64) {
    const FPS: f64 = 60.0;
    let mut indicator_frames = Vec::new();
    let mut roi_frames = Vec::new();

    let mut push_idle = |n: usize, roi_value: u8| {
        for _ in 0..n {
            indicator_frames.push(solid(0, 4));
            roi_frames.push(solid(roi_value, 4));
        }
    };

    let mut roi_value: u8 = 10;
    push_idle(5, roi_value); // pre-roll, ROI stable at 10

    for _event in 0..3 {
        // indicator flash: 2 frames bright
        indicator_frames.push(solid(255, 4));
        indicator_frames.push(solid(255, 4));
        roi_frames.push(solid(roi_value, 4));
        roi_frames.push(solid(roi_value, 4));
        // 2 more idle frames (indicator back to dark) before the ROI starts changing
        indicator_frames.push(solid(0, 4));
        indicator_frames.push(solid(0, 4));
        roi_frames.push(solid(roi_value, 4));
        roi_frames.push(solid(roi_value, 4));
        // ROI changes to its new value 4 frames after the flash's leading edge and holds
        roi_value = roi_value.wrapping_add(80);
        for _ in 0..6 {
            indicator_frames.push(solid(0, 4));
            roi_frames.push(solid(roi_value, 4));
        }
    }

    let indicator = FrameStream { width: 2, height: 2, frames: indicator_frames };
    let roi = FrameStream { width: 2, height: 2, frames: roi_frames };
    (indicator, roi, FPS)
}

#[test]
fn switch_pipeline_matches_known_ground_truth_within_one_frame() {
    let (indicator, roi, fps) = synthetic_capture();
    assert_eq!(indicator.frames.len(), roi.frames.len(), "sanity: same capture length");

    let brightness = indicator.mean_brightness();
    let edges = rising_edges(&brightness, 0.5);
    assert_eq!(edges.len(), 3, "expected 3 keypress events");

    let diffs = roi.diff_series();
    let frame_ms = 1000.0 / fps;

    let mut first_change_samples = Vec::new();
    let mut settled_samples = Vec::new();
    for &edge in &edges {
        let first_change = first_change_frame(&diffs, edge, 0.1).expect("first-change detected");
        // Settled search starts from first_change, not the raw edge -- see bin/whisker.rs's
        // Command::Switch handler for why (a pre-change plateau would otherwise look "settled").
        let settled = settled_frame(&diffs, first_change, 0.01, 3).expect("settled detected");

        // Ground truth: ROI starts changing 4 frames after the flash's leading edge.
        assert!(
            (first_change as i64 - edge as i64 - 4).abs() <= 1,
            "first-change frame offset should be 4 +/- 1, got {}",
            first_change - edge
        );
        first_change_samples.push(frames_to_ms(first_change - edge, fps));
        settled_samples.push(frames_to_ms(settled - edge, fps));
    }

    let first_change_stats = summarize(&first_change_samples).unwrap();
    let expected_first_change_ms = 4.0 * frame_ms;
    assert!(
        (first_change_stats.p50 - expected_first_change_ms).abs() <= frame_ms,
        "p50 first-change latency {} should be within one frame of {}",
        first_change_stats.p50,
        expected_first_change_ms
    );

    let settled_stats = summarize(&settled_samples).unwrap();
    // Settled requires 3 quiet frames after the change lands at offset 4, so the earliest
    // detectable settled point is also frame offset 4 (the change frame itself, if it holds).
    assert!(settled_stats.p50 >= expected_first_change_ms - frame_ms);
}
