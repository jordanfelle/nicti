//! The graph-driven renderer: walks a [`graph::RenderGraph`] in topological order, dispatching a
//! `Baked` node's `BakedExec` only on a cache miss (a `cache::Tier` keyed by
//! `graph::RenderGraph::cache_key`, exactly the key PR1 already proved is stable/invalidated
//! correctly), and fusing every `Live`/`Geometry` node's contribution into a single dispatch each
//! -- ADR-0044 decision rule #1: a live-slider change must cost zero bake dispatches, and a crop
//! drag must cost zero bake *and* zero live dispatches.
//!
//! The concrete per-stage GPU work (the real live-suffix shader, the real geometry sample pass)
//! is a later slice of #45 -- `BakedExec`/`LiveExec`/`GeometryExec` are the shape every stage
//! plugs into, proven correct here against mock implementations that just count calls.

use std::sync::Arc;

use crate::cache::Tier;
use crate::frame::{Extent, FrameTexture};
use crate::gpu::GpuContext;
use crate::graph::{GraphError, RenderGraph};

/// One `Baked` node's GPU work: read `input` (the previous baked stage's output, `None` only for
/// the very first node, e.g. decode) and write `output`. Both textures are already allocated by
/// the caller at the render's target extent.
pub trait BakedExec: Send + Sync {
    fn encode(
        &self,
        gpu: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        input: Option<&FrameTexture>,
        output: &FrameTexture,
    );
}

/// The single fused live-suffix dispatch (WB, exposure, tone, vibrance, ... -- ADR-0044's "one
/// fused live dispatch"): reads the baked chain's final output, writes the live output. Exactly
/// one `LiveExec` is invoked per `Renderer`, regardless of how many `Live`-kind nodes exist in the
/// graph or how many of them changed -- that fusion is the whole point of decision rule #1.
pub trait LiveExec: Send + Sync {
    fn encode(
        &self,
        gpu: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        input: &FrameTexture,
        output: &FrameTexture,
    );
}

/// The geometry/present pass (crop/rotate/zoom/pan -- ADR-0044's affine sample pass): reads the
/// live suffix's own output only, writes the final presented frame.
pub trait GeometryExec: Send + Sync {
    fn encode(
        &self,
        gpu: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        input: &FrameTexture,
        output: &FrameTexture,
    );
}

#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error(transparent)]
    Graph(#[from] GraphError),
    #[error("render request has no baked stages -- at least a decode node is required")]
    EmptyBakedChain,
}

/// Per-render dispatch counts, for tests (and later, real telemetry) to assert against.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RenderStats {
    pub bake_dispatches: u32,
    pub live_dispatches: u32,
    pub geometry_dispatches: u32,
}

/// One render call's inputs: the graph (already carrying every node's current `own_hash`), the
/// ordered chain of `Baked` node ids with their executors, and the sets of `Live`/`Geometry` node
/// ids whose combined cache keys decide whether the single fused live/geometry dispatch reruns.
pub struct RenderRequest<'a> {
    pub graph: &'a RenderGraph,
    /// `Baked` node ids in topological (dependency) order, each with its `BakedExec`.
    pub baked_chain: &'a [(&'a str, &'a dyn BakedExec)],
    pub live: &'a dyn LiveExec,
    pub live_nodes: &'a [&'a str],
    pub geometry: &'a dyn GeometryExec,
    pub geometry_nodes: &'a [&'a str],
    pub extent: Extent,
    /// The geometry (crop) pass's own output size (#272). Baked and live stages always render at
    /// `extent`; only the final present/crop pass is sized to this, so a Develop preview can shrink
    /// to the crop rect. Ignored by `render_live`/`render_live_from` (no geometry pass runs).
    pub geometry_extent: Extent,
}

/// Folds `extent` into `key` -- every cache key this module stores or looks up must be scoped to
/// the target output size, or a render at one extent (e.g. screen resolution) could return a
/// texture cached for a different one (e.g. full resolution) on a later render whose node cache
/// keys are otherwise identical. A partial cache hit is just as real a risk: if only one baked
/// node was evicted and gets re-encoded at a new extent, but reads a still-cached input from a
/// previous extent, the stage's input and output would silently mismatch in size.
fn keyed_by_extent(key: blake3::Hash, extent: Extent) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(key.as_bytes());
    hasher.update(&extent.width.to_le_bytes());
    hasher.update(&extent.height.to_le_bytes());
    hasher.finalize()
}

/// Chains `input_key` (the previous stage's own output key -- the last baked node's cache key
/// for the live composite, or the live composite's own key for the geometry composite) with
/// every named node's cache key into one composite hash, then folds in `extent`. `input_key` is
/// what makes this correct even when `ids` is empty: `live_nodes`/`geometry_nodes` being empty is
/// a real, reachable state (no `Live`-kind stage exists in the graph yet, e.g. before #45's later
/// slices add one) -- without folding in the upstream stage's own key, an empty `ids` would give
/// the same composite key on every render regardless of what its input actually produced,
/// treating every render as a cache hit and never re-dispatching even though the baked chain (or
/// the live output, for geometry) changed underneath it. Sorted by id first (matching
/// `RenderGraph::cache_key`'s own convention for upstream ids), so this doesn't silently depend
/// on the caller's slice order -- `live_nodes`/`geometry_nodes` is only guaranteed stable across
/// calls when it comes from a fixed literal or `RenderGraph`'s own `topological_order`; a caller
/// building it from anything else (a filter, a set) could otherwise see the identical set of
/// nodes hash differently between two renders and pay a spurious dispatch.
fn composite_key(
    graph: &RenderGraph,
    ids: &[&str],
    input_key: blake3::Hash,
    extent: Extent,
) -> Result<blake3::Hash, GraphError> {
    let mut sorted: Vec<&str> = ids.to_vec();
    sorted.sort_unstable();
    let mut hasher = blake3::Hasher::new();
    hasher.update(input_key.as_bytes());
    for id in sorted {
        hasher.update(graph.cache_key(id)?.as_bytes());
    }
    Ok(keyed_by_extent(hasher.finalize(), extent))
}

