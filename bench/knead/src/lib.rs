//! Real-NEF harness for #45's Tapetum render pipeline: a real `nicti_cornea::LibRawDecoder`
//! decode wired through the real 11-stage graph (decode -> demosaic/denoise/lens/heal passthrough
//! -> fused WB/working-space/exposure/tone/vibrance live suffix -> crop), at fixed, neutral
//! default params -- no exposure/WB/tone adjustment beyond as-shot white balance and the real
//! camera->working-space color matrix, so a golden comparison is checking the pipeline's own
//! correctness, not any particular editing choice. Replaces `spikes/loaf`'s own `bench`
//! subcommand now that a real pipeline exists to benchmark instead of synthetic mock stages.

use std::path::Path;
use std::sync::Arc;

use image::RgbImage;
use nicti_cornea::{LibRawDecoder, LinearFrame, RawDecoder};
use nicti_render::color;
use nicti_render::frame::{read_frame, Extent, FrameTexture};
use nicti_render::geometry::{self, Affine2D};
use nicti_render::gpu::{GpuContext, GpuPreference};
use nicti_render::graph::{RenderGraph, StageKind, StageNode};
use nicti_render::renderer::{BakedExec, RenderRequest, Renderer};
use nicti_render::stages::{
    CropKernel, DecodeExec, DecodeKernel, LiveSuffixKernel, PassthroughExec, CROP, DECODE,
    DEMOSAIC, DENOISE, EXPOSURE, HEAL, LENS, TONE, VIBRANCE, WB, WORKING_SPACE,
};

