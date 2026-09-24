//! `HealStage` / `Spot`: the edit-model representation for clone/heal/remove operations, per
//! `docs/adr/0002-non-destructive-edit-model.md`'s `StageEntry { schema_version, params }`
//! pattern -- `HealStage` is the `params` shape a `"heal"` stage entry would carry, not a
//! replacement for `StageEntry` itself (that struct lives in the future `nicti-render`/catalog
//! crate, per ADR-0002/#22, not here).
//!
//! **Geometry choice:** each `Spot`'s destination is a **circle** (`center` + `radius`), not a
//! freehand brush path. A brush path (`Vec<(f32, f32)>`) is strictly more expressive, but a
//! circle is (a) enough to prove the cache-key/serialization-size questions this ticket actually
//! asks, (b) matches `cpu_reference`'s clone/heal implementations above, which are themselves
//! circle-based, and (c) keeps the size-comparison test's numbers meaningful rather than
//! dependent on an arbitrarily long path. If #51 needs freehand strokes, extending `Spot` with a
//! `Geometry` enum (`Circle { center, radius }` vs `Path(Vec<(f32, f32)>)`) is a compatible,
//! additive change under `schema_version`'s migration story (ADR-0002 §"Schema evolution").
//!
//! `cache_key()` follows `spikes/pawprint/src/lib.rs::cache_key`'s canonical-JSON + `blake3`
//! pattern exactly: chain the caller-supplied upstream hash with this stage's own canonical hash,
//! so changing a heal stage's spots only invalidates its own cache entry and everything
//! downstream of it, never anything upstream.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What a `Spot` does to the destination region.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpotKind {
    /// Hard(-ish) patch copy from `source_offset`, feathered at the edge (`cpu_reference::clone_stamp`).
    Clone,
    /// Gradient-domain (Poisson) blend from `source_offset` (`cpu_reference::spot_heal`).
    Heal,
    /// AI-driven object removal -- no `source_offset`, filled via `mask_recipe`'s model instead.
    Remove,
}

/// A model-based mask recipe (never derived pixels) for a `Remove` spot -- the "the recipe, not
/// the pixels" rule ADR-0002 already applies to AI masks generally. `model_version` pinning
/// means a future MobileSAM/LaMa upgrade is an explicit, opt-in re-run on old edits rather than a
/// silent quality change, exactly as ADR-0002 states for AI masks elsewhere.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MaskRecipe {
    pub model_id: String,
    pub model_version: u32,
    pub params: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
}

/// One clone/heal/remove operation. `center`/`radius` describe the destination circle;
/// `source_offset` (Clone/Heal only) is the vector from `center` to the source patch's own
/// center, matching `cpu_reference`'s `(offset_x, offset_y)` convention exactly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Spot {
    pub kind: SpotKind,
    pub center: (f32, f32),
    pub radius: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_offset: Option<(f32, f32)>,
    pub feather: f32,
    pub opacity: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mask_recipe: Option<MaskRecipe>,
}

impl Spot {
    pub fn clone_spot(
        center: (f32, f32),
        radius: f32,
        source_offset: (f32, f32),
        feather: f32,
    ) -> Self {
        Self {
            kind: SpotKind::Clone,
            center,
            radius,
            source_offset: Some(source_offset),
            feather,
            opacity: 1.0,
            mask_recipe: None,
        }
    }

    pub fn heal_spot(
        center: (f32, f32),
        radius: f32,
        source_offset: (f32, f32),
        feather: f32,
    ) -> Self {
        Self {
            kind: SpotKind::Heal,
            center,
            radius,
            source_offset: Some(source_offset),
            feather,
            opacity: 1.0,
            mask_recipe: None,
        }
    }

    pub fn remove_spot(
        center: (f32, f32),
        radius: f32,
        feather: f32,
        mask_recipe: MaskRecipe,
    ) -> Self {
        Self {
            kind: SpotKind::Remove,
            center,
            radius,
            source_offset: None,
            feather,
            opacity: 1.0,
            mask_recipe: Some(mask_recipe),
        }
    }
}

/// The `params` payload a `"heal"` `StageEntry` (ADR-0002) would carry: an ordered list of spots,
/// applied in list order (earlier spots first) -- order matters here in a way it deliberately
/// doesn't for ADR-0002's `stages` map, since two spots overlapping the same pixels give a
/// different result depending on which is applied last.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HealStage {
    pub spots: Vec<Spot>,
}

/// Normalize `-0.0 -> 0.0` in a JSON value tree -- identical rule to
/// `spikes/pawprint/src/lib.rs::canonicalize`, needed for the same reason: `blake3` hashes raw
/// bytes, and `-0.0`/`0.0` serialize to different bytes despite comparing equal as floats.
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