/// Drives one render graph: caches `Baked` output per node (byte-budgeted, evicting LRU), and
/// caches the fused live/geometry outputs each as one entry keyed by their own composite hash.
pub struct Renderer {
    gpu: Arc<GpuContext>,
    baked_cache: Tier<Arc<FrameTexture>>,
    live_key: Option<blake3::Hash>,
    live_output: Option<Arc<FrameTexture>>,
    geometry_key: Option<blake3::Hash>,
    geometry_output: Option<Arc<FrameTexture>>,
    last_stats: RenderStats,
}

impl Renderer {
    pub fn new(gpu: Arc<GpuContext>, baked_budget_bytes: u64) -> Self {
        Self {
            gpu,
            baked_cache: Tier::new(baked_budget_bytes, |f: &Arc<FrameTexture>| f.byte_size()),
            live_key: None,
            live_output: None,
            geometry_key: None,
            geometry_output: None,
            last_stats: RenderStats::default(),
        }
    }

    pub fn last_stats(&self) -> RenderStats {
        self.last_stats
    }

    /// Runs the baked chain into `encoder`, returning its last output and that node's cache key. A
    /// node whose key is resident in `baked_cache` is a hit (zero dispatches).
    fn run_baked(
        &mut self,
        req: &RenderRequest<'_>,
        encoder: &mut wgpu::CommandEncoder,
        stats: &mut RenderStats,
    ) -> Result<(Arc<FrameTexture>, blake3::Hash), RenderError> {
        if req.baked_chain.is_empty() {
            return Err(RenderError::EmptyBakedChain);
        }
        let mut current: Option<Arc<FrameTexture>> = None;
        // A node's key chains from everything upstream, so a cached output for node k is valid
        // whatever became of the nodes before it. Resume after the *last* cached node: when the
        // byte budget holds fewer frames than the chain is long, earlier nodes get evicted as later
        // ones are inserted, and re-running them just to reach a cached final output would redo
        // the whole chain on every render.
        let mut start = 0;
        for (i, (id, _)) in req.baked_chain.iter().enumerate().rev() {
            let key = keyed_by_extent(req.graph.cache_key(id)?, req.extent);
            if let Some(cached) = self.baked_cache.get(&key) {
                current = Some(Arc::clone(cached));
                start = i + 1;
                break;
            }
        }
        for (id, exec) in &req.baked_chain[start..] {
            let key = keyed_by_extent(req.graph.cache_key(id)?, req.extent);
            if let Some(cached) = self.baked_cache.get(&key) {
                current = Some(Arc::clone(cached));
                continue;
            }
            let output = Arc::new(FrameTexture::new(&self.gpu, req.extent));
            exec.encode(&self.gpu, encoder, current.as_deref(), &output);
            stats.bake_dispatches += 1;
            self.baked_cache.put(key, Arc::clone(&output));
            current = Some(output);
        }
        let baked_output = current.ok_or(RenderError::EmptyBakedChain)?;
        // The key of the chain's last node, whether it was cached or just computed.
        let last = req
            .baked_chain
            .last()
            .map(|(id, _)| *id)
            .expect("checked non-empty");
        Ok((baked_output, req.graph.cache_key(last)?))
    }

