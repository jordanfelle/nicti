//! The render half shared by export (#57) and the rendered-preview job (#145 "Eyeshine"): the
//! kernels/graph/renderer one batch reuses across photos, and the "document -> live-suffix frame"
//! step. Each caller then tiles the crop itself, at its own output size.

use std::collections::HashMap;
use std::sync::Arc;

use nicti_calico::dcp::DcpProfile;
use nicti_cornea::LinearFrame;
use nicti_pawprint::EditDocument;
use nicti_tapetum::frame::{Extent, FrameTexture};
use nicti_tapetum::gpu::GpuContext;
use nicti_tapetum::graph::RenderGraph;
use nicti_tapetum::heal::{HealExec, HealKernel, RemovalSet};
use nicti_tapetum::mask::compose as mask_compose;
use nicti_tapetum::mask::engine::{AiAlpha, MaskEngine, MaskInputs};
use nicti_tapetum::mask::params::MaskParams;
use nicti_tapetum::renderer::{BakedExec, RenderRequest, Renderer};
use nicti_tapetum::spine::{
    self, build_graph, build_registry, RenderInputs, GEOMETRY_IDS, LIVE_IDS,
};
use nicti_tapetum::stages::{
    CropKernel, DecodeExec, DecodeKernel, LiveSuffixKernel, PassthroughExec, DECODE, DEMOSAIC,
    DENOISE, HEAL, LENS, MASKS, NEUTRAL,
};
use nicti_tapetum::StageRegistry;

/// The kernels/graph/renderer one batch reuses across photos. Built once, on the GPU lane.
pub(crate) struct ExportRenderer {
    pub(crate) decode_kernel: DecodeKernel,
    pub(crate) live_kernel: LiveSuffixKernel,
    pub(crate) crop_kernel: CropKernel,
    pub(crate) heal_kernel: HealKernel,
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
            graph: build_graph(),
            registry: build_registry(),
            // A zero baked-cache budget: consecutive photos never share baked output, and a full
            // frame texture is ~350 MB of VRAM not worth keeping.
            renderer: Renderer::new(Arc::clone(gpu), 0),
            mask_engine: None,
        }
    }
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
    /// `Some(baked AI alphas by bake key)` applies the photo's local adjustments (#354; export),
    /// at the frame's own extent rather than Develop's 4096 px cap. `None` leaves them out (the
    /// rendered-preview job, which marks such previews partial instead).
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
    let passthrough = PassthroughExec;
    let baked_chain: Vec<(&str, &dyn BakedExec)> = vec![
        (DECODE, &decode_exec),
        (DEMOSAIC, &passthrough),
        (DENOISE, &passthrough),
        (LENS, &passthrough),
        (HEAL, &heal_exec),
    ];
    let req = RenderRequest {
        graph: &ctx.graph,
        baked_chain: &baked_chain,
        live: &ctx.live_kernel,
        live_nodes: &LIVE_IDS,
        geometry: &ctx.crop_kernel,
        geometry_nodes: &GEOMETRY_IDS,
        extent,
    };
    // Local corrections only when the caller asked for them (`masks`); the global Presence's
    // clarity/texture/dehaze (#380) always needs the baked frame and bases, masks or not -- a
    // rendered preview (`masks: None`) still shows them, with an empty mask set.
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
