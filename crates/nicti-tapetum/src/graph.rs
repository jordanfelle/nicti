//! Tapetum's stage-cached render graph: a DAG of stage nodes, each with a cache key chained from
//! its upstream nodes' keys (`nicti_pawprint::chain`) plus its own params hash. This is the
//! structural claim ADR-0044's decision rule #1 tests: changing a `Baked` stage's params
//! invalidates it and every downstream node; changing a `Live` stage's params invalidates only
//! downstream `Live`/`Geometry` nodes, never a `Baked` one, because in this graph `Baked` nodes
//! are always upstream of `Live` ones -- a live slider drag must trigger zero bake dispatches.
//!
//! Node identity is a plain `String` id (ADR-0021's `vendor.stage_name` convention), not a typed
//! enum -- the render-stage extension point (`crates/nicti-tapetum::RenderStage`, #45) is exactly
//! this: an open set of stage ids a plugin can add to, so the graph can't be closed over a fixed
//! enum of "the" stages.
//!
//! Promoted from `spikes/loaf/src/graph.rs`, with two additions the spike's own tests flagged as
//! missing: a persistent memoized cache key (instead of rebuilding the memo on every call) and
//! [`RenderGraph::set_own_hash`], the "update a node in place" API a real params edit needs --
//! the spike's own cache-key test had to reconstruct the whole graph because this didn't exist.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet, VecDeque};

use nicti_pawprint::{CanonicalError, EditDocument, StageEntry};
use serde::{Deserialize, Serialize};

use crate::StageRegistry;

/// Whether a node's output is baked (cached, invalidated only when it or an upstream node
/// changes) or live (recomputed every frame, never itself cached) or geometry (a per-frame
/// transform with no upstream data dependency on pixel content -- crop/rotate/zoom/pan).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageKind {
    Baked,
    Live,
    Geometry,
}

#[derive(Debug, Clone)]
pub struct StageNode {
    pub id: String,
    pub kind: StageKind,
    /// Ids of the nodes this one reads from. Order matters for hashing (`nicti_pawprint::chain`)
    /// -- always push in a stable, deterministic order (this module sorts by id before chaining,
    /// so callers don't have to worry about insertion order themselves).
    pub upstream: Vec<String>,
    /// This node's own canonical params hash (`nicti_pawprint::hash_value` of whatever
    /// `StageEntry::params` or recipe this node represents) -- independent of any upstream node.
    pub own_hash: blake3::Hash,
}

#[derive(Debug, Default)]
pub struct RenderGraph {
    nodes: BTreeMap<String, StageNode>,
    /// Prebuilt forward adjacency (upstream id -> ids that read from it), built once as nodes are
    /// added -- upstream edges never change after `add_node` (only `set_own_hash` mutates a node,
    /// and only its own hash), so this never needs rebuilding.
    children: BTreeMap<String, Vec<String>>,
    /// Memoized cache keys, persisted across calls. `set_own_hash` clears exactly the entries for
    /// nodes it invalidates; nothing else touches this map.
    memo: RefCell<BTreeMap<String, blake3::Hash>>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GraphError {
    #[error("stage {0:?} already exists in the graph")]
    DuplicateStage(String),
    #[error("stage {0:?} references unknown upstream stage {1:?}")]
    UnknownUpstream(String, String),
    #[error("stage {0:?} not found")]
    UnknownStage(String),
    #[error("graph has a cycle involving stage {0:?}")]
    Cycle(String),
}

#[derive(Debug, thiserror::Error)]
pub enum ApplyDocumentError {
    #[error(transparent)]
    Graph(#[from] GraphError),
    #[error("stage {0:?} is a graph node but has no registered RenderStage")]
    UnregisteredStage(String),
    #[error("stage {0:?}'s params failed to canonicalize: {1}")]
    Canonical(String, CanonicalError),
}

/// The result of [`RenderGraph::set_own_hash`]: `all` is every node whose cache key changed
/// (`changed` itself, plus everything transitively downstream); `bakes` is the subset of those
/// that are `Baked` -- the actual rebake set a scheduler (Pounce, #54) would need to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invalidation {
    pub all: HashSet<String>,
    pub bakes: HashSet<String>,
}

impl RenderGraph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a node. Upstream ids must already exist -- nodes are added in dependency order,
    /// matching how a real pipeline is built up stage by stage (decode first, live suffix last).
    pub fn add_node(&mut self, node: StageNode) -> Result<(), GraphError> {
        if self.nodes.contains_key(&node.id) {
            return Err(GraphError::DuplicateStage(node.id.clone()));
        }
        for up in &node.upstream {
            if !self.nodes.contains_key(up) {
                return Err(GraphError::UnknownUpstream(node.id.clone(), up.clone()));
            }
        }
        for up in &node.upstream {
            self.children
                .entry(up.clone())
                .or_default()
                .push(node.id.clone());
        }
        self.children.entry(node.id.clone()).or_default();
        self.nodes.insert(node.id.clone(), node);
        Ok(())
    }