/// Canonical bytes for a `HealStage`: `serde_json` (default `Map`, `preserve_order` off, so keys
/// are already sorted) plus explicit float normalization -- the same two-part recipe
/// `docs/adr/0002-non-destructive-edit-model.md` settles on for `StageEntry` hashing generally.
fn canonical_bytes(stage: &HealStage) -> Vec<u8> {
    let mut value = serde_json::to_value(stage).expect("HealStage always serializes to JSON");
    canonicalize(&mut value);
    serde_json::to_vec(&value).expect("canonicalized JSON value always serializes")
}

/// This stage's own canonical hash, independent of anything upstream.
pub fn hash_stage(stage: &HealStage) -> blake3::Hash {
    blake3::hash(&canonical_bytes(stage))
}

/// The Tapetum-style cache key for this stage: chains the caller-supplied upstream hash with
/// this stage's own hash, mirroring `pawprint::EditDocument::cache_key`'s chaining rule exactly
/// (asset identity / prior stages -> this stage, so only this stage and everything downstream of
/// it is invalidated when spots change).
pub fn cache_key(stage: &HealStage, upstream_hash: blake3::Hash) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(upstream_hash.as_bytes());
    hasher.update(hash_stage(stage).as_bytes());
    hasher.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_clone(i: u32) -> Spot {
        Spot::clone_spot((100.0 + i as f32, 200.0 - i as f32), 8.5, (12.0, -4.0), 2.0)
    }

    fn sample_remove(i: u32) -> Spot {
        Spot::remove_spot(
            (50.0 + i as f32, 60.0),
            12.0,
            3.0,
            MaskRecipe {
                model_id: "nicti.ai.lama".to_string(),
                model_version: 1,
                params: serde_json::json!({ "prompt": "object", "index": i }),
                seed: Some(42),
            },
        )
    }

    #[test]
    fn spot_list_sizes_at_1_10_and_50() {
        for &n in &[1usize, 10, 50] {
            let spots: Vec<Spot> = (0..n as u32)
                .map(|i| {
                    if i % 2 == 0 {
                        sample_clone(i)
                    } else {
                        sample_remove(i)
                    }
                })
                .collect();
            let stage = HealStage { spots };
            let bytes = canonical_bytes(&stage);
            // Informal comparison against ADR-0002's ~563-byte/5-stage-document reference point
            // (see docs/adr/0007-healing-and-removal.md's "Measured results" for the actual
            // numbers this run produced) -- not a hard assertion on an exact byte count, since
            // that would make this test brittle to any field-naming change. The real assertion
            // is monotonicity: more spots must never serialize smaller.
            eprintln!(
                "HealStage with {n} spots serializes to {} bytes",
                bytes.len()
            );
            assert!(bytes.len() > n * 20, "suspiciously small for {n} spots");
        }
    }

    #[test]
    fn more_spots_never_serializes_smaller() {
        let sizes: Vec<usize> = [1usize, 10, 50]
            .iter()
            .map(|&n| {
                let spots: Vec<Spot> = (0..n as u32).map(sample_clone).collect();
                canonical_bytes(&HealStage { spots }).len()
            })
            .collect();
        assert!(sizes[0] < sizes[1]);
        assert!(sizes[1] < sizes[2]);
    }

    #[test]
    fn hash_is_stable_across_key_order_and_reserialization() {
        let stage = HealStage {
            spots: vec![sample_clone(0), sample_remove(1)],
        };
        let h1 = hash_stage(&stage);
        // Round-trip through JSON and back, then hash again -- must match exactly.
        let json = serde_json::to_string(&stage).unwrap();
        let stage2: HealStage = serde_json::from_str(&json).unwrap();
        let h2 = hash_stage(&stage2);
        assert_eq!(h1, h2);
    }

    #[test]
    fn negative_zero_hashes_the_same_as_positive_zero() {
        let mut a = sample_clone(0);
        a.center.0 = 0.0;
        let mut b = sample_clone(0);
        b.center.0 = -0.0;
        let stage_a = HealStage { spots: vec![a] };
        let stage_b = HealStage { spots: vec![b] };
        assert_eq!(hash_stage(&stage_a), hash_stage(&stage_b));
    }

    #[test]
    fn cache_key_changes_when_this_stage_changes_but_upstream_hash_does_not() {
        let upstream = blake3::hash(b"asset+upstream-stages");
        let stage_a = HealStage {
            spots: vec![sample_clone(0)],
        };
        let mut stage_b = stage_a.clone();
        stage_b.spots[0].radius += 1.0;

        let key_a = cache_key(&stage_a, upstream);
        let key_b = cache_key(&stage_b, upstream);
        assert_ne!(key_a, key_b);
    }

    #[test]
    fn cache_key_changes_when_upstream_hash_changes_but_stage_does_not() {
        let stage = HealStage {
            spots: vec![sample_clone(0)],
        };
        let key_1 = cache_key(&stage, blake3::hash(b"upstream-v1"));
        let key_2 = cache_key(&stage, blake3::hash(b"upstream-v2"));
        assert_ne!(key_1, key_2);
    }
}
