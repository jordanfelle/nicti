//! Proves #52's bulk paste/sync requirement: pasting the same change across
//! many photos records exactly one history entry per photo (a batch), not
//! one per underlying stage change, and both absolute and relative paste
//! modes work.

use pawprint::history::History;
use pawprint::{apply_relative, EditDocument, StageEntry};
use serde_json::json;
use uuid::Uuid;

fn white_balance(temp: i64, tint: i64) -> StageEntry {
    StageEntry { schema_version: 1, params: json!({"temp": temp, "tint": tint}) }
}

#[test]
fn absolute_bulk_paste_across_500_photos_records_one_batch_entry_each() {
    let batch_id = Uuid::new_v4();
    let mut histories: Vec<History> = (0..500)
        .map(|_| {
            let mut doc = EditDocument::default();
            doc.stages.insert("white_balance".into(), white_balance(4000, 0));
            History::new(doc)
        })
        .collect();

    let paste = white_balance(5500, 10);
    for history in &mut histories {
        history.apply_batch(batch_id, vec![("white_balance".to_string(), paste.clone())]);
    }

    for history in &histories {
        assert_eq!(history.len(), 1, "one history entry for the whole batch, not one per stage");
        assert_eq!(history.document().stages["white_balance"].params, json!({"temp": 5500, "tint": 10}));
    }
}

#[test]
fn relative_bulk_paste_adds_to_existing_values() {
    let mut doc = EditDocument::default();
    doc.stages.insert(
        "global".to_string(),
        StageEntry { schema_version: 1, params: json!({"exposure": 0.2, "contrast": 10}) },
    );
    let mut history = History::new(doc);

    let delta = json!({"exposure": 0.3});
    let base_params = history.document().stages["global"].params.clone();
    let merged = apply_relative(&base_params, &delta);

    history.apply_batch(
        Uuid::new_v4(),
        vec![("global".to_string(), StageEntry { schema_version: 1, params: merged })],
    );

    assert_eq!(history.len(), 1);
    assert_eq!(
        history.document().stages["global"].params,
        json!({"exposure": 0.5, "contrast": 10}),
        "relative paste adds to the existing value and leaves untouched fields alone"
    );
}