    pub fn node(&self, id: &str) -> Option<&StageNode> {
        self.nodes.get(id)
    }

    /// Kahn's-algorithm topological order. `add_node`'s upstream-must-exist rule already rules out
    /// a cycle in practice, but this stays a real check (not an assumption) in case a future
    /// caller relaxes that rule.
    pub fn topological_order(&self) -> Result<Vec<String>, GraphError> {
        let mut indegree: BTreeMap<&str, usize> = self
            .nodes
            .iter()
            .map(|(id, n)| (id.as_str(), n.upstream.len()))
            .collect();

        let mut queue: VecDeque<&str> = indegree
            .iter()
            .filter(|(_, &d)| d == 0)
            .map(|(id, _)| *id)
            .collect();
        let mut order = Vec::with_capacity(self.nodes.len());
        while let Some(id) = queue.pop_front() {
            order.push(id.to_string());
            if let Some(kids) = self.children.get(id) {
                for kid in kids {
                    let d = indegree.get_mut(kid.as_str()).unwrap();
                    *d -= 1;
                    if *d == 0 {
                        queue.push_back(kid.as_str());
                    }
                }
            }
        }

        if order.len() != self.nodes.len() {
            let stuck = indegree
                .into_iter()
                .find(|(_, d)| *d > 0)
                .map(|(id, _)| id.to_string())
                .unwrap_or_default();
            return Err(GraphError::Cycle(stuck));
        }
        Ok(order)
    }

    /// This node's cache key: its own hash chained with its upstream nodes' cache keys, computed
    /// recursively and memoized across calls -- a deep chain (decode -> demosaic -> denoise ->
    /// lens -> heal) isn't recomputed from scratch each time this is called.
    pub fn cache_key(&self, id: &str) -> Result<blake3::Hash, GraphError> {
        if let Some(&h) = self.memo.borrow().get(id) {
            return Ok(h);
        }
        let node = self
            .nodes
            .get(id)
            .ok_or_else(|| GraphError::UnknownStage(id.to_string()))?;
        let mut upstream_sorted = node.upstream.clone();
        upstream_sorted.sort();
        let mut upstream_hashes = Vec::with_capacity(upstream_sorted.len());
        for up in &upstream_sorted {
            upstream_hashes.push(self.cache_key(up)?);
        }
        let key = nicti_pawprint::chain(&upstream_hashes, node.own_hash);
        self.memo.borrow_mut().insert(id.to_string(), key);
        Ok(key)
    }

    /// The set of node ids whose cache key changes as a result of `changed`'s own params changing
    /// -- `changed` itself, plus every node reachable from it by following `upstream` edges
    /// forward (i.e. every node that transitively reads from `changed`). This is what "invalidate"
    /// means in this graph: a `Baked` node's cached output is stale and must be rebaked; a
    /// `Live`/`Geometry` node has nothing cached to begin with, so "invalidated" just means its
    /// next frame's dispatch uses new inputs.
    pub fn invalidated_by(&self, changed: &str) -> Result<HashSet<String>, GraphError> {
        if !self.nodes.contains_key(changed) {
            return Err(GraphError::UnknownStage(changed.to_string()));
        }
        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();
        visited.insert(changed.to_string());
        queue.push_back(changed.to_string());
        while let Some(id) = queue.pop_front() {
            if let Some(kids) = self.children.get(&id) {
                for kid in kids {
                    if visited.insert(kid.clone()) {
                        queue.push_back(kid.clone());
                    }
                }
            }
        }
        Ok(visited)
    }

