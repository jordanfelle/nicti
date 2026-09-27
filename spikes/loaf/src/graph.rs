//! Tapetum's stage-cached render graph: a DAG of stage nodes, each with a cache key chained from
//! its upstream nodes' keys (`hash::chain`) plus its own params hash. This is the structural claim
//! ADR-0044's decision rule #1 tests: changing a `Baked` stage's params invalidates it and every
//! downstream node; changing a `Live` stage's params invalidates only downstream `Live`/`Geometry`
//! nodes, never a `Baked` one, because in this graph `Baked` nodes are always upstream of `Live`
//! ones -- a live slider drag must trigger zero bake dispatches.
//!
//! Node identity is a plain `String` id (ADR-0002's `vendor.stage_name` convention), not a typed
//! enum -- the render-stage extension point (`crates/nicti-render::RenderStage`, #45) is exactly
//! this: an open set of stage ids a plugin can add to, so the graph can't be closed over a fixed
//! enum of "the" stages.

use std::collections::{BTreeMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

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
    /// Ids of the nodes this one reads from. Order matters for hashing (`hash::chain`) -- always
    /// push in a stable, deterministic order (this module sorts by id before chaining, so callers
    /// don't have to worry about insertion order themselves).
    pub upstream: Vec<String>,
    /// This node's own canonical params hash (`hash::hash_value` of whatever `StageEntry::params`
    /// or recipe this node represents) -- independent of any upstream node.
    pub own_hash: blake3::Hash,
}

#[derive(Debug, Default)]
pub struct RenderGraph {
    nodes: BTreeMap<String, StageNode>,
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
        let mut indegree: BTreeMap<&str, usize> =
            self.nodes.keys().map(|id| (id.as_str(), 0usize)).collect();
        let mut children: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for node in self.nodes.values() {
            for up in &node.upstream {
                *indegree.get_mut(node.id.as_str()).unwrap() += 1;
                children
                    .entry(up.as_str())
                    .or_default()
                    .push(node.id.as_str());
            }
        }

        let mut queue: VecDeque<&str> = indegree
            .iter()
            .filter(|(_, &d)| d == 0)
            .map(|(id, _)| *id)
            .collect();
        let mut order = Vec::with_capacity(self.nodes.len());
        while let Some(id) = queue.pop_front() {
            order.push(id.to_string());
            if let Some(kids) = children.get(id) {
                for &kid in kids {
                    let d = indegree.get_mut(kid).unwrap();
                    *d -= 1;
                    if *d == 0 {
                        queue.push_back(kid);
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
    /// recursively (memoized via `cache`) so a deep chain (decode -> demosaic -> denoise -> lens
    /// -> heal) isn't recomputed from scratch at every level.
    pub fn cache_key(&self, id: &str) -> Result<blake3::Hash, GraphError> {
        let mut cache = BTreeMap::new();
        self.cache_key_memo(id, &mut cache)
    }

    fn cache_key_memo(
        &self,
        id: &str,
        cache: &mut BTreeMap<String, blake3::Hash>,
    ) -> Result<blake3::Hash, GraphError> {
        if let Some(&h) = cache.get(id) {
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
            upstream_hashes.push(self.cache_key_memo(up.as_str(), cache)?);
        }
        let key = crate::hash::chain(&upstream_hashes, node.own_hash);
        cache.insert(id.to_string(), key);
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
        let mut children: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for node in self.nodes.values() {
            for up in &node.upstream {
                children
                    .entry(up.as_str())
                    .or_default()
                    .push(node.id.as_str());
            }
        }

        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();
        visited.insert(changed.to_string());
        queue.push_back(changed);
        while let Some(id) = queue.pop_front() {
            if let Some(kids) = children.get(id) {
                for &kid in kids {
                    if visited.insert(kid.to_string()) {
                        queue.push_back(kid);
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // demosaic, denoise, lens, heal are all Baked and downstream of (or equal to) demosaic.
        assert_eq!(
            bakes,
            ["demosaic", "denoise", "lens", "heal"]
                .into_iter()
                .map(String::from)
                .collect()
        );
        // decode is upstream, must not be touched.
        let all = g.invalidated_by("demosaic").unwrap();
        assert!(!all.contains("decode"));
        // The live suffix and crop are downstream of every Baked stage, so they're invalidated
        // too (their next frame reads new baked input) even though they're not Baked themselves.
        assert!(all.contains("wb"));
        assert!(all.contains("crop"));
    }

    #[test]
    fn changing_crop_invalidates_only_crop_itself() {
        // Decision rule #3: crop is the sink of this graph, so nothing reads from it -- changing
        // its params (a drag) invalidates exactly {crop}, and in particular touches zero Baked
        // stages (asserted generically by invalidated_bakes, and specifically here).
        let g = hero_shaped_graph();
        let all = g.invalidated_by("crop").unwrap();
        assert_eq!(all, ["crop"].into_iter().map(String::from).collect());
        assert!(g.invalidated_bakes("crop").unwrap().is_empty());
    }

    #[test]
    fn cache_key_changes_only_for_the_changed_stage_and_its_descendants() {
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

        // Rebuild the graph with "demosaic"'s own_hash changed -- add_node has no update method,
        // so this test reconstructs the graph, matching what a real params edit does semantically
        // (a new StageEntry replaces the old one).
        let mut g2 = RenderGraph::new();
        for id in g.topological_order().unwrap() {
            let mut node = g.nodes.remove(&id).unwrap();
            if id == "demosaic" {
                node.own_hash = blake3::hash(b"demosaic-changed");
            }
            g2.add_node(node).unwrap();
        }
        g = g2;

        for (id, before_key) in &before {
            let after_key = g.cache_key(id).unwrap();
            let should_change =
                g.invalidated_bakes("demosaic").unwrap().contains(id) || id == "demosaic";
            if should_change {
                assert_ne!(before_key, &after_key, "{id} should have a new cache key");
            } else if !g.invalidated_by("demosaic").unwrap().contains(id) {
                assert_eq!(before_key, &after_key, "{id} should be unaffected");
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
}
