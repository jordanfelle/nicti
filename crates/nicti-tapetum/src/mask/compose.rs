//! Combining a mask group's components, the AI bake key, and the document stamp that lets an
//! asynchronously arriving alpha invalidate exactly the corrections that use it.
//!
//! Fold semantics are amended from the research spike (ADR-0049): `Add` is a union (`max`), not
//! `min(acc + w, 1)`, because two overlapping feathered edges summed into a visible seam.

use std::collections::{BTreeMap, HashMap, HashSet};

use nicti_pawprint::EditDocument;
use serde_json::Value;

use super::params::{MaskComponent, MaskGroup, MaskParams, MaskSource, Op};
use super::Field;
use crate::coat::{self, MaskRecipe};
use crate::stages::MASKS;

/// One component's contribution to the running composite, per pixel: the raw weight is optionally
/// inverted (`1 - w`), scaled by `opacity`, then combined by `op`. This single function is the math
/// the GPU `mask_compose` kernel's CPU twin and every test share.
pub fn fold_step(acc: f32, weight: f32, invert: bool, opacity: f32, op: Op) -> f32 {
    let mut w = weight.clamp(0.0, 1.0);
    if invert {
        w = 1.0 - w;
    }
    w *= opacity;
    match op {
        Op::Add => acc.max(w),
        Op::Subtract => acc * (1.0 - w),
        Op::Intersect => acc * w,
    }
}

/// A component's field didn't match the extent being composed -- e.g. a preview-resolution alpha
/// that was never refined to the mask extent. Composing refuses to resample silently (ADR-0048).
#[derive(Debug, thiserror::Error, PartialEq)]
#[error(
    "component field is {got_width}x{got_height}, but compose() expected \
     {expected_width}x{expected_height} -- refine it to the mask extent first"
)]
pub struct ResolutionMismatch {
    pub expected_width: usize,
    pub expected_height: usize,
    pub got_width: usize,
    pub got_height: usize,
}

/// Composes `group` into one weight field at `width x height`. `source_field` supplies each
/// component's raw weights (a rasterized gradient, a refined AI alpha, a range selection);
/// returning `None` means "not available" -- a model that isn't installed or hasn't finished
/// baking -- and that component is **skipped**, contributing nothing (not even through `invert`),
/// so a missing model never turns into "select everything".
pub fn compose(
    group: &MaskGroup,
    width: usize,
    height: usize,
    mut source_field: impl FnMut(&MaskSource) -> Option<Field>,
) -> Result<Field, ResolutionMismatch> {
    let mut out = Field::new(width, height, 0.0);
    for MaskComponent {
        source,
        op,
        invert,
        opacity,
    } in &group.components
    {
        let Some(weights) = source_field(source) else {
            continue;
        };
        if weights.width != width || weights.height != height {
            return Err(ResolutionMismatch {
                expected_width: width,
                expected_height: height,
                got_width: weights.width,
                got_height: weights.height,
            });
        }
        for (acc, &w) in out.data.iter_mut().zip(&weights.data) {
            *acc = fold_step(*acc, w, *invert, *opacity, *op);
        }
    }
    Ok(out)
}

/// A stable hex identity for any serializable value. A free-form recipe `params` can hold a JSON
/// null (or a float that serializes as one), which the canonical hasher refuses; that must not be a
/// panic on every render, so fall back to hashing the JSON text -- still a stable identity.
pub(super) fn stable_hash<T: serde::Serialize>(value: &T) -> blake3::Hash {
    nicti_pawprint::hash_value(value).unwrap_or_else(|_| {
        blake3::hash(serde_json::to_string(value).unwrap_or_default().as_bytes())
    })
}

/// The bake key for one AI source: the neutral render it runs on plus its recipe. Deliberately
/// **independent of `invert`, `opacity` and `op`** -- those apply live after the bake, so "Select
/// Subject" and "Select Background" share one key and the model runs once. `None` for any
/// non-AI source (nothing to bake).
pub fn ai_bake_key(source: &MaskSource, neutral_key: blake3::Hash) -> Option<blake3::Hash> {
    let recipe = source.recipe()?;
    let mut h = blake3::Hasher::new();
    h.update(neutral_key.as_bytes());
    h.update(stable_hash(recipe).as_bytes());
    Some(h.finalize())
}

