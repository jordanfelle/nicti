//! The registered segmentation providers and the table that says which serves what.
//!
//! **Choosing a model is data, not code.** A mask's recipe records `model_id` + `model_version` +
//! `params.target` (ADR-0021). [`default_model_for`] is the one place that maps a *target* to the
//! model a *new* mask uses; a future model picker only has to write a different `model_id` into
//! the recipe, and a newer or better model is just one more `register` call here. Existing edits
//! keep resolving to the exact model and version they were made with.

use std::sync::Arc;

use nicti_claw::{Descriptor, Registry};
use nicti_stalk::{SegmentError, SegmentTarget, SegmentationRegistry};
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
}
