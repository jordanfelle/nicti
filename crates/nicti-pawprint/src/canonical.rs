//! Canonical-JSON + blake3 hashing, promoted from `spikes/loaf/src/hash.rs` (itself generalized
//! from `spikes/pawprint`'s original one-upstream-hash `hash_stage`/`cache_key`, and the identical
//! copies in `spikes/groom::spot` / `spikes/siamese::compose`) to an arbitrary number of upstream
//! hashes -- Tapetum's render graph (`crate::EditDocument` feeds `nicti_render::graph`) is a DAG,
//! not a flat per-stage list, so a node can have more than one upstream (e.g. the AI-mask bake
//! stage depends on both the neutral-render stage and its own recipe params).
//!
//! `-0.0` is normalized to `0.0`. Object keys are sorted explicitly (via a `BTreeMap` round-trip)
//! rather than relying on `serde_json::Map`'s default `BTreeMap`-backed ordering: that default
//! only holds while the `preserve_order` feature stays off everywhere in the workspace's
//! dependency graph, and Cargo feature unification means any crate anywhere enabling it (directly
//! or transitively) would silently switch every `serde_json::Map` in the process to
//! insertion-order, breaking this hash's stability without touching a single line here.
//!
//! **NaN/Infinity do NOT refuse to serialize** -- `serde_json::to_value`/`to_string`/`to_vec` all
//! silently convert a non-finite `f32`/`f64` to JSON `null` and return `Ok`, never `Err`. Left
//! unhandled, this means two stages with *different* non-finite params (or one non-finite param
//! vs. a genuinely absent/`None` field) could hash identically, defeating the whole point of a
//! cache key. Since this codebase's own convention never emits an explicit JSON `null` for a real
//! field (`Option` fields use `#[serde(skip_serializing_if = "Option::is_none")]` instead,
//! omitting the key entirely rather than writing `null`), any `null` appearing anywhere in a
//! canonicalized value is itself already anomalous -- `hash_value` returns `Err` rather than
//! trying to reconstruct which field's non-finite float produced it.

use serde::Serialize;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CanonicalError {
    #[error(
        "canonicalized value contains a JSON null -- either a genuine null (this codebase's \
         params never emit one; use #[serde(skip_serializing_if = \"Option::is_none\")] instead) \
         or a NaN/Infinity float serde_json silently converted to null. Refusing to hash rather \
         than risk two different params colliding on the same cache key."
    )]
    NonFinite,
}

/// Normalizes `-0.0` to `0.0` and, for every object, rebuilds it from a sorted `BTreeMap` so the
/// resulting insertion order is always sorted-by-key regardless of whether `serde_json`'s
/// `preserve_order` feature is enabled anywhere in the dependency graph.
fn canonicalize(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Number(n) => {
            if let Some(f) = n.as_f64() {
                if f == 0.0 {
                    *n = serde_json::Number::from_f64(0.0).expect("0.0 is finite");
                }
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(canonicalize),
        serde_json::Value::Object(map) => {
            let mut sorted: std::collections::BTreeMap<String, serde_json::Value> =
                std::mem::take(map).into_iter().collect();
            for v in sorted.values_mut() {
                canonicalize(v);
            }
            *map = sorted.into_iter().collect();
        }
        _ => {}
    }
}

/// True if `value` contains a JSON `null` anywhere (top-level or nested in an array/object) --
/// what `hash_value` uses to detect a non-finite float `serde_json` silently converted, since a
/// legitimate `null` shouldn't otherwise reach this point (see this module's doc comment).
fn contains_null(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => true,
        serde_json::Value::Array(items) => items.iter().any(contains_null),
        serde_json::Value::Object(map) => map.values().any(contains_null),
        _ => false,
    }
}

