//! The render half shared by export (#57) and the rendered-preview job (#145 "Eyeshine"): the
//! kernels/graph/renderer one batch reuses across photos, and the "document -> live-suffix frame"
//! step. Each caller then tiles the crop itself, at its own output size.

use std::sync::Arc;

use nicti_calico::dcp::DcpProfile;
use nicti_cornea::LinearFrame;
use nicti_pawprint::EditDocument;
use nicti_tapetum::frame::{Extent, FrameTexture};
use nicti_tapetum::gpu::GpuContext;
use nicti_tapetum::graph::RenderGraph;
use nicti_tapetum::heal::{HealExec, HealKernel, RemovalSet};
use nicti_tapetum::renderer::{BakedExec, RenderRequest, Renderer};
use nicti_tapetum::spine::{self, build_graph, build_registry, RenderInputs, LIVE_IDS};
use nicti_tapetum::stages::{
    CropKernel, DecodeExec, DecodeKernel, LiveSuffixKernel, PassthroughExec, CROP, DECODE,
    DEMOSAIC, DENOISE, HEAL, LENS,
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
        }
    }
}

/// A photo's full-extent live-suffix frame plus what the crop pass needs.
pub(crate) struct LiveRender {
    pub(crate) live: Arc<FrameTexture>,
    pub(crate) inputs: RenderInputs,
}

/// Applies `edit` (with the photo's identity stamped in) to the graph and renders the whole frame
/// through the live suffix at native resolution. `Err` carries the user-facing failure text.
pub(crate) fn render_live_frame(
    ctx: &mut ExportRenderer,
    gpu: &Arc<GpuContext>,
    edit: &EditDocument,
    identity: blake3::Hash,
    frame: &LinearFrame,
    profile: Option<&DcpProfile>,
    look: Option<&nicti_calico::xmp_profile::LookProfile>,
) -> Result<LiveRender, String> {
    // The photo's identity goes into the render document, not `set_own_hash` -- see
    // `spine::stamp_source_identity` for why the latter is silently undone.
    let mut doc = edit.clone();
    spine::stamp_source_identity(&mut doc, identity);
    ctx.graph
        .apply_document(&doc, &ctx.registry)
        .map_err(|e| format!("internal error applying the edit: {e:?}"))?;
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
        geometry_nodes: &[CROP],
        extent,
    };
    let live = ctx
        .renderer
        .render_live(&req)
        .map_err(|e| format!("render failed: {e:?}"))?;
    Ok(LiveRender { live, inputs })
}
