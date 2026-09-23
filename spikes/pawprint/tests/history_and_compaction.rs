//! Proves the ADR's history claims: undo/redo, a slider-drag burst
//! compacting to one step without losing the pre-drag undo target, and a
//! named snapshot surviving compaction untouched.

use pawprint::history::History;
use pawprint::{EditDocument, StageEntry};
use serde_json::json;

fn exposure(value: f64) -> StageEntry {
    StageEntry {
        schema_version: 1,
        params: json!({"exposure": value}),
    }
}

#[test]
fn undo_redo_round_trip() {
    let mut history = History::new(EditDocument::default());
    history.apply("global", "exposure_slider", exposure(0.0));
    history.apply("global", "exposure_slider", exposure(0.3));

    assert_eq!(
        history.document().stages["global"].params,
        json!({"exposure": 0.3})
    );
    assert!(history.undo());
    assert_eq!(
        history.document().stages["global"].params,
        json!({"exposure": 0.0})
    );
    assert!(history.redo());
    assert_eq!(
        history.document().stages["global"].params,
        json!({"exposure": 0.3})
    );
}

#[test]
fn slider_burst_compacts_to_one_step_and_undo_lands_pre_drag() {
    const WINDOW_MS: u128 = 500;
    let mut history = History::new(EditDocument::default());
    // Pre-drag baseline, established well before the drag starts (a real
    // gap, not just "the previous tick") so it doesn't get swept into the
    // burst's coalescing window.
    history.apply_at("global", "exposure_slider", exposure(0.0), 0);
    let drag_start_ms = 10 * WINDOW_MS;
    for tick in 1..=200u128 {
        // 1ms apart: every tick falls well inside the coalescing window.
        history.apply_at(
            "global",
            "exposure_slider",
            exposure(tick as f64 * 0.01),
            drag_start_ms + tick,
        );
    }
    assert_eq!(history.len(), 201, "one entry per tick before compaction");

    history.compact(WINDOW_MS);
    assert_eq!(history.len(), 2, "baseline + one compacted burst");

    assert_eq!(
        history.document().stages["global"].params,
        json!({"exposure": 2.0})
    );
    assert!(history.undo());
    assert_eq!(
        history.document().stages["global"].params,
        json!({"exposure": 0.0}),
        "undo after compaction must land on the pre-drag value, not a mid-burst tick"
    );
}

#[test]
fn named_snapshot_survives_compaction() {
    let mut history = History::new(EditDocument::default());
    history.apply("global", "exposure_slider", exposure(0.1));
    history.apply("global", "exposure_slider", exposure(0.2));
    history.snapshot("before crop experiment");
    history.apply("global", "exposure_slider", exposure(0.3));
    history.apply("global", "exposure_slider", exposure(0.4));

    let len_before = history.len();
    history.compact(u128::MAX);

    assert!(
        history.len() < len_before,
        "the two runs either side of the snapshot should each compact"
    );
    assert_eq!(history.snapshot_names(), vec!["before crop experiment"]);
    assert_eq!(
        history
            .snapshot_document("before crop experiment")
            .unwrap()
            .stages["global"]
            .params,
        json!({"exposure": 0.2})
    );
}

#[test]
fn compaction_is_a_noop_with_a_pending_redo() {
    let mut history = History::new(EditDocument::default());
    history.apply("global", "exposure_slider", exposure(0.1));
    history.apply("global", "exposure_slider", exposure(0.2));
    history.undo();
    let len_before = history.len();

    history.compact(u128::MAX);

    assert_eq!(
        history.len(),
        len_before,
        "compact must not touch a log with a pending redo"
    );
}
