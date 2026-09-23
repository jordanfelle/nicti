//! Proves the ADR's hashing claims: canonical hashing is stable across
//! object-key insertion order and serialize/deserialize round-trips, and an
//! isolated stage change only changes that stage's hash and its downstream
//! cache-key chain, not unrelated stages.

use pawprint::{apply_relative, EditDocument, StageEntry};
use serde_json::json;

fn entry(schema_version: u32, params: serde_json::Value) -> StageEntry {
    StageEntry { schema_version, params }
}

#[test]
fn hash_is_stable_across_key_insertion_order() {
    let a = entry(1, json!({"exposure": 0.3, "contrast": 10, "temp": 5500}));
    let b = entry(1, json!({"temp": 5500, "exposure": 0.3, "contrast": 10}));
    assert_eq!(pawprint::hash_stage(&a), pawprint::hash_stage(&b));
}

#[test]
fn hash_is_stable_across_serialize_roundtrip() {
    let original = entry(1, json!({"exposure": 0.3, "contrast": 10}));
    let bytes = serde_json::to_vec(&original).unwrap();
    let reloaded: StageEntry = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(pawprint::hash_stage(&original), pawprint::hash_stage(&reloaded));
}

#[test]
fn relative_paste_integer_arithmetic_hashes_the_same_as_direct_construction() {
    // A logically identical edit (temp=5500) reached two different ways
    // must hash identically — the whole point of canonical hashing. Before
    // the fix, `apply_relative` always produced a JSON float (5500.0) even
    // for integer inputs, which hashed differently from a JSON int (5500).
    let base = json!({"temp": 4000});
    let delta = json!({"temp": 1500});
    let via_relative_paste = entry(1, apply_relative(&base, &delta));
    let via_direct_construction = entry(1, json!({"temp": 5500}));

    assert_eq!(via_relative_paste.params, json!({"temp": 5500}), "integer + integer must stay an integer");
    assert_eq!(pawprint::hash_stage(&via_relative_paste), pawprint::hash_stage(&via_direct_construction));
}

#[test]
fn negative_zero_hashes_the_same_as_positive_zero() {
    let a = entry(1, json!({"exposure": -0.0}));
    let b = entry(1, json!({"exposure": 0.0}));
    assert_eq!(pawprint::hash_stage(&a), pawprint::hash_stage(&b));
}

#[test]
fn changing_one_stage_leaves_other_stages_hashes_untouched() {
    let mut doc = EditDocument::default();
    doc.stages.insert("white_balance".into(), entry(1, json!({"temp": 5500})));
    doc.stages.insert("crop".into(), entry(1, json!({"x": 0, "y": 0})));

    let crop_hash_before = doc.stage_hash("crop").unwrap();

    doc.stages.insert("white_balance".into(), entry(1, json!({"temp": 6000})));

    assert_eq!(doc.stage_hash("crop").unwrap(), crop_hash_before, "unrelated stage hash must not change");
}

#[test]
fn cache_key_changes_when_upstream_hash_changes_but_not_otherwise() {
    let mut doc = EditDocument::default();
    doc.stages.insert("denoise".into(), entry(1, json!({"strength": 0.5})));

    let upstream_a = blake3::hash(b"upstream-a");
    let upstream_b = blake3::hash(b"upstream-b");
    let asset = b"asset-content-id";

    let key_a = doc.cache_key(asset, &[upstream_a], "denoise").unwrap();
    let key_a_again = doc.cache_key(asset, &[upstream_a], "denoise").unwrap();
    let key_b = doc.cache_key(asset, &[upstream_b], "denoise").unwrap();

    assert_eq!(key_a, key_a_again, "same inputs must hash identically");
    assert_ne!(key_a, key_b, "changing an upstream stage hash must invalidate the downstream cache key");
}
