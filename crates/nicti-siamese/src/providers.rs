//! The registered segmentation providers and the table that says which serves what.
//!
//! **Choosing a model is data, not code.** A mask's recipe records `model_id` + `model_version` +
//! `params.target` (ADR-0021). [`default_model_for`] is the one place that maps a *target* to the
//! model a *new* mask uses; a future model picker only has to write a different `model_id` into
//! the recipe, and a newer or better model is just one more `register` call here. Existing edits
//! keep resolving to the exact model and version they were made with.

use std::sync::Arc;

use nicti_claw::{Descriptor, Registry};
use nicti_stalk::{resolve_provider, SegmentError, SegmentTarget, SegmentationRegistry};
use nicti_tapetum::coat::MaskRecipe;

use crate::birefnet::{BiRefNetProvider, BIREFNET_ID, BIREFNET_VERSION};
use crate::sky::{SkyProvider, SKY_ID, SKY_VERSION};

/// Every segmentation provider this build ships.
pub fn segmentation_registry() -> SegmentationRegistry {
    let mut registry: SegmentationRegistry = Registry::new();
    registry
        .register(
            Descriptor {
                id: BIREFNET_ID,
                schema_version: 1,
            },
            || Arc::new(BiRefNetProvider),
        )
        .expect("the birefnet id is valid and unique");
    registry
        .register(
            Descriptor {
                id: SKY_ID,
                schema_version: 1,
            },
            || Arc::new(SkyProvider),
        )
        .expect("the sky id is valid and unique");
    registry
}

/// The model a *new* mask of `target` uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DefaultModel {
    pub model_id: &'static str,
    pub model_version: &'static str,
}

/// The default model for `target`: BiRefNet for the subject (background is its inverse), the
/// interim heuristic for sky.
pub fn default_model_for(target: SegmentTarget) -> DefaultModel {
    match target {
        SegmentTarget::Subject => DefaultModel {
            model_id: BIREFNET_ID,
            model_version: BIREFNET_VERSION,
        },
        SegmentTarget::Sky => DefaultModel {
            model_id: SKY_ID,
            model_version: SKY_VERSION,
        },
    }
}

/// The recipe a new AI mask of `target` stores.
pub fn recipe_for(target: SegmentTarget) -> MaskRecipe {
    let model = default_model_for(target);
    MaskRecipe {
        model_id: model.model_id.to_owned(),
        model_version: model.model_version.to_owned(),
        params: serde_json::json!({ "target": target.as_str() }),
        seed: None,
    }
}

/// One selectable model for a target, with what the picker shows before the user commits to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelChoice {
    pub model_id: String,
    pub model_version: String,
    pub label: String,
    /// Bytes to download for this model itself (0 for a heuristic). The shared ONNX Runtime is
    /// not counted.
    pub download_bytes: u64,
    /// The distinct licenses of the model's artifacts, comma-separated (empty = nothing to
    /// download).
    pub license: String,
}

/// Every registered provider that can segment `target`, the target's default model first and the
/// rest by id: what the Masks panel's model picker lists (#346). Choosing one only writes its
/// `model_id`/`model_version` into the new mask's recipe; existing masks keep theirs.
pub fn choices_for(registry: &SegmentationRegistry, target: SegmentTarget) -> Vec<ModelChoice> {
    let default_id = default_model_for(target).model_id;
    let mut choices: Vec<ModelChoice> = registry
        .descriptors()
        .into_iter()
        .filter_map(|d| registry.get(d.id))
        .filter(|p| p.targets().contains(&target))
        .map(|p| {
            let artifacts = p.artifacts();
            let mut licenses: Vec<&str> = artifacts.iter().map(|a| a.license).collect();
            licenses.sort_unstable();
            licenses.dedup();
            ModelChoice {
                model_id: p.id().to_owned(),
                model_version: p.model_version().to_owned(),
                label: p.label().to_owned(),
                download_bytes: artifacts.iter().map(|a| a.download_size).sum(),
                license: licenses.join(", "),
            }
        })
        .collect();
    choices.sort_by(|a, b| {
        (a.model_id != default_id, &a.model_id).cmp(&(b.model_id != default_id, &b.model_id))
    });
    choices
}