    /// Runs and **submits** just the baked chain, returning the last baked frame.
    ///
    /// A stage that needs the baked frame *before* the live dispatch is recorded -- the mask engine
    /// reads it to refine AI alphas and to build range masks -- cannot get it from inside
    /// [`LiveExec::encode`]: `render` records every pass into one encoder that is only submitted at
    /// the end, so anything submitted from within would run first and read an unwritten texture.
    /// Call this, use the frame, then call [`Self::render`]: its baked stages are now all cache
    /// hits, so nothing is computed twice. (Does not touch [`Self::last_stats`].)
    pub fn render_baked(
        &mut self,
        req: &RenderRequest<'_>,
    ) -> Result<Arc<FrameTexture>, RenderError> {
        let mut stats = RenderStats::default();
        let mut encoder = self
            .gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("nicti-tapetum baked chain"),
            });
        let (baked, _) = self.run_baked(req, &mut encoder, &mut stats)?;
        self.gpu.queue.submit(Some(encoder.finish()));
        Ok(baked)
    }

    /// Renders `req`, returning the final (post-geometry) frame. A `Baked` node whose cache key
    /// is already resident in `baked_cache` is a cache hit -- zero dispatches for it. The fused
    /// live/geometry passes are each either a cache hit (their composite key matches the stored
    /// one from the previous render) or exactly one dispatch.
    pub fn render(&mut self, req: &RenderRequest<'_>) -> Result<Arc<FrameTexture>, RenderError> {
        self.render_through(req, true, None)
    }

    /// Like [`Self::render`] but stops after the fused live suffix, returning its output without
    /// running (or allocating a target for) the geometry pass -- what a tiled full-res export
    /// (#57) wants, since `TiledRender` runs the crop itself one tile at a time against this
    /// texture. `req.geometry`/`req.geometry_nodes` are ignored.
    pub fn render_live(
        &mut self,
        req: &RenderRequest<'_>,
    ) -> Result<Arc<FrameTexture>, RenderError> {
        self.render_through(req, false, None)
    }

    /// [`Self::render_live`] over a baked frame the caller already holds (from
    /// [`Self::render_baked`]), so the baked chain is not run a second time. Export (#354) needs
    /// this: its renderer has a zero baked-cache budget, so the "following render finds it cached"
    /// assumption [`Self::render_baked`] documents does not hold there.
    pub fn render_live_from(
        &mut self,
        req: &RenderRequest<'_>,
        baked: Arc<FrameTexture>,
    ) -> Result<Arc<FrameTexture>, RenderError> {
        self.render_through(req, false, Some(baked))
    }

    fn render_through(
        &mut self,
        req: &RenderRequest<'_>,
        with_geometry: bool,
        pre_baked: Option<Arc<FrameTexture>>,
    ) -> Result<Arc<FrameTexture>, RenderError> {
        let mut stats = RenderStats::default();
        let mut encoder = self
            .gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("nicti-tapetum frame"),
            });
        let (baked_output, baked_output_key) = match pre_baked {
            Some(baked) => {
                let last = req
                    .baked_chain
                    .last()
                    .map(|(id, _)| *id)
                    .ok_or(RenderError::EmptyBakedChain)?;
                (baked, req.graph.cache_key(last)?)
            }
            None => self.run_baked(req, &mut encoder, &mut stats)?,
        };

        let live_key = composite_key(req.graph, req.live_nodes, baked_output_key, req.extent)?;
        let live_output = if self.live_key == Some(live_key) {
            Arc::clone(
                self.live_output
                    .as_ref()
                    .expect("live_key set implies live_output set"),
            )
        } else {
            let output = Arc::new(FrameTexture::new(&self.gpu, req.extent));
            req.live
                .encode(&self.gpu, &mut encoder, &baked_output, &output);
            stats.live_dispatches += 1;
            self.live_key = Some(live_key);
            self.live_output = Some(Arc::clone(&output));
            output
        };

        if !with_geometry {
            self.gpu.queue.submit(Some(encoder.finish()));
            self.last_stats = stats;
            return Ok(live_output);
        }

        let geometry_key =
            composite_key(req.graph, req.geometry_nodes, live_key, req.geometry_extent)?;
        let geometry_output = if self.geometry_key == Some(geometry_key) {
            Arc::clone(
                self.geometry_output
                    .as_ref()
                    .expect("geometry_key set implies geometry_output set"),
            )
        } else {
            let output = Arc::new(FrameTexture::new(&self.gpu, req.geometry_extent));
            req.geometry
                .encode(&self.gpu, &mut encoder, &live_output, &output);
            stats.geometry_dispatches += 1;
            self.geometry_key = Some(geometry_key);
            self.geometry_output = Some(Arc::clone(&output));
            output
        };

        self.gpu.queue.submit(Some(encoder.finish()));
        self.last_stats = stats;
        Ok(geometry_output)
    }
}

