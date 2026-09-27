//! The mask-group model: an ordered composition of AI-recipe and vector-geometry sources, per
//! ADR-0002's "the recipe, not the pixels" rule (`docs/adr/0002-non-destructive-edit-model.md:117-123`).
//! Named `MaskGroup`/`MaskComponent` to match LRC's own `MaskGroupBasedCorrections` shape
//! (`spikes/shed/src/develop.rs`, `docs/adr/0023-lrc-catalog-import-mapping.md:93`) so #49/#62's
//! import maps onto it directly, not a differently-shaped Nicti-only structure.
//!
//! **Resolves two conflicts the `#48` research sweep found between `spikes/pawprint`'s spike data
//! and `spikes/groom`'s `MaskRecipe`:**
//!
//! 1. **`model_version` type**: `spikes/pawprint/tests/sizing.rs` uses a `String` (`"0.4.1"`),
//!    and `spikes/groom/src/spot.rs::MaskRecipe` (originally a `u32`) was aligned to match in #172
//!    -- a segmentation-model release is a semver-ish string upstream (BiRefNet/MobileSAM/SAM2
//!    tags aren't sequential integers), so this module's `AiRecipe` and groom's `MaskRecipe` both
//!    use `String` now.
//! 2. **The inverse-mask double-recipe problem**: `spikes/pawprint/tests/sizing.rs`'s
//!    `mask.inverse_subject_0` stores a *second, separate* `{model_id, model_version, ...}` recipe
//!    rather than referencing the subject mask it inverts -- which would bake the same model
//!    twice for the hero scenario's "Select Subject" + "Select Subject, Invert" pair. This module
//!    instead makes **inverse a property of the component** (`invert: bool`), not a second recipe:
//!    two components can carry the *same* `AiRecipe`, one with `invert: false` and one with
//!    `invert: true`, and `ai_bake_key()` is deliberately independent of `invert`/`opacity` so both
//!    components hash to the same bake key -- the model runs once, `1.0 - alpha` happens live in
//!    the shader (`gpu.rs::compose`). See `bake_keys_dedupe_the_shared_recipe_between_a_mask_and_its_inverse`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::geometry::Geometry;
use crate::image::Field;

/// `{model_id, model_version, params, seed?}` -- never the derived pixels, per ADR-0002. Matches
/// `spikes/groom/src/spot.rs::MaskRecipe`'s shape exactly, including `model_version: String`
/// (see conflict 1 above).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AiRecipe {
    pub model_id: String,
    pub model_version: String,
    pub params: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
}

/// Where one mask component's raw weight field comes from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MaskSource {
    Ai(AiRecipe),
    Geometry(Geometry),
}

/// How a component's weight combines with the running composite so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Add,
    Subtract,
    Intersect,
}

/// One component of a `MaskGroup`. `invert` flips the *source's* raw weight (`1.0 - w`) before
/// `op` combines it with the running composite -- this is where the hero scenario's "Select
/// Subject, Invert" is expressed for an `Ai` source (see this module's doc comment); a `Geometry`
/// source's own `invert` field (`geometry::LinearGradient`/`RadialGradient`) is a *second*,
/// independent invert of the rasterized gradient shape itself, not redundant with this one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MaskComponent {
    pub source: MaskSource,
    pub op: Op,
    #[serde(default)]
    pub invert: bool,
    pub opacity: f32,
}

/// The `params` payload a `"mask"` `StageEntry` (ADR-0002) would carry: an ordered list of
/// components, composed left to right.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MaskGroup {
    pub components: Vec<MaskComponent>,
}

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

fn canonical_bytes<T: Serialize>(value: &T) -> Vec<u8> {
    let mut v = serde_json::to_value(value).expect("value always serializes to JSON");
    canonicalize(&mut v);
    serde_json::to_vec(&v).expect("canonicalized JSON value always serializes")
}

