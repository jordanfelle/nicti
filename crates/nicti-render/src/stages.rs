//! Concrete render stages wired to a real `nicti_cornea::LinearFrame`: decode's bake, passthrough
//! slots for demosaic/denoise/lens/heal (their own algorithms are later tickets -- #40, #39,
//! #51), the fused live suffix, and crop's geometry pass.
//!
//! Every `*Kernel` here is built once (`::new`, compiles its shader) and reused across many
//! renders -- rebuilding a pipeline per call measured ~1000x too slow in this repo's own prior
//! research (see `gpu::make_compute_pipeline`'s doc comment). `LiveSuffixKernel`/`CropKernel`
//! take their per-render parameters via a `set_*` method (a `queue.write_buffer`, not a pipeline
//! rebuild) that the caller invokes before `Renderer::render` -- `Renderer`'s own `BakedExec`/
//! `LiveExec`/`GeometryExec` traits (from PR2) don't carry per-call params, since a `Baked` node's
//! executor is only ever invoked on a cache miss, but the fused live/geometry executors run
//! (or not) based on state the caller already computed to build the `RenderRequest` in the first
//! place -- the caller has the current params in hand regardless.

use bytemuck::{Pod, Zeroable};
use nicti_claw::Module;
use nicti_cornea::LinearFrame;
use serde_json::{json, Value};
use wgpu::util::DeviceExt;

use crate::color;
use crate::frame::FrameTexture;
use crate::geometry::Affine2D;
use crate::gpu::{make_compute_pipeline, GpuContext};
use crate::graph::StageKind;
use crate::renderer::{BakedExec, GeometryExec, LiveExec};
use crate::RenderStage;

pub const DECODE: &str = "nicti.decode";
pub const DEMOSAIC: &str = "nicti.demosaic";
pub const DENOISE: &str = "nicti.denoise";
pub const LENS: &str = "nicti.lens";
pub const HEAL: &str = "nicti.heal";
pub const WB: &str = "nicti.wb";
pub const EXPOSURE: &str = "nicti.exposure";
pub const WORKING_SPACE: &str = "nicti.working_space";
pub const TONE: &str = "nicti.tone";
pub const VIBRANCE: &str = "nicti.vibrance";
pub const CROP: &str = "nicti.crop";

/// A `RenderStage` whose identity/kind/defaults are all fixed at construction -- every stage id
/// in this module needs the same `Module`/`RenderStage` boilerplate, so one type serves all of
/// them rather than ten near-identical structs.
pub struct BasicStage {
    id: &'static str,
    kind: StageKind,
    default_params: fn() -> Value,
}