/// Hash of a whole group including every component's `invert`/`opacity`/`op`, so any visible
/// change recomposes -- unlike [`ai_bake_key`].
pub fn hash_group(group: &MaskGroup) -> blake3::Hash {
    stable_hash(group)
}

/// A model run the current document needs but doesn't have yet.
#[derive(Debug, Clone, PartialEq)]
pub struct BakeRequest {
    pub key: blake3::Hash,
    pub recipe: MaskRecipe,
}

/// Every distinct AI recipe the *active* corrections use, keyed by bake key, in first-use order.
/// A mask and its inverse (same recipe) yield one request.
pub fn bake_requests(params: &MaskParams, neutral_key: blake3::Hash) -> Vec<BakeRequest> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for correction in params.sanitized().active() {
        for component in &correction.mask.components {
            if let (Some(key), Some(recipe)) = (
                ai_bake_key(&component.source, neutral_key),
                component.source.recipe(),
            ) {
                if seen.insert(key) {
                    out.push(BakeRequest {
                        key,
                        recipe: recipe.clone(),
                    });
                }
            }
        }
    }
    out
}

/// Stamps which AI alphas are ready into the document's masks entry as `"ai_alphas": {bake key hex
/// -> alpha content hash hex}`. [`MaskParams`] ignores the unknown field when parsing, but
/// `apply_document` hashes the whole entry, so an alpha arriving (or being replaced) changes the
/// live composite's key through the normal path rather than a side channel. Only alphas an *active*
/// correction uses are stamped, so one arriving for a disabled mask can't thrash the cache. A
/// no-op when the document has no masks entry or nothing it uses is ready.
pub fn stamp_ai_alpha_state(
    doc: &mut EditDocument,
    neutral_key: blake3::Hash,
    ready: &HashMap<blake3::Hash, blake3::Hash>,
) {
    let Some(entry) = doc.stages.get_mut(MASKS) else {
        return;
    };
    let params: MaskParams = coat::parse(&entry.params);
    let stamped: BTreeMap<String, String> = bake_requests(&params, neutral_key)
        .into_iter()
        .filter_map(|r| {
            ready
                .get(&r.key)
                .map(|content| (r.key.to_hex().to_string(), content.to_hex().to_string()))
        })
        .collect();
    if stamped.is_empty() {
        return;
    }
    if let Value::Object(map) = &mut entry.params {
        map.insert(
            "ai_alphas".to_owned(),
            serde_json::to_value(stamped).expect("a string map serializes"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::super::params::{LocalAdjust, LocalCorrection, Stroke};
    use super::super::raster::rasterize_source;
    use super::*;
    use nicti_pawprint::StageEntry;
    use serde_json::json;

    fn recipe(target: &str) -> MaskRecipe {
        MaskRecipe {
            model_id: "nicti.mask.birefnet".into(),
            model_version: "1".into(),
            params: json!({ "target": target }),
            seed: None,
        }
    }

    fn component(source: MaskSource, op: Op, invert: bool, opacity: f32) -> MaskComponent {
        MaskComponent {
            source,
            op,
            invert,
            opacity,
        }
    }

    fn dab_at(x: f32, r: f32, erase: bool) -> MaskSource {
        MaskSource::Brush {
            strokes: vec![Stroke {
                points: vec![[x, 0.5]],
                radius: r,
                feather: 0.0,
                flow: 1.0,
                erase,
            }],
        }
    }

    fn geometry(group: &MaskGroup, w: usize, h: usize) -> Field {
        compose(group, w, h, |s| rasterize_source(s, w, h)).unwrap()
    }

    #[test]
    fn fold_step_matches_the_documented_math() {
        // Add is a union: two half-covers do not sum to a full one.
        assert_eq!(fold_step(0.5, 0.5, false, 1.0, Op::Add), 0.5);
        assert_eq!(fold_step(0.2, 0.7, false, 1.0, Op::Add), 0.7);
        assert!((fold_step(1.0, 0.25, false, 1.0, Op::Subtract) - 0.75).abs() < 1e-6);
        assert_eq!(fold_step(0.8, 0.5, false, 1.0, Op::Intersect), 0.4);
        // Invert then opacity, in that order.
        assert!((fold_step(0.0, 0.25, true, 0.5, Op::Add) - 0.375).abs() < 1e-6);
        // A wild weight is clamped, never amplified.
        assert_eq!(fold_step(0.0, 9.0, false, 1.0, Op::Add), 1.0);
    }

    #[test]
    fn overlapping_feathers_do_not_form_a_seam_under_add() {
        // Two identical half-weight fields: min(a + w, 1) would give 1.0 (the spike's seam).
        let a = 0.5;
        assert!(fold_step(a, 0.5, false, 1.0, Op::Add) < 0.51);
    }

    #[test]
    fn a_mask_and_its_inverse_partition_the_frame() {
        let disc = dab_at(0.5, 0.25, false);
        let subject = MaskGroup {
            components: vec![component(disc.clone(), Op::Add, false, 1.0)],
        };
        let background = MaskGroup {
            components: vec![component(disc, Op::Add, true, 1.0)],
        };
        let (s, b) = (geometry(&subject, 40, 40), geometry(&background, 40, 40));
        for (x, y) in s.data.iter().zip(&b.data) {
            assert!((x + y - 1.0).abs() < 1e-6);
        }
    }

    #[test]
    fn subtract_cuts_and_intersect_keeps_the_overlap() {
        let big = component(dab_at(0.5, 0.4, false), Op::Add, false, 1.0);
        let hole = component(dab_at(0.5, 0.1, false), Op::Subtract, false, 1.0);
        let cut = geometry(
            &MaskGroup {
                components: vec![big.clone(), hole],
            },
            50,
            50,
        );
        assert!(cut.get(25, 25) < 1e-6, "the centre was cut out");
        assert!(cut.get(25, 10) > 0.99, "the ring survived");

        let lens = component(dab_at(0.6, 0.4, false), Op::Intersect, false, 1.0);
        let inter = geometry(
            &MaskGroup {
                components: vec![big, lens],
            },
            50,
            50,
        );
        assert!(inter.get(25, 25) > 0.99, "inside both");
        assert!(inter.get(3, 25) < 1e-6, "inside only the first");
    }

    #[test]
    fn an_unavailable_source_is_skipped_even_when_inverted() {
        let ai = component(MaskSource::Ai(recipe("subject")), Op::Add, true, 1.0);
        let g = MaskGroup {
            components: vec![ai],
        };
        let f = compose(&g, 4, 4, |_| None).unwrap();
        assert!(
            f.data.iter().all(|&v| v == 0.0),
            "a missing model must never select everything"
        );
    }

    #[test]
    fn a_mismatched_alpha_is_a_clean_error_not_a_panic() {
        let g = MaskGroup {
            components: vec![component(
                MaskSource::Ai(recipe("subject")),
                Op::Add,
                false,
                1.0,
            )],
        };
        let err = compose(&g, 16, 12, |_| Some(Field::new(4, 3, 1.0))).unwrap_err();
        assert_eq!(
            err,
            ResolutionMismatch {
                expected_width: 16,
                expected_height: 12,
                got_width: 4,
                got_height: 3
            }
        );
    }

    #[test]
    fn the_bake_key_ignores_invert_opacity_and_op_but_not_the_neutral_render_or_recipe() {
        let neutral = blake3::hash(b"neutral-render-1");
        let subject = MaskSource::Ai(recipe("subject"));
        let key = ai_bake_key(&subject, neutral).unwrap();
        // The key only takes the source, so invert/opacity/op cannot reach it; prove it through
        // the group-level requests instead: a mask and its inverse request one bake.
        let params = MaskParams {
            corrections: vec![correction(vec![
                component(subject.clone(), Op::Add, false, 1.0),
                component(subject.clone(), Op::Subtract, true, 0.3),
            ])],
        };
        let reqs = bake_requests(&params, neutral);
        assert_eq!(reqs.len(), 1, "subject + inverse share one model run");
        assert_eq!(reqs[0].key, key);

        assert_ne!(
            key,
            ai_bake_key(&subject, blake3::hash(b"neutral-render-2")).unwrap(),
            "a different neutral render is a different bake"
        );
        assert_ne!(
            key,
            ai_bake_key(&MaskSource::Ai(recipe("sky")), neutral).unwrap(),
            "a different recipe is a different bake"
        );
        assert!(ai_bake_key(&dab_at(0.5, 0.1, false), neutral).is_none());
    }

    fn correction(components: Vec<MaskComponent>) -> LocalCorrection {
        LocalCorrection {
            id: "c".into(),
            mask: MaskGroup { components },
            adjust: LocalAdjust {
                exposure: 1.0,
                ..LocalAdjust::default()
            },
            ..LocalCorrection::default()
        }
    }

    #[test]
    fn a_recipe_with_a_json_null_still_gets_a_stable_key() {
        let mut r = recipe("subject");
        r.params = json!({ "target": null });
        let s = MaskSource::Ai(r);
        let neutral = blake3::hash(b"n");
        assert_eq!(ai_bake_key(&s, neutral), ai_bake_key(&s, neutral));
    }

    #[test]
    fn the_group_hash_covers_every_visible_field() {
        let base = MaskGroup {
            components: vec![component(
                MaskSource::Ai(recipe("subject")),
                Op::Add,
                false,
                1.0,
            )],
        };
        let mut inverted = base.clone();
        inverted.components[0].invert = true;
        let mut faded = base.clone();
        faded.components[0].opacity = 0.5;
        let mut op = base.clone();
        op.components[0].op = Op::Intersect;
        let h = hash_group(&base);
        assert_ne!(h, hash_group(&inverted));
        assert_ne!(h, hash_group(&faded));
        assert_ne!(h, hash_group(&op));
    }

    fn doc_with(params: &MaskParams) -> EditDocument {
        let mut doc = EditDocument::default();
        doc.stages.insert(
            MASKS.to_owned(),
            StageEntry {
                schema_version: 1,
                params: serde_json::to_value(params).unwrap(),
            },
        );
        doc
    }

    #[test]
    fn stamping_marks_only_alphas_an_active_correction_uses() {
        let neutral = blake3::hash(b"neutral");
        let subject = MaskSource::Ai(recipe("subject"));
        let sky = MaskSource::Ai(recipe("sky"));
        let mut disabled = correction(vec![component(sky.clone(), Op::Add, false, 1.0)]);
        disabled.enabled = false;
        let params = MaskParams {
            corrections: vec![
                correction(vec![component(subject.clone(), Op::Add, false, 1.0)]),
                disabled,
            ],
        };
        let key_subject = ai_bake_key(&subject, neutral).unwrap();
        let key_sky = ai_bake_key(&sky, neutral).unwrap();
        let ready = HashMap::from([
            (key_subject, blake3::hash(b"alpha-1")),
            (key_sky, blake3::hash(b"alpha-2")),
        ]);

        let mut doc = doc_with(&params);
        stamp_ai_alpha_state(&mut doc, neutral, &ready);
        let stamped = doc.stages[MASKS].params["ai_alphas"].as_object().unwrap();
        assert_eq!(stamped.len(), 1, "the disabled mask's alpha is not stamped");
        assert!(stamped.contains_key(&key_subject.to_hex().to_string()));
        // The stamped entry still parses as plain params and still hashes.
        let back: MaskParams = coat::parse(&doc.stages[MASKS].params);
        assert_eq!(back, params);
        nicti_pawprint::hash_value(&doc.stages[MASKS]).unwrap();
    }

    #[test]
    fn a_new_alpha_changes_the_entry_hash_and_nothing_ready_changes_nothing() {
        let neutral = blake3::hash(b"neutral");
        let subject = MaskSource::Ai(recipe("subject"));
        let params = MaskParams {
            corrections: vec![correction(vec![component(
                subject.clone(),
                Op::Add,
                false,
                1.0,
            )])],
        };
        let key = ai_bake_key(&subject, neutral).unwrap();
        let hash_with = |ready: &HashMap<blake3::Hash, blake3::Hash>| {
            let mut doc = doc_with(&params);
            stamp_ai_alpha_state(&mut doc, neutral, ready);
            nicti_pawprint::hash_value(&doc.stages[MASKS]).unwrap()
        };
        let none = hash_with(&HashMap::new());
        assert_eq!(none, hash_with(&HashMap::new()));
        let first = hash_with(&HashMap::from([(key, blake3::hash(b"a"))]));
        let second = hash_with(&HashMap::from([(key, blake3::hash(b"b"))]));
        assert_ne!(none, first, "an alpha arriving must rebake the composite");
        assert_ne!(first, second, "a replaced alpha must rebake it too");
    }

    #[test]
    fn stamping_a_document_with_no_masks_entry_is_a_no_op() {
        let mut doc = EditDocument::default();
        stamp_ai_alpha_state(&mut doc, blake3::hash(b"n"), &HashMap::new());
        assert!(doc.stages.is_empty());
    }
}