    /// Of `invalidated_by(changed)`, only the ids whose `StageKind` is `Baked` -- the actual bake
    /// dispatches a scheduler (Pounce, #54) would need to (re-)run. Decision rule #1 is precisely
    /// "changing a Live stage's params yields an empty set here."
    pub fn invalidated_bakes(&self, changed: &str) -> Result<HashSet<String>, GraphError> {
        let all = self.invalidated_by(changed)?;
        Ok(all
            .into_iter()
            .filter(|id| self.nodes.get(id).map(|n| n.kind == StageKind::Baked) == Some(true))
            .collect())
    }

    /// Sets `id`'s own params hash in place -- what a real params edit does (replacing a
    /// `StageEntry`), as opposed to the spike this was promoted from, which had no update API and
    /// had to reconstruct the whole graph to test this. Clears the memoized cache key for exactly
    /// the nodes this invalidates (`invalidated_by(id)`); every other node's memoized key is left
    /// untouched, which is itself part of decision rule #1's contract -- a live-stage change must
    /// not even recompute, let alone change, an unrelated or upstream node's cache key.
    pub fn set_own_hash(
        &mut self,
        id: &str,
        own_hash: blake3::Hash,
    ) -> Result<Invalidation, GraphError> {
        {
            let node = self
                .nodes
                .get_mut(id)
                .ok_or_else(|| GraphError::UnknownStage(id.to_string()))?;
            node.own_hash = own_hash;
        }
        let all = self.invalidated_by(id)?;
        {
            let mut memo = self.memo.borrow_mut();
            for invalidated in &all {
                memo.remove(invalidated);
            }
        }
        let bakes = all
            .iter()
            .filter(|id| {
                self.nodes
                    .get(id.as_str())
                    .map(|n| n.kind == StageKind::Baked)
                    == Some(true)
            })
            .cloned()
            .collect();
        Ok(Invalidation { all, bakes })
    }