/// Canonical hash of any serializable value -- the "this stage's own params" half of a cache key.
///
/// Returns `Err(CanonicalError::NonFinite)` if the value contains (or, via a non-finite
/// `f32`/`f64` field, silently becomes) a JSON `null` anywhere -- see this module's doc comment
/// for why that's treated as a hard error rather than silently hashing it.
pub fn hash_value<T: Serialize>(value: &T) -> Result<blake3::Hash, CanonicalError> {
    let mut v = serde_json::to_value(value).expect("value always serializes to JSON");
    canonicalize(&mut v);
    if contains_null(&v) {
        return Err(CanonicalError::NonFinite);
    }
    let bytes = serde_json::to_vec(&v).expect("canonicalized JSON value always serializes");
    Ok(blake3::hash(&bytes))
}

/// Chains `own_hash` with every `upstream_hashes` entry, in the order given. This is Tapetum's
/// cache key for one graph node: change any upstream node's hash (its params, or transitively its
/// own upstream) and this key changes; change nothing upstream, and only `own_hash` changing
/// changes it. `upstream_hashes` is ordered (not a set) because two upstream edges swapped could
/// otherwise hash identically to two different actual graphs -- callers must feed a stable order
/// (e.g. sorted by upstream node id).
pub fn chain(upstream_hashes: &[blake3::Hash], own_hash: blake3::Hash) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    for h in upstream_hashes {
        hasher.update(h.as_bytes());
    }
    hasher.update(own_hash.as_bytes());
    hasher.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_value_is_stable_across_insertion_order() {
        let a = serde_json::json!({ "b": 2, "a": 1 });
        let b = serde_json::json!({ "a": 1, "b": 2 });
        assert_eq!(hash_value(&a).unwrap(), hash_value(&b).unwrap());
    }

    #[test]
    fn hash_value_normalizes_negative_zero() {
        let a = serde_json::json!({ "wb": -0.0 });
        let b = serde_json::json!({ "wb": 0.0 });
        assert_eq!(hash_value(&a).unwrap(), hash_value(&b).unwrap());
    }

    #[test]
    fn hash_value_sorts_nested_object_keys() {
        let a = serde_json::json!({ "outer": { "z": 1, "a": 2 } });
        let b = serde_json::json!({ "outer": { "a": 2, "z": 1 } });
        assert_eq!(hash_value(&a).unwrap(), hash_value(&b).unwrap());
    }

    /// Regression test for a CodeRabbit-found bug in the spike this was promoted from:
    /// `serde_json::to_value` silently converts a non-finite `f64` field to JSON `null` and
    /// returns `Ok`. `hash_value` must not silently hash a NaN-clobbered value as if it were an
    /// ordinary `null`/absent field.
    #[test]
    fn hash_value_errs_on_a_field_that_would_silently_become_null() {
        #[derive(Serialize)]
        struct Params {
            exposure_stops: f64,
        }
        let params = Params {
            exposure_stops: f64::NAN,
        };
        assert_eq!(hash_value(&params), Err(CanonicalError::NonFinite));
    }

    #[test]
    fn hash_value_errs_on_infinity_too() {
        #[derive(Serialize)]
        struct Params {
            exposure_stops: f64,
        }
        let params = Params {
            exposure_stops: f64::INFINITY,
        };
        assert_eq!(hash_value(&params), Err(CanonicalError::NonFinite));
    }

    #[test]
    fn hash_value_does_not_err_on_ordinary_finite_params() {
        let value = serde_json::json!({ "exposure_stops": 0.5, "wb": [1.0, 1.0, 0.9] });
        assert!(hash_value(&value).is_ok());
    }

    #[test]
    fn chain_is_order_sensitive() {
        let h1 = blake3::hash(b"one");
        let h2 = blake3::hash(b"two");
        let own = blake3::hash(b"own");
        assert_ne!(chain(&[h1, h2], own), chain(&[h2, h1], own));
    }

    #[test]
    fn chain_changes_when_any_upstream_changes() {
        let own = blake3::hash(b"own");
        let base = chain(&[blake3::hash(b"one")], own);
        let changed = chain(&[blake3::hash(b"one-changed")], own);
        assert_ne!(base, changed);
    }
}
