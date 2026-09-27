//! Wires a synthetic `LinearFrame` through the real Tapetum pipeline (decode -> demosaic/denoise/
//! lens/heal passthrough -> fused live suffix -> crop), so the Develop placeholder panel has a
//! real, non-mock render to display. Loading an actual NEF is #31's (loupe) scope -- that needs
//! the `libraw` feature, which this crate deliberately never forwards (`nicti-cornea`'s own
//! feature-gate doc comment: a caller wanting the real decoder enables it explicitly, this crate
//! isn't that caller yet). Graph-building shape copied from `bench/knead/src/lib.rs::build_graph`
//! (not depended on -- `bench/knead` isn't a production crate, see its own Cargo.toml).

use std::sync::Arc;

use nicti_cornea::LinearFrame;
use nicti_tapetum::color;
use nicti_tapetum::frame::{Extent, FrameTexture};
use nicti_tapetum::geometry::Affine2D;
use nicti_tapetum::gpu::GpuContext;
use nicti_tapetum::graph::{RenderGraph, StageKind, StageNode};
use nicti_tapetum::renderer::{BakedExec, RenderRequest, Renderer};
use nicti_tapetum::stages::{
    CropKernel, DecodeExec, DecodeKernel, LiveSuffixKernel, PassthroughExec, CROP, DECODE,
    DEMOSAIC, DENOISE, EXPOSURE, HEAL, LENS, TONE, VIBRANCE, WB, WORKING_SPACE,
};

const LIVE_IDS: [&str; 5] = [WB, WORKING_SPACE, EXPOSURE, TONE, VIBRANCE];

fn build_graph() -> RenderGraph {
    let mut graph = RenderGraph::new();
    let baked_ids = [DECODE, DEMOSAIC, DENOISE, LENS, HEAL];
    let mut prev: Option<&str> = None;
    for id in baked_ids {
        graph
            .add_node(StageNode {
                id: id.to_string(),
                kind: StageKind::Baked,
                upstream: prev.map(|p| vec![p.to_string()]).unwrap_or_default(),
                own_hash: blake3::hash(id.as_bytes()),
            })
            .unwrap();
        prev = Some(id);
    }
    for id in LIVE_IDS {
        graph
            .add_node(StageNode {
                id: id.to_string(),
                kind: StageKind::Live,
                upstream: vec![prev.unwrap().to_string()],
                own_hash: blake3::hash(id.as_bytes()),
            })
            .unwrap();
        prev = Some(id);
    }
    graph
        .add_node(StageNode {
            id: CROP.to_string(),
            kind: StageKind::Geometry,
            upstream: vec![prev.unwrap().to_string()],
            own_hash: blake3::hash(CROP.as_bytes()),
        })
        .unwrap();
    graph
}

/// A synthetic 64x64 "RAW" gradient frame -- a real `LinearFrame`, real decode/normalize/live-
/// suffix/crop dispatches, but no real file on disk (loading one is #31's job). Distinct per-
/// channel gradients so a color-pipeline bug (e.g. a channel swap) is visible, not masked by a
/// flat test color.
fn synthetic_linear_frame() -> LinearFrame {
    const SIZE: u32 = 64;
    let black = 0u32;
    let maximum = 4095u32;
    let mut pixels = Vec::with_capacity((SIZE * SIZE * 3) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let r = (x * maximum / SIZE) as u16;
            let g = (y * maximum / SIZE) as u16;
            let b = (((x + y) * maximum) / (2 * SIZE)) as u16;
            pixels.extend_from_slice(&[r, g, b]);
        }
    }
    LinearFrame {
        make: "Nicti".to_string(),
        model: "Synthetic".to_string(),
        width: SIZE,
        height: SIZE,
        black,
        maximum,
        cam_mul: [1.0, 1.0, 1.0, 1.0],
        pre_mul: [1.0, 1.0, 1.0, 1.0],
        cam_xyz: [
            1.0, 0.0, 0.0, // R row
            0.0, 1.0, 0.0, // G row
            0.0, 0.0, 1.0, // B row
            0.0, 0.0, 0.0, // unused G2 row
        ],
        cblack: [0, 0, 0, 0],
        pixels,
    }
}

/// Owns the long-lived kernels and `Renderer` a real develop view needs -- built once
/// (`DecodeKernel`/`LiveSuffixKernel`/`CropKernel` compile a pipeline apiece; rebuilding one per
/// render measured ~1000x too slow in this repo's own prior research, see
/// `nicti_tapetum::gpu::make_compute_pipeline`'s own doc comment) and reused across every render.
pub struct DevelopView {
    frame: LinearFrame,
    extent: Extent,
    graph: RenderGraph,
    decode_kernel: DecodeKernel,
    live_kernel: LiveSuffixKernel,
    crop_kernel: CropKernel,
    renderer: Renderer,
}

impl DevelopView {
    pub fn new(gpu: Arc<GpuContext>) -> Self {
        let frame = synthetic_linear_frame();
        let extent = Extent {
            width: frame.width,
            height: frame.height,
        };
        let decode_kernel = DecodeKernel::new(&gpu);
        let live_kernel = LiveSuffixKernel::new(&gpu);
        let matrix = color::camera_to_working_space_matrix(frame.cam_mul, &frame.cam_xyz);
        live_kernel.set_params(&gpu, matrix, 1.0, 0.0, 0.0);
        let crop_kernel = CropKernel::new(&gpu);
        crop_kernel.set_transform(Affine2D::IDENTITY);
        let renderer = Renderer::new(Arc::clone(&gpu), 500_000_000);

        Self {
            frame,
            extent,
            graph: build_graph(),
            decode_kernel,
            live_kernel,
            crop_kernel,
            renderer,
        }
    }

    /// Renders the current frame at its native extent, returning the final (post-crop) texture --
    /// a cache hit on every call after the first, since nothing about the request changes yet
    /// (real per-render params land once #46/#47's live sliders exist).
    pub fn render(&mut self) -> Arc<FrameTexture> {
        let decode_exec = DecodeExec {
            kernel: &self.decode_kernel,
            frame: &self.frame,
        };
        let passthrough = PassthroughExec;
        let baked_chain: Vec<(&str, &dyn BakedExec)> = vec![
            (DECODE, &decode_exec),
            (DEMOSAIC, &passthrough),
            (DENOISE, &passthrough),
            (LENS, &passthrough),
            (HEAL, &passthrough),
        ];
        let req = RenderRequest {
            graph: &self.graph,
            baked_chain: &baked_chain,
            live: &self.live_kernel,
            live_nodes: &LIVE_IDS,
            geometry: &self.crop_kernel,
            geometry_nodes: &[CROP],
            extent: self.extent,
        };
        self.renderer
            .render(&req)
            .expect("the synthetic frame's own graph/extent are always internally consistent")
    }
}
