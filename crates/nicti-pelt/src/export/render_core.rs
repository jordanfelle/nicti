//! The render half shared by export (#57) and the rendered-preview job (#145 "Eyeshine"): the
//! kernels/graph/renderer one batch reuses across photos, and the "document -> live-suffix frame"
//! step. Each caller then tiles the crop itself, at its own output size.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nicti_calico::dcp::DcpProfile;
use nicti_cornea::LinearFrame;
use nicti_pawprint::EditDocument;
use nicti_pounce::Submitter;
use nicti_tapetum::frame::{Extent, FrameTexture};
use nicti_tapetum::gpu::GpuContext;
use nicti_tapetum::graph::RenderGraph;
use nicti_tapetum::heal::{HealExec, HealKernel, RemovalSet};
use nicti_tapetum::mask::compose as mask_compose;
use nicti_tapetum::mask::engine::{AiAlpha, MaskEngine, MaskInputs};
use nicti_tapetum::mask::params::MaskParams;
use nicti_tapetum::renderer::{BakedExec, RenderRequest, Renderer};
use nicti_tapetum::slit::{LensExec, LensKernel};
use nicti_tapetum::spine::{
    self, build_graph, build_registry, RenderInputs, GEOMETRY_IDS, LIVE_IDS,
};
use nicti_tapetum::stages::{
    CropKernel, DecodeExec, DecodeKernel, LiveSuffixKernel, PassthroughExec, DECODE, DEMOSAIC,
    DENOISE, HEAL, LENS, MASKS, NEUTRAL,
};
use nicti_tapetum::StageRegistry;

use crate::stash::fetch_alpha;
use crate::t2::SharedLarder;

/// The kernels/graph/renderer one batch reuses across photos. Built once, on the GPU lane.
pub(crate) struct ExportRenderer {
    pub(crate) decode_kernel: DecodeKernel,
    pub(crate) live_kernel: LiveSuffixKernel,
    pub(crate) crop_kernel: CropKernel,
    pub(crate) heal_kernel: HealKernel,
    pub(crate) lens_kernel: LensKernel,
    pub(crate) graph: RenderGraph,
    pub(crate) registry: StageRegistry,
    pub(crate) renderer: Renderer,
    /// Builds local-adjustment masks at the frame's own extent, uncached (#354). Created on the
    /// first photo that needs it (a mask, or a global clarity/texture/dehaze, #380): it compiles its
    /// own pipelines, which a batch with neither should not pay for.
    pub(crate) mask_engine: Option<MaskEngine>,
}

impl ExportRenderer {
    pub(crate) fn new(gpu: &Arc<GpuContext>) -> Self {
        ExportRenderer {
            decode_kernel: DecodeKernel::new(gpu),
            live_kernel: LiveSuffixKernel::new(gpu),
            crop_kernel: CropKernel::new(gpu),
            heal_kernel: HealKernel::new(gpu),
            lens_kernel: LensKernel::new(gpu),
            graph: build_graph(),
            registry: build_registry(),
            // A zero baked-cache budget: consecutive photos never share baked output, and a full
            // frame texture is ~350 MB of VRAM not worth keeping.
            renderer: Renderer::new(Arc::clone(gpu), 0),
            mask_engine: None,
        }
    }
}

/// How many nodes the baked chain `render_live_frame` runs has (decode, demosaic, denoise, lens,
/// heal). Asserted there, so the VRAM estimate below can't silently go stale.
const BAKED_CHAIN_NODES: u64 = 5;
/// Full-extent frames the live suffix allocates beyond its output when sharpen, noise reduction
/// or local detail is on: `stage_a` plus two blurs of a horizontal and a vertical pass each
/// (`LiveSuffixKernel::encode`/`run_blur`). Counted unconditionally -- the bound is per photo
/// worst case, not per edit.
const DETAIL_SCRATCH_FRAMES: u64 = 5;
/// The renderer keeps the previous photo's live output until the next one replaces it
/// (`Renderer::live_output`), and the kernels are reused across photos.
const RETAINED_LIVE_FRAMES: u64 = 1;
/// The decode stage's packed u16 RGB strip buffers: 6 bytes/pixel, rounded up to a whole frame.
const DECODE_BUFFER_FRAMES: u64 = 1;