impl Module for BasicStage {
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

impl RenderStage for BasicStage {
    fn kind(&self) -> StageKind {
        self.kind
    }
    fn default_params(&self) -> Value {
        (self.default_params)()
    }
}

pub fn decode_stage() -> BasicStage {
    BasicStage {
        id: DECODE,
        kind: StageKind::Baked,
        default_params: || json!({}),
    }
}
pub fn demosaic_stage() -> BasicStage {
    BasicStage {
        id: DEMOSAIC,
        kind: StageKind::Baked,
        default_params: || json!({}),
    }
}
pub fn denoise_stage() -> BasicStage {
    BasicStage {
        id: DENOISE,
        kind: StageKind::Baked,
        default_params: || json!({}),
    }
}
pub fn lens_stage() -> BasicStage {
    BasicStage {
        id: LENS,
        kind: StageKind::Baked,
        default_params: || json!({}),
    }
}
pub fn heal_stage() -> BasicStage {
    BasicStage {
        id: HEAL,
        kind: StageKind::Baked,
        default_params: || json!({}),
    }
}
pub fn wb_stage() -> BasicStage {
    BasicStage {
        id: WB,
        kind: StageKind::Live,
        default_params: || json!({"r_mult": 1.0, "b_mult": 1.0}),
    }
}
pub fn exposure_stage() -> BasicStage {
    BasicStage {
        id: EXPOSURE,
        kind: StageKind::Live,
        default_params: || json!({"stops": 0.0}),
    }
}
/// Fixed to linear ProPhoto RGB for now -- no adjustable params yet. Kept as its own graph node
/// (matching the stage id ADR-0044 names) so a future working-space choice slots in without
/// restructuring the pipeline.
pub fn working_space_stage() -> BasicStage {
    BasicStage {
        id: WORKING_SPACE,
        kind: StageKind::Live,
        default_params: || json!({}),
    }
}
pub fn tone_stage() -> BasicStage {
    BasicStage {
        id: TONE,
        kind: StageKind::Live,
        default_params: || json!({"contrast": 0.0}),
    }
}
pub fn vibrance_stage() -> BasicStage {
    BasicStage {
        id: VIBRANCE,
        kind: StageKind::Live,
        default_params: || json!({"amount": 0.0}),
    }
}
pub fn crop_stage() -> BasicStage {
    BasicStage {
        id: CROP,
        kind: StageKind::Geometry,
        default_params: || json!({"x": 0.0, "y": 0.0}),
    }
}

// ---------------------------------------------------------------------------------------------
// Decode (Baked): uploads a LinearFrame's pixel data and runs normalize.wgsl.
// ---------------------------------------------------------------------------------------------

/// Matches `normalize.wgsl`'s `Params`: two `vec4<u32>`s, chosen so the layout is unambiguous
/// between Rust and WGSL (a 5th scalar field would leave WGSL's uniform-buffer alignment rules
/// to insert padding before a trailing `vec4`, which this side would then have to reproduce
/// exactly by hand -- two clean vec4s sidesteps that entirely).
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct NormalizeParams {
    /// width, strip_rows, row_offset, black
    dims: [u32; 4],
    /// maximum, cblack.r, cblack.g, cblack.b
    limits: [u32; 4],
}

pub struct DecodeKernel {
    pipeline: wgpu::ComputePipeline,
}

impl DecodeKernel {
    pub fn new(gpu: &GpuContext) -> Self {
        Self {
            pipeline: make_compute_pipeline(
                &gpu.device,
                include_str!("../shaders/normalize.wgsl"),
                "main",
            ),
        }
    }
}

/// How many rows of a `width`-wide, packed-two-u16-per-u32 pixel buffer fit in `max_bytes` (an
/// adapter's `max_storage_buffer_binding_size`) -- always at least 1 (a single row must always
/// fit; a caller with a genuinely un-bindable single row has a bigger problem than this function
/// can solve) and never more than `height` (no point splitting into more rows than the frame has).
fn rows_per_strip(width: u32, height: u32, max_bytes: u64) -> u32 {
    let bytes_per_row = (u64::from(width) * 3).div_ceil(2) * 4;
    (max_bytes / bytes_per_row).clamp(1, u64::from(height)) as u32
}

/// Per-render decode executor -- holds the specific photo's `LinearFrame` (real per-image data,
/// unlike `DecodeKernel`'s pipeline, which is built once and shared across every decode).
pub struct DecodeExec<'a> {
    pub kernel: &'a DecodeKernel,
    pub frame: &'a LinearFrame,
}