/// All node ids a graph's topological order visits, filtered to `Baked` kind -- a convenience for
/// building a `RenderRequest`'s `baked_chain` order without hand-maintaining it.
pub fn baked_ids_in_order(graph: &RenderGraph) -> Result<Vec<String>, GraphError> {
    let order = graph.topological_order()?;
    Ok(order
        .into_iter()
        .filter(|id| {
            graph
                .node(id)
                .map(|n| n.kind == crate::graph::StageKind::Baked)
                == Some(true)
                // `nicti.neutral` is Baked so keys can chain from it, but nothing renders it.
                && id.as_str() != crate::stages::NEUTRAL
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{StageKind, StageNode};
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct CountingBaked(AtomicU32);
    impl BakedExec for CountingBaked {
        fn encode(
            &self,
            _gpu: &GpuContext,
            _encoder: &mut wgpu::CommandEncoder,
            _input: Option<&FrameTexture>,
            _output: &FrameTexture,
        ) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct CountingLive(AtomicU32);
    impl LiveExec for CountingLive {
        fn encode(
            &self,
            _gpu: &GpuContext,
            _encoder: &mut wgpu::CommandEncoder,
            _input: &FrameTexture,
            _output: &FrameTexture,
        ) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct CountingGeometry(AtomicU32);
    impl GeometryExec for CountingGeometry {
        fn encode(
            &self,
            _gpu: &GpuContext,
            _encoder: &mut wgpu::CommandEncoder,
            _input: &FrameTexture,
            _output: &FrameTexture,
        ) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    use crate::test_util::shared_test_gpu as test_gpu;

    /// Builds the hero-shaped graph (decode -> demosaic -> denoise -> lens -> heal, all Baked;
    /// then wb -> tone (Live); then crop (Geometry)) with the given per-node `own_hash` overrides
    /// applied on top of a fixed baseline, matching `graph.rs`'s own test fixture shape.
    fn hero_graph(overrides: &[(&str, &[u8])]) -> RenderGraph {
        let mut g = RenderGraph::new();
        let own_hash = |id: &str| {
            overrides
                .iter()
                .find(|(o, _)| *o == id)
                .map(|(_, bytes)| blake3::hash(bytes))
                .unwrap_or_else(|| blake3::hash(id.as_bytes()))
        };
        let baked = ["decode", "demosaic", "denoise", "lens", "heal"];
        let mut prev: Option<&str> = None;
        for id in baked {
            g.add_node(StageNode {
                id: id.to_string(),
                kind: StageKind::Baked,
                upstream: prev.map(|p| vec![p.to_string()]).unwrap_or_default(),
                own_hash: own_hash(id),
            })
            .unwrap();
            prev = Some(id);
        }
        for id in ["wb", "tone"] {
            g.add_node(StageNode {
                id: id.to_string(),
                kind: StageKind::Live,
                upstream: vec![prev.unwrap().to_string()],
                own_hash: own_hash(id),
            })
            .unwrap();
            prev = Some(id);
        }
        g.add_node(StageNode {
            id: "crop".to_string(),
            kind: StageKind::Geometry,
            upstream: vec![prev.unwrap().to_string()],
            own_hash: own_hash("crop"),
        })
        .unwrap();
        g
    }

    fn baked_chain<'a>(
        graph: &RenderGraph,
        exec: &'a dyn BakedExec,
    ) -> Vec<(&'a str, &'a dyn BakedExec)> {
        // Leak the ids for the test's lifetime -- simplest way to get `&'a str` out of a
        // Vec<String> without restructuring RenderRequest's borrow shape just for tests.
        baked_ids_in_order(graph)
            .unwrap()
            .into_iter()
            .map(|id| (&*Box::leak(id.into_boxed_str()), exec))
            .collect()
    }

    fn extent() -> Extent {
        Extent {
            width: 4,
            height: 4,
        }
    }

    #[test]
    fn render_live_dispatches_no_geometry_and_still_caches_the_live_output() {
        let Some(gpu) = test_gpu() else { return };
        let baked_exec = CountingBaked(AtomicU32::new(0));
        let live_exec = CountingLive(AtomicU32::new(0));
        let geom_exec = CountingGeometry(AtomicU32::new(0));
        let mut renderer = Renderer::new(gpu, 1_000_000_000);
        let g = hero_graph(&[]);
        let chain = baked_chain(&g, &baked_exec);
        let req = RenderRequest {
            graph: &g,
            baked_chain: &chain,
            live: &live_exec,
            live_nodes: &[],
            geometry: &geom_exec,
            geometry_nodes: &[],
            extent: extent(),
            geometry_extent: extent(),
        };

        renderer.render_live(&req).unwrap();
        let first = renderer.last_stats();
        assert_eq!(first.live_dispatches, 1);
        assert_eq!(
            first.geometry_dispatches, 0,
            "no geometry pass for a live-only render"
        );
        assert_eq!(geom_exec.0.load(Ordering::SeqCst), 0);

        renderer.render_live(&req).unwrap();
        assert_eq!(
            renderer.last_stats().live_dispatches,
            0,
            "second render is a cache hit"
        );

        // A following full render still runs geometry (and reuses the cached live output).
        renderer.render(&req).unwrap();
        assert_eq!(renderer.last_stats().live_dispatches, 0);
        assert_eq!(renderer.last_stats().geometry_dispatches, 1);
    }

    #[test]
    fn render_live_from_reuses_a_held_baked_frame_even_with_no_baked_cache() {
        // Export's renderer has a zero baked-cache budget, so "the next render finds the baked
        // frame cached" (what `render_baked` documents) is false there (#354).
        let Some(gpu) = test_gpu() else { return };
        let baked_exec = CountingBaked(AtomicU32::new(0));
        let live_exec = CountingLive(AtomicU32::new(0));
        let geom_exec = CountingGeometry(AtomicU32::new(0));
        let mut renderer = Renderer::new(gpu, 0);
        let g = hero_graph(&[]);
        let chain = baked_chain(&g, &baked_exec);
        let req = RenderRequest {
            graph: &g,
            baked_chain: &chain,
            live: &live_exec,
            live_nodes: &[],
            geometry: &geom_exec,
            geometry_nodes: &[],
            extent: extent(),
            geometry_extent: extent(),
        };

        let baked = renderer.render_baked(&req).unwrap();
        let chain_len = baked_exec.0.load(Ordering::SeqCst);
        assert_eq!(chain_len as usize, chain.len());

        renderer.render_live_from(&req, baked).unwrap();
        assert_eq!(
            baked_exec.0.load(Ordering::SeqCst),
            chain_len,
            "the held frame is used: no baked stage runs again"
        );
        let stats = renderer.last_stats();
        assert_eq!((stats.bake_dispatches, stats.live_dispatches), (0, 1));
        assert_eq!(geom_exec.0.load(Ordering::SeqCst), 0);

        // The contrast that motivates it: plain `render_live` re-bakes at a zero budget.
        let mut renderer = Renderer::new(test_gpu().unwrap(), 0);
        renderer.render_baked(&req).unwrap();
        let before = baked_exec.0.load(Ordering::SeqCst);
        renderer.render_live(&req).unwrap();
        assert!(baked_exec.0.load(Ordering::SeqCst) > before);
    }

    #[test]
    fn a_baked_change_still_forces_a_live_dispatch_with_no_live_nodes() {
        // Regression test: composite_key must not treat an empty live_nodes/geometry_nodes slice
        // as a constant key regardless of what upstream produced -- a graph with zero Live-kind
        // stages wired in yet (a real, reachable state -- e.g. before #45's later slices add one)
        // must still re-run the live/geometry pass when the baked chain's own output changes,
        // not silently reuse a stale cached output forever after the first render.
        let Some(gpu) = test_gpu() else { return };
        let baked_exec = CountingBaked(AtomicU32::new(0));
        let live_exec = CountingLive(AtomicU32::new(0));
        let geom_exec = CountingGeometry(AtomicU32::new(0));
        let mut renderer = Renderer::new(gpu, 1_000_000_000);

        let g1 = hero_graph(&[]);
        let chain1 = baked_chain(&g1, &baked_exec);
        renderer
            .render(&RenderRequest {
                graph: &g1,
                baked_chain: &chain1,
                live: &live_exec,
                live_nodes: &[],
                geometry: &geom_exec,
                geometry_nodes: &[],
                extent: extent(),
                geometry_extent: extent(),
            })
            .unwrap();
        assert_eq!(renderer.last_stats().live_dispatches, 1);
        assert_eq!(renderer.last_stats().geometry_dispatches, 1);

        let g2 = hero_graph(&[("demosaic", b"demosaic-changed")]);
        let chain2 = baked_chain(&g2, &baked_exec);
        renderer
            .render(&RenderRequest {
                graph: &g2,
                baked_chain: &chain2,
                live: &live_exec,
                live_nodes: &[],
                geometry: &geom_exec,
                geometry_nodes: &[],
                extent: extent(),
                geometry_extent: extent(),
            })
            .unwrap();
        let stats = renderer.last_stats();
        assert_eq!(
            stats.bake_dispatches, 4,
            "demosaic/denoise/lens/heal must still rebake"
        );
        assert_eq!(
            stats.live_dispatches, 1,
            "an empty live_nodes slice must not hide a changed baked upstream"
        );
        assert_eq!(
            stats.geometry_dispatches, 1,
            "an empty geometry_nodes slice must not hide a changed live upstream"
        );
    }

    #[test]
    fn rendering_the_same_unchanged_graph_at_a_different_extent_is_never_a_cache_hit() {
        // Regression test: a cache key that ignored `req.extent` could return a texture sized
        // for a previous render's extent (e.g. screen resolution) when asked to render the same
        // unchanged graph at a different one (e.g. full resolution) -- every layer (baked, live,
        // geometry) must treat a changed extent as a fresh dispatch, never a hit.
        let Some(gpu) = test_gpu() else { return };
        let baked_exec = CountingBaked(AtomicU32::new(0));
        let live_exec = CountingLive(AtomicU32::new(0));
        let geom_exec = CountingGeometry(AtomicU32::new(0));
        let mut renderer = Renderer::new(gpu, 1_000_000_000);

        let g = hero_graph(&[]);
        let chain = baked_chain(&g, &baked_exec);
        let screen_res = Extent {
            width: 4,
            height: 4,
        };
        let full_res = Extent {
            width: 8,
            height: 8,
        };

        let out1 = renderer
            .render(&RenderRequest {
                graph: &g,
                baked_chain: &chain,
                live: &live_exec,
                live_nodes: &["wb", "tone"],
                geometry: &geom_exec,
                geometry_nodes: &["crop"],
                extent: screen_res,
                geometry_extent: screen_res,
            })
            .unwrap();
        assert_eq!(out1.extent, screen_res);

        let out2 = renderer
            .render(&RenderRequest {
                graph: &g,
                baked_chain: &chain,
                live: &live_exec,
                live_nodes: &["wb", "tone"],
                geometry: &geom_exec,
                geometry_nodes: &["crop"],
                extent: full_res,
                geometry_extent: full_res,
            })
            .unwrap();
        let stats = renderer.last_stats();
        assert_eq!(
            stats.bake_dispatches, 5,
            "a new extent must re-dispatch every baked stage, not reuse the old-sized cache"
        );
        assert_eq!(stats.live_dispatches, 1);
        assert_eq!(stats.geometry_dispatches, 1);
        assert_eq!(
            out2.extent, full_res,
            "output must actually be sized for the new extent"
        );
    }

    #[test]
    fn a_live_only_change_triggers_zero_bake_dispatches() {
        let Some(gpu) = test_gpu() else { return };
        let baked_exec = CountingBaked(AtomicU32::new(0));
        let live_exec = CountingLive(AtomicU32::new(0));
        let geom_exec = CountingGeometry(AtomicU32::new(0));
        let mut renderer = Renderer::new(gpu, 1_000_000_000);

        let g1 = hero_graph(&[]);
        let chain = baked_chain(&g1, &baked_exec);
        let req1 = RenderRequest {
            graph: &g1,
            baked_chain: &chain,
            live: &live_exec,
            live_nodes: &["wb", "tone"],
            geometry: &geom_exec,
            geometry_nodes: &["crop"],
            extent: extent(),
            geometry_extent: extent(),
        };
        renderer.render(&req1).unwrap();
        assert_eq!(renderer.last_stats().bake_dispatches, 5);
        assert_eq!(renderer.last_stats().live_dispatches, 1);

        // Change only "wb"'s own_hash.
        let g2 = hero_graph(&[("wb", b"wb-changed")]);
        let chain2 = baked_chain(&g2, &baked_exec);
        let req2 = RenderRequest {
            graph: &g2,
            baked_chain: &chain2,
            live: &live_exec,
            live_nodes: &["wb", "tone"],
            geometry: &geom_exec,
            geometry_nodes: &["crop"],
            extent: extent(),
            geometry_extent: extent(),
        };
        renderer.render(&req2).unwrap();
        assert_eq!(
            renderer.last_stats().bake_dispatches,
            0,
            "a live-stage change must trigger zero bake dispatches"
        );
        assert_eq!(renderer.last_stats().live_dispatches, 1);
    }

    /// `render_baked` (used by the mask engine, which needs the baked frame *before* the live
    /// dispatch is recorded) must do the baked work exactly once: it bakes and submits the chain,
    /// and the `render` that follows finds every baked node cached.
    #[test]
    fn render_baked_does_the_baked_work_once_and_the_following_render_reuses_it() {
        let Some(gpu) = test_gpu() else { return };
        let baked_exec = CountingBaked(AtomicU32::new(0));
        let live_exec = CountingLive(AtomicU32::new(0));
        let geom_exec = CountingGeometry(AtomicU32::new(0));
        let mut renderer = Renderer::new(gpu, 1_000_000_000);
        let g = hero_graph(&[]);
        let chain = baked_chain(&g, &baked_exec);
        let req = RenderRequest {
            graph: &g,
            baked_chain: &chain,
            live: &live_exec,
            live_nodes: &["wb", "tone"],
            geometry: &geom_exec,
            geometry_nodes: &["crop"],
            extent: extent(),
            geometry_extent: extent(),
        };
        let baked = renderer.render_baked(&req).unwrap();
        assert_eq!(baked.extent, extent());
        assert_eq!(baked_exec.0.load(Ordering::SeqCst), 5, "the chain ran once");
        assert_eq!(live_exec.0.load(Ordering::SeqCst), 0, "no live work yet");

        renderer.render(&req).unwrap();
        assert_eq!(
            baked_exec.0.load(Ordering::SeqCst),
            5,
            "render reuses the baked outputs render_baked left in the cache"
        );
        assert_eq!(renderer.last_stats().bake_dispatches, 0);
        assert_eq!(renderer.last_stats().live_dispatches, 1);

        // And it is itself a cache hit the second time.
        renderer.render_baked(&req).unwrap();
        assert_eq!(baked_exec.0.load(Ordering::SeqCst), 5);
    }

    /// When the byte budget holds fewer frames than the chain is long, earlier nodes are evicted as
    /// later ones are inserted. A render must resume after the last node still cached, not re-run
    /// the chain from the top just to reach a cached final output -- `render_baked` runs on every
    /// frame of a mask-carrying document, so that would redo the whole bake each time.
    #[test]
    fn a_tiny_cache_budget_does_not_make_every_render_rebake_the_chain() {
        let Some(gpu) = test_gpu() else { return };
        let one_frame = FrameTexture::new(&gpu, extent()).byte_size();
        let baked_exec = CountingBaked(AtomicU32::new(0));
        let live_exec = CountingLive(AtomicU32::new(0));
        let geom_exec = CountingGeometry(AtomicU32::new(0));
        let mut renderer = Renderer::new(gpu, one_frame);
        let g = hero_graph(&[]);
        let chain = baked_chain(&g, &baked_exec);
        let req = RenderRequest {
            graph: &g,
            baked_chain: &chain,
            live: &live_exec,
            live_nodes: &["wb", "tone"],
            geometry: &geom_exec,
            geometry_nodes: &["crop"],
            extent: extent(),
            geometry_extent: extent(),
        };
        renderer.render_baked(&req).unwrap();
        assert_eq!(
            baked_exec.0.load(Ordering::SeqCst),
            5,
            "first time: the whole chain"
        );
        for _ in 0..3 {
            renderer.render_baked(&req).unwrap();
            renderer.render(&req).unwrap();
        }
        assert_eq!(
            baked_exec.0.load(Ordering::SeqCst),
            5,
            "the last node stayed cached, so nothing upstream of it runs again"
        );
        assert_eq!(renderer.last_stats().bake_dispatches, 0);
    }

    #[test]
    fn the_keying_only_neutral_node_is_not_part_of_the_baked_chain() {
        let ids = baked_ids_in_order(&crate::spine::build_graph()).unwrap();
        assert!(
            !ids.iter().any(|id| id == crate::stages::NEUTRAL),
            "{ids:?}"
        );
        assert_eq!(
            ids,
            crate::spine::BAKED_IDS,
            "exactly the real baked prefix"
        );
    }

    #[test]
    fn render_baked_needs_a_baked_chain() {
        let Some(gpu) = test_gpu() else { return };
        let live_exec = CountingLive(AtomicU32::new(0));
        let geom_exec = CountingGeometry(AtomicU32::new(0));
        let mut renderer = Renderer::new(gpu, 1_000_000);
        let g = hero_graph(&[]);
        let err = renderer.render_baked(&RenderRequest {
            graph: &g,
            baked_chain: &[],
            live: &live_exec,
            live_nodes: &[],
            geometry: &geom_exec,
            geometry_nodes: &[],
            extent: extent(),
            geometry_extent: extent(),
        });
        assert!(matches!(err, Err(RenderError::EmptyBakedChain)));
    }

    #[test]
    fn reordering_live_nodes_between_renders_is_still_a_cache_hit() {
        // Regression test: `composite_key` must not depend on the caller's slice order --
        // `live_nodes: &["wb", "tone"]` then `&["tone", "wb"]` (same set, reordered) is the
        // *same* set of live stages and neither changed, so this must still be a cache hit (0
        // live dispatches), not a spurious re-dispatch from the composite key hashing
        // differently purely due to argument order.
        let Some(gpu) = test_gpu() else { return };
        let baked_exec = CountingBaked(AtomicU32::new(0));
        let live_exec = CountingLive(AtomicU32::new(0));
        let geom_exec = CountingGeometry(AtomicU32::new(0));
        let mut renderer = Renderer::new(gpu, 1_000_000_000);

        let g = hero_graph(&[]);
        let chain = baked_chain(&g, &baked_exec);
        renderer
            .render(&RenderRequest {
                graph: &g,
                baked_chain: &chain,
                live: &live_exec,
                live_nodes: &["wb", "tone"],
                geometry: &geom_exec,
                geometry_nodes: &["crop"],
                extent: extent(),
                geometry_extent: extent(),
            })
            .unwrap();
        assert_eq!(renderer.last_stats().live_dispatches, 1);

        renderer
            .render(&RenderRequest {
                graph: &g,
                baked_chain: &chain,
                live: &live_exec,
                live_nodes: &["tone", "wb"],
                geometry: &geom_exec,
                geometry_nodes: &["crop"],
                extent: extent(),
                geometry_extent: extent(),
            })
            .unwrap();
        assert_eq!(
            renderer.last_stats().live_dispatches,
            0,
            "reordering the same set of live nodes must not cost a dispatch"
        );
    }

    #[test]
    fn a_crop_only_change_triggers_zero_bake_and_zero_live_dispatches() {
        let Some(gpu) = test_gpu() else { return };
        let baked_exec = CountingBaked(AtomicU32::new(0));
        let live_exec = CountingLive(AtomicU32::new(0));
        let geom_exec = CountingGeometry(AtomicU32::new(0));
        let mut renderer = Renderer::new(gpu, 1_000_000_000);

        let g1 = hero_graph(&[]);
        let chain1 = baked_chain(&g1, &baked_exec);
        renderer
            .render(&RenderRequest {
                graph: &g1,
                baked_chain: &chain1,
                live: &live_exec,
                live_nodes: &["wb", "tone"],
                geometry: &geom_exec,
                geometry_nodes: &["crop"],
                extent: extent(),
                geometry_extent: extent(),
            })
            .unwrap();

        let g2 = hero_graph(&[("crop", b"crop-changed")]);
        let chain2 = baked_chain(&g2, &baked_exec);
        renderer
            .render(&RenderRequest {
                graph: &g2,
                baked_chain: &chain2,
                live: &live_exec,
                live_nodes: &["wb", "tone"],
                geometry: &geom_exec,
                geometry_nodes: &["crop"],
                extent: extent(),
                geometry_extent: extent(),
            })
            .unwrap();

        let stats = renderer.last_stats();
        assert_eq!(stats.bake_dispatches, 0);
        assert_eq!(stats.live_dispatches, 0);
        assert_eq!(stats.geometry_dispatches, 1);
    }

    /// #272: only the geometry pass follows `geometry_extent`; a change to it costs one geometry
    /// dispatch and no bake or live work, and the output is sized to it.
    #[test]
    fn a_geometry_extent_change_redispatches_only_the_geometry_pass() {
        let Some(gpu) = test_gpu() else { return };
        let baked_exec = CountingBaked(AtomicU32::new(0));
        let live_exec = CountingLive(AtomicU32::new(0));
        let geom_exec = CountingGeometry(AtomicU32::new(0));
        let mut renderer = Renderer::new(gpu, 1_000_000_000);
        let g = hero_graph(&[]);
        let chain = baked_chain(&g, &baked_exec);
        let small = Extent {
            width: extent().width / 2,
            height: extent().height / 2,
        };
        let mut render = |geometry_extent: Extent| {
            let out = renderer
                .render(&RenderRequest {
                    graph: &g,
                    baked_chain: &chain,
                    live: &live_exec,
                    live_nodes: &["wb", "tone"],
                    geometry: &geom_exec,
                    geometry_nodes: &["crop"],
                    extent: extent(),
                    geometry_extent,
                })
                .unwrap();
            (out.extent, renderer.last_stats())
        };

        let (e, _) = render(extent());
        assert_eq!(e, extent());
        let (e, stats) = render(small);
        assert_eq!(e, small, "the output is sized to geometry_extent");
        assert_eq!(stats.bake_dispatches, 0);
        assert_eq!(stats.live_dispatches, 0);
        assert_eq!(stats.geometry_dispatches, 1);
        let (_, stats) = render(small);
        assert_eq!(stats.geometry_dispatches, 0, "an unchanged extent is a hit");
        let (e, stats) = render(extent());
        assert_eq!(e, extent());
        assert_eq!(stats.geometry_dispatches, 1);
    }

    #[test]
    fn an_identical_re_render_triggers_zero_dispatches_of_any_kind() {
        let Some(gpu) = test_gpu() else { return };
        let baked_exec = CountingBaked(AtomicU32::new(0));
        let live_exec = CountingLive(AtomicU32::new(0));
        let geom_exec = CountingGeometry(AtomicU32::new(0));
        let mut renderer = Renderer::new(gpu, 1_000_000_000);

        let g = hero_graph(&[]);
        let chain = baked_chain(&g, &baked_exec);
        let req = RenderRequest {
            graph: &g,
            baked_chain: &chain,
            live: &live_exec,
            live_nodes: &["wb", "tone"],
            geometry: &geom_exec,
            geometry_nodes: &["crop"],
            extent: extent(),
            geometry_extent: extent(),
        };
        renderer.render(&req).unwrap();
        renderer.render(&req).unwrap();

        let stats = renderer.last_stats();
        assert_eq!(stats.bake_dispatches, 0);
        assert_eq!(stats.live_dispatches, 0);
        assert_eq!(stats.geometry_dispatches, 0);
    }

    #[test]
    fn undo_to_a_prior_baked_state_is_a_cache_hit() {
        let Some(gpu) = test_gpu() else { return };
        let baked_exec = CountingBaked(AtomicU32::new(0));
        let live_exec = CountingLive(AtomicU32::new(0));
        let geom_exec = CountingGeometry(AtomicU32::new(0));
        let mut renderer = Renderer::new(gpu, 1_000_000_000);

        let g1 = hero_graph(&[]);
        let chain1 = baked_chain(&g1, &baked_exec);
        renderer
            .render(&RenderRequest {
                graph: &g1,
                baked_chain: &chain1,
                live: &live_exec,
                live_nodes: &["wb", "tone"],
                geometry: &geom_exec,
                geometry_nodes: &["crop"],
                extent: extent(),
                geometry_extent: extent(),
            })
            .unwrap();

        let g2 = hero_graph(&[("demosaic", b"demosaic-changed")]);
        let chain2 = baked_chain(&g2, &baked_exec);
        renderer
            .render(&RenderRequest {
                graph: &g2,
                baked_chain: &chain2,
                live: &live_exec,
                live_nodes: &["wb", "tone"],
                geometry: &geom_exec,
                geometry_nodes: &["crop"],
                extent: extent(),
                geometry_extent: extent(),
            })
            .unwrap();
        assert_eq!(renderer.last_stats().bake_dispatches, 4); // demosaic, denoise, lens, heal

        // "Undo": back to the original demosaic own_hash.
        let g3 = hero_graph(&[]);
        let chain3 = baked_chain(&g3, &baked_exec);
        renderer
            .render(&RenderRequest {
                graph: &g3,
                baked_chain: &chain3,
                live: &live_exec,
                live_nodes: &["wb", "tone"],
                geometry: &geom_exec,
                geometry_nodes: &["crop"],
                extent: extent(),
                geometry_extent: extent(),
            })
            .unwrap();
        assert_eq!(
            renderer.last_stats().bake_dispatches,
            0,
            "reverting to a previously-baked state should hit the cache, not re-dispatch"
        );
    }

    #[test]
    fn a_demosaic_change_rebakes_exactly_the_downstream_baked_stages() {
        let Some(gpu) = test_gpu() else { return };
        let baked_exec = CountingBaked(AtomicU32::new(0));
        let live_exec = CountingLive(AtomicU32::new(0));
        let geom_exec = CountingGeometry(AtomicU32::new(0));
        let mut renderer = Renderer::new(gpu, 1_000_000_000);

        let g1 = hero_graph(&[]);
        let chain1 = baked_chain(&g1, &baked_exec);
        renderer
            .render(&RenderRequest {
                graph: &g1,
                baked_chain: &chain1,
                live: &live_exec,
                live_nodes: &["wb", "tone"],
                geometry: &geom_exec,
                geometry_nodes: &["crop"],
                extent: extent(),
                geometry_extent: extent(),
            })
            .unwrap();

        let g2 = hero_graph(&[("demosaic", b"demosaic-changed")]);
        let invalidated = g2.invalidated_bakes("demosaic").unwrap();
        let expected_bakes: HashSet<&str> = invalidated.iter().map(String::as_str).collect();
        // Confirm this matches the ADR's own claim before using it as the expected count.
        assert_eq!(
            expected_bakes,
            ["demosaic", "denoise", "lens", "heal"]
                .into_iter()
                .collect()
        );

        let chain2 = baked_chain(&g2, &baked_exec);
        renderer
            .render(&RenderRequest {
                graph: &g2,
                baked_chain: &chain2,
                live: &live_exec,
                live_nodes: &["wb", "tone"],
                geometry: &geom_exec,
                geometry_nodes: &["crop"],
                extent: extent(),
                geometry_extent: extent(),
            })
            .unwrap();

        assert_eq!(
            renderer.last_stats().bake_dispatches,
            expected_bakes.len() as u32
        );
    }
}
