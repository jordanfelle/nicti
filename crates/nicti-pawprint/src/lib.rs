//! Non-destructive edit document (ADR-0021, #21): a fixed-order map of stage id -> parameters,
//! hashed canonically so identical edits always produce the same Tapetum (#44) cache key.
//! Promoted from `spikes/pawprint`; `canonical` additionally absorbs `spikes/loaf/src/hash.rs`'s
//! DAG-generalized `chain`, since both are the same canonical-hashing scheme.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod canonical;
pub mod history;

pub use canonical::{chain, hash_value, CanonicalError};

/// One pipeline stage's parameters. `params` is a raw JSON `Value` rather than a typed struct so
/// a stage this build doesn't know about (a plugin not installed locally, or a newer schema
/// version) round-trips untouched instead of being silently dropped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageEntry {
    pub schema_version: u32,
    pub params: Value,
}

/// The full non-destructive edit for one asset variant: a fixed-order map of stage id ->
/// parameters. Render order is owned by the pipeline (#44/#45), not by this document -- `stages`
/// is a `BTreeMap` so serialization is key-sorted (stable hashing), not to express a render
/// sequence.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EditDocument {
    pub stages: BTreeMap<String, StageEntry>,
}

impl EditDocument {
    /// One stage's canonical hash, or `None` if the document has no entry for it.
    pub fn stage_hash(&self, stage_id: &str) -> Option<Result<blake3::Hash, CanonicalError>> {
        self.stages.get(stage_id).map(hash_value)
    }

    /// Whole-document hash: chains every stage's canonical hash, in the `BTreeMap`'s
    /// already-sorted stage-id order, so it's stable the same way `stage_hash` is.
    pub fn content_hash(&self) -> Result<blake3::Hash, CanonicalError> {
        let mut hasher = blake3::Hasher::new();
        for (stage_id, stage) in &self.stages {
            hasher.update(stage_id.as_bytes());
            hasher.update(hash_value(stage)?.as_bytes());
        }
        Ok(hasher.finalize())
    }
}

/// Apply a relative paste (e.g. "+0.3 EV") onto a base params object: numeric fields present in
/// both add together, everything else in `delta` overwrites the base field outright. Used for
/// #52's relative bulk-sync mode; absolute paste is just replacing `params` wholesale, no helper
/// needed.
///
/// Integer + integer stays an integer (checked, falling back to float only on overflow or if
/// either side wasn't representable as an integer in the first place). This matters beyond
/// cosmetics: `hash_value` hashes the canonical JSON bytes, and a JSON int and a JSON float
/// holding the same numeric value serialize differently (`5500` vs `5500.0`) and therefore hash
/// differently -- without this, two edits that are logically identical (reached via
/// relative-paste arithmetic vs. any other path) would get different Tapetum cache keys, defeating
/// the whole point of canonical hashing.
pub fn apply_relative(base: &Value, delta: &Value) -> Value {
    match (base, delta) {
        (Value::Object(b), Value::Object(d)) => {
            let mut out = b.clone();
            for (k, dv) in d {
                let merged = match (out.get(k), dv) {
                    (Some(Value::Number(bn)), Value::Number(dn)) => add_numbers(bn, dn),
                    _ => dv.clone(),
                };
                out.insert(k.clone(), merged);
            }
            Value::Object(out)
        }
        (_, other) => other.clone(),
    }
}

fn add_numbers(a: &serde_json::Number, b: &serde_json::Number) -> Value {
    if let (Some(ai), Some(bi)) = (a.as_i64(), b.as_i64()) {
        if let Some(sum) = ai.checked_add(bi) {
            return Value::Number(serde_json::Number::from(sum));
        }
    }
    let sum = a.as_f64().unwrap_or(0.0) + b.as_f64().unwrap_or(0.0);
    serde_json::Number::from_f64(sum)
        .map(Value::Number)
        .unwrap_or_else(|| Value::Number(a.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(params: Value) -> StageEntry {
        StageEntry {
            schema_version: 1,
            params,
        }
    }

    #[test]
    fn empty_document_serializes_to_the_documented_shape() {
        let doc = EditDocument::default();
        assert_eq!(serde_json::to_string(&doc).unwrap(), r#"{"stages":{}}"#);
    }

    #[test]
    fn content_hash_is_stable_and_changes_with_any_stage() {
        let mut a = EditDocument::default();
        a.stages.insert(
            "nicti.wb".to_string(),
            entry(serde_json::json!({"temp": 5500})),
        );
        let mut b = a.clone();
        assert_eq!(a.content_hash().unwrap(), b.content_hash().unwrap());

        b.stages.insert(
            "nicti.wb".to_string(),
            entry(serde_json::json!({"temp": 5600})),
        );
        assert_ne!(a.content_hash().unwrap(), b.content_hash().unwrap());
    }

    #[test]
    fn apply_relative_adds_numeric_fields_and_keeps_integer_shape() {
        let base = serde_json::json!({"exposure_stops": 5500});
        let delta = serde_json::json!({"exposure_stops": 100});
        let merged = apply_relative(&base, &delta);
        assert_eq!(merged, serde_json::json!({"exposure_stops": 5600}));
    }

    #[test]
    fn apply_relative_overwrites_non_numeric_fields() {
        let base = serde_json::json!({"mode": "auto"});
        let delta = serde_json::json!({"mode": "manual"});
        assert_eq!(
            apply_relative(&base, &delta),
            serde_json::json!({"mode": "manual"})
        );
    }
}
