//! AI model provider extension point (ADR-0019 §7/§8) and, for masking (#49), the *segmentation*
//! provider layer built on it.
//!
//! `ModelProvider` settles identity and versioning via `Module`. [`SegmentationProvider`] adds what
//! a mask needs -- which targets a model can segment, its pinned version, its artifacts, and how to
//! load it into a [`Segmenter`] -- **without naming any inference backend**: the traits speak plain
//! data (`&[f32]` in, [`AlphaMap`] out), so this crate stays free of `ort` and a provider can wrap
//! ONNX Runtime, a different runtime, or a heuristic. That is what makes models pluggable: a mask's
//! recipe records `model_id` + `model_version` (ADR-0021's "the recipe, not the pixels"), and
//! [`resolve_provider`] finds the registered provider for exactly that pair. A newer or alternative
//! model is a new registration -- never a silent replacement of an old edit's model.
//!
//! Every `ModelProvider` implementation, first- or third-party, must follow
//! `docs/adr/0218-local-only-ai.md`: offline inference/training by default, no telemetry, no
//! hosted API; weight fetches are explicit, user-initiated, and checksum-verified, never a silent
//! auto-fetch; cloud AI is a separate opt-in feature, not a buried toggle.

pub mod models;

use std::path::Path;
use std::sync::Arc;

use nicti_claw::{Module, Registry};

/// An AI model provider (e.g. a masking model, a denoise model).
pub trait ModelProvider: Module {
    /// Human-readable name for the UI.
    fn label(&self) -> &str {
        self.id()
    }

    /// The downloadable artifacts this provider needs installed before [`SegmentationProvider::load`]
    /// can succeed (empty for a model with nothing to download, such as a heuristic).
    fn artifacts(&self) -> Vec<&'static models::Artifact> {
        Vec::new()
    }
}

/// Registry of AI model provider modules, keyed by namespaced id.
pub type ModelRegistry = Registry<dyn ModelProvider>;

/// What a segmentation model is asked to select.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SegmentTarget {
    /// The main subject (background is its inverse -- one model run serves both).
    Subject,
    /// The sky.
    Sky,
}

impl SegmentTarget {
    /// The name stored in a recipe's `params.target`.
    pub fn as_str(self) -> &'static str {
        match self {
            SegmentTarget::Subject => "subject",
            SegmentTarget::Sky => "sky",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "subject" => Some(SegmentTarget::Subject),
            "sky" => Some(SegmentTarget::Sky),
            _ => None,
        }
    }

    /// The target a recipe's `params` ask for, defaulting to [`SegmentTarget::Subject`] when absent.
    pub fn from_params(params: &serde_json::Value) -> Result<Self, SegmentError> {
        match params.get("target") {
            None | Some(serde_json::Value::Null) => Ok(SegmentTarget::Subject),
            Some(serde_json::Value::String(s)) => {
                Self::parse(s).ok_or_else(|| SegmentError::UnsupportedTarget(s.clone()))
            }
            Some(other) => Err(SegmentError::UnsupportedTarget(other.to_string())),
        }
    }
}

/// The image a model runs on: display-referred sRGB `0..=1`, interleaved `HWC`, the same fixed
/// neutral render every time (ADR-0048), so a tone slider can never change a model's output.
#[derive(Debug, Clone, Copy)]
pub struct ModelImage<'a> {
    pub width: usize,
    pub height: usize,
    pub rgb: &'a [f32],
    /// Identity of the photo, so a provider can cache per-image work (an embedding).
    pub image_key: u64,
}

impl ModelImage<'_> {
    /// Checks the buffer matches the stated size.
    pub fn validate(&self) -> Result<(), SegmentError> {
        if self.width == 0 || self.height == 0 || self.rgb.len() != self.width * self.height * 3 {
            return Err(SegmentError::BadInput(format!(
                "{}x{} image with {} floats",
                self.width,
                self.height,
                self.rgb.len()
            )));
        }
        Ok(())
    }
}

/// A model's alpha: one weight per pixel in `0..=1`, at whatever resolution the model produced.
#[derive(Debug, Clone, PartialEq)]
pub struct AlphaMap {
    pub width: usize,
    pub height: usize,
    pub alpha: Vec<f32>,
}