/// The bake key for one `Ai` recipe: canonical hash of `{model_id, model_version, params, seed}`
/// **plus** the caller-supplied upstream-model-input hash (the fixed neutral render the model
/// actually ran against -- see `docs/adr/0024-masking.md`'s "AI model input is decoupled from tone
/// sliders" proposal). Deliberately **independent of `invert`/`opacity`**: those are applied live
/// after the bake, so two components sharing the same recipe (a mask and its inverse) get the same
/// bake key and the model runs once. Returns `None` for a `Geometry` source, which has no bake
/// step at all -- it rasterizes live every frame (`geometry::Geometry::rasterize`/`gpu::rasterize`).
pub fn ai_bake_key(
    source: &MaskSource,
    upstream_model_input_hash: blake3::Hash,
) -> Option<blake3::Hash> {
    match source {
        MaskSource::Ai(recipe) => {
            let mut hasher = blake3::Hasher::new();
            hasher.update(upstream_model_input_hash.as_bytes());
            hasher.update(&canonical_bytes(recipe));
            Some(hasher.finalize())
        }
        MaskSource::Geometry(_) => None,
    }
}

/// This stage's own canonical hash -- every component including `invert`/`opacity`/`op`, so any
/// visible change to the composite invalidates it, unlike `ai_bake_key` above.
pub fn hash_group(group: &MaskGroup) -> blake3::Hash {
    blake3::hash(&canonical_bytes(group))
}

/// The Tapetum-style cache key for this stage, mirroring `spikes/groom/src/spot.rs::cache_key`'s
/// upstream-hash-chaining pattern.
pub fn cache_key(group: &MaskGroup, upstream_hash: blake3::Hash) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(upstream_hash.as_bytes());
    hasher.update(hash_group(group).as_bytes());
    hasher.finalize()
}

/// A baked alpha `Field` a `lookup_baked_alpha` callback returned doesn't match `compose`'s own
/// `width`/`height` -- e.g. the caller handed `compose` a still-preview-resolution alpha instead
/// of refining it to full resolution first (`refine::guided_upsample`) before calling `compose`.
#[derive(Debug, thiserror::Error, PartialEq)]
#[error(
    "baked alpha is {got_width}x{got_height}, but compose() expected {expected_width}x{expected_height} \
     -- refine it to full resolution (e.g. via refine::guided_upsample) before calling compose()"
)]
pub struct AlphaResolutionMismatch {
    pub expected_width: usize,
    pub expected_height: usize,
    pub got_width: usize,
    pub got_height: usize,
}

