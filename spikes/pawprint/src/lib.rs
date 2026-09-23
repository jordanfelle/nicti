//! Throwaway spike for #21 / `docs/adr/0002-non-destructive-edit-model.md`.
//!
//! Proves four claims the ADR makes: canonical per-stage hashing is stable
//! and isolated to the stage that changed, history compaction collapses a
//! slider-drag burst without losing the pre-drag undo target, bulk paste
//! records one history entry per photo instead of one per changed stage, and
//! a `nicti:` XMP packet round-trips a document losslessly. This is not a
//! production crate — the real crate layout is #20's decision.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod history;
pub mod xmp;

/// One pipeline stage's parameters. `params` is a raw JSON `Value` rather
/// than a typed struct so a stage this build doesn't know about (a plugin
/// not installed locally, or a newer schema version) round-trips untouched
/// instead of being silently dropped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageEntry {
    pub schema_version: u32,
    pub params: Value,
}

/// The full non-destructive edit for one asset variant: a fixed-order map of
/// stage id -> parameters. Render order is owned by the pipeline (#44), not
/// by this document — `stages` is a `BTreeMap` so serialization is
/// key-sorted (stable hashing), not to express a render sequence.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EditDocument {
    pub stages: BTreeMap<String, StageEntry>,
}

impl EditDocument {
    pub fn stage_hash(&self, stage_id: &str) -> Option<blake3::Hash> {
        self.stages.get(stage_id).map(hash_stage)
    }

    /// The Tapetum (#44) cache key for one stage: this stage's own hash
    /// chained with the caller-supplied upstream hashes, so a change to an
    /// upstream stage invalidates everything downstream of it without
    /// touching stages that don't depend on it.
    pub fn cache_key(
        &self,
        asset_identity: &[u8],
        upstream_hashes: &[blake3::Hash],
        stage_id: &str,
    ) -> Option<blake3::Hash> {
        let own = self.stage_hash(stage_id)?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(asset_identity);
        for h in upstream_hashes {
            hasher.update(h.as_bytes());
        }
        hasher.update(own.as_bytes());
        Some(hasher.finalize())
    }
}

/// Apply a relative paste (e.g. "+0.3 EV") onto a base params object: numeric
/// fields present in both add together, everything else in `delta`
/// overwrites the base field outright. Used for #52's relative bulk-sync
/// mode; absolute paste is just replacing `params` wholesale, no helper
/// needed.
pub fn apply_relative(base: &Value, delta: &Value) -> Value {
    match (base, delta) {
        (Value::Object(b), Value::Object(d)) => {
            let mut out = b.clone();
            for (k, dv) in d {
                let merged = match (out.get(k), dv) {
                    (Some(Value::Number(bn)), Value::Number(dn)) => {
                        let sum = bn.as_f64().unwrap_or(0.0) + dn.as_f64().unwrap_or(0.0);
                        serde_json::Number::from_f64(sum)
                            .map(Value::Number)
                            .unwrap_or_else(|| Value::Number(bn.clone()))
                    }
                    _ => dv.clone(),
                };
                out.insert(k.clone(), merged);
            }
            Value::Object(out)
        }
        (_, other) => other.clone(),
    }
}

/// Normalize -0.0 to 0.0 in place. NaN/Infinity can't reach here in the
/// first place: `serde_json` refuses to serialize them (returns an error)
/// as long as the `arbitrary_precision` feature stays off, which this
/// spike's Cargo.toml doesn't enable — see docs/adr/0002's serialization
/// appendix for the citation trail this relies on.
fn canonicalize(value: &mut Value) {
    match value {
        Value::Number(n) => {
            if let Some(f) = n.as_f64() {
                if f == 0.0 {
                    *n = serde_json::Number::from_f64(0.0).expect("0.0 is finite");
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(canonicalize),
        Value::Object(map) => map.values_mut().for_each(canonicalize),
        _ => {}
    }
}

/// Canonical bytes for one stage. `serde_json::Map` is backed by a
/// `BTreeMap` (sorted key order) as long as the `preserve_order` feature is
/// off, which it is here, so object keys always serialize in sorted order
/// regardless of insertion order.
fn canonical_bytes(stage: &StageEntry) -> Vec<u8> {
    let mut params = stage.params.clone();
    canonicalize(&mut params);
    let canon = StageEntry { schema_version: stage.schema_version, params };
    serde_json::to_vec(&canon).expect("canonical stage params always serialize")
}

pub fn hash_stage(stage: &StageEntry) -> blake3::Hash {
    blake3::hash(&canonical_bytes(stage))
}
