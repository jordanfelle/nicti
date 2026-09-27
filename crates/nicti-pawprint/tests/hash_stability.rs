//! Proves the ADR's hashing claims: canonical hashing is stable across object-key insertion order
//! and serialize/deserialize round-trips, and an isolated stage change only changes that stage's
//! hash, not an unrelated stage's. Promoted from `spikes/pawprint/tests/hash_stability.rs`; the
//! DAG cache-key chaining this file's own `cache_key_changes_when_upstream_hash_changes_but_not_
//! otherwise` test proved is now `nicti_render::graph::RenderGraph`'s job (its own tests cover the
//! same claim, generalized to more than one upstream), since `EditDocument`'s flat
//! single-upstream `cache_key` doesn't generalize to Tapetum's DAG and was dropped rather than
//! kept as a second, narrower cache-key implementation.

use nicti_pawprint::{apply_relative, hash_value, EditDocument, StageEntry};
use serde_json::json;

fn entry(schema_version: u32, params: serde_json::Value) -> StageEntry {
    StageEntry {
        schema_version,
        params,
    }
}

#[test]
fn hash_is_stable_across_key_insertion_order() {
    let a = entry(1, json!({"exposure": 0.3, "contrast": 10, "temp": 5500}));
    let b = entry(1, json!({"temp": 5500, "exposure": 0.3, "contrast": 10}));
    assert_eq!(hash_value(&a).unwrap(), hash_value(&b).unwrap());
}

#[test]
fn hash_is_stable_across_serialize_roundtrip() {
    let original = entry(1, json!({"exposure": 0.3, "contrast": 10}));
    let bytes = serde_json::to_vec(&original).unwrap();
    let reloaded: StageEntry = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        hash_value(&original).unwrap(),
        hash_value(&reloaded).unwrap()
    );
}

#[test]
fn relative_paste_integer_arithmetic_hashes_the_same_as_direct_construction() {
    // A logically identical edit (temp=5500) reached two different ways must hash identically --
    // the whole point of canonical hashing. `apply_relative` must produce a JSON int (5500) for
    // integer inputs, not a JSON float (5500.0), which would hash differently from a JSON int.
    let base = json!({"temp": 4000});
    let delta = json!({"temp": 1500});
    let via_relative_paste = entry(1, apply_relative(&base, &delta));
    let via_direct_construction = entry(1, json!({"temp": 5500}));

    assert_eq!(
        via_relative_paste.params,
        json!({"temp": 5500}),
        "integer + integer must stay an integer"
    );
    assert_eq!(
        hash_value(&via_relative_paste).unwrap(),
        hash_value(&via_direct_construction).unwrap()
    );
}

#[test]
fn negative_zero_hashes_the_same_as_positive_zero() {
    let a = entry(1, json!({"exposure": -0.0}));
    let b = entry(1, json!({"exposure": 0.0}));
    assert_eq!(hash_value(&a).unwrap(), hash_value(&b).unwrap());
}

#[test]
fn changing_one_stage_leaves_other_stages_hashes_untouched() {
    let mut doc = EditDocument::default();
    doc.stages
        .insert("white_balance".into(), entry(1, json!({"temp": 5500})));
    doc.stages
        .insert("crop".into(), entry(1, json!({"x": 0, "y": 0})));

    let crop_hash_before = doc.stage_hash("crop").unwrap().unwrap();

    doc.stages
        .insert("white_balance".into(), entry(1, json!({"temp": 6000})));

    assert_eq!(
        doc.stage_hash("crop").unwrap().unwrap(),
        crop_hash_before,
        "unrelated stage hash must not change"
    );
}
