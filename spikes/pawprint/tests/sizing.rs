//! Rough per-photo byte sizes, printed for the ADR's 2M-asset sizing table.
//! Not an assertion of a specific number (that would make the test brittle
//! against encoding tweaks) — just a documented order-of-magnitude check
//! plus a sanity ceiling so a regression that bloats the format loudly fails
//! CI instead of silently landing in the ADR's numbers.

use pawprint::history::History;
use pawprint::{EditDocument, StageEntry};
use serde_json::json;

fn realistic_document() -> EditDocument {
    let mut doc = EditDocument::default();
    doc.stages.insert(
        "white_balance".to_string(),
        StageEntry { schema_version: 1, params: json!({"temp": 5500, "tint": 10}) },
    );
    doc.stages.insert(
        "global".to_string(),
        StageEntry {
            schema_version: 1,
            params: json!({
                "exposure": 0.3, "contrast": 10, "highlights": -20, "shadows": 15,
                "whites": 5, "blacks": -5, "vibrance": 20, "saturation": 0,
            }),
        },
    );
    doc.stages.insert(
        "mask.subject_0".to_string(),
        StageEntry {
            schema_version: 1,
            params: json!({"model_id": "sam2-nicti", "model_version": "0.4.1", "exposure": 0.4, "clarity": 10}),
        },
    );
    doc.stages.insert(
        "mask.inverse_subject_0".to_string(),
        StageEntry {
            schema_version: 1,
            params: json!({"model_id": "sam2-nicti", "model_version": "0.4.1", "exposure": -0.2}),
        },
    );
    doc.stages.insert(
        "denoise".to_string(),
        StageEntry { schema_version: 1, params: json!({"model_id": "nafnet-nicti", "strength": 0.6}) },
    );
    doc
}

#[test]
fn per_photo_document_and_history_sizes() {
    let doc = realistic_document();
    let document_bytes = serde_json::to_vec(&doc).unwrap().len();

    let mut history = History::new(EditDocument::default());
    // A realistic-ish edit session: a handful of coarse steps, each of
    // which started life as a multi-tick slider drag that compaction
    // collapsed to one entry (proven separately in
    // history_and_compaction.rs), plus one named snapshot.
    for stage_id in ["white_balance", "global", "mask.subject_0", "mask.inverse_subject_0", "denoise"] {
        let entry = doc.stages[stage_id].clone();
        history.apply(stage_id, &format!("{stage_id}_edit"), entry);
    }
    history.snapshot("v1 edit");

    // Actually serialize the log rather than guessing a per-entry constant
    // — the snapshot entry alone carries a full document copy, which a hand
    // -picked estimate would badly undercount.
    let history_bytes = history.serialized_len();

    println!(
        "pawprint sizing: document={document_bytes}B, compacted-history(~{} entries)≈{history_bytes}B",
        history.len()
    );

    // Sanity ceilings, not tight assertions — catch a format regression
    // (e.g. someone switching to an uncompacted verbose encoding) without
    // hand-tuning an exact byte count into the test.
    assert!(document_bytes < 2_000, "a single edit document should stay well under 2KB");
    assert!(
        history_bytes < 10_000,
        "compacted per-photo history (a handful of steps + one snapshot) should stay well under 10KB"
    );
}
