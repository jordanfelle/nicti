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

use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use nicti_calico::dcp::TableEncoding;
use nicti_calico::huesatmap::HueSatMap;
use nicti_calico::profile::{fingerprint, ProfileSolution};
use nicti_claw::Module;
use nicti_cornea::LinearFrame;
use serde_json::{json, Value};
use wgpu::util::DeviceExt;

use crate::coat::{
    self, CropParams, ExposureParams, HslParams, NoiseReductionParams, SharpenParams,
    ToneCurveParams, ToneParams, VibranceParams, WbParams,
};
use crate::color;
use crate::detail::{self, MAX_BLUR_RADIUS};
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
pub const TONE_CURVE: &str = "nicti.tone_curve";
pub const VIBRANCE: &str = "nicti.vibrance";
pub const HSL: &str = "nicti.hsl";
pub const SHARPEN: &str = "nicti.sharpen";
pub const NOISE_REDUCTION: &str = "nicti.noise_reduction";
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
pub fn heal_stage() -> crate::heal::HealStage {
    crate::heal::HealStage
}
pub fn wb_stage() -> BasicStage {
    BasicStage {
        id: WB,
        kind: StageKind::Live,
        default_params: || coat::default_value::<WbParams>(),
    }
}
pub fn exposure_stage() -> BasicStage {
    BasicStage {
        id: EXPOSURE,
        kind: StageKind::Live,
        default_params: || coat::default_value::<ExposureParams>(),
    }
}
/// Fixed to linear ProPhoto RGB for now. Its only param is the selected DCP camera profile (#42,
/// `coat::CameraProfileParams`), whose content hash makes a profile switch invalidate the live
/// output; the default (no profile) serializes to the historical `{}`. Kept as its own graph node
/// (matching the stage id ADR-0044 names) so a future working-space choice slots in without
/// restructuring the pipeline.
pub fn working_space_stage() -> BasicStage {
    BasicStage {
        id: WORKING_SPACE,
        kind: StageKind::Live,
        default_params: || coat::default_value::<coat::CameraProfileParams>(),
    }
}
pub fn tone_stage() -> BasicStage {
    BasicStage {
        id: TONE,
        kind: StageKind::Live,
        default_params: || coat::default_value::<ToneParams>(),
    }
}
pub fn tone_curve_stage() -> BasicStage {
    BasicStage {
        id: TONE_CURVE,
        kind: StageKind::Live,
        default_params: || coat::default_value::<ToneCurveParams>(),
    }
}
pub fn vibrance_stage() -> BasicStage {
    BasicStage {
        id: VIBRANCE,
        kind: StageKind::Live,
        default_params: || coat::default_value::<VibranceParams>(),
    }
}
pub fn hsl_stage() -> BasicStage {
    BasicStage {
        id: HSL,
        kind: StageKind::Live,
        default_params: || coat::default_value::<HslParams>(),
    }
}
pub fn sharpen_stage() -> BasicStage {
    BasicStage {
        id: SHARPEN,
        kind: StageKind::Live,
        default_params: || coat::default_value::<SharpenParams>(),
    }
}
pub fn noise_reduction_stage() -> BasicStage {
    BasicStage {
        id: NOISE_REDUCTION,
        kind: StageKind::Live,
        default_params: || coat::default_value::<NoiseReductionParams>(),
    }
}
pub fn crop_stage() -> BasicStage {
    BasicStage {
        id: CROP,
        kind: StageKind::Geometry,
        default_params: || coat::default_value::<CropParams>(),
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

/// CPU twin of `normalize.wgsl`: the heal stage's exact input, as row-major linear camera RGBA
/// (black level and per-channel `cblack` subtracted, scaled by `maximum - black` to roughly
/// [0, 1]; alpha 1). The AI-removal path (#51) runs on this rather than reading a GPU texture
/// back, so its patches are in precisely the space `heal.wgsl` composites them into. Kept honest
/// by `decode_gpu_matches_cpu_reference`, which checks the GPU decode against this function.
pub fn normalize_pixels(frame: &LinearFrame) -> Vec<[f32; 4]> {
    let range = frame.maximum as f32 - frame.black as f32;
    let sample = |i: usize, c: usize| {
        (frame.pixels[i * 3 + c] as f32 - frame.black as f32 - frame.cblack[c] as f32) / range
    };
    (0..(frame.width * frame.height) as usize)
        .map(|i| [sample(i, 0), sample(i, 1), sample(i, 2), 1.0])
        .collect()
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

const CURVE_LUT_GROUPS: usize = 64; // 256 entries, 4 per vec4.
const HSL_BAND_COUNT: usize = 8;
const BLUR_WEIGHT_GROUPS: usize = 9; // (2*MAX_BLUR_RADIUS+1)=35 taps, 4 per vec4, 9 groups.

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct LiveUniforms {
    col0: [f32; 4],
    col1: [f32; 4],
    col2: [f32; 4],
    /// exposure_mult, contrast, highlights, shadows.
    tone0: [f32; 4],
    /// whites, blacks, vibrance, _pad.
    tone1: [f32; 4],
    /// #46's Tone Curve LUT (`color::build_tone_curve_lut`), 256 entries packed 4-per-vec4.
    curve_lut: [[f32; 4]; CURVE_LUT_GROUPS],
    /// #46's 8-band HSL panel, one vec4 (hue, saturation, luminance, unused) per band.
    hsl_bands: [[f32; 4]; HSL_BAND_COUNT],
    /// DCP camera profile (#42): HueSatMap enabled, LookTable enabled, HueSatMap sRGB value
    /// encoding, LookTable sRGB value encoding (all 0/1).
    profile0: [f32; 4],
    /// Baseline-exposure multiplier (`2^BaselineExposureOffset`, `1.0` with no profile), yzw unused.
    profile1: [f32; 4],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct BlurUniforms {
    /// x: 0 = horizontal, 1 = vertical. yzw unused.
    direction: [u32; 4],
    weights: [[f32; 4]; BLUR_WEIGHT_GROUPS],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct CombineUniforms {
    /// luminance, color, detail, unused.
    nr: [f32; 4],
    /// amount, unused, detail, unused.
    sharpen: [f32; 4],
}

/// The live suffix's full per-render parameter set -- everything [`LiveSuffixKernel::set_params`]
/// needs to write its uniform buffers. Grouped into one struct (rather than a growing list of
/// positional args) now that #46 adds several more fields beyond the original exposure/contrast/
/// vibrance trio.
pub struct LiveParams {
    pub working_space_matrix: color::Mat3,
    pub exposure: ExposureParams,
    pub tone: ToneParams,
    pub tone_curve: ToneCurveParams,
    pub vibrance: VibranceParams,
    pub hsl: HslParams,
    pub sharpen: SharpenParams,
    pub noise_reduction: NoiseReductionParams,
    /// A solved DCP camera profile (`nicti_calico::profile::ProfileSolution`, #42): its
    /// HueSatMap / baseline exposure / LookTable run right after the camera->working matrix.
    /// The caller is responsible for putting the solution's `camera_to_working` into
    /// `working_space_matrix` -- this field carries only the tables. `None` = no profile.
    pub camera_profile: Option<Arc<ProfileSolution>>,
    /// Render extent's long edge divided by the source frame's own long edge -- lets Sharpening/
    /// Noise Reduction's blur radii scale with actual output resolution (a screen-res preview and
    /// a full-res export should sharpen the same image *content*, not the same pixel count). `1.0`
    /// for a native-extent render.
    pub pixel_scale: f32,
}

impl Default for LiveParams {
    fn default() -> Self {
        Self {
            working_space_matrix: color::mat3_identity(),
            exposure: ExposureParams::default(),
            tone: ToneParams::default(),
            tone_curve: ToneCurveParams::default(),
            vibrance: VibranceParams::default(),
            hsl: HslParams::default(),
            sharpen: SharpenParams::default(),
            noise_reduction: NoiseReductionParams::default(),
            camera_profile: None,
            pixel_scale: 1.0,
        }
    }
}

fn pack_lut(lut: &[f32; 256]) -> [[f32; 4]; CURVE_LUT_GROUPS] {
    std::array::from_fn(|g| {
        let base = g * 4;
        [lut[base], lut[base + 1], lut[base + 2], lut[base + 3]]
    })
}

fn pack_hsl(hsl: &HslParams) -> [[f32; 4]; HSL_BAND_COUNT] {
    std::array::from_fn(|i| {
        let band = hsl.bands[i];
        [band.hue, band.saturation, band.luminance, 0.0]
    })
}

fn pack_blur_weights(kernel: [f32; 2 * MAX_BLUR_RADIUS + 1]) -> [[f32; 4]; BLUR_WEIGHT_GROUPS] {
    // BLUR_WEIGHT_GROUPS*4 (36) is one slot wider than the kernel's own 35 taps -- the trailing
    // slot stays zero, matching `detail_blur.wgsl`'s own fixed `RADIUS`-driven loop bound (which
    // only ever reads the first 35).
    let mut out = [[0.0f32; 4]; BLUR_WEIGHT_GROUPS];
    for (i, &w) in kernel.iter().enumerate() {
        out[i / 4][i % 4] = w;
    }
    out
}

/// Per-render state [`LiveSuffixKernel::encode`] needs but that isn't part of the main per-pixel
/// uniform buffer -- mirrors `CropKernel`'s own `transform: Mutex<Affine2D>` pattern for the same
/// reason: `encode` only has `&self` (the `LiveExec` trait's signature), and the numeric NR/
/// sharpen values decide both which code path to take (the fast single-dispatch path when both
/// are a no-op) and what blur radii to build, not just what to write into a uniform buffer.
#[derive(Debug, Clone, Copy)]
struct DetailState {
    sharpen: SharpenParams,
    noise_reduction: NoiseReductionParams,
    pixel_scale: f32,
}

impl Default for DetailState {
    fn default() -> Self {
        Self {
            sharpen: SharpenParams::default(),
            noise_reduction: NoiseReductionParams::default(),
            pixel_scale: 1.0,
        }
    }
}

/// Fixed blur sigma (in native pixels, before `pixel_scale`) for Noise Reduction's own reference
/// blur -- LRC's Luminance/Color Noise Reduction sliders don't expose a separate radius control,
/// unlike Sharpening's `radius_px`, so this is a single v1 constant rather than a slider-driven
/// value.
const NR_BASE_SIGMA: f32 = 2.0;

/// Fuses #46's Tone Curve + HSL into the same per-pixel dispatch every other live stage already
/// shares (`live_suffix.wgsl`), and adds a second, separate multi-pass path for Sharpening/Noise
/// Reduction (`detail_blur.wgsl` + `detail_combine.wgsl`) -- unlike every other live stage, those
/// two need neighboring pixels, not just this pixel's own value, so they can't be folded into the
/// same per-pixel shader. `LiveExec::encode` is still called exactly once per render either way
/// (ADR-0044's "one fused live dispatch" invariant is about dispatch *count* from the render
/// graph's point of view, not about how many compute passes one `encode` call may itself record --
/// `DecodeExec` already sets this precedent for a baked node), and when both Sharpening and Noise
/// Reduction are at their default (no-op) values, `encode` takes a single-pass fast path identical
/// to this stage's pre-#46 cost -- the common case (an unedited or Detail-panel-untouched image)
/// pays nothing extra.
pub struct LiveSuffixKernel {
    pipeline: wgpu::ComputePipeline,
    uniform_buf: wgpu::Buffer,
    blur_pipeline: wgpu::ComputePipeline,
    combine_pipeline: wgpu::ComputePipeline,
    nr_h_buf: wgpu::Buffer,
    nr_v_buf: wgpu::Buffer,
    sharpen_h_buf: wgpu::Buffer,
    sharpen_v_buf: wgpu::Buffer,
    combine_buf: wgpu::Buffer,
    detail_state: std::sync::Mutex<DetailState>,
    /// The DCP tables currently bound (1x1x1 dummies when a profile lacks them) and a fingerprint
    /// of each so `set_params` only re-uploads when content actually changed.
    profile_tables: std::sync::Mutex<ProfileTables>,
    profile_sampler: wgpu::Sampler,
}

struct ProfileTables {
    hue_sat_view: wgpu::TextureView,
    look_view: wgpu::TextureView,
    hue_sat_fp: Option<u64>,
    look_fp: Option<u64>,
}

/// A 3D table texture: width = saturation, height = hue, depth = value (the DNG SDK's on-disk
/// order, value outermost / hue middle / saturation innermost, uploads with no transpose).
fn upload_table(gpu: &GpuContext, map: &HueSatMap) -> wgpu::TextureView {
    // A table larger than the adapter's 3D-texture limit would be a wgpu validation panic on the
    // render thread. `nicti-calico` already bounds table axes at parse time; this is the
    // belt-and-braces check against the actual device (an unusable table degrades to the no-op
    // dummy rather than crashing).
    let max_dim = gpu.device.limits().max_texture_dimension_3d as usize;
    let unusable = map.sat_divisions == 0
        || map.hue_divisions == 0
        || map.val_divisions == 0
        || map
            .sat_divisions
            .max(map.hue_divisions)
            .max(map.val_divisions)
            > max_dim
        || map.data.len() != map.sat_divisions * map.hue_divisions * map.val_divisions;
    if unusable {
        return dummy_table(gpu);
    }
    let size = wgpu::Extent3d {
        width: map.sat_divisions as u32,
        height: map.hue_divisions as u32,
        depth_or_array_layers: map.val_divisions as u32,
    };
    let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("dcp table"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D3,
        format: wgpu::TextureFormat::Rgba16Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let texels: Vec<u16> = map
        .data
        .iter()
        .flat_map(|e| [e[0], e[1], e[2], 0.0].map(|v| half::f16::from_f32(v).to_bits()))
        .collect();
    gpu.queue.write_texture(
        texture.as_image_copy(),
        bytemuck::cast_slice(&texels),
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(map.sat_divisions as u32 * 8), // 4 x f16
            rows_per_image: Some(map.hue_divisions as u32),
        },
        size,
    );
    texture.create_view(&wgpu::TextureViewDescriptor::default())
}

/// The bound-but-unused table when a profile has no HueSatMap/LookTable: the auto-derived bind
/// group layout includes bindings 3/4 whenever the shader references them, so they must always be
/// supplied. Never sampled (`profile0` flags gate every read).
fn dummy_table(gpu: &GpuContext) -> wgpu::TextureView {
    upload_table(
        gpu,
        &HueSatMap {
            hue_divisions: 1,
            sat_divisions: 1,
            val_divisions: 1,
            data: vec![[0.0, 1.0, 1.0]],
        },
    )
}

fn make_uniform_buffer(gpu: &GpuContext, label: &str, size: u64) -> wgpu::Buffer {
    gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

impl LiveSuffixKernel {
    pub fn new(gpu: &GpuContext) -> Self {
        let pipeline = make_compute_pipeline(
            &gpu.device,
            include_str!("../shaders/live_suffix.wgsl"),
            "main",
        );
        let blur_pipeline = make_compute_pipeline(
            &gpu.device,
            include_str!("../shaders/detail_blur.wgsl"),
            "main",
        );
        let combine_pipeline = make_compute_pipeline(
            &gpu.device,
            include_str!("../shaders/detail_combine.wgsl"),
            "main",
        );
        let uniform_buf = make_uniform_buffer(
            gpu,
            "live_suffix uniforms",
            std::mem::size_of::<LiveUniforms>() as u64,
        );
        let blur_size = std::mem::size_of::<BlurUniforms>() as u64;
        Self {
            pipeline,
            uniform_buf,
            blur_pipeline,
            combine_pipeline,
            nr_h_buf: make_uniform_buffer(gpu, "detail_blur nr h", blur_size),
            nr_v_buf: make_uniform_buffer(gpu, "detail_blur nr v", blur_size),
            sharpen_h_buf: make_uniform_buffer(gpu, "detail_blur sharpen h", blur_size),
            sharpen_v_buf: make_uniform_buffer(gpu, "detail_blur sharpen v", blur_size),
            combine_buf: make_uniform_buffer(
                gpu,
                "detail_combine uniforms",
                std::mem::size_of::<CombineUniforms>() as u64,
            ),
            detail_state: std::sync::Mutex::new(DetailState::default()),
            profile_tables: std::sync::Mutex::new(ProfileTables {
                hue_sat_view: dummy_table(gpu),
                look_view: dummy_table(gpu),
                hue_sat_fp: None,
                look_fp: None,
            }),
            profile_sampler: gpu.device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("dcp table sampler"),
                address_mode_u: wgpu::AddressMode::ClampToEdge, // saturation
                address_mode_v: wgpu::AddressMode::Repeat,      // hue wraps
                address_mode_w: wgpu::AddressMode::ClampToEdge, // value
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                ..Default::default()
            }),
        }
    }

    /// Uploads this render's params -- call before `Renderer::render` whenever any of them
    /// changed (a no-op `write_buffer`, not a pipeline rebuild, if nothing did).
    pub fn set_params(&self, gpu: &GpuContext, params: &LiveParams) {
        let m = params.working_space_matrix;
        let exposure_mult = color::exposure_multiplier(params.exposure.stops);
        let lut = color::build_tone_curve_lut(&params.tone_curve);
        let u = LiveUniforms {
            col0: [m[0][0], m[1][0], m[2][0], 0.0],
            col1: [m[0][1], m[1][1], m[2][1], 0.0],
            col2: [m[0][2], m[1][2], m[2][2], 0.0],
            tone0: [
                exposure_mult,
                params.tone.contrast,
                params.tone.highlights,
                params.tone.shadows,
            ],
            tone1: [
                params.tone.whites,
                params.tone.blacks,
                params.vibrance.amount,
                0.0,
            ],
            curve_lut: pack_lut(&lut),
            hsl_bands: pack_hsl(&params.hsl),
            profile0: [0.0; 4],
            profile1: [1.0, 0.0, 0.0, 0.0],
        };
        let mut u = u;
        {
            let mut tables = self.profile_tables.lock().unwrap();
            let srgb = |e: TableEncoding| f32::from(e == TableEncoding::Srgb);
            let (hsm, look) = match &params.camera_profile {
                Some(p) => (p.hue_sat_map.as_ref(), p.look_table.as_ref()),
                None => (None, None),
            };
            // Re-upload a table only when its content changed (a WB drag re-blends the HueSatMap
            // every frame, but an unchanged blend costs nothing).
            let hsm_fp = hsm.map(fingerprint);
            if hsm_fp != tables.hue_sat_fp {
                tables.hue_sat_view =
                    hsm.map_or_else(|| dummy_table(gpu), |m| upload_table(gpu, m));
                tables.hue_sat_fp = hsm_fp;
            }
            let look_fp = look.map(fingerprint);
            if look_fp != tables.look_fp {
                tables.look_view = look.map_or_else(|| dummy_table(gpu), |m| upload_table(gpu, m));
                tables.look_fp = look_fp;
            }
            if let Some(p) = &params.camera_profile {
                u.profile0 = [
                    f32::from(hsm.is_some()),
                    f32::from(look.is_some()),
                    srgb(p.hue_sat_encoding),
                    srgb(p.look_encoding),
                ];
                u.profile1[0] = p.baseline_exposure_multiplier;
            }
        }
        gpu.queue
            .write_buffer(&self.uniform_buf, 0, bytemuck::bytes_of(&u));

        *self.detail_state.lock().unwrap() = DetailState {
            sharpen: params.sharpen,
            noise_reduction: params.noise_reduction,
            pixel_scale: params.pixel_scale,
        };
    }

    fn dispatch_pointwise(
        &self,
        gpu: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        input: &FrameTexture,
        output: &FrameTexture,
    ) {
        let bind_group_layout = self.pipeline.get_bind_group_layout(0);
        let tables = self.profile_tables.lock().unwrap();
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
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&tables.hue_sat_view),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::TextureView(&tables.look_view),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: wgpu::BindingResource::Sampler(&self.profile_sampler),
                },
            ],
        });
        drop(tables);
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

    /// One direction (horizontal or vertical) of a separable blur -- `call.buf` is one of this
    /// kernel's own dedicated blur uniform buffers (never shared between two distinct blur
    /// invocations recorded in the same `encode` call: `gpu.queue.write_buffer` writes all land
    /// before the encoder's own commands ever execute, so two dispatches sharing one buffer would
    /// both see only the *last* write, not a snapshot each -- see this kernel's own struct doc
    /// comment).
    fn dispatch_blur(
        &self,
        gpu: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        input: &FrameTexture,
        output: &FrameTexture,
        call: BlurCall<'_>,
    ) {
        gpu.queue
            .write_buffer(call.buf, 0, bytemuck::bytes_of(&call.uniforms));

        let bind_group_layout = self.blur_pipeline.get_bind_group_layout(0);
        let bind_group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("detail_blur bind group"),
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
                    resource: call.buf.as_entire_binding(),
                },
            ],
        });
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("detail_blur"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&self.blur_pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.dispatch_workgroups(
            output.extent.width.div_ceil(8),
            output.extent.height.div_ceil(8),
            1,
        );
    }

    /// Runs both blur directions for one sigma, allocating fresh intermediate textures --
    /// intermediates never need pooling here (unlike a hot-path bake tier) since this only runs
    /// on an already-live-recomputed frame, at most a handful of times per user interaction.
    fn run_blur(
        &self,
        gpu: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        src: &FrameTexture,
        sigma: f32,
        h_buf: &wgpu::Buffer,
        v_buf: &wgpu::Buffer,
    ) -> FrameTexture {
        let extent = src.extent;
        let weights = pack_blur_weights(detail::gaussian_kernel(sigma));
        let horizontal = FrameTexture::new(gpu, extent);
        self.dispatch_blur(
            gpu,
            encoder,
            src,
            &horizontal,
            BlurCall {
                uniforms: BlurUniforms {
                    direction: [0, 0, 0, 0],
                    weights,
                },
                buf: h_buf,
            },
        );
        let vertical = FrameTexture::new(gpu, extent);
        self.dispatch_blur(
            gpu,
            encoder,
            &horizontal,
            &vertical,
            BlurCall {
                uniforms: BlurUniforms {
                    direction: [1, 0, 0, 0],
                    weights,
                },
                buf: v_buf,
            },
        );
        vertical
    }
}