/// A bound on the VRAM one render of an `extent` frame holds (#255), for the job's declared
/// `vram_bytes`. With a zero baked-cache budget the renderer still allocates one full-extent
/// `FrameTexture` per baked-chain node plus one for the live suffix, and all stay referenced by
/// the single command encoder until it is submitted, so they are live together; the detail
/// scratch, retained previous output and decode buffers are on top (consts above).
/// `tile_bytes` is the tiled crop pass's staging cap (`TileBudget::max_staging_bytes`); a padded
/// input and output tile are each bounded by it. **Not counted:** the mask engine's atlas, bases
/// and guided textures (only when a photo has masks, or global clarity/texture/dehaze; several GB
/// at 45 MP per `render_live_frame`), so for such photos this is a lower bound; see
/// [`has_local_masks`] and its use for the rendered preview.
pub(crate) fn render_vram_estimate(extent: Extent, tile_bytes: u64) -> u64 {
    let frame = u64::from(extent.width) * u64::from(extent.height) * 8;
    let frames =
        BAKED_CHAIN_NODES + 1 + DETAIL_SCRATCH_FRAMES + RETAINED_LIVE_FRAMES + DECODE_BUFFER_FRAMES;
    frames * frame + 2 * tile_bytes
}

/// True when `edit` has local adjustments that make `render_live_frame` build the mask atlas.
pub(crate) fn has_local_masks(edit: &EditDocument) -> bool {
    spine::resolve::<MaskParams>(edit, MASKS)
        .active()
        .next()
        .is_some()
}

/// [`render_vram_estimate`] clamped to the GPU lane's total budget. Pounce drops a background job
/// declaring more than the total outright (no terminal status, so its result slot would never
/// settle), so a frame bigger than the whole lane is declared as the entire lane instead: it runs
/// alone rather than not at all. `0` once the runtime is gone (the submit is refused anyway).
pub(crate) fn declared_render_vram(extent: Extent, tile_bytes: u64, submitter: &Submitter) -> u64 {
    submitter.gpu_vram_budget().map_or(0, |budget| {
        render_vram_estimate(extent, tile_bytes).min(budget)
    })
}

/// A photo's full-extent live-suffix frame plus what the crop pass needs.
pub(crate) struct LiveRender {
    pub(crate) live: Arc<FrameTexture>,
    pub(crate) inputs: RenderInputs,
}

/// One photo's inputs to [`render_live_frame`].
pub(crate) struct LiveSource<'a> {
    pub(crate) edit: &'a EditDocument,
    pub(crate) identity: blake3::Hash,
    pub(crate) frame: &'a LinearFrame,
    pub(crate) profile: Option<&'a DcpProfile>,
    pub(crate) look: Option<&'a nicti_calico::xmp_profile::LookProfile>,
    /// `Some(baked AI alphas by bake key)` applies the photo's local adjustments (#354; export and
    /// the rendered-preview job, #399), at the frame's own extent rather than Develop's 4096 px
    /// cap. `None` leaves them out.
    pub(crate) masks: Option<&'a HashMap<blake3::Hash, Arc<AiAlpha>>>,
}

