//! Proves a stage id this build doesn't recognize (e.g. a Claw plugin
//! stage that isn't installed locally) round-trips byte-identically instead
//! of being silently dropped, since `StageEntry::params` is opaque `Value`.

use pawprint::{EditDocument, StageEntry};
use serde_json::json;

#[test]
fn unrecognized_plugin_stage_roundtrips_byte_identically() {
    let mut doc = EditDocument::default();
    doc.stages.insert(
        "vendor.mystery_stage".to_string(),
        StageEntry {
            schema_version: 7,
            params: json!({
                "some_future_field": [1, 2, 3],
                "nested": {"a": true, "b": null},
            }),
        },
    );
    doc.stages.insert(
        "white_balance".to_string(),
        StageEntry { schema_version: 1, params: json!({"temp": 5500}) },
    );

    let bytes = serde_json::to_vec(&doc).unwrap();
    let reloaded: EditDocument = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(doc, reloaded, "an unrecognized stage must survive a full save/load cycle unchanged");

    let bytes_again = serde_json::to_vec(&reloaded).unwrap();
    assert_eq!(bytes, bytes_again, "re-serializing an untouched unknown stage must be byte-identical");
}