/// One real render of a NEF: decodes it, runs it through the full graph at full resolution and
/// neutral params, and returns the assembled kernels/state a caller (a golden comparison, a perf
/// subcommand) can drive further -- e.g. re-timing just the live-suffix or crop dispatch alone
/// without re-decoding.
pub struct RealRender {
    pub gpu: Arc<GpuContext>,
    pub frame: LinearFrame,
    pub extent: Extent,
    pub decode_kernel: DecodeKernel,
    pub live_kernel: LiveSuffixKernel,
    pub crop_kernel: CropKernel,
    pub graph: RenderGraph,
}

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
    let live_ids = [WB, WORKING_SPACE, EXPOSURE, TONE, VIBRANCE];
    for id in live_ids {
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

pub const LIVE_IDS: [&str; 5] = [WB, WORKING_SPACE, EXPOSURE, TONE, VIBRANCE];

impl RealRender {
    /// Decodes `nef_path` via the real LibRaw-backed decoder and prepares every kernel this
    /// crate's pipeline needs -- doesn't itself dispatch a render (see [`Self::render`]).
    pub fn decode(nef_path: &Path) -> anyhow::Result<Self> {
        let frame = LibRawDecoder
            .decode_linear(nef_path)
            .map_err(|e| anyhow::anyhow!("decoding {}: {e}", nef_path.display()))?;
        let extent = Extent {
            width: frame.width,
            height: frame.height,
        };
        let gpu = Arc::new(
            GpuContext::new(GpuPreference::Auto)
                .map_err(|e| anyhow::anyhow!("no wgpu adapter available: {e}"))?,
        );
        let decode_kernel = DecodeKernel::new(&gpu);
        let live_kernel = LiveSuffixKernel::new(&gpu);
        let matrix = color::camera_to_working_space_matrix(frame.cam_mul, &frame.cam_xyz);
        // Neutral defaults: as-shot WB + the real color matrix, no exposure/tone/vibrance
        // adjustment -- see this module's own doc comment for why.
        live_kernel.set_params(&gpu, matrix, 1.0, 0.0, 0.0);
        let crop_kernel = CropKernel::new(&gpu);
        crop_kernel.set_transform(Affine2D::IDENTITY);

        Ok(Self {
            gpu,
            frame,
            extent,
            decode_kernel,
            live_kernel,
            crop_kernel,
            graph: build_graph(),
        })
    }

    /// Runs one full render (every baked stage, the fused live dispatch, and the crop/present
    /// pass) at this decode's full resolution, returning the GPU-resident output texture.
    pub fn render(&self) -> anyhow::Result<Arc<FrameTexture>> {
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
        let mut renderer = Renderer::new(Arc::clone(&self.gpu), 2_000_000_000);
        let req = RenderRequest {
            graph: &self.graph,
            baked_chain: &baked_chain,
            live: &self.live_kernel,
            live_nodes: &LIVE_IDS,
            geometry: &self.crop_kernel,
            geometry_nodes: &[CROP],
            extent: self.extent,
        };
        renderer
            .render(&req)
            .map_err(|e| anyhow::anyhow!("render failed: {e:?}"))
    }

    /// Runs only the baked prefix (decode + demosaic/denoise/lens/heal passthroughs), one-shot,
    /// no caching -- the real input a live-suffix dispatch actually receives in the full
    /// pipeline. Exists so a caller benchmarking the live-suffix or crop stage in isolation (the
    /// `bench-live-suffix`/`bench-present`/`bench-tile` CLI subcommands) times it against its own
    /// true predecessor's output, not [`Self::render`]'s already-fully-processed (post-crop)
    /// final result -- a real bug an earlier version of this harness had (caught in #45 PR4's
    /// adversarial review): re-running `LiveExec`/`GeometryExec` a second time over an image
    /// that's already been through the live suffix and crop once doesn't crash (same extent),
    /// but it isn't measuring the stage's real, single-pass cost against realistic input.
    pub fn render_baked(&self) -> anyhow::Result<FrameTexture> {
        let decode_exec = DecodeExec {
            kernel: &self.decode_kernel,
            frame: &self.frame,
        };
        let passthrough = PassthroughExec;
        let baked_chain: [(&str, &dyn BakedExec); 5] = [
            (DECODE, &decode_exec),
            (DEMOSAIC, &passthrough),
            (DENOISE, &passthrough),
            (LENS, &passthrough),
            (HEAL, &passthrough),
        ];
        let mut current: Option<FrameTexture> = None;
        for (_, exec) in baked_chain {
            let mut encoder =
                self.gpu
                    .device
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                        label: Some("knead baked prefix"),
                    });
            let output = FrameTexture::new(&self.gpu, self.extent);
            exec.encode(&self.gpu, &mut encoder, current.as_ref(), &output);
            self.gpu.queue.submit(Some(encoder.finish()));
            current = Some(output);
        }
        current.ok_or_else(|| anyhow::anyhow!("baked chain is empty"))
    }

    /// Runs the baked prefix, then one live-suffix dispatch over it -- the real input a crop/
    /// present dispatch actually receives in the full pipeline. See [`Self::render_baked`]'s own
    /// doc comment for why this matters.
    pub fn render_live(&self) -> anyhow::Result<FrameTexture> {
        let baked = self.render_baked()?;
        let output = FrameTexture::new(&self.gpu, self.extent);
        let mut encoder = self
            .gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("knead live suffix"),
            });
        nicti_render::renderer::LiveExec::encode(
            &self.live_kernel,
            &self.gpu,
            &mut encoder,
            &baked,
            &output,
        );
        self.gpu.queue.submit(Some(encoder.finish()));
        Ok(output)
    }

    /// Renders and reads the result back as a display-encoded (sRGB OETF) [`RgbImage`], for a
    /// golden comparison or a `.png` dump.
    pub fn render_to_image(&self) -> anyhow::Result<RgbImage> {
        let output = self.render()?;
        let pixels = read_frame(&self.gpu, &output);
        let mut img = RgbImage::new(self.extent.width, self.extent.height);
        for (i, px) in pixels.iter().enumerate() {
            let rgb = geometry::output_encode([px[0], px[1], px[2]]);
            let to_u8 = |c: f32| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
            let x = (i as u32) % self.extent.width;
            let y = (i as u32) / self.extent.width;
            img.put_pixel(
                x,
                y,
                image::Rgb([to_u8(rgb[0]), to_u8(rgb[1]), to_u8(rgb[2])]),
            );
        }
        Ok(img)
    }
}

/// [`nicti_prowl::golden::Render`] impl backed by a real decode + render -- `render_name` in a
/// [`nicti_prowl::golden::CompareRequest`] should include this crate's name and a pipeline
/// version so a future intentional pipeline change blesses a new golden rather than silently
/// comparing against a golden from a different code path.
pub struct KneadRenderer;

impl nicti_prowl::golden::Render for KneadRenderer {
    fn render(&self, source_path: &Path) -> anyhow::Result<RgbImage> {
        RealRender::decode(source_path)?.render_to_image()
    }
}