/// Applies `edit` (with the photo's identity stamped in) to the graph and renders the whole frame
/// through the live suffix at native resolution. `Err` carries the user-facing failure text.
pub(crate) fn render_live_frame(
    ctx: &mut ExportRenderer,
    gpu: &Arc<GpuContext>,
    src: LiveSource<'_>,
) -> Result<LiveRender, String> {
    let LiveSource {
        edit,
        identity,
        frame,
        profile,
        look,
        masks,
    } = src;
    // The photo's identity goes into the render document, not `set_own_hash` -- see
    // `spine::stamp_source_identity` for why the latter is silently undone.
    let mut doc = edit.clone();
    spine::stamp_source_identity(&mut doc, identity);
    ctx.graph
        .apply_document(&doc, &ctx.registry)
        .map_err(|e| format!("internal error applying the edit: {e:?}"))?;
    if let Some(alphas) = masks {
        // Which AI alphas are ready goes into the masks entry (as in Develop), so the live
        // composite's key follows them. It needs the neutral render's key, known once the identity
        // is applied above -- hence the second, nearly free `apply_document`.
        let neutral_key = ctx
            .graph
            .cache_key(NEUTRAL)
            .map_err(|e| format!("internal error keying the masks: {e:?}"))?;
        let ready: HashMap<blake3::Hash, blake3::Hash> =
            alphas.iter().map(|(k, a)| (*k, a.content_hash)).collect();
        mask_compose::stamp_ai_alpha_state(&mut doc, neutral_key, &ready);
        ctx.graph
            .apply_document(&doc, &ctx.registry)
            .map_err(|e| format!("internal error applying the edit: {e:?}"))?;
    }
    let extent = Extent {
        width: frame.width,
        height: frame.height,
    };
    // Always full resolution: pixel_scale 1.0.
    let inputs = spine::resolve_inputs(&doc, frame, extent, profile, look, 1.0);
    ctx.live_kernel.set_params(gpu, &inputs.live);

    let decode_exec = DecodeExec {
        kernel: &ctx.decode_kernel,
        frame,
    };
    // Clone/heal spots render from the document; AI removal patches aren't persisted (#324), so an
    // empty set means those spots are skipped.
    let no_removals = RemovalSet::new();
    let heal_exec = HealExec {
        kernel: &ctx.heal_kernel,
        params: &inputs.heal,
        removals: &no_removals,
    };
    let lens_exec = LensExec {
        kernel: &ctx.lens_kernel,
        params: &inputs.lens,
        frame,
    };
    let passthrough = PassthroughExec;
    let baked_chain: Vec<(&str, &dyn BakedExec)> = vec![
        (DECODE, &decode_exec),
        (DEMOSAIC, &passthrough),
        (DENOISE, &passthrough),
        (LENS, &lens_exec),
        (HEAL, &heal_exec),
    ];
    debug_assert_eq!(baked_chain.len() as u64, BAKED_CHAIN_NODES);
    let req = RenderRequest {
        graph: &ctx.graph,
        baked_chain: &baked_chain,
        live: &ctx.live_kernel,
        live_nodes: &LIVE_IDS,
        geometry: &ctx.crop_kernel,
        geometry_nodes: &GEOMETRY_IDS,
        extent,
        geometry_extent: extent,
    };
    // Local corrections only when the caller asked for them (`masks`); the global Presence's
    // clarity/texture/dehaze (#380) always needs the baked frame and bases, masks or not -- a
    // caller that passes no masks (`masks: None`) still shows them, with an empty mask set.
    let doc_masks: MaskParams = spine::resolve(&doc, MASKS);
    let no_masks = MaskParams::default();
    let no_alphas = HashMap::new();
    let wants_locals = masks.is_some() && doc_masks.active().next().is_some();
    let mask_params = if wants_locals { &doc_masks } else { &no_masks };
    let alphas = masks.unwrap_or(&no_alphas);
    let live = {
        if wants_locals || inputs.live.presence.needs_bases() {
            // The engine needs the *baked* frame (AI refines and range masks follow it). This
            // renderer's baked cache has a zero budget, so hand the frame straight to the live
            // pass rather than rely on the cache (Develop's way) -- else it would bake twice.
            let baked = ctx
                .renderer
                .render_baked(&req)
                .map_err(|e| format!("render failed: {e:?}"))?;
            let neutral_key = ctx
                .graph
                .cache_key(NEUTRAL)
                .map_err(|e| format!("internal error keying the masks: {e:?}"))?;
            let guide_key = ctx
                .graph
                .cache_key(HEAL)
                .map_err(|e| format!("internal error keying the masks: {e:?}"))?;
            // Range masks measure the frame as shot: the as-shot matrix, as in Develop.
            let range_matrix = nicti_tapetum::color::camera_to_working_space_matrix(
                frame.cam_mul,
                &frame.cam_xyz,
                &nicti_tapetum::coat::WbParams::default(),
            );
            // Native resolution: only the device's texture limit caps the mask extent.
            let engine = ctx.mask_engine.get_or_insert_with(|| {
                MaskEngine::for_export(gpu, gpu.limits.max_texture_dimension_2d)
            });
            let mask_frame = engine.prepare(
                gpu,
                &MaskInputs {
                    params: mask_params,
                    ai_alphas: alphas,
                    neutral_key,
                    guide: &baked,
                    guide_key,
                    range_matrix,
                    presence: inputs.live.presence,
                },
            );
            ctx.live_kernel.set_masks(gpu, mask_frame.as_ref());
            ctx.renderer.render_live_from(&req, baked)
        } else {
            // This kernel is reused across photos: never leave the previous photo's atlas bound.
            ctx.live_kernel.set_masks(gpu, None);
            ctx.renderer.render_live(&req)
        }
    };
    // The live pass is submitted: unbind the atlas and free the engine's textures (several GB
    // at 45 MP with many corrections) now rather than hold them through the tile loop, the next photo, or idle.
    ctx.live_kernel.set_masks(gpu, None);
    if let Some(engine) = ctx.mask_engine.as_mut() {
        engine.release();
    }
    let live = live.map_err(|e| format!("render failed: {e:?}"))?;
    Ok(LiveRender { live, inputs })
}

