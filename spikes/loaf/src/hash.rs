//! Canonical-JSON + blake3 hashing, generalized from `spikes/pawprint/src/lib.rs`'s
//! `hash_stage`/`cache_key` (and `spikes/groom/src/spot.rs`/`spikes/siamese/src/compose.rs`'s
//! identical one-upstream-hash copies of the same pattern) to an arbitrary number of upstream
//! hashes -- Tapetum's render graph is a DAG, not pawprint's flat per-stage list, so a node can
//! have more than one upstream (e.g. the AI-mask bake stage depends on both the neutral-render
//! stage and its own recipe params).
//!
//! `-0.0` is normalized to `0.0` and `serde_json`'s `preserve_order`/`arbitrary_precision`
//! features stay off (object keys sort) -- the exact citation trail for that part is in
//! ADR-0021; this module doesn't re-derive it, just reuses the same rule.
//!
//! **NaN/Infinity do NOT refuse to serialize** -- an earlier version of this doc comment claimed
//! they did (citing ADR-0021's own claim for pawprint's one-upstream case, taken at face value
//! rather than re-verified here); a CodeRabbit review of this PR found the opposite is true:
//! `serde_json::to_value`/`to_string`/`to_vec` all silently convert a non-finite `f32`/`f64` to
//! JSON `null` and return `Ok`, never `Err` -- confirmed empirically, not assumed. Left unhandled,
//! this means two stages with *different* non-finite params (or one non-finite param vs. a
//! genuinely absent/`None` field) could hash identically, defeating the whole point of a cache
//! key. Since this codebase's own convention never emits an explicit JSON `null` for a real field
//! (`Option` fields use `#[serde(skip_serializing_if = "Option::is_none")]` instead, omitting the
//! key entirely rather than writing `null`; see `spikes/siamese/src/compose.rs::AiRecipe.seed` for
//! the pattern this module's own callers are expected to follow), any `null` appearing anywhere in
//! a canonicalized value is itself already anomalous -- `hash_value` treats one as a hard error
//! rather than trying to reconstruct which field's non-finite float produced it (the information
//! needed to do that is already gone by the time `serde_json` has converted it to `null`; a fully
//! correct fix would intercept `serialize_f32`/`serialize_f64` before that conversion via a custom
//! `serde::Serializer` wrapper, which is real additional machinery a throwaway spike doesn't need
//! given how directly this codebase's params are already constructed as `serde_json::Value`s built
//! from finite float literals in practice).

use serde::Serialize;

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
        serde_json::Value::Object(map) => map.values_mut().for_each(canonicalize),
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
/// Panics if the value contains (or, via a non-finite `f32`/`f64` field, silently becomes) a JSON
/// `null` anywhere -- see this module's doc comment for why that's treated as a hard error rather
/// than silently hashing it.
pub fn hash_value<T: Serialize>(value: &T) -> blake3::Hash {
    let mut v = serde_json::to_value(value).expect("value always serializes to JSON");
    canonicalize(&mut v);
    assert!(
        !contains_null(&v),
        "hash_value: canonicalized params contain a JSON null -- either a genuine null (this \
         codebase's params never emit one; use #[serde(skip_serializing_if = \"Option::is_none\")] \
         instead) or a NaN/Infinity float serde_json silently converted to null. Refusing to hash \
         rather than risk two different params colliding on the same cache key."
    );
    let bytes = serde_json::to_vec(&v).expect("canonicalized JSON value always serializes");
    blake3::hash(&bytes)
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
        assert_eq!(hash_value(&a), hash_value(&b));
    }

    #[test]
    fn hash_value_normalizes_negative_zero() {
        let a = serde_json::json!({ "wb": -0.0 });
        let b = serde_json::json!({ "wb": 0.0 });
        assert_eq!(hash_value(&a), hash_value(&b));
    }

    /// Regression test for a CodeRabbit-found bug: `serde_json::to_value` silently converts a
    /// non-finite `f64` field to JSON `null` and returns `Ok`, contradicting an earlier version of
    /// this module's own doc comment (which claimed NaN/Infinity "refuse to serialize" -- verified
    /// empirically to be false, see the corrected doc comment above). `hash_value` must not
    /// silently hash a NaN-clobbered value as if it were an ordinary `null`/absent field.
    #[test]
    #[should_panic(expected = "canonicalized params contain a JSON null")]
    fn hash_value_panics_on_a_field_that_would_silently_become_null() {
        #[derive(Serialize)]
        struct Params {
            exposure_stops: f64,
        }
        let params = Params {
            exposure_stops: f64::NAN,
        };
        hash_value(&params);
    }

    #[test]
    #[should_panic(expected = "canonicalized params contain a JSON null")]
    fn hash_value_panics_on_infinity_too() {
        #[derive(Serialize)]
        struct Params {
            exposure_stops: f64,
        }
        let params = Params {
            exposure_stops: f64::INFINITY,
        };
        hash_value(&params);
    }

    #[test]
    fn hash_value_does_not_panic_on_ordinary_finite_params() {
        let value = serde_json::json!({ "exposure_stops": 0.5, "wb": [1.0, 1.0, 0.9] });
        // Must not panic.
        let _ = hash_value(&value);
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
