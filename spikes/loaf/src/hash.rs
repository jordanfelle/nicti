//! Canonical-JSON + blake3 hashing, generalized from `spikes/pawprint/src/lib.rs`'s
//! `hash_stage`/`cache_key` (and `spikes/groom/src/spot.rs`/`spikes/siamese/src/compose.rs`'s
//! identical one-upstream-hash copies of the same pattern) to an arbitrary number of upstream
//! hashes -- Tapetum's render graph is a DAG, not pawprint's flat per-stage list, so a node can
//! have more than one upstream (e.g. the AI-mask bake stage depends on both the neutral-render
//! stage and its own recipe params).
//!
//! `-0.0` is normalized to `0.0` and `serde_json`'s `preserve_order`/`arbitrary_precision`
//! features stay off (object keys sort, NaN/Infinity refuse to serialize) -- the exact citation
//! trail is in ADR-0002; this module doesn't re-derive it, just reuses the same rule.

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

/// Canonical hash of any serializable value -- the "this stage's own params" half of a cache key.
pub fn hash_value<T: Serialize>(value: &T) -> blake3::Hash {
    let mut v = serde_json::to_value(value).expect("value always serializes to JSON");
    canonicalize(&mut v);
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