/// How long a render waits for the Larder in total, per photo, across all its alphas. Off the UI
/// thread, and a miss would drop a mask from the file, so this waits longer than an interactive
/// fetch does; a shared budget (not one per alpha) bounds a photo with many AI corrections when the
/// Larder is held by something long like a compaction.
const ALPHA_LOCK_WAIT: Duration = Duration::from_secs(30);

/// The baked AI-mask alphas stored for `edit` (#354), plus how many were missing. Neither export
/// nor the rendered preview bakes: an alpha that isn't stored is left out (the engine skips that
/// component, as Develop does while a bake is pending). `cancelled` stops the loop early.
pub(crate) fn load_stored_alphas(
    larder: Option<&SharedLarder>,
    asset_id: i64,
    edit: &EditDocument,
    identity: blake3::Hash,
    cancelled: impl Fn() -> bool,
) -> (HashMap<blake3::Hash, Arc<AiAlpha>>, usize) {
    let params: MaskParams = spine::resolve(edit, MASKS);
    // Only corrections that change pixels: `bake_requests` also covers enabled-but-inert ones (a
    // fresh Select Subject with no slider moved), which a render neither needs nor should wait on.
    let active = MaskParams {
        corrections: params.active().cloned().collect(),
    };
    let neutral_key = spine::neutral_key(edit, identity);
    let wanted = mask_compose::bake_requests(&active, neutral_key);
    let mut found = HashMap::new();
    let mut missing = 0usize;
    let deadline = Instant::now() + ALPHA_LOCK_WAIT;
    for request in &wanted {
        if cancelled() {
            break;
        }
        // What is left of the photo's budget (zero still makes one non-waiting attempt).
        let left = deadline.saturating_duration_since(Instant::now());
        match larder.and_then(|l| fetch_alpha(l, asset_id, &request.key, left)) {
            Some(a) => {
                found.insert(request.key, a);
            }
            None => missing += 1,
        }
    }
    (found, missing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_pounce::Pounce;

    const MIB: u64 = 1024 * 1024;

    #[test]
    fn estimate_counts_every_full_extent_frame_plus_tile_scratch() {
        // 8280 x 5520 is ADR-0044's ~349 MB full-res frame: 5 baked + 1 live + 5 detail scratch
        // + 1 retained + 1 decode = 13 of them, plus two 64 MiB tiles.
        let extent = Extent {
            width: 8280,
            height: 5520,
        };
        let frame = 8280u64 * 5520 * 8;
        assert_eq!(
            render_vram_estimate(extent, 64 * MIB),
            13 * frame + 128 * MIB
        );
    }

    #[test]
    fn declaration_is_clamped_to_the_lane_budget_not_dropped() {
        let big = Extent {
            width: 8280,
            height: 5520,
        };
        let small = Extent {
            width: 64,
            height: 64,
        };
        let pounce = Pounce::new(1024 * MIB, 1, 1, || {});
        let sub = pounce.submitter();
        // A frame whose estimate exceeds the whole lane declares the whole lane (it runs alone).
        assert!(render_vram_estimate(big, 64 * MIB) > 1024 * MIB);
        assert_eq!(declared_render_vram(big, 64 * MIB, &sub), 1024 * MIB);
        // A small frame declares its own, nonzero, estimate.
        let est = render_vram_estimate(small, 64 * MIB);
        assert_eq!(declared_render_vram(small, 64 * MIB, &sub), est);
        assert!(est > 0);
    }

    #[test]
    fn declaration_is_zero_once_the_runtime_is_gone() {
        let sub = Pounce::new(1024 * MIB, 1, 1, || {}).submitter();
        let extent = Extent {
            width: 64,
            height: 64,
        };
        assert_eq!(declared_render_vram(extent, 64 * MIB, &sub), 0);
    }
}
