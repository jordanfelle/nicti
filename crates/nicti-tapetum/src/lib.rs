//! Render stage extension point (ADR-0019 §7/§8) and Tapetum's (#44/#45) stage-cached render
//! engine. `graph`/`cache`/`prefetch` are the CPU-only render graph, promoted from `spikes/loaf`
//! (#45's first slice). `gpu`/`frame` are the shared wgpu device and GPU-resident RGBA16F frame
//! textures; `renderer` is the real `RenderStage` execution signature and the graph-driven
//! `Renderer` that dispatches against it (#45's second slice). `color`/`geometry`/`stages` are the
//! concrete decode/live-suffix/geometry pipeline wired to a real `nicti_cornea::LinearFrame`
//! (#45's third slice) -- denoise/lens/heal stay passthrough slots for their own tickets.

pub mod autolevel;
pub mod cache;
pub mod coat;
pub mod color;
pub mod detail;
pub mod frame;
pub mod geometry;
pub mod gpu;
pub mod graph;
pub mod heal;
pub mod histogram;
pub mod perk;
pub mod prefetch;
pub mod renderer;
pub mod stages;
pub mod tile;

#[cfg(test)]
pub(crate) mod test_util;

use nicti_claw::{Module, Registry};
use nicti_pawprint::{CanonicalError, StageEntry};

pub use renderer::{BakedExec, GeometryExec, LiveExec, RenderError, RenderStats, Renderer};

/// A render stage (e.g. white balance, a local-adjustment mask, an AI denoise stage).
pub trait RenderStage: Module {
    /// Baked (cached, invalidated by upstream), Live (recomputed every frame) or Geometry (a
    /// per-frame transform with no upstream data dependency) -- reuses Tapetum's own
    /// [`graph::StageKind`] rather than inventing a parallel classification.
    fn kind(&self) -> graph::StageKind;

    /// Default parameters for a fresh `StageEntry` when a document has no explicit entry for
    /// this stage yet.
    fn default_params(&self) -> serde_json::Value;

    /// This implementation's own version, independent of `schema_version` -- bump it when the
    /// stage's algorithm/shader changes in a way that must invalidate cached output even though
    /// the params schema (and therefore every existing `StageEntry`) didn't change. Defaults to
    /// 0 for a stage that has never needed this.
    fn impl_version(&self) -> u32 {
        0
    }

    /// This stage's contribution to its own `graph::StageNode::own_hash` -- the id (so two
    /// different stages with coincidentally identical params never collide), `impl_version`, and
    /// the entry's own canonical hash, all chained together.
    fn cache_contribution(&self, entry: &StageEntry) -> Result<blake3::Hash, CanonicalError> {
        nicti_pawprint::hash_value(&(self.id(), self.impl_version(), entry))
    }
}

/// Registry of render stage modules, keyed by namespaced id (ADR-0021's `vendor.stage_name`
/// convention).
pub type StageRegistry = Registry<dyn RenderStage>;

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_claw::Descriptor;
    use serde_json::Value;
    use std::sync::Arc;

    struct Dummy;

    impl Module for Dummy {
        fn id(&self) -> &str {
            "nicti.stage.dummy"
        }

        fn schema_version(&self) -> u32 {
            1
        }

        fn migrate_params(&self, _from_version: u32, params: Value) -> Option<Value> {
            Some(params)
        }
    }

    impl RenderStage for Dummy {
        fn kind(&self) -> graph::StageKind {
            graph::StageKind::Baked
        }

        fn default_params(&self) -> Value {
            Value::Object(Default::default())
        }
    }

    fn make_dummy() -> Arc<dyn RenderStage> {
        Arc::new(Dummy)
    }

    #[test]
    fn dummy_registers_and_resolves_as_trait_object() {
        let mut registry: StageRegistry = Registry::new();
        registry
            .register(
                Descriptor {
                    id: "nicti.stage.dummy",
                    schema_version: 1,
                },
                make_dummy,
            )
            .expect("registration should succeed");

        let resolved = registry
            .get("nicti.stage.dummy")
            .expect("dummy stage is registered");
        assert_eq!(resolved.id(), "nicti.stage.dummy");
    }

    struct DummyTwo;

    impl Module for DummyTwo {
        fn id(&self) -> &str {
            "nicti.stage.dummy_two"
        }

        fn schema_version(&self) -> u32 {
            1
        }

        fn migrate_params(&self, _from_version: u32, params: Value) -> Option<Value> {
            Some(params)
        }
    }

    impl RenderStage for DummyTwo {
        fn kind(&self) -> graph::StageKind {
            graph::StageKind::Baked
        }

        fn default_params(&self) -> Value {
            Value::Object(Default::default())
        }
    }

    #[test]
    fn cache_contribution_differs_across_stage_ids_for_identical_entries() {
        let entry = StageEntry {
            schema_version: 1,
            params: serde_json::json!({"exposure_stops": 0.3}),
        };
        let a = Dummy.cache_contribution(&entry).unwrap();
        let b = DummyTwo.cache_contribution(&entry).unwrap();
        assert_ne!(
            a, b,
            "two different stages with identical params must not collide"
        );
    }

    #[test]
    fn cache_contribution_changes_with_impl_version() {
        struct Versioned(u32);
        impl Module for Versioned {
            fn id(&self) -> &str {
                "nicti.stage.versioned"
            }
            fn schema_version(&self) -> u32 {
                1
            }
            fn migrate_params(&self, _from_version: u32, params: Value) -> Option<Value> {
                Some(params)
            }
        }
        impl RenderStage for Versioned {
            fn kind(&self) -> graph::StageKind {
                graph::StageKind::Baked
            }
            fn default_params(&self) -> Value {
                Value::Object(Default::default())
            }
            fn impl_version(&self) -> u32 {
                self.0
            }
        }

        let entry = StageEntry {
            schema_version: 1,
            params: serde_json::json!({}),
        };
        let v1 = Versioned(1).cache_contribution(&entry).unwrap();
        let v2 = Versioned(2).cache_contribution(&entry).unwrap();
        assert_ne!(
            v1, v2,
            "bumping impl_version must invalidate cached output even with unchanged params"
        );
    }
}