impl AlphaMap {
    /// `Err` for a zero-sized or mismatched buffer. Values are clamped to `0..=1` and NaN becomes
    /// 0: a model's output is untrusted input like any other.
    pub fn new(width: usize, height: usize, mut alpha: Vec<f32>) -> Result<Self, SegmentError> {
        if width == 0 || height == 0 || alpha.len() != width * height {
            return Err(SegmentError::BadInput(format!(
                "alpha {width}x{height} with {} values",
                alpha.len()
            )));
        }
        for v in &mut alpha {
            *v = if v.is_finite() {
                v.clamp(0.0, 1.0)
            } else {
                0.0
            };
        }
        Ok(Self {
            width,
            height,
            alpha,
        })
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SegmentError {
    /// No provider is registered under this id.
    #[error("no mask model is registered as `{0}`")]
    UnknownModel(String),
    /// A provider exists but not at the version the recipe pins. A newer model is never substituted
    /// silently: the edit keeps meaning what it meant when it was made.
    #[error(
        "mask model `{model_id}` is installed at version {installed}, but this edit pins {pinned}"
    )]
    VersionMismatch {
        model_id: String,
        pinned: String,
        installed: String,
    },
    #[error("mask model does not support target `{0}`")]
    UnsupportedTarget(String),
    #[error("the mask model is not installed: {0}")]
    NotInstalled(String),
    #[error("the mask model failed its integrity check: {0}")]
    Verify(String),
    #[error("could not load the mask model: {0}")]
    Load(String),
    #[error("mask inference failed: {0}")]
    Inference(String),
    #[error("bad input to the mask model: {0}")]
    BadInput(String),
}

/// A loaded model, ready to segment. `Send` so a Pounce worker thread can own it.
pub trait Segmenter: Send {
    fn segment(
        &mut self,
        image: &ModelImage,
        params: &serde_json::Value,
    ) -> Result<AlphaMap, SegmentError>;
}

/// What a provider needs to load itself.
pub struct LoadContext<'a> {
    pub store: &'a models::ModelStore,
    /// An explicit ONNX Runtime library (the `NICTI_ORT_DYLIB` override); `None` = the store's own.
    pub ort_dylib: Option<&'a Path>,
}

/// A registered segmentation model.
pub trait SegmentationProvider: ModelProvider {
    /// The version a recipe pins (`MaskRecipe.model_version`).
    fn model_version(&self) -> &str;

    /// What this model can segment.
    fn targets(&self) -> &[SegmentTarget];

    /// Loads the model. May be expensive (reads and verifies hundreds of MB): call it on a worker
    /// thread, once, and keep the [`Segmenter`].
    fn load(&self, ctx: &LoadContext) -> Result<Box<dyn Segmenter>, SegmentError>;
}

/// Registry of segmentation providers, keyed by namespaced id.
pub type SegmentationRegistry = Registry<dyn SegmentationProvider>;