/// The recipe a new mask of `target` stores when the user picked `preferred`: that model if it is
/// registered and supports the target, otherwise (a stale preference, a model removed from this
/// build) the target's default. Never an unresolvable recipe.
pub fn recipe_for_preference(
    registry: &SegmentationRegistry,
    target: SegmentTarget,
    preferred: Option<&str>,
) -> MaskRecipe {
    let picked = preferred.and_then(|id| {
        let p = registry.get(id)?;
        p.targets().contains(&target).then(|| MaskRecipe {
            model_id: p.id().to_owned(),
            model_version: p.model_version().to_owned(),
            params: serde_json::json!({ "target": target.as_str() }),
            seed: None,
        })
    });
    picked.unwrap_or_else(|| recipe_for(target))
}

/// `(pinned, current)` when `recipe` names a registered model that now has a different version:
/// the one case where an existing mask offers "Update mask". A model that is unknown, or does not
/// support the recipe's target, is not an update.
pub fn stale_model(
    registry: &SegmentationRegistry,
    recipe: &MaskRecipe,
) -> Option<(String, String)> {
    let target = target_of(recipe).ok()?;
    // `resolve_provider` checks the version before the target, so a model that cannot serve this
    // target at all would still report a mismatch: only a model that can is worth updating.
    if !registry.get(&recipe.model_id)?.targets().contains(&target) {
        return None;
    }
    match resolve_provider(registry, &recipe.model_id, &recipe.model_version, target) {
        Err(SegmentError::VersionMismatch {
            pinned, installed, ..
        }) => Some((pinned, installed)),
        _ => None,
    }
}

/// `recipe` re-pinned to the registered version of its model, or `None` when it is not stale. The
/// caller applies it as an explicit edit; nothing swaps a pinned model silently.
pub fn updated_recipe(registry: &SegmentationRegistry, recipe: &MaskRecipe) -> Option<MaskRecipe> {
    let (_, current) = stale_model(registry, recipe)?;
    Some(MaskRecipe {
        model_version: current,
        ..recipe.clone()
    })
}