impl BakedExec for DecodeExec<'_> {
    fn encode(
        &self,
        gpu: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        _input: Option<&FrameTexture>,
        output: &FrameTexture,
    ) {
        let frame = self.frame;
        // `frame.pixels`'s length and `frame.maximum > frame.black` are invariants
        // `nicti_cornea::LinearFrame` documents but doesn't itself enforce, and this dispatch's
        // workgroup count comes from `frame.width`/`frame.height` while `output`'s extent comes
        // from the caller's own `RenderRequest::extent` -- nothing upstream of this function
        // guarantees the two agree. A real assert (not `debug_assert!`, which release builds
        // strip) here turns a would-be out-of-bounds storage-buffer read, a partially-written or
        // overflowing texture write, or a NaN/Inf from dividing by a non-positive range into an
        // immediate, attributable panic instead of a silently wrong or corrupted render.
        let expected_pixels = (frame.width as usize)
            .checked_mul(frame.height as usize)
            .and_then(|count| count.checked_mul(3))
            .expect("LinearFrame dimensions overflow a usize");
        assert_eq!(
            frame.pixels.len(),
            expected_pixels,
            "LinearFrame.pixels.len() ({}) doesn't match width*height*3 ({expected_pixels})",
            frame.pixels.len()
        );
        assert_eq!(
            (frame.width, frame.height),
            (output.extent.width, output.extent.height),
            "DecodeExec's output texture extent must match the LinearFrame's own dimensions"
        );
        assert!(
            frame.maximum > frame.black,
            "LinearFrame.maximum ({}) must exceed .black ({}) or normalize's range is non-positive",
            frame.maximum,
            frame.black
        );
        // WGSL has no native u16 storage-buffer element type -- pack two u16 samples per u32
        // (little-endian: sample 2n in the low 16 bits, sample 2n+1 in the high 16 bits), halving
        // upload size versus one u32 per sample. `normalize.wgsl`'s `unpack_sample` does the
        // matching unpack. A full-res 8280x5520 frame is still ~274MB packed -- comfortably under
        // real-hardware storage-binding limits (`GpuContext` already requests `adapter.limits()`),
        // but exceeds a software adapter's (e.g. lavapipe's 128MB) at full resolution -- confirmed
        // in practice, not just in theory, against a real ref-10k Nikon Z8 file. The upload is
        // therefore split into row-strips, each sized to fit under
        // `gpu.limits.max_storage_buffer_binding_size`, dispatched as separate compute passes
        // within this same encoder; `normalize.wgsl`'s `row_offset` param is what lets each
        // strip's dispatch write to the correct absolute row of the one full-frame output texture.
        let max_rows_per_strip = rows_per_strip(
            frame.width,
            frame.height,
            gpu.limits.max_storage_buffer_binding_size,
        );

        let bind_group_layout = self.kernel.pipeline.get_bind_group_layout(0);
        let mut row_offset = 0u32;
        while row_offset < frame.height {
            let strip_rows = max_rows_per_strip.min(frame.height - row_offset);
            let start = row_offset as usize * frame.width as usize * 3;
            let end = (row_offset + strip_rows) as usize * frame.width as usize * 3;
            let pixels_u32: Vec<u32> = frame.pixels[start..end]
                .chunks(2)
                .map(|pair| u32::from(pair[0]) | (u32::from(*pair.get(1).unwrap_or(&0)) << 16))
                .collect();
            let pixel_buf = gpu
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("decode pixels (strip)"),
                    contents: bytemuck::cast_slice(&pixels_u32),
                    usage: wgpu::BufferUsages::STORAGE,
                });
            let params = NormalizeParams {
                dims: [frame.width, strip_rows, row_offset, frame.black],
                limits: [
                    frame.maximum,
                    frame.cblack[0],
                    frame.cblack[1],
                    frame.cblack[2],
                ],
            };
            let params_buf = gpu
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("decode params (strip)"),
                    contents: bytemuck::bytes_of(&params),
                    usage: wgpu::BufferUsages::UNIFORM,
                });
            let bind_group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("decode bind group (strip)"),
                layout: &bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: pixel_buf.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&output.view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: params_buf.as_entire_binding(),
                    },
                ],
            });

            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("decode strip"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.kernel.pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.dispatch_workgroups(frame.width.div_ceil(8), strip_rows.div_ceil(8), 1);
            drop(pass);

            row_offset += strip_rows;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Demosaic/denoise/lens/heal (Baked passthrough): a plain texture copy. LibRaw already
// demosaiced (see nicti_cornea's own doc comment); denoise/lens/heal have no algorithm yet.
// ---------------------------------------------------------------------------------------------

pub struct PassthroughExec;

impl BakedExec for PassthroughExec {
    fn encode(
        &self,
        _gpu: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        input: Option<&FrameTexture>,
        output: &FrameTexture,
    ) {
        let input = input.expect("a passthrough stage always has an upstream baked node");
        encoder.copy_texture_to_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &input.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyTextureInfo {
                texture: &output.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::Extent3d {
                width: output.extent.width,
                height: output.extent.height,
                depth_or_array_layers: 1,
            },
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Live suffix (fused Live dispatch): WB + camera->working-space + exposure + tone + vibrance.
// ---------------------------------------------------------------------------------------------

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct LiveUniforms {
    col0: [f32; 4],
    col1: [f32; 4],
    col2: [f32; 4],
    exposure_mult: f32,
    contrast: f32,
    vibrance: f32,
    _pad: f32,
}

pub struct LiveSuffixKernel {
    pipeline: wgpu::ComputePipeline,
    uniform_buf: wgpu::Buffer,
}

impl LiveSuffixKernel {
    pub fn new(gpu: &GpuContext) -> Self {
        let pipeline = make_compute_pipeline(
            &gpu.device,
            include_str!("../shaders/live_suffix.wgsl"),
            "main",
        );
        let uniform_buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("live_suffix uniforms"),
            size: std::mem::size_of::<LiveUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            pipeline,
            uniform_buf,
        }
    }

    /// Uploads this render's params -- call before `Renderer::render` whenever any of them
    /// changed (a no-op `write_buffer`, not a pipeline rebuild, if nothing did).
    pub fn set_params(
        &self,
        gpu: &GpuContext,
        working_space_matrix: color::Mat3,
        exposure_mult: f32,
        contrast: f32,
        vibrance: f32,
    ) {
        let m = working_space_matrix;
        let u = LiveUniforms {
            col0: [m[0][0], m[1][0], m[2][0], 0.0],
            col1: [m[0][1], m[1][1], m[2][1], 0.0],
            col2: [m[0][2], m[1][2], m[2][2], 0.0],
            exposure_mult,
            contrast,
            vibrance,
            _pad: 0.0,
        };
        gpu.queue
            .write_buffer(&self.uniform_buf, 0, bytemuck::bytes_of(&u));
    }
}

impl LiveExec for LiveSuffixKernel {
    fn encode(
        &self,
        gpu: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        input: &FrameTexture,
        output: &FrameTexture,
    ) {
        let bind_group_layout = self.pipeline.get_bind_group_layout(0);
        let bind_group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("live_suffix bind group"),
            layout: &bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&input.view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&output.view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: self.uniform_buf.as_entire_binding(),
                },
            ],
        });
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("live_suffix"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.dispatch_workgroups(
            output.extent.width.div_ceil(8),
            output.extent.height.div_ceil(8),
            1,
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Crop (Geometry): a bilinear affine sample over the live suffix's own output.
// ---------------------------------------------------------------------------------------------

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct PresentUniforms {
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    tx: f32,
    ty: f32,
    out_width: u32,
    out_height: u32,
}

pub struct CropKernel {
    pipeline: wgpu::ComputePipeline,
    transform: std::sync::Mutex<Affine2D>,
    uniform_buf: wgpu::Buffer,
}

impl CropKernel {
    pub fn new(gpu: &GpuContext) -> Self {
        let pipeline = make_compute_pipeline(
            &gpu.device,
            include_str!("../shaders/present_sample.wgsl"),
            "main",
        );
        let uniform_buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("present_sample uniforms"),
            size: std::mem::size_of::<PresentUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            pipeline,
            transform: std::sync::Mutex::new(Affine2D::IDENTITY),
            uniform_buf,
        }
    }

    /// Records this render's crop transform -- the actual uniform-buffer write is deferred to
    /// `encode` (which also needs the output extent, only known once a `FrameTexture` exists).
    pub fn set_transform(&self, transform: Affine2D) {
        *self.transform.lock().unwrap() = transform;
    }
}

impl GeometryExec for CropKernel {
    fn encode(
        &self,
        gpu: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        input: &FrameTexture,
        output: &FrameTexture,
    ) {
        let transform = *self.transform.lock().unwrap();
        let u = PresentUniforms {
            a: transform.a,
            b: transform.b,
            c: transform.c,
            d: transform.d,
            tx: transform.tx,
            ty: transform.ty,
            out_width: output.extent.width,
            out_height: output.extent.height,
        };
        gpu.queue
            .write_buffer(&self.uniform_buf, 0, bytemuck::bytes_of(&u));

        let bind_group_layout = self.pipeline.get_bind_group_layout(0);
        let bind_group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("present_sample bind group"),
            layout: &bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&input.view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&output.view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: self.uniform_buf.as_entire_binding(),
                },
            ],
        });
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("present_sample"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.dispatch_workgroups(
            output.extent.width.div_ceil(8),
            output.extent.height.div_ceil(8),
            1,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_per_strip_fits_the_whole_frame_when_it_fits_under_the_limit() {
        assert_eq!(rows_per_strip(100, 100, u64::MAX), 100);
    }

    #[test]
    fn rows_per_strip_splits_when_the_full_frame_exceeds_the_limit() {
        // width=8280 (real Z8 width), 128MiB (lavapipe's own max_storage_buffer_binding_size).
        let strip = rows_per_strip(8280, 5520, 128 * 1024 * 1024);
        assert_eq!(strip, 2701);
        assert!(strip < 5520, "a real full-res frame must actually be split");
    }

    #[test]
    fn rows_per_strip_never_returns_zero_even_if_a_single_row_would_not_fit() {
        assert_eq!(rows_per_strip(8280, 5520, 1), 1);
    }

    #[test]
    fn rows_per_strip_never_exceeds_the_frame_height() {
        assert_eq!(rows_per_strip(10, 5, u64::MAX), 5);
    }

    #[test]
    fn every_stage_id_is_namespaced() {
        for id in [
            DECODE,
            DEMOSAIC,
            DENOISE,
            LENS,
            HEAL,
            WB,
            EXPOSURE,
            WORKING_SPACE,
            TONE,
            VIBRANCE,
            CROP,
        ] {
            assert!(id.starts_with("nicti."), "{id} must be namespaced");
        }
    }

    #[test]
    fn baked_stage_kinds_are_correctly_classified() {
        assert_eq!(decode_stage().kind(), StageKind::Baked);
        assert_eq!(demosaic_stage().kind(), StageKind::Baked);
        assert_eq!(denoise_stage().kind(), StageKind::Baked);
        assert_eq!(lens_stage().kind(), StageKind::Baked);
        assert_eq!(heal_stage().kind(), StageKind::Baked);
    }

    #[test]
    fn live_stage_kinds_are_correctly_classified() {
        assert_eq!(wb_stage().kind(), StageKind::Live);
        assert_eq!(exposure_stage().kind(), StageKind::Live);
        assert_eq!(working_space_stage().kind(), StageKind::Live);
        assert_eq!(tone_stage().kind(), StageKind::Live);
        assert_eq!(vibrance_stage().kind(), StageKind::Live);
    }

    #[test]
    fn crop_stage_kind_is_geometry() {
        assert_eq!(crop_stage().kind(), StageKind::Geometry);
    }

    fn test_gpu() -> Option<GpuContext> {
        match GpuContext::new(crate::gpu::GpuPreference::Auto) {
            Ok(ctx) => Some(ctx),
            Err(_) => {
                eprintln!("no wgpu adapter available in this environment, skipping");
                None
            }
        }
    }

    /// A tiny synthetic 2x2 "RAW" frame with distinct per-pixel, per-channel values, non-zero
    /// black level and per-channel `cblack`, and a `maximum` that doesn't evenly divide -- picked
    /// to actually exercise the normalize math rather than degenerate to a trivial case.
    fn synthetic_linear_frame() -> LinearFrame {
        LinearFrame {
            make: "Test".to_string(),
            model: "Synthetic".to_string(),
            width: 2,
            height: 2,
            black: 100,
            maximum: 1100,
            cam_mul: [2.0, 1.0, 1.5, 1.0],
            pre_mul: [2.0, 1.0, 1.5, 1.0],
            cam_xyz: [
                0.6, 0.2, 0.1, // R row
                0.15, 0.75, 0.1, // G row
                0.05, 0.15, 0.9, // B row
                0.0, 0.0, 0.0, // unused G2 row
            ],
            cblack: [10, 20, 5, 0],
            // 2x2 pixels, 3 u16 samples each (R,G,B), row-major.
            pixels: vec![
                600, 500, 400, // (0,0)
                1100, 1100, 1100, // (1,0) -- at maximum on every channel
                100, 120, 105, // (0,1) -- at/near black on every channel
                800, 300, 950, // (1,1)
            ],
        }
    }

    /// CPU reference for `normalize.wgsl`'s exact per-pixel formula.
    fn normalize_cpu_reference(frame: &LinearFrame) -> Vec<[f32; 4]> {
        let range = frame.maximum as f32 - frame.black as f32;
        (0..(frame.width * frame.height) as usize)
            .map(|i| {
                let base = i * 3;
                let r = (frame.pixels[base] as f32 - frame.black as f32 - frame.cblack[0] as f32)
                    / range;
                let g =
                    (frame.pixels[base + 1] as f32 - frame.black as f32 - frame.cblack[1] as f32)
                        / range;
                let b =
                    (frame.pixels[base + 2] as f32 - frame.black as f32 - frame.cblack[2] as f32)
                        / range;
                [r, g, b, 1.0]
            })
            .collect()
    }

    #[test]
    fn decode_gpu_matches_cpu_reference() {
        let Some(gpu) = test_gpu() else { return };
        let frame = synthetic_linear_frame();
        let kernel = DecodeKernel::new(&gpu);
        let exec = DecodeExec {
            kernel: &kernel,
            frame: &frame,
        };
        let extent = crate::frame::Extent {
            width: frame.width,
            height: frame.height,
        };
        let output = FrameTexture::new(&gpu, extent);
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        exec.encode(&gpu, &mut encoder, None, &output);
        gpu.queue.submit(Some(encoder.finish()));

        let actual = crate::test_util::read_frame(&gpu, &output);
        let expected = normalize_cpu_reference(&frame);
        for (i, (a, e)) in actual.iter().zip(expected.iter()).enumerate() {
            for c in 0..4 {
                assert!(
                    (a[c] - e[c]).abs() < 0.01,
                    "pixel {i} channel {c}: gpu={} cpu={}",
                    a[c],
                    e[c]
                );
            }
        }
    }

    /// Regression test for a stride mixup: `synthetic_linear_frame` is square (2x2), so a bug
    /// that swapped `p.width`/`p.height` in `normalize.wgsl`'s row-stride math would be invisible
    /// there. A non-square (3x2) frame with distinct values at every position makes a
    /// width/height swap produce a wrong pixel somewhere.
    #[test]
    fn decode_gpu_matches_cpu_reference_on_a_non_square_frame() {
        let Some(gpu) = test_gpu() else { return };
        let frame = LinearFrame {
            make: "Test".to_string(),
            model: "Synthetic".to_string(),
            width: 3,
            height: 2,
            black: 50,
            maximum: 1000,
            cam_mul: [1.0, 1.0, 1.0, 1.0],
            pre_mul: [1.0, 1.0, 1.0, 1.0],
            cam_xyz: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0],
            cblack: [0, 0, 0, 0],
            // 3x2 pixels, distinct at every position so a width/height swap changes some pixel.
            pixels: vec![
                100, 110, 120, // (0,0)
                200, 210, 220, // (1,0)
                300, 310, 320, // (2,0)
                400, 410, 420, // (0,1)
                500, 510, 520, // (1,1)
                600, 610, 620, // (2,1)
            ],
        };
        let kernel = DecodeKernel::new(&gpu);
        let exec = DecodeExec {
            kernel: &kernel,
            frame: &frame,
        };
        let extent = crate::frame::Extent {
            width: frame.width,
            height: frame.height,
        };
        let output = FrameTexture::new(&gpu, extent);
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        exec.encode(&gpu, &mut encoder, None, &output);
        gpu.queue.submit(Some(encoder.finish()));

        let actual = crate::test_util::read_frame(&gpu, &output);
        let expected = normalize_cpu_reference(&frame);
        assert_eq!(actual.len(), expected.len());
        for (i, (a, e)) in actual.iter().zip(expected.iter()).enumerate() {
            for c in 0..4 {
                assert!(
                    (a[c] - e[c]).abs() < 0.01,
                    "pixel {i} channel {c}: gpu={} cpu={}",
                    a[c],
                    e[c]
                );
            }
        }
    }

    /// Regression test for the u16-pair packing: a 1x1 frame has exactly 3 samples (odd), so the
    /// last `u32` pair is padded with an unused zero -- proves that padding is never read (it
    /// would corrupt the B channel if it were).
    #[test]
    fn decode_gpu_matches_cpu_reference_with_an_odd_total_sample_count() {
        let Some(gpu) = test_gpu() else { return };
        let frame = LinearFrame {
            make: "Test".to_string(),
            model: "Synthetic".to_string(),
            width: 1,
            height: 1,
            black: 10,
            maximum: 500,
            cam_mul: [1.0, 1.0, 1.0, 1.0],
            pre_mul: [1.0, 1.0, 1.0, 1.0],
            cam_xyz: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0],
            cblack: [0, 0, 0, 0],
            pixels: vec![100, 200, 300], // R, G, B -- 3 samples, odd total.
        };
        let kernel = DecodeKernel::new(&gpu);
        let exec = DecodeExec {
            kernel: &kernel,
            frame: &frame,
        };
        let extent = crate::frame::Extent {
            width: frame.width,
            height: frame.height,
        };
        let output = FrameTexture::new(&gpu, extent);
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        exec.encode(&gpu, &mut encoder, None, &output);
        gpu.queue.submit(Some(encoder.finish()));

        let actual = crate::test_util::read_frame(&gpu, &output);
        let expected = normalize_cpu_reference(&frame);
        for c in 0..4 {
            assert!(
                (actual[0][c] - expected[0][c]).abs() < 0.01,
                "channel {c}: gpu={} cpu={}",
                actual[0][c],
                expected[0][c]
            );
        }
    }

    /// Regression test: `DecodeExec::encode`'s guard must reject a `LinearFrame` whose
    /// `pixels.len()` doesn't match its declared `width*height*3` -- previously nothing checked
    /// this, so a mismatched frame would silently read past (or short of) the storage buffer.
    #[test]
    #[should_panic(expected = "doesn't match width*height*3")]
    fn decode_panics_on_a_pixel_count_mismatch() {
        // No adapter available: nothing to prove, so satisfy `should_panic` without pretending
        // the assert under test actually ran -- consistent with every other GPU test's `test_gpu`
        // skip convention in this module.
        let Some(gpu) = test_gpu() else {
            panic!("doesn't match width*height*3");
        };
        let mut frame = synthetic_linear_frame();
        frame.pixels.pop(); // now one sample short of width*height*3.
        let kernel = DecodeKernel::new(&gpu);
        let exec = DecodeExec {
            kernel: &kernel,
            frame: &frame,
        };
        let extent = crate::frame::Extent {
            width: frame.width,
            height: frame.height,
        };
        let output = FrameTexture::new(&gpu, extent);
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        exec.encode(&gpu, &mut encoder, None, &output);
    }

    #[test]
    fn live_suffix_gpu_matches_cpu_reference() {
        let Some(gpu) = test_gpu() else { return };
        let extent = crate::frame::Extent {
            width: 2,
            height: 2,
        };
        let input_data = vec![
            [0.2, 0.3, 0.1, 1.0],
            [0.5, 0.05, 0.4, 1.0],
            [0.9, 0.9, 0.9, 1.0],
            [0.0, 0.0, 0.0, 1.0],
        ];
        let input = crate::test_util::upload_frame(&gpu, extent, &input_data);
        let output = FrameTexture::new(&gpu, extent);

        let cam_mul = [1.8, 1.0, 1.3, 1.0];
        let cam_xyz = [
            0.55, 0.2, 0.1, 0.2, 0.7, 0.15, 0.05, 0.1, 0.85, 0.0, 0.0, 0.0,
        ];
        let matrix = color::camera_to_working_space_matrix(cam_mul, &cam_xyz);
        let exposure_mult = color::exposure_multiplier(0.7);
        let contrast = 0.3;
        let vibrance = 0.4;

        let kernel = LiveSuffixKernel::new(&gpu);
        kernel.set_params(&gpu, matrix, exposure_mult, contrast, vibrance);
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        kernel.encode(&gpu, &mut encoder, &input, &output);
        gpu.queue.submit(Some(encoder.finish()));

        let actual = crate::test_util::read_frame(&gpu, &output);
        for (i, px) in input_data.iter().enumerate() {
            let mut rgb = color::mat3_apply(matrix, [px[0], px[1], px[2]]);
            rgb = rgb.map(|c| c * exposure_mult);
            rgb = color::apply_tone(rgb, contrast);
            rgb = color::apply_vibrance(rgb, vibrance);
            for c in 0..3 {
                assert!(
                    (actual[i][c] - rgb[c]).abs() < 0.01,
                    "pixel {i} channel {c}: gpu={} cpu={}",
                    actual[i][c],
                    rgb[c]
                );
            }
        }
    }

    #[test]
    fn present_sample_gpu_matches_cpu_reference() {
        let Some(gpu) = test_gpu() else { return };
        let extent = crate::frame::Extent {
            width: 4,
            height: 4,
        };
        let input_data: Vec<[f32; 4]> = (0..16)
            .map(|i| [i as f32 * 0.01, i as f32 * 0.02, i as f32 * 0.03, 1.0])
            .collect();
        let input = crate::test_util::upload_frame(&gpu, extent, &input_data);
        let out_extent = crate::frame::Extent {
            width: 2,
            height: 2,
        };
        let output = FrameTexture::new(&gpu, out_extent);

        let transform = Affine2D::crop(1.0, 1.0);
        let kernel = CropKernel::new(&gpu);
        kernel.set_transform(transform);
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        kernel.encode(&gpu, &mut encoder, &input, &output);
        gpu.queue.submit(Some(encoder.finish()));

        let actual = crate::test_util::read_frame(&gpu, &output);
        for oy in 0..out_extent.height {
            for ox in 0..out_extent.width {
                let expected = crate::geometry::sample_bilinear(
                    &input_data,
                    (extent.width, extent.height),
                    &transform,
                    (ox, oy),
                );
                let got = actual[(oy * out_extent.width + ox) as usize];
                for c in 0..4 {
                    assert!(
                        (got[c] - expected[c]).abs() < 0.01,
                        "({ox},{oy}) channel {c}: gpu={} cpu={}",
                        got[c],
                        expected[c]
                    );
                }
            }
        }
    }

    /// Wires every stage together through the real `Renderer` -- decode, the four passthrough
    /// baked slots, the fused live suffix, and crop -- and checks both the dispatch counts (one
    /// per baked node, one live, one geometry) and the actual output pixels against composing
    /// every CPU reference function by hand. This is the whole pipeline PR3 exists to prove,
    /// not just each kernel in isolation.
    #[test]
    fn full_pipeline_end_to_end_produces_correctly_colored_output() {
        let Some(gpu) = test_gpu() else { return };
        let gpu = std::sync::Arc::new(gpu);
        let frame = synthetic_linear_frame();
        let extent = crate::frame::Extent {
            width: frame.width,
            height: frame.height,
        };

        let mut graph = crate::graph::RenderGraph::new();
        let baked_ids = [DECODE, DEMOSAIC, DENOISE, LENS, HEAL];
        let mut prev: Option<&str> = None;
        for id in baked_ids {
            graph
                .add_node(crate::graph::StageNode {
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
                .add_node(crate::graph::StageNode {
                    id: id.to_string(),
                    kind: StageKind::Live,
                    upstream: vec![prev.unwrap().to_string()],
                    own_hash: blake3::hash(id.as_bytes()),
                })
                .unwrap();
            prev = Some(id);
        }
        graph
            .add_node(crate::graph::StageNode {
                id: CROP.to_string(),
                kind: StageKind::Geometry,
                upstream: vec![prev.unwrap().to_string()],
                own_hash: blake3::hash(CROP.as_bytes()),
            })
            .unwrap();

        let decode_kernel = DecodeKernel::new(&gpu);
        let decode_exec = DecodeExec {
            kernel: &decode_kernel,
            frame: &frame,
        };
        let passthrough = PassthroughExec;
        let baked_chain: Vec<(&str, &dyn BakedExec)> = vec![
            (DECODE, &decode_exec),
            (DEMOSAIC, &passthrough),
            (DENOISE, &passthrough),
            (LENS, &passthrough),
            (HEAL, &passthrough),
        ];

        let live_kernel = LiveSuffixKernel::new(&gpu);
        let matrix = color::camera_to_working_space_matrix(frame.cam_mul, &frame.cam_xyz);
        let exposure_mult = color::exposure_multiplier(0.5);
        let contrast = 0.2;
        let vibrance = 0.3;
        live_kernel.set_params(&gpu, matrix, exposure_mult, contrast, vibrance);

        let crop_kernel = CropKernel::new(&gpu);
        crop_kernel.set_transform(Affine2D::IDENTITY);

        let mut renderer =
            crate::renderer::Renderer::new(std::sync::Arc::clone(&gpu), 1_000_000_000);
        let req = crate::renderer::RenderRequest {
            graph: &graph,
            baked_chain: &baked_chain,
            live: &live_kernel,
            live_nodes: &live_ids,
            geometry: &crop_kernel,
            geometry_nodes: &[CROP],
            extent,
        };
        let output = renderer.render(&req).unwrap();
        assert_eq!(renderer.last_stats().bake_dispatches, 5);
        assert_eq!(renderer.last_stats().live_dispatches, 1);
        assert_eq!(renderer.last_stats().geometry_dispatches, 1);

        let actual = crate::test_util::read_frame(&gpu, &output);
        let decoded = normalize_cpu_reference(&frame);
        for (i, d) in decoded.iter().enumerate() {
            let mut rgb = color::mat3_apply(matrix, [d[0], d[1], d[2]]);
            rgb = rgb.map(|c| c * exposure_mult);
            rgb = color::apply_tone(rgb, contrast);
            rgb = color::apply_vibrance(rgb, vibrance);
            for c in 0..3 {
                assert!(
                    (actual[i][c] - rgb[c]).abs() < 0.02,
                    "pixel {i} channel {c}: gpu={} expected={}",
                    actual[i][c],
                    rgb[c]
                );
            }
        }
    }
}