/// Finds the provider a recipe names, checking it is at the pinned version and supports the target.
pub fn resolve_provider(
    registry: &SegmentationRegistry,
    model_id: &str,
    model_version: &str,
    target: SegmentTarget,
) -> Result<Arc<dyn SegmentationProvider>, SegmentError> {
    let provider = registry
        .get(model_id)
        .ok_or_else(|| SegmentError::UnknownModel(model_id.to_owned()))?;
    if provider.model_version() != model_version {
        return Err(SegmentError::VersionMismatch {
            model_id: model_id.to_owned(),
            pinned: model_version.to_owned(),
            installed: provider.model_version().to_owned(),
        });
    }
    if !provider.targets().contains(&target) {
        return Err(SegmentError::UnsupportedTarget(target.as_str().to_owned()));
    }
    Ok(provider)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_claw::Descriptor;
    use serde_json::Value;
    use std::sync::Arc;

    struct Dummy;

    impl Module for Dummy {
        fn id(&self) -> &str {
            "nicti.ai.dummy"
        }

        fn schema_version(&self) -> u32 {
            1
        }

        fn migrate_params(&self, _from_version: u32, params: Value) -> Option<Value> {
            Some(params)
        }
    }

    impl ModelProvider for Dummy {}

    fn make_dummy() -> Arc<dyn ModelProvider> {
        Arc::new(Dummy)
    }

    #[test]
    fn dummy_registers_and_resolves_as_trait_object() {
        let mut registry: ModelRegistry = Registry::new();
        registry
            .register(
                Descriptor {
                    id: "nicti.ai.dummy",
                    schema_version: 1,
                },
                make_dummy,
            )
            .expect("registration should succeed");

        let resolved = registry
            .get("nicti.ai.dummy")
            .expect("dummy model provider is registered");
        assert_eq!(resolved.id(), "nicti.ai.dummy");
    }

    struct FakeSegmenter;
    impl Segmenter for FakeSegmenter {
        fn segment(
            &mut self,
            image: &ModelImage,
            _params: &serde_json::Value,
        ) -> Result<AlphaMap, SegmentError> {
            image.validate()?;
            AlphaMap::new(
                image.width,
                image.height,
                vec![1.0; image.width * image.height],
            )
        }
    }

    struct FakeProvider {
        id: &'static str,
        version: &'static str,
        targets: Vec<SegmentTarget>,
    }
    impl Module for FakeProvider {
        fn id(&self) -> &str {
            self.id
        }
        fn schema_version(&self) -> u32 {
            1
        }
        fn migrate_params(&self, _from_version: u32, params: Value) -> Option<Value> {
            Some(params)
        }
    }
    impl ModelProvider for FakeProvider {}
    impl SegmentationProvider for FakeProvider {
        fn model_version(&self) -> &str {
            self.version
        }
        fn targets(&self) -> &[SegmentTarget] {
            &self.targets
        }
        fn load(&self, _ctx: &LoadContext) -> Result<Box<dyn Segmenter>, SegmentError> {
            Ok(Box::new(FakeSegmenter))
        }
    }

    fn registry() -> SegmentationRegistry {
        let mut r: SegmentationRegistry = Registry::new();
        r.register(
            Descriptor {
                id: "test.seg.a",
                schema_version: 1,
            },
            || {
                Arc::new(FakeProvider {
                    id: "test.seg.a",
                    version: "2",
                    targets: vec![SegmentTarget::Subject],
                })
            },
        )
        .unwrap();
        // A second model, to show a registry holds several and a newer one is just another entry.
        r.register(
            Descriptor {
                id: "test.seg.b",
                schema_version: 1,
            },
            || {
                Arc::new(FakeProvider {
                    id: "test.seg.b",
                    version: "1",
                    targets: vec![SegmentTarget::Subject, SegmentTarget::Sky],
                })
            },
        )
        .unwrap();
        r
    }

    #[test]
    fn resolves_the_exact_pinned_provider() {
        let r = registry();
        let p = resolve_provider(&r, "test.seg.a", "2", SegmentTarget::Subject).unwrap();
        assert_eq!(p.id(), "test.seg.a");
        let p = resolve_provider(&r, "test.seg.b", "1", SegmentTarget::Sky).unwrap();
        assert_eq!(p.id(), "test.seg.b");
    }

    #[test]
    fn an_unknown_model_a_wrong_version_and_an_unsupported_target_are_distinct_errors() {
        let r = registry();
        let err = |id, v, t| resolve_provider(&r, id, v, t).err().unwrap();
        assert_eq!(
            err("nope", "1", SegmentTarget::Subject),
            SegmentError::UnknownModel("nope".into())
        );
        // An edit pinned to version 1 of a model now at version 2 must NOT run the newer one.
        assert_eq!(
            err("test.seg.a", "1", SegmentTarget::Subject),
            SegmentError::VersionMismatch {
                model_id: "test.seg.a".into(),
                pinned: "1".into(),
                installed: "2".into()
            }
        );
        assert_eq!(
            err("test.seg.a", "2", SegmentTarget::Sky),
            SegmentError::UnsupportedTarget("sky".into())
        );
    }

    #[test]
    fn a_loaded_segmenter_produces_a_validated_alpha() {
        let r = registry();
        let p = resolve_provider(&r, "test.seg.a", "2", SegmentTarget::Subject).unwrap();
        let store = models::ModelStore::new(std::env::temp_dir().join("nicti-stalk-fake"));
        let mut seg = p
            .load(&LoadContext {
                store: &store,
                ort_dylib: None,
            })
            .unwrap();
        let rgb = vec![0.5f32; 4 * 3 * 3];
        let img = ModelImage {
            width: 4,
            height: 3,
            rgb: &rgb,
            image_key: 7,
        };
        let a = seg.segment(&img, &serde_json::json!({})).unwrap();
        assert_eq!((a.width, a.height, a.alpha.len()), (4, 3, 12));
        // A buffer of the wrong length is rejected, not indexed out of bounds.
        let bad = ModelImage {
            rgb: &rgb[..5],
            ..img
        };
        assert!(matches!(
            seg.segment(&bad, &serde_json::json!({})),
            Err(SegmentError::BadInput(_))
        ));
    }

    #[test]
    fn an_alpha_map_clamps_scrubs_and_rejects_mismatches() {
        let a = AlphaMap::new(2, 2, vec![-3.0, 0.5, f32::NAN, 9.0]).unwrap();
        assert_eq!(a.alpha, vec![0.0, 0.5, 0.0, 1.0]);
        assert!(AlphaMap::new(2, 2, vec![0.0; 3]).is_err());
        assert!(AlphaMap::new(0, 2, vec![]).is_err());
    }

    #[test]
    fn the_target_round_trips_and_defaults_to_subject() {
        for t in [SegmentTarget::Subject, SegmentTarget::Sky] {
            assert_eq!(SegmentTarget::parse(t.as_str()), Some(t));
        }
        assert_eq!(SegmentTarget::parse("water"), None);
        assert_eq!(
            SegmentTarget::from_params(&serde_json::json!({})),
            Ok(SegmentTarget::Subject)
        );
        assert_eq!(
            SegmentTarget::from_params(&serde_json::json!({ "target": "sky" })),
            Ok(SegmentTarget::Sky)
        );
        assert!(SegmentTarget::from_params(&serde_json::json!({ "target": "water" })).is_err());
        assert!(SegmentTarget::from_params(&serde_json::json!({ "target": 5 })).is_err());
    }
}