/// The target a stored recipe asks for.
pub fn target_of(recipe: &MaskRecipe) -> Result<SegmentTarget, SegmentError> {
    SegmentTarget::from_params(&recipe.params)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_stalk::resolve_provider;

    #[test]
    fn every_target_has_a_default_model_that_is_registered_at_that_version() {
        let registry = segmentation_registry();
        for target in [SegmentTarget::Subject, SegmentTarget::Sky] {
            let m = default_model_for(target);
            let p = resolve_provider(&registry, m.model_id, m.model_version, target)
                .unwrap_or_else(|e| panic!("{target:?}: {e}"));
            assert_eq!(p.id(), m.model_id);
        }
    }

    #[test]
    fn a_new_recipe_round_trips_its_target_and_carries_no_json_null() {
        for target in [SegmentTarget::Subject, SegmentTarget::Sky] {
            let r = recipe_for(target);
            assert_eq!(target_of(&r).unwrap(), target);
            // The canonical hasher (used for the bake key) refuses JSON null; a fresh recipe must
            // never carry one.
            assert!(!serde_json::to_string(&r).unwrap().contains("null"));
        }
    }

    #[test]
    fn the_two_targets_use_different_models_so_subject_and_sky_never_share_a_bake() {
        assert_ne!(
            default_model_for(SegmentTarget::Subject).model_id,
            default_model_for(SegmentTarget::Sky).model_id
        );
    }

    #[test]
    fn a_stale_pin_is_rejected_rather_than_silently_upgraded() {
        let registry = segmentation_registry();
        let err = resolve_provider(
            &registry,
            BIREFNET_ID,
            "an-older-export",
            SegmentTarget::Subject,
        )
        .err()
        .unwrap();
        assert!(
            matches!(err, SegmentError::VersionMismatch { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn a_target_the_model_cannot_serve_is_rejected() {
        let registry = segmentation_registry();
        let err = resolve_provider(&registry, BIREFNET_ID, BIREFNET_VERSION, SegmentTarget::Sky)
            .err()
            .unwrap();
        assert_eq!(err, SegmentError::UnsupportedTarget("sky".into()));
    }

    mod picker {
        use super::*;
        use nicti_claw::Module;
        use nicti_stalk::{LoadContext, ModelProvider, SegmentationProvider, Segmenter};

        /// A second, smaller subject model (test only: no second model ships yet).
        struct Small<const V: u8>;

        impl<const V: u8> Module for Small<V> {
            fn id(&self) -> &str {
                "test.ai.small"
            }
            fn schema_version(&self) -> u32 {
                1
            }
            fn migrate_params(
                &self,
                _from: u32,
                p: serde_json::Value,
            ) -> Option<serde_json::Value> {
                Some(p)
            }
        }
        impl<const V: u8> ModelProvider for Small<V> {
            fn label(&self) -> &str {
                "Small subject model"
            }
        }
        impl<const V: u8> SegmentationProvider for Small<V> {
            fn model_version(&self) -> &str {
                ["0", "1", "2", "3"][usize::from(V)]
            }
            fn targets(&self) -> &[SegmentTarget] {
                &[SegmentTarget::Subject]
            }
            fn load(&self, _: &LoadContext) -> Result<Box<dyn Segmenter>, SegmentError> {
                Err(SegmentError::Load("test".into()))
            }
        }

        fn registry_with_small(version: &'static str) -> SegmentationRegistry {
            let mut r = segmentation_registry();
            let descriptor = Descriptor {
                id: "test.ai.small",
                schema_version: 1,
            };
            match version {
                "1" => r.register(descriptor, || Arc::new(Small::<1>)),
                "2" => r.register(descriptor, || Arc::new(Small::<2>)),
                _ => r.register(descriptor, || Arc::new(Small::<3>)),
            }
            .unwrap();
            r
        }

        #[test]
        fn the_picker_lists_every_provider_for_a_target_default_first() {
            let one = choices_for(&segmentation_registry(), SegmentTarget::Subject);
            assert_eq!(one.len(), 1, "only BiRefNet ships for the subject today");
            assert_eq!(one[0].model_id, BIREFNET_ID);
            assert!(one[0].download_bytes > 100_000_000, "{one:?}");
            assert!(one[0].license.contains("MIT"), "{one:?}");
            let sky = choices_for(&segmentation_registry(), SegmentTarget::Sky);
            assert_eq!((sky.len(), sky[0].download_bytes), (1, 0), "{sky:?}");

            let r = registry_with_small("1");
            let two = choices_for(&r, SegmentTarget::Subject);
            let ids: Vec<&str> = two.iter().map(|c| c.model_id.as_str()).collect();
            assert_eq!(ids, [BIREFNET_ID, "test.ai.small"], "default first");
            assert_eq!(two[1].label, "Small subject model");
            // The small model does not segment the sky, so it is not offered there.
            assert_eq!(choices_for(&r, SegmentTarget::Sky).len(), 1);
        }

        #[test]
        fn a_preference_picks_that_model_and_a_stale_one_falls_back_to_the_default() {
            let r = registry_with_small("3");
            let t = SegmentTarget::Subject;
            let picked = recipe_for_preference(&r, t, Some("test.ai.small"));
            assert_eq!(
                (picked.model_id.as_str(), picked.model_version.as_str()),
                ("test.ai.small", "3")
            );
            assert_eq!(target_of(&picked).unwrap(), t);
            assert!(resolve_provider(&r, &picked.model_id, &picked.model_version, t).is_ok());
            // Unknown, wrong-target and absent preferences all give the default.
            assert_eq!(
                recipe_for_preference(&r, t, Some("gone.model")),
                recipe_for(t)
            );
            assert_eq!(
                recipe_for_preference(&r, SegmentTarget::Sky, Some("test.ai.small")),
                recipe_for(SegmentTarget::Sky)
            );
            assert_eq!(recipe_for_preference(&r, t, None), recipe_for(t));
        }

        #[test]
        fn a_newer_registered_version_offers_an_explicit_update_and_nothing_else_does() {
            let t = SegmentTarget::Subject;
            let old = recipe_for_preference(&registry_with_small("1"), t, Some("test.ai.small"));
            let r2 = registry_with_small("2");
            assert_eq!(
                stale_model(&r2, &old),
                Some(("1".to_owned(), "2".to_owned()))
            );
            let up = updated_recipe(&r2, &old).expect("stale");
            assert_eq!(up.model_version, "2");
            assert_eq!(
                (up.model_id.as_str(), &up.params),
                (old.model_id.as_str(), &old.params)
            );
            assert!(resolve_provider(&r2, &up.model_id, &up.model_version, t).is_ok());
            // Up to date, unknown model, unsupported target, unreadable target: not an update.
            assert!(stale_model(&r2, &up).is_none() && updated_recipe(&r2, &up).is_none());
            let unknown = MaskRecipe {
                model_id: "gone.model".into(),
                ..old.clone()
            };
            assert!(stale_model(&r2, &unknown).is_none());
            let wrong_target = MaskRecipe {
                params: serde_json::json!({ "target": "sky" }),
                ..old.clone()
            };
            assert!(stale_model(&r2, &wrong_target).is_none());
            let no_target = MaskRecipe {
                params: serde_json::json!({ "target": "banana" }),
                ..old
            };
            assert!(stale_model(&r2, &no_target).is_none());
        }
    }
}