/// Bundles a blur dispatch's uniform contents with the dedicated buffer to write them into --
/// see [`LiveSuffixKernel::dispatch_blur`]'s own doc comment for why the buffer must be one of
/// this kernel's own per-invocation buffers, never shared.
struct BlurCall<'a> {
    uniforms: BlurUniforms,
    buf: &'a wgpu::Buffer,
}

impl LiveExec for LiveSuffixKernel {
    fn encode(
        &self,
        gpu: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        input: &FrameTexture,
        output: &FrameTexture,
    ) {
        let detail = *self.detail_state.lock().unwrap();
        if detail.sharpen.is_noop() && detail.noise_reduction.is_noop() {
            self.dispatch_pointwise(gpu, encoder, input, output);
            return;
        }

        let extent = output.extent;
        let stage_a = FrameTexture::new(gpu, extent);
        self.dispatch_pointwise(gpu, encoder, input, &stage_a);

        let nr_sigma = (NR_BASE_SIGMA * detail.pixel_scale).max(0.05);
        let sharpen_sigma = (detail.sharpen.radius_px * detail.pixel_scale).max(0.05);
        let nr_blurred = self.run_blur(
            gpu,
            encoder,
            &stage_a,
            nr_sigma,
            &self.nr_h_buf,
            &self.nr_v_buf,
        );
        let sharpen_blurred = self.run_blur(
            gpu,
            encoder,
            &stage_a,
            sharpen_sigma,
            &self.sharpen_h_buf,
            &self.sharpen_v_buf,
        );

        let cu = CombineUniforms {
            nr: [
                detail.noise_reduction.luminance,
                detail.noise_reduction.color,
                detail.noise_reduction.detail,
                0.0,
            ],
            sharpen: [detail.sharpen.amount, 0.0, detail.sharpen.detail, 0.0],
        };
        gpu.queue
            .write_buffer(&self.combine_buf, 0, bytemuck::bytes_of(&cu));

        let bind_group_layout = self.combine_pipeline.get_bind_group_layout(0);
        let bind_group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("detail_combine bind group"),
            layout: &bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&stage_a.view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&nr_blurred.view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&sharpen_blurred.view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&output.view),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: self.combine_buf.as_entire_binding(),
                },
            ],
        });
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("detail_combine"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&self.combine_pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.dispatch_workgroups(extent.width.div_ceil(8), extent.height.div_ceil(8), 1);
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
            TONE_CURVE,
            VIBRANCE,
            HSL,
            SHARPEN,
            NOISE_REDUCTION,
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
        assert_eq!(tone_curve_stage().kind(), StageKind::Live);
        assert_eq!(vibrance_stage().kind(), StageKind::Live);
        assert_eq!(hsl_stage().kind(), StageKind::Live);
        assert_eq!(sharpen_stage().kind(), StageKind::Live);
        assert_eq!(noise_reduction_stage().kind(), StageKind::Live);
    }

    #[test]
    fn crop_stage_kind_is_geometry() {
        assert_eq!(crop_stage().kind(), StageKind::Geometry);
    }

    use crate::test_util::shared_test_gpu as test_gpu;

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

    /// CPU reference for `normalize.wgsl`'s exact per-pixel formula -- the production
    /// `normalize_pixels`, so every decode parity test below also covers the function the AI
    /// removal path relies on.
    fn normalize_cpu_reference(frame: &LinearFrame) -> Vec<[f32; 4]> {
        normalize_pixels(frame)
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

    /// A synthetic camera profile with smooth, non-trivial HueSatMap and sRGB-encoded LookTable
    /// plus a baseline exposure offset, so every profile stage visibly changes the output.
    fn synthetic_profile() -> nicti_calico::dcp::DcpProfile {
        use nicti_calico::dcp::{DcpProfile, TableEncoding};
        let d50 = [0.9642, 1.0, 0.8249];
        let fm = [[d50[0], 0.0, 0.0], [0.0, d50[1], 0.0], [0.0, 0.0, d50[2]]];
        let identity = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
        let smooth = |hue_div: usize, sat_div: usize, val_div: usize, amp: f32| {
            let mut data = Vec::new();
            for v in 0..val_div {
                for h in 0..hue_div {
                    for sa in 0..sat_div {
                        let ang = h as f32 / hue_div as f32 * std::f32::consts::TAU;
                        data.push([
                            amp * ang.sin(),
                            0.85 + 0.1 * sa as f32 / sat_div as f32,
                            1.0 + 0.15 * (v as f32 / val_div as f32) * ang.cos().abs(),
                        ]);
                    }
                }
            }
            HueSatMap {
                hue_divisions: hue_div,
                sat_divisions: sat_div,
                val_divisions: val_div,
                data,
            }
        };
        DcpProfile {
            name: "synthetic".into(),
            unique_camera_model: "TEST".into(),
            illuminant1_cct: 2856.0,
            illuminant2_cct: 6504.0,
            color_matrix1: identity,
            color_matrix2: identity,
            forward_matrix1: Some(fm),
            forward_matrix2: Some(fm),
            hue_sat_map1: Some(smooth(12, 4, 3, 12.0)),
            hue_sat_map2: Some(smooth(12, 4, 3, 6.0)),
            look_table: Some(smooth(9, 3, 4, 8.0)),
            tone_curve_points: None,
            baseline_exposure_offset: -0.3,
            hue_sat_map_encoding: TableEncoding::Linear,
            look_table_encoding: TableEncoding::Srgb,
        }
    }

    #[test]
    fn live_suffix_with_a_camera_profile_matches_the_cpu_reference() {
        let Some(gpu) = test_gpu() else { return };
        let extent = crate::frame::Extent {
            width: 4,
            height: 2,
        };
        // Camera-space samples with real chroma (a saturated red, green, blue, skin-ish, neutral).
        let input_data = vec![
            [0.55, 0.08, 0.06, 1.0],
            [0.10, 0.50, 0.09, 1.0],
            [0.07, 0.10, 0.60, 1.0],
            [0.45, 0.30, 0.20, 1.0],
            [0.30, 0.30, 0.30, 1.0],
            [0.50, 0.42, 0.05, 1.0],
            [0.05, 0.40, 0.45, 1.0],
            [0.35, 0.12, 0.40, 1.0],
        ];
        let input = crate::test_util::upload_frame(&gpu, extent, &input_data);
        let output = FrameTexture::new(&gpu, extent);

        let gains = [1.8f64, 1.0, 1.3];
        let solution = Arc::new(synthetic_profile().solve(gains));
        let params = LiveParams {
            working_space_matrix: solution.camera_to_working,
            camera_profile: Some(solution.clone()),
            ..Default::default()
        };
        let kernel = LiveSuffixKernel::new(&gpu);
        kernel.set_params(&gpu, &params);
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        kernel.encode(&gpu, &mut encoder, &input, &output);
        gpu.queue.submit(Some(encoder.finish()));
        let actual = crate::test_util::read_frame(&gpu, &output);

        // A second render with the profile's tables stripped: the profile stages must visibly
        // matter, or this test proves nothing.
        let mut stripped = (*solution).clone();
        stripped.hue_sat_map = None;
        stripped.look_table = None;
        stripped.baseline_exposure_multiplier = 1.0;
        let mut biggest_effect = 0.0f32;

        let tone = ToneParams::default();
        let lut = color::build_tone_curve_lut(&ToneCurveParams::default());
        let hsl = HslParams::default();
        for (i, px) in input_data.iter().enumerate() {
            let mut rgb = solution.apply_cpu([px[0], px[1], px[2]]);
            let plain = stripped.apply_cpu([px[0], px[1], px[2]]);
            for c in 0..3 {
                biggest_effect = biggest_effect.max((rgb[c] - plain[c]).abs());
            }
            rgb = color::apply_tone(rgb, &tone);
            rgb = color::apply_tone_curve(rgb, &lut);
            rgb = color::apply_vibrance(rgb, 0.0);
            rgb = color::apply_hsl(rgb, &hsl);
            for c in 0..3 {
                assert!(
                    (actual[i][c] - rgb[c]).abs() < 0.015,
                    "pixel {i} channel {c}: gpu={} cpu={} (input {px:?})",
                    actual[i][c],
                    rgb[c]
                );
            }
        }
        assert!(
            biggest_effect > 0.02,
            "the profile barely changed the output ({biggest_effect}); the test is vacuous"
        );
    }

    #[test]
    fn changing_the_profile_tables_re_uploads_them_and_dropping_the_profile_restores_the_matrix_only_result(
    ) {
        let Some(gpu) = test_gpu() else { return };
        let extent = crate::frame::Extent {
            width: 1,
            height: 1,
        };
        let input = crate::test_util::upload_frame(&gpu, extent, &[[0.5, 0.2, 0.1, 1.0]]);
        let output = FrameTexture::new(&gpu, extent);
        let kernel = LiveSuffixKernel::new(&gpu);
        let render = |params: &LiveParams| {
            kernel.set_params(&gpu, params);
            let mut encoder = gpu
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
            kernel.encode(&gpu, &mut encoder, &input, &output);
            gpu.queue.submit(Some(encoder.finish()));
            crate::test_util::read_frame(&gpu, &output)[0]
        };
        let solution = Arc::new(synthetic_profile().solve([1.5, 1.0, 1.2]));
        let matrix = solution.camera_to_working;
        let none = LiveParams {
            working_space_matrix: matrix,
            ..Default::default()
        };
        let with = LiveParams {
            working_space_matrix: matrix,
            camera_profile: Some(solution.clone()),
            ..Default::default()
        };
        let before = render(&none);
        let profiled = render(&with);
        let after = render(&none);
        assert_ne!(before, profiled, "profile should change the pixel");
        assert_eq!(
            before, after,
            "dropping the profile must restore the plain result"
        );
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
        let matrix = color::camera_to_working_space_matrix(cam_mul, &cam_xyz, &WbParams::default());
        let exposure = ExposureParams { stops: 0.7 };
        let tone = ToneParams {
            contrast: 0.3,
            highlights: -0.2,
            shadows: 0.15,
            whites: 0.1,
            blacks: -0.05,
        };
        let vibrance = VibranceParams { amount: 0.4 };
        let tone_curve = ToneCurveParams {
            shadows: 0.2,
            darks: -0.1,
            lights: 0.15,
            highlights: -0.05,
        };
        let mut hsl = HslParams::default();
        hsl.bands[0] = crate::coat::HslBand {
            hue: 0.3,
            saturation: -0.4,
            luminance: 0.2,
        };
        hsl.bands[4] = crate::coat::HslBand {
            hue: -0.2,
            saturation: 0.3,
            luminance: -0.1,
        };
        let exposure_mult = color::exposure_multiplier(exposure.stops);
        let lut = color::build_tone_curve_lut(&tone_curve);

        let kernel = LiveSuffixKernel::new(&gpu);
        kernel.set_params(
            &gpu,
            &LiveParams {
                working_space_matrix: matrix,
                exposure,
                tone,
                tone_curve,
                vibrance,
                hsl,
                ..Default::default()
            },
        );
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        kernel.encode(&gpu, &mut encoder, &input, &output);
        gpu.queue.submit(Some(encoder.finish()));

        let actual = crate::test_util::read_frame(&gpu, &output);
        for (i, px) in input_data.iter().enumerate() {
            let mut rgb = color::mat3_apply(matrix, [px[0], px[1], px[2]]);
            rgb = rgb.map(|c| c * exposure_mult);
            rgb = color::apply_tone(rgb, &tone);
            rgb = color::apply_tone_curve(rgb, &lut);
            rgb = color::apply_vibrance(rgb, vibrance.amount);
            rgb = color::apply_hsl(rgb, &hsl);
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

    /// Same shape as `present_sample_gpu_matches_cpu_reference`, but with a real straighten
    /// rotation baked into the affine transform (#47) -- proves the GPU kernel matches
    /// `geometry::affine_for_crop`'s composed rotation+translation, not just the pre-#47
    /// translation-only case.
    #[test]
    fn present_sample_gpu_matches_cpu_reference_with_straighten_rotation() {
        let Some(gpu) = test_gpu() else { return };
        let extent = crate::frame::Extent {
            width: 6,
            height: 6,
        };
        let input_data: Vec<[f32; 4]> = (0..36)
            .map(|i| [i as f32 * 0.005, i as f32 * 0.01, i as f32 * 0.015, 1.0])
            .collect();
        let input = crate::test_util::upload_frame(&gpu, extent, &input_data);
        let out_extent = crate::frame::Extent {
            width: 6,
            height: 6,
        };
        let output = FrameTexture::new(&gpu, out_extent);

        let rect = crate::geometry::CropRect {
            x: 0.0,
            y: 0.0,
            width: 6.0,
            height: 6.0,
        };
        let transform = crate::geometry::affine_for_crop(rect, 12.0);
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
        let live_ids = [
            WB,
            WORKING_SPACE,
            EXPOSURE,
            TONE,
            TONE_CURVE,
            VIBRANCE,
            HSL,
            SHARPEN,
            NOISE_REDUCTION,
        ];
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
        let matrix = color::camera_to_working_space_matrix(
            frame.cam_mul,
            &frame.cam_xyz,
            &WbParams::default(),
        );
        let exposure = ExposureParams { stops: 0.5 };
        let tone = ToneParams {
            contrast: 0.2,
            ..Default::default()
        };
        let vibrance = VibranceParams { amount: 0.3 };
        let tone_curve = ToneCurveParams {
            shadows: 0.1,
            ..Default::default()
        };
        let mut hsl = HslParams::default();
        hsl.bands[0].saturation = 0.2;
        let sharpen = SharpenParams {
            amount: 0.5,
            radius_px: 1.0,
            detail: 0.5,
        };
        let noise_reduction = NoiseReductionParams {
            luminance: 0.4,
            color: 0.3,
            detail: 0.5,
        };
        let exposure_mult = color::exposure_multiplier(exposure.stops);
        let lut = color::build_tone_curve_lut(&tone_curve);
        live_kernel.set_params(
            &gpu,
            &LiveParams {
                working_space_matrix: matrix,
                exposure,
                tone,
                tone_curve,
                vibrance,
                hsl,
                sharpen,
                noise_reduction,
                camera_profile: None,
                pixel_scale: 1.0,
            },
        );

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
        let point_wise: Vec<[f32; 3]> = decoded
            .iter()
            .map(|d| {
                let mut rgb = color::mat3_apply(matrix, [d[0], d[1], d[2]]);
                rgb = rgb.map(|c| c * exposure_mult);
                rgb = color::apply_tone(rgb, &tone);
                rgb = color::apply_tone_curve(rgb, &lut);
                rgb = color::apply_vibrance(rgb, vibrance.amount);
                rgb = color::apply_hsl(rgb, &hsl);
                rgb
            })
            .collect();

        // Detail (Sharpen/NR) needs neighboring pixels -- build the same blur-per-channel-plane
        // + combine pipeline `LiveSuffixKernel::encode`'s multi-pass path runs on the GPU,
        // against the point-wise result above, over the whole (tiny, 2x2) frame.
        let detail_extent = crate::detail::Extent2D {
            width: extent.width as usize,
            height: extent.height as usize,
        };
        let planes: [Vec<f32>; 3] =
            std::array::from_fn(|c| point_wise.iter().map(|p| p[c]).collect());
        let nr_blurred_planes = planes
            .clone()
            .map(|p| crate::detail::gaussian_blur(&p, detail_extent, 2.0));
        let sharpen_blurred_planes = planes
            .clone()
            .map(|p| crate::detail::gaussian_blur(&p, detail_extent, sharpen.radius_px));

        for (i, original) in point_wise.iter().enumerate() {
            let blurred_nr = std::array::from_fn(|c| nr_blurred_planes[c][i]);
            let blurred_sharpen = std::array::from_fn(|c| sharpen_blurred_planes[c][i]);
            let rgb = crate::detail::apply_detail_rgb(
                *original,
                blurred_nr,
                blurred_sharpen,
                &noise_reduction,
                &sharpen,
            );
            for c in 0..3 {
                assert!(
                    (actual[i][c] - rgb[c]).abs() < 0.03,
                    "pixel {i} channel {c}: gpu={} expected={}",
                    actual[i][c],
                    rgb[c]
                );
            }
        }
    }
}