    /// Applies an `EditDocument`'s params to every node currently in the graph, setting each
    /// node's `own_hash` from `RenderStage::cache_contribution` -- without this, a node's
    /// `own_hash` stays whatever it was set to at `add_node` time (typically a placeholder like
    /// `blake3::hash(id.as_bytes())`), so a slider change never actually changes the live
    /// composite key and nothing re-renders. A node with no entry in `document` uses its
    /// registered stage's `default_params()`. Node ids not present in `registry` are an error --
    /// every node in a real graph is backed by a real stage, unlike `document`, which may
    /// legitimately be missing an entry for a stage still at its defaults.
    pub fn apply_document(
        &mut self,
        document: &EditDocument,
        registry: &StageRegistry,
    ) -> Result<(), ApplyDocumentError> {
        let ids: Vec<String> = self.nodes.keys().cloned().collect();
        for id in ids {
            let stage = registry
                .get(&id)
                .ok_or_else(|| ApplyDocumentError::UnregisteredStage(id.clone()))?;
            let entry = document
                .stages
                .get(&id)
                .cloned()
                .unwrap_or_else(|| StageEntry {
                    schema_version: stage.schema_version(),
                    params: stage.default_params(),
                });
            let hash = stage
                .cache_contribution(&entry)
                .map_err(|e| ApplyDocumentError::Canonical(id.clone(), e))?;
            self.set_own_hash(&id, hash)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RenderStage;

    /// Builds the graph this ADR proposes: decode -> demosaic -> denoise -> lens -> heal (all
    /// Baked), then wb -> huesat -> tone -> vibrance (all Live), then crop (Geometry).
    fn hero_shaped_graph() -> RenderGraph {
        let mut g = RenderGraph::new();
        let baked_chain = ["decode", "demosaic", "denoise", "lens", "heal"];
        let mut prev: Option<&str> = None;
        for id in baked_chain {
            g.add_node(StageNode {
                id: id.to_string(),
                kind: StageKind::Baked,
                upstream: prev.map(|p| vec![p.to_string()]).unwrap_or_default(),
                own_hash: blake3::hash(id.as_bytes()),
            })
            .unwrap();
            prev = Some(id);
        }
        let live_chain = ["wb", "huesat", "tone", "vibrance"];
        for id in live_chain {
            g.add_node(StageNode {
                id: id.to_string(),
                kind: StageKind::Live,
                upstream: vec![prev.unwrap().to_string()],
                own_hash: blake3::hash(id.as_bytes()),
            })
            .unwrap();
            prev = Some(id);
        }
        g.add_node(StageNode {
            id: "crop".to_string(),
            kind: StageKind::Geometry,
            upstream: vec![prev.unwrap().to_string()],
            own_hash: blake3::hash(b"crop"),
        })
        .unwrap();
        g
    }

    /// A DAG (not a chain): both a neutral-render node and a recipe node feed one mask-bake node,
    /// proving multi-parent chaining -- ADR-0048's mask bake depends on both the neutral render
    /// and its own recipe params.
    fn mask_shaped_graph() -> RenderGraph {
        let mut g = RenderGraph::new();
        g.add_node(StageNode {
            id: "heal".to_string(),
            kind: StageKind::Baked,
            upstream: vec![],
            own_hash: blake3::hash(b"heal"),
        })
        .unwrap();
        g.add_node(StageNode {
            id: "neutral_render".to_string(),
            kind: StageKind::Baked,
            upstream: vec!["heal".to_string()],
            own_hash: blake3::hash(b"neutral_render"),
        })
        .unwrap();
        g.add_node(StageNode {
            id: "mask_recipe".to_string(),
            kind: StageKind::Baked,
            upstream: vec![],
            own_hash: blake3::hash(b"mask_recipe"),
        })
        .unwrap();
        g.add_node(StageNode {
            id: "mask_bake".to_string(),
            kind: StageKind::Baked,
            upstream: vec!["neutral_render".to_string(), "mask_recipe".to_string()],
            own_hash: blake3::hash(b"mask_bake"),
        })
        .unwrap();
        g.add_node(StageNode {
            id: "tone".to_string(),
            kind: StageKind::Live,
            upstream: vec!["heal".to_string()],
            own_hash: blake3::hash(b"tone"),
        })
        .unwrap();
        g
    }

    #[test]
    fn topological_order_respects_upstream_edges() {
        let g = hero_shaped_graph();
        let order = g.topological_order().unwrap();
        let pos = |id: &str| order.iter().position(|x| x == id).unwrap();
        assert!(pos("decode") < pos("demosaic"));
        assert!(pos("heal") < pos("wb"));
        assert!(pos("vibrance") < pos("crop"));
    }

    #[test]
    fn changing_a_live_stage_triggers_zero_bake_dispatches() {
        // ADR-0044 decision rule #1: a live slider (e.g. white balance) must not invalidate any
        // Baked node.
        let g = hero_shaped_graph();
        let bakes = g.invalidated_bakes("wb").unwrap();
        assert!(
            bakes.is_empty(),
            "changing a Live stage invalidated Baked stages: {bakes:?}"
        );
        // It does invalidate the Live/Geometry stages downstream of it.
        let all = g.invalidated_by("wb").unwrap();
        assert!(all.contains("wb"));
        assert!(all.contains("huesat"));
        assert!(all.contains("tone"));
        assert!(all.contains("vibrance"));
        assert!(all.contains("crop"));
        assert!(!all.contains("decode"));
    }

    #[test]
    fn changing_a_baked_stage_invalidates_itself_and_every_downstream_stage() {
        let g = hero_shaped_graph();
        let bakes = g.invalidated_bakes("demosaic").unwrap();
        assert_eq!(
            bakes,
            ["demosaic", "denoise", "lens", "heal"]
                .into_iter()
                .map(String::from)
                .collect()
        );
        let all = g.invalidated_by("demosaic").unwrap();
        assert!(!all.contains("decode"));
        assert!(all.contains("wb"));
        assert!(all.contains("crop"));
    }

    #[test]
    fn changing_crop_invalidates_only_crop_itself() {
        // Decision rule #3: crop is the sink of this graph, so nothing reads from it -- changing
        // its params (a drag) invalidates exactly {crop}, and in particular touches zero Baked
        // stages.
        let g = hero_shaped_graph();
        let all = g.invalidated_by("crop").unwrap();
        assert_eq!(all, ["crop"].into_iter().map(String::from).collect());
        assert!(g.invalidated_bakes("crop").unwrap().is_empty());
    }

    #[test]
    fn a_tone_change_leaves_an_unrelated_mask_bake_key_unchanged() {
        // ADR-0048: the mask bake's own key must not depend on a live slider -- it shares no edge
        // with `tone` in this graph.
        let mut g = mask_shaped_graph();
        let before = g.cache_key("mask_bake").unwrap();
        g.set_own_hash("tone", blake3::hash(b"tone-changed"))
            .unwrap();
        let after = g.cache_key("mask_bake").unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn set_own_hash_changes_exactly_the_invalidated_keys_and_nothing_else() {
        for changed in [
            "decode", "demosaic", "denoise", "lens", "heal", "wb", "huesat", "tone", "vibrance",
            "crop",
        ] {
            let mut g = hero_shaped_graph();
            let before: BTreeMap<String, blake3::Hash> = g
                .topological_order()
                .unwrap()
                .into_iter()
                .map(|id| {
                    let key = g.cache_key(&id).unwrap();
                    (id, key)
                })
                .collect();

            let invalidation = g
                .set_own_hash(
                    changed,
                    blake3::hash(format!("{changed}-changed").as_bytes()),
                )
                .unwrap();
            assert_eq!(invalidation.all, g.invalidated_by(changed).unwrap());

            for (id, before_key) in &before {
                let after_key = g.cache_key(id).unwrap();
                if invalidation.all.contains(id) {
                    assert_ne!(
                        before_key, &after_key,
                        "changing {changed:?} should have changed {id:?}'s cache key"
                    );
                } else {
                    assert_eq!(
                        before_key, &after_key,
                        "changing {changed:?} should NOT have changed {id:?}'s cache key"
                    );
                }
            }
        }
    }

    #[test]
    fn unknown_upstream_is_rejected_at_add_time() {
        let mut g = RenderGraph::new();
        let err = g
            .add_node(StageNode {
                id: "a".to_string(),
                kind: StageKind::Baked,
                upstream: vec!["nonexistent".to_string()],
                own_hash: blake3::hash(b"a"),
            })
            .unwrap_err();
        assert_eq!(
            err,
            GraphError::UnknownUpstream("a".to_string(), "nonexistent".to_string())
        );
    }

    fn make_tone_stage() -> std::sync::Arc<dyn RenderStage> {
        std::sync::Arc::new(crate::stages::tone_stage())
    }
    fn make_wb_stage() -> std::sync::Arc<dyn RenderStage> {
        std::sync::Arc::new(crate::stages::wb_stage())
    }

    /// A registry with just `nicti.tone` and `nicti.wb` registered, matching the real stages in
    /// `crate::stages` -- enough to exercise `apply_document` without depending on that module's
    /// full stage set.
    fn tone_and_wb_registry() -> StageRegistry {
        let mut registry: StageRegistry = StageRegistry::new();
        registry
            .register(
                nicti_claw::Descriptor {
                    id: crate::stages::TONE,
                    schema_version: 1,
                },
                make_tone_stage,
            )
            .unwrap();
        registry
            .register(
                nicti_claw::Descriptor {
                    id: crate::stages::WB,
                    schema_version: 1,
                },
                make_wb_stage,
            )
            .unwrap();
        registry
    }

    fn tone_and_wb_graph() -> RenderGraph {
        let mut g = RenderGraph::new();
        g.add_node(StageNode {
            id: crate::stages::WB.to_string(),
            kind: StageKind::Live,
            upstream: vec![],
            own_hash: blake3::hash(b"placeholder"),
        })
        .unwrap();
        g.add_node(StageNode {
            id: crate::stages::TONE.to_string(),
            kind: StageKind::Live,
            upstream: vec![crate::stages::WB.to_string()],
            own_hash: blake3::hash(b"placeholder"),
        })
        .unwrap();
        g
    }

    #[test]
    fn apply_document_with_no_entries_uses_each_stage_own_default_params() {
        let registry = tone_and_wb_registry();
        let mut g = tone_and_wb_graph();
        let document = EditDocument::default();
        g.apply_document(&document, &registry).unwrap();

        let expected = crate::stages::tone_stage()
            .cache_contribution(&StageEntry {
                schema_version: 1,
                params: crate::stages::tone_stage().default_params(),
            })
            .unwrap();
        assert_eq!(g.node(crate::stages::TONE).unwrap().own_hash, expected);
    }

    #[test]
    fn apply_document_sets_own_hash_from_the_documents_entry() {
        let registry = tone_and_wb_registry();
        let mut g = tone_and_wb_graph();
        let mut document = EditDocument::default();
        document.stages.insert(
            crate::stages::TONE.to_string(),
            StageEntry {
                schema_version: 1,
                params: serde_json::json!({"contrast": 0.5}),
            },
        );
        g.apply_document(&document, &registry).unwrap();

        let expected = crate::stages::tone_stage()
            .cache_contribution(&StageEntry {
                schema_version: 1,
                params: serde_json::json!({"contrast": 0.5}),
            })
            .unwrap();
        assert_eq!(g.node(crate::stages::TONE).unwrap().own_hash, expected);
    }

    #[test]
    fn apply_document_changing_only_tone_leaves_wbs_own_hash_unchanged_but_invalidates_the_key() {
        let registry = tone_and_wb_registry();
        let mut g = tone_and_wb_graph();
        g.apply_document(&EditDocument::default(), &registry)
            .unwrap();
        let wb_hash_before = g.node(crate::stages::WB).unwrap().own_hash;
        let wb_key_before = g.cache_key(crate::stages::WB).unwrap();
        let tone_key_before = g.cache_key(crate::stages::TONE).unwrap();

        let mut document = EditDocument::default();
        document.stages.insert(
            crate::stages::TONE.to_string(),
            StageEntry {
                schema_version: 1,
                params: serde_json::json!({"contrast": 0.9}),
            },
        );
        g.apply_document(&document, &registry).unwrap();

        assert_eq!(
            g.node(crate::stages::WB).unwrap().own_hash,
            wb_hash_before,
            "changing only tone's document entry must not touch wb's own_hash"
        );
        assert_eq!(
            g.cache_key(crate::stages::WB).unwrap(),
            wb_key_before,
            "wb's cache key must not change either -- it's upstream of tone, not downstream"
        );
        assert_ne!(
            g.cache_key(crate::stages::TONE).unwrap(),
            tone_key_before,
            "tone's own cache key must change when its document entry changes"
        );
    }

    #[test]
    fn apply_document_errs_on_a_graph_node_with_no_registered_stage() {
        let registry: StageRegistry = StageRegistry::new(); // nothing registered
        let mut g = RenderGraph::new();
        g.add_node(StageNode {
            id: "nicti.unregistered".to_string(),
            kind: StageKind::Live,
            upstream: vec![],
            own_hash: blake3::hash(b"placeholder"),
        })
        .unwrap();
        let err = g
            .apply_document(&EditDocument::default(), &registry)
            .unwrap_err();
        assert!(
            matches!(err, ApplyDocumentError::UnregisteredStage(id) if id == "nicti.unregistered")
        );
    }
}