/// Composes a `MaskGroup` into a single weight `Field`, given each `Ai` component's already-baked
/// alpha (looked up by `ai_bake_key`, so a mask and its inverse share one lookup) and each
/// `Geometry` component's own rasterized field (computed live -- see `geometry::Geometry::rasterize`).
/// CPU reference the `gpu` module's WGSL compose kernel is checked against.
///
/// **Deliberately does not silently resample a mismatched alpha** -- ADR-0024 requires the
/// preview-to-full-resolution path to go through a guided-filter refine (`refine.rs`), not an
/// implicit resample buried inside compose; a caller that skips that step gets a clear
/// [`AlphaResolutionMismatch`] error instead of `compose` guessing at what refinement to apply, or
/// (before this fix) an out-of-bounds panic from indexing a shorter `weight.data` at `out.data`'s
/// own length.
pub fn compose(
    group: &MaskGroup,
    width: usize,
    height: usize,
    upstream_model_input_hash: blake3::Hash,
    lookup_baked_alpha: impl Fn(blake3::Hash) -> Option<Field>,
) -> Result<Field, AlphaResolutionMismatch> {
    let mut out = Field::new(width, height, 0.0);
    for component in &group.components {
        let mut weight = match &component.source {
            MaskSource::Ai(_) => {
                let key = ai_bake_key(&component.source, upstream_model_input_hash)
                    .expect("Ai source always has a bake key");
                lookup_baked_alpha(key).unwrap_or_else(|| Field::new(width, height, 0.0))
            }
            MaskSource::Geometry(geometry) => geometry.rasterize(width, height),
        };
        if weight.width != width || weight.height != height {
            return Err(AlphaResolutionMismatch {
                expected_width: width,
                expected_height: height,
                got_width: weight.width,
                got_height: weight.height,
            });
        }
        if component.invert {
            for v in &mut weight.data {
                *v = 1.0 - *v;
            }
        }
        for v in &mut weight.data {
            *v *= component.opacity;
        }
        match component.op {
            Op::Add => {
                for i in 0..out.data.len() {
                    out.data[i] = (out.data[i] + weight.data[i]).min(1.0);
                }
            }
            Op::Subtract => {
                for i in 0..out.data.len() {
                    out.data[i] = (out.data[i] - weight.data[i]).max(0.0);
                }
            }
            Op::Intersect => {
                for i in 0..out.data.len() {
                    out.data[i] *= weight.data[i];
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{Dab, Stroke};
    use std::collections::HashMap;

    fn subject_recipe() -> AiRecipe {
        AiRecipe {
            model_id: "nicti.ai.birefnet".to_string(),
            model_version: "1.4.0".to_string(),
            params: serde_json::json!({ "target": "subject" }),
            seed: None,
        }
    }

    #[test]
    fn ai_bake_key_ignores_invert_and_opacity() {
        let upstream = blake3::hash(b"neutral-render-v1");
        let subject = MaskSource::Ai(subject_recipe());
        let key_a = ai_bake_key(&subject, upstream).unwrap();
        // Same recipe, would-be different component fields -- but ai_bake_key only takes the
        // source, so there's nothing else to vary; the real dedup proof is the group-level test
        // below.
        let key_b = ai_bake_key(&subject, upstream).unwrap();
        assert_eq!(key_a, key_b);
    }

    #[test]
    fn bake_keys_dedupe_the_shared_recipe_between_a_mask_and_its_inverse() {
        let upstream = blake3::hash(b"neutral-render-v1");
        let subject = MaskComponent {
            source: MaskSource::Ai(subject_recipe()),
            op: Op::Add,
            invert: false,
            opacity: 1.0,
        };
        let inverse = MaskComponent {
            source: MaskSource::Ai(subject_recipe()),
            op: Op::Add,
            invert: true,
            opacity: 1.0,
        };
        let key_subject = ai_bake_key(&subject.source, upstream).unwrap();
        let key_inverse = ai_bake_key(&inverse.source, upstream).unwrap();
        assert_eq!(
            key_subject, key_inverse,
            "a mask and its inverse must share one bake key so the model runs once"
        );

        let group = MaskGroup {
            components: vec![subject, inverse],
        };
        // But the whole-group hash (what actually gates re-composition) must still differ from a
        // group with only the subject component -- invert is a real, visible difference.
        let subject_only = MaskGroup {
            components: vec![MaskComponent {
                source: MaskSource::Ai(subject_recipe()),
                op: Op::Add,
                invert: false,
                opacity: 1.0,
            }],
        };
        assert_ne!(hash_group(&group), hash_group(&subject_only));
    }

    #[test]
    fn compose_inverts_the_same_baked_alpha_for_the_inverse_component() {
        let upstream = blake3::hash(b"neutral-render-v1");
        let recipe = subject_recipe();
        let key = ai_bake_key(&MaskSource::Ai(recipe.clone()), upstream).unwrap();

        let mut baked = Field::new(2, 1, 0.0);
        baked.data = vec![1.0, 0.25];
        let mut alphas = HashMap::new();
        alphas.insert(key, baked);

        let group = MaskGroup {
            components: vec![MaskComponent {
                source: MaskSource::Ai(recipe.clone()),
                op: Op::Add,
                invert: false,
                opacity: 1.0,
            }],
        };
        let subject_field = compose(&group, 2, 1, upstream, |k| alphas.get(&k).cloned())
            .expect("baked alpha matches compose's own resolution");

        let inverse_group = MaskGroup {
            components: vec![MaskComponent {
                source: MaskSource::Ai(recipe),
                op: Op::Add,
                invert: true,
                opacity: 1.0,
            }],
        };
        let inverse_field = compose(&inverse_group, 2, 1, upstream, |k| alphas.get(&k).cloned())
            .expect("baked alpha matches compose's own resolution");

        for i in 0..2 {
            assert!((subject_field.data[i] + inverse_field.data[i] - 1.0).abs() < 1e-6);
        }
    }

    #[test]
    fn geometry_component_bypasses_the_bake_lookup_entirely() {
        let group = MaskGroup {
            components: vec![MaskComponent {
                source: MaskSource::Geometry(Geometry::Brush(vec![Stroke {
                    dabs: vec![Dab {
                        center: (1.0, 1.0),
                        radius: 5.0,
                        feather: 0.0,
                        flow: 1.0,
                    }],
                    erase: false,
                }])),
                op: Op::Add,
                invert: false,
                opacity: 1.0,
            }],
        };
        let upstream = blake3::hash(b"unused");
        let field = compose(&group, 4, 4, upstream, |_| panic!("must not be called"))
            .expect("geometry-only group never looks up a baked alpha");
        assert!(field.get(1, 1) > 0.0);
    }

    #[test]
    fn subtract_and_intersect_ops_compose_as_expected() {
        let base = MaskComponent {
            source: MaskSource::Geometry(Geometry::Brush(vec![Stroke {
                dabs: vec![Dab {
                    center: (2.0, 2.0),
                    radius: 10.0,
                    feather: 0.0,
                    flow: 1.0,
                }],
                erase: false,
            }])),
            op: Op::Add,
            invert: false,
            opacity: 1.0,
        };
        let subtract_all = MaskComponent {
            source: MaskSource::Geometry(Geometry::Brush(vec![Stroke {
                dabs: vec![Dab {
                    center: (2.0, 2.0),
                    radius: 10.0,
                    feather: 0.0,
                    flow: 1.0,
                }],
                erase: false,
            }])),
            op: Op::Subtract,
            invert: false,
            opacity: 1.0,
        };
        let group = MaskGroup {
            components: vec![base, subtract_all],
        };
        let upstream = blake3::hash(b"unused");
        let field = compose(&group, 4, 4, upstream, |_| None)
            .expect("geometry-only group never looks up a baked alpha");
        assert!(field.data.iter().all(|&v| v.abs() < 1e-6));
    }

    #[test]
    fn model_version_is_a_string_not_an_integer() {
        // Regression guard for conflict 1: this must compile with a semver-ish string, which a
        // u32 field (spikes/groom's MaskRecipe) couldn't hold.
        let recipe = AiRecipe {
            model_id: "nicti.ai.mobilesam".to_string(),
            model_version: "0.4.1".to_string(),
            params: serde_json::json!({}),
            seed: Some(1),
        };
        assert_eq!(recipe.model_version, "0.4.1");
    }

    #[test]
    fn compose_rejects_a_mismatched_baked_alpha_resolution_instead_of_panicking() {
        let upstream = blake3::hash(b"neutral-render-v1");
        let recipe = subject_recipe();
        let key = ai_bake_key(&MaskSource::Ai(recipe.clone()), upstream).unwrap();
        // A still-preview-resolution alpha (4x3), while compose() is asked for the full 16x12
        // resolution -- exactly the "caller skipped refine::guided_upsample" case this guards.
        let mismatched = Field::new(4, 3, 1.0);
        let mut alphas = HashMap::new();
        alphas.insert(key, mismatched);

        let group = MaskGroup {
            components: vec![MaskComponent {
                source: MaskSource::Ai(recipe),
                op: Op::Add,
                invert: false,
                opacity: 1.0,
            }],
        };
        let err = compose(&group, 16, 12, upstream, |k| alphas.get(&k).cloned()).expect_err(
            "a resolution mismatch must be a clean Err, not an index-out-of-bounds panic",
        );
        assert_eq!(
            err,
            AlphaResolutionMismatch {
                expected_width: 16,
                expected_height: 12,
                got_width: 4,
                got_height: 3,
            }
        );
    }

    #[test]
    fn group_round_trips_through_json() {
        let group = MaskGroup {
            components: vec![MaskComponent {
                source: MaskSource::Ai(subject_recipe()),
                op: Op::Add,
                invert: false,
                opacity: 0.8,
            }],
        };
        let json = serde_json::to_string(&group).unwrap();
        let back: MaskGroup = serde_json::from_str(&json).unwrap();
        assert_eq!(group, back);
    }
}
