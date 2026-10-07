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
    self, ColorGradeParams, CropParams, DefringeParams, EffectsParams, ExposureParams, HslParams,
    NoiseReductionParams, PointColorParams, PointCurveParams, PresenceParams, SharpenParams,
    ToneCurveParams, ToneParams, VibranceParams, VignetteStyle, WbParams,
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
/// Freeform RGB/R/G/B point curves (#432), applied right after the parametric tone curve.
pub const POINT_CURVE: &str = "nicti.point_curve";
pub const VIBRANCE: &str = "nicti.vibrance";
/// Global Texture/Clarity/Dehaze/Saturation (#380); summed with the per-mask deltas in the shader.
pub const PRESENCE: &str = "nicti.presence";
/// Purple/green fringe desaturation (#428): fused into the live dispatch, right after the
/// camera->working matrix, so a slider drag is a uniform write and never rebakes anything.
pub const DEFRINGE: &str = "nicti.defringe";
pub const HSL: &str = "nicti.hsl";
/// Color Grading wheels (#432), OkLab, after HSL.
pub const COLOR_GRADE: &str = "nicti.color_grade";
/// Point Color samples (#432), OkLCh, after Color Grading.
pub const POINT_COLOR: &str = "nicti.point_color";
pub const SHARPEN: &str = "nicti.sharpen";
pub const NOISE_REDUCTION: &str = "nicti.noise_reduction";
pub const CROP: &str = "nicti.crop";
/// Post-crop vignette and grain (#380). A Geometry node: it runs inside the crop's own sample pass,
/// so editing it re-runs only that pass (never the live suffix) and is evaluated in crop-normalized
/// coordinates.
pub const EFFECTS: &str = "nicti.effects";
/// Local corrections (#49): every mask + its adjustments, one stage so a whole set pastes/syncs as a
/// unit and the graph stays fixed-shape. Live: its cost is uniforms, not bakes.
pub const MASKS: &str = "nicti.masks";
/// The fixed neutral render AI masks infer on (post-lens, pre-heal, default tone -- ADR-0049). A
/// keying-only node: nothing renders it, it exists so an AI bake key chains from LENS and so a tone
/// or white-balance edit provably cannot invalidate a model's output.
pub const NEUTRAL: &str = "nicti.neutral";

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

pub fn neutral_stage() -> BasicStage {
    BasicStage {
        id: NEUTRAL,
        kind: StageKind::Baked,
        default_params: || json!({}),
    }
}
pub fn masks_stage() -> BasicStage {
    BasicStage {
        id: MASKS,
        kind: StageKind::Live,
        default_params: || json!({ "corrections": [] }),
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
pub fn lens_stage() -> crate::slit::LensStage {
    crate::slit::LensStage
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
pub fn presence_stage() -> BasicStage {
    BasicStage {
        id: PRESENCE,
        kind: StageKind::Live,
        default_params: || coat::default_value::<PresenceParams>(),
    }
}
pub fn point_curve_stage() -> BasicStage {
    BasicStage {
        id: POINT_CURVE,
        kind: StageKind::Live,
        default_params: || coat::default_value::<PointCurveParams>(),
    }
}
pub fn color_grade_stage() -> BasicStage {
    BasicStage {
        id: COLOR_GRADE,
        kind: StageKind::Live,
        default_params: || coat::default_value::<ColorGradeParams>(),
    }
}
pub fn point_color_stage() -> BasicStage {
    BasicStage {
        id: POINT_COLOR,
        kind: StageKind::Live,
        default_params: || coat::default_value::<PointColorParams>(),
    }
}
pub fn defringe_stage() -> BasicStage {
    BasicStage {
        id: DEFRINGE,
        kind: StageKind::Live,
        default_params: || coat::default_value::<DefringeParams>(),
    }
}
pub fn effects_stage() -> BasicStage {
    BasicStage {
        id: EFFECTS,
        kind: StageKind::Geometry,
        default_params: || coat::default_value::<EffectsParams>(),
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
    /// #321: Look `.xmp` table enabled, its sRGB value encoding, profile tone curve enabled, unused.
    profile2: [f32; 4],
    /// #380: global Presence -- texture, clarity, dehaze, saturation.
    presence: [f32; 4],
    /// #428: purple amount, purple hue lo, purple hue hi, green amount.
    defringe0: [f32; 4],
    /// #428: green hue lo, green hue hi, unused, unused.
    defringe1: [f32; 4],
    /// #432: x = point curves enabled (the LUT texture at binding 13 is only read when 1), yzw unused.
    point_curve: [f32; 4],
    /// #432 OkLab ops (`oklab::OkLabOps`): x = any active, y = grading active, z = point colour
    /// active, w unused.
    ok_flags: [f32; 4],
    /// ProPhoto -> LMS rows, then LMS -> ProPhoto rows (w unused).
    ok_to: [[f32; 4]; 3],
    ok_from: [[f32; 4]; 3],
    /// Grading split midpoint and width, yzw unused.
    grade_k: [f32; 4],
    /// Shadows, midtones, highlights, global wheel offsets: (dL, da, db, unused).
    grade_w: [[f32; 4]; 4],
    /// Per point-colour slot, three vec4s: (L, chroma, hue rad, present), (hue/chroma/light half
    /// widths, has hue), (hue shift rad, saturation, lightness shift, variance).
    points: [[f32; 4]; POINT_COLOR_VEC4S],
}

const POINT_COLOR_VEC4S: usize = crate::coat::MAX_POINT_COLORS * 3;

/// Writes `ops` (Color Grading + Point Color, #432) into `u`'s OkLab uniform fields.
fn write_oklab(u: &mut LiveUniforms, ops: &crate::oklab::OkLabOps) {
    let rows = |m: &color::Mat3| -> [[f32; 4]; 3] {
        std::array::from_fn(|r| [m[r][0], m[r][1], m[r][2], 0.0])
    };
    u.ok_to = rows(&ops.to_lms);
    u.ok_from = rows(&ops.from_lms);
    (u.grade_k, u.grade_w) = match &ops.grade {
        Some(g) => (
            [g.split, g.width, 0.0, 0.0],
            [g.shadows, g.midtones, g.highlights, g.global].map(|o| [o[0], o[1], o[2], 0.0]),
        ),
        None => ([0.0; 4], [[0.0; 4]; 4]),
    };
    u.points = [[0.0; 4]; POINT_COLOR_VEC4S];
    for (i, p) in ops.points.iter().enumerate() {
        if let Some(p) = p {
            u.points[i * 3] = [p.l, p.c, p.h, 1.0];
            u.points[i * 3 + 1] = [
                p.hue_half,
                p.chroma_half,
                p.light_half,
                f32::from(p.has_hue),
            ];
            u.points[i * 3 + 2] = [p.dh, p.sat, p.dl, p.var];
        }
    }
    u.ok_flags = [
        f32::from(!ops.is_noop()),
        f32::from(ops.grade.is_some()),
        f32::from(ops.points.iter().any(Option::is_some)),
        0.0,
    ];
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
    /// x = local sharpness/noise corrections are bound (#49); the rest is padding.
    local: [f32; 4],
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
    /// Freeform point curves (#432); [`PointCurveParams::sanitized`] by the caller or here.
    pub point_curve: PointCurveParams,
    /// Color Grading wheels (#432), OkLab, after HSL.
    pub color_grade: ColorGradeParams,
    /// Point Color samples (#432), OkLCh, after Color Grading.
    pub point_color: PointColorParams,
    pub vibrance: VibranceParams,
    /// Global Texture/Clarity/Dehaze/Saturation (#380). When [`PresenceParams::needs_bases`], the
    /// caller must also bind a `MaskFrame` carrying the matching spatial bases (`MaskEngine::prepare`
    /// with this value) or the spatial part is skipped.
    pub presence: PresenceParams,
    /// Purple/green fringe desaturation (#428), applied right after the camera->working matrix.
    pub defringe: DefringeParams,
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
            point_curve: PointCurveParams::default(),
            color_grade: ColorGradeParams::default(),
            point_color: PointColorParams::default(),
            vibrance: VibranceParams::default(),
            presence: PresenceParams::default(),
            defringe: DefringeParams::default(),
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
    /// The point-curve LUT texture (#432) currently bound (a 2x3 dummy when disabled) and a
    /// fingerprint so `set_params` only re-uploads on a content change.
    point_curve_tex: std::sync::Mutex<PointCurveTex>,
    /// Local corrections (#49): the uniform block (`count` + up to 16 corrections), the atlas
    /// currently bound (a 1x1 dummy and count 0 when there are none) and its sampler.
    mask_buf: wgpu::Buffer,
    mask_state: std::sync::Mutex<MaskBinding>,
    mask_sampler: wgpu::Sampler,
}

/// What the live shader's mask bindings currently point at.
struct MaskBinding {
    /// `None` = no local corrections: the dummy atlas below is bound and count is 0.
    frame: Option<crate::mask::atlas::MaskFrame>,
    /// True when some bound correction adjusts sharpness or noise, which forces the multi-pass
    /// detail path even if the global Detail panel is untouched.
    local_detail: bool,
    dummy: Arc<crate::mask::atlas::Atlas>,
    /// 1x1 stand-ins for the spatial bases (`bases_tex` / `haze_tex`), never read while their
    /// header flag is 0.
    dummy_bands: FrameTexture,
    dummy_haze: crate::mask::kernels::FieldTexture,
}

/// Bytes of the mask uniform block: two `vec4` headers plus 16 corrections x 5 `vec4`s.
const MASK_UNIFORM_BYTES: u64 = 32
    + (crate::mask::params::MAX_CORRECTIONS as u64)
        * std::mem::size_of::<crate::mask::local::LocalUniform>() as u64;

struct PointCurveTex {
    view: wgpu::TextureView,
    fp: Option<u64>,
}

struct ProfileTables {
    hue_sat_view: wgpu::TextureView,
    look_view: wgpu::TextureView,
    hue_sat_fp: Option<u64>,
    look_fp: Option<u64>,
    look_profile_view: wgpu::TextureView,
    look_profile_fp: Option<u64>,
    tone_view: wgpu::TextureView,
    tone_fp: Option<u64>,
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

/// The profile tone curve as an N x 1 `R32Float` texture (read with `textureLoad`, so no
/// filtering support is needed). The dummy is a 2 x 1 identity-free ramp that is never read.
fn upload_tone_lut(gpu: &GpuContext, samples: &[f32]) -> wgpu::TextureView {
    let size = wgpu::Extent3d {
        width: samples.len() as u32,
        height: 1,
        depth_or_array_layers: 1,
    };
    let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("profile tone lut"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R32Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    gpu.queue.write_texture(
        texture.as_image_copy(),
        bytemuck::cast_slice(samples),
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(samples.len() as u32 * 4),
            rows_per_image: Some(1),
        },
        size,
    );
    texture.create_view(&wgpu::TextureViewDescriptor::default())
}

/// The point-curve LUTs (#432) as a 256 x 3 `R32Float` texture, one row per channel (R, G, B),
/// read with `textureLoad`. `None` uploads a never-read 256 x 3 dummy.
fn upload_point_curve_luts(gpu: &GpuContext, luts: Option<&[[f32; 256]; 3]>) -> wgpu::TextureView {
    let flat: Vec<f32> = match luts {
        Some(l) => l.iter().flatten().copied().collect(),
        None => vec![0.0; 256 * 3],
    };
    let size = wgpu::Extent3d {
        width: 256,
        height: 3,
        depth_or_array_layers: 1,
    };
    let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("point curve lut"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R32Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    gpu.queue.write_texture(
        texture.as_image_copy(),
        bytemuck::cast_slice(&flat),
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(256 * 4),
            rows_per_image: Some(3),
        },
        size,
    );
    texture.create_view(&wgpu::TextureViewDescriptor::default())
}

fn dummy_tone_lut(gpu: &GpuContext) -> wgpu::TextureView {
    upload_tone_lut(gpu, &[0.0, 1.0])
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
                look_profile_view: dummy_table(gpu),
                look_profile_fp: None,
                tone_view: dummy_tone_lut(gpu),
                tone_fp: None,
            }),
            point_curve_tex: std::sync::Mutex::new(PointCurveTex {
                view: upload_point_curve_luts(gpu, None),
                fp: None,
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
            mask_buf: make_uniform_buffer(gpu, "live_suffix mask uniforms", MASK_UNIFORM_BYTES),
            mask_state: std::sync::Mutex::new(MaskBinding {
                frame: None,
                local_detail: false,
                dummy: Arc::new(crate::mask::atlas::Atlas::new(gpu, 1, 1, 0)),
                dummy_bands: FrameTexture::new(
                    gpu,
                    crate::frame::Extent {
                        width: 1,
                        height: 1,
                    },
                ),
                dummy_haze: crate::mask::kernels::FieldTexture::new(gpu, 1, 1),
            }),
            // Clamp on every axis: unlike the DCP sampler, the mask must not wrap at the borders.
            mask_sampler: gpu.device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("mask atlas sampler"),
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                address_mode_w: wgpu::AddressMode::ClampToEdge,
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                ..Default::default()
            }),
        }
    }

    /// Binds this render's local corrections (`None` clears them). Call alongside
    /// [`Self::set_params`] whenever the masks changed; it writes the mask uniform block and never
    /// rebuilds a pipeline.
    pub fn set_masks(&self, gpu: &GpuContext, frame: Option<&crate::mask::atlas::MaskFrame>) {
        let count = frame.map_or(0, |f| {
            f.uniforms.len().min(crate::mask::params::MAX_CORRECTIONS)
        });
        let mut block = vec![0u8; MASK_UNIFORM_BYTES as usize];
        // header: (count, bands bound, haze bound, 0), then the airlight, then the corrections.
        let header: [f32; 8] = match frame {
            Some(f) => [
                count as f32,
                f32::from(f.bases.bands.is_some()),
                f32::from(f.bases.haze.is_some()),
                0.0,
                f.bases.airlight[0],
                f.bases.airlight[1],
                f.bases.airlight[2],
                0.0,
            ],
            None => [0.0; 8],
        };
        block[..32].copy_from_slice(bytemuck::cast_slice(&header));
        if let Some(f) = frame {
            let bytes: &[u8] = bytemuck::cast_slice(&f.uniforms[..count]);
            block[32..32 + bytes.len()].copy_from_slice(bytes);
        }
        gpu.queue.write_buffer(&self.mask_buf, 0, &block);
        let mut state = self.mask_state.lock().unwrap();
        state.local_detail = frame.is_some_and(|f| {
            f.uniforms[..count]
                .iter()
                .any(|u| u.d0[0] > 0.0 && (u.d2[3] != 0.0 || u.d3[3] != 0.0))
        });
        state.frame = frame.cloned();
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
            profile2: [0.0; 4],
            presence: [
                params.presence.texture,
                params.presence.clarity,
                params.presence.dehaze,
                params.presence.saturation,
            ],
            defringe0: [
                params.defringe.purple_amount,
                params.defringe.purple_hue_lo,
                params.defringe.purple_hue_hi,
                params.defringe.green_amount,
            ],
            defringe1: [
                params.defringe.green_hue_lo,
                params.defringe.green_hue_hi,
                0.0,
                0.0,
            ],
            point_curve: [0.0; 4],
            ok_flags: [0.0; 4],
            ok_to: [[0.0; 4]; 3],
            ok_from: [[0.0; 4]; 3],
            grade_k: [0.0; 4],
            grade_w: [[0.0; 4]; 4],
            points: [[0.0; 4]; POINT_COLOR_VEC4S],
        };
        let mut u = u;
        {
            let ops = crate::oklab::OkLabOps::new(&params.color_grade, &params.point_color);
            write_oklab(&mut u, &ops);
        }
        {
            let luts = color::build_point_curve_luts(&params.point_curve);
            let fp = luts.as_ref().map(|l| {
                let hash = blake3::hash(bytemuck::cast_slice(l.as_slice()));
                u64::from_le_bytes(hash.as_bytes()[..8].try_into().expect("8 bytes"))
            });
            let mut tex = self.point_curve_tex.lock().unwrap();
            if fp != tex.fp {
                tex.view = upload_point_curve_luts(gpu, luts.as_ref());
                tex.fp = fp;
            }
            u.point_curve[0] = f32::from(luts.is_some());
        }
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
            let look_profile = params
                .camera_profile
                .as_ref()
                .and_then(|p| p.look_profile.as_ref());
            let lp_fp = look_profile.map(fingerprint);
            if lp_fp != tables.look_profile_fp {
                tables.look_profile_view =
                    look_profile.map_or_else(|| dummy_table(gpu), |m| upload_table(gpu, m));
                tables.look_profile_fp = lp_fp;
            }
            let tone = params.camera_profile.as_ref().map(|p| &p.tone_lut);
            let tone_fp = tone.map(|t| t.fingerprint());
            if tone_fp != tables.tone_fp {
                tables.tone_view = tone.map_or_else(
                    || dummy_tone_lut(gpu),
                    |t| upload_tone_lut(gpu, t.samples()),
                );
                tables.tone_fp = tone_fp;
            }
            if let Some(p) = &params.camera_profile {
                u.profile2 = [
                    f32::from(look_profile.is_some()),
                    srgb(p.look_profile_encoding),
                    1.0,
                    0.0,
                ];
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
        let point_curve = self.point_curve_tex.lock().unwrap();
        let mask_state = self.mask_state.lock().unwrap();
        let atlas_view = match &mask_state.frame {
            Some(f) => &f.atlas.array_view,
            None => &mask_state.dummy.array_view,
        };
        let bands_view = match mask_state
            .frame
            .as_ref()
            .and_then(|f| f.bases.bands.as_ref())
        {
            Some(b) => &b.view,
            None => &mask_state.dummy_bands.view,
        };
        let haze_view = match mask_state
            .frame
            .as_ref()
            .and_then(|f| f.bases.haze.as_ref())
        {
            Some(h) => &h.view,
            None => &mask_state.dummy_haze.view,
        };
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
                wgpu::BindGroupEntry {
                    binding: 6,
                    resource: wgpu::BindingResource::TextureView(atlas_view),
                },
                wgpu::BindGroupEntry {
                    binding: 7,
                    resource: self.mask_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 8,
                    resource: wgpu::BindingResource::Sampler(&self.mask_sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 9,
                    resource: wgpu::BindingResource::TextureView(bands_view),
                },
                wgpu::BindGroupEntry {
                    binding: 10,
                    resource: wgpu::BindingResource::TextureView(haze_view),
                },
                wgpu::BindGroupEntry {
                    binding: 11,
                    resource: wgpu::BindingResource::TextureView(&tables.look_profile_view),
                },
                wgpu::BindGroupEntry {
                    binding: 12,
                    resource: wgpu::BindingResource::TextureView(&tables.tone_view),
                },
                wgpu::BindGroupEntry {
                    binding: 13,
                    resource: wgpu::BindingResource::TextureView(&point_curve.view),
                },
            ],
        });
        drop(point_curve);
        drop(mask_state);
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
        let local_detail = self.mask_state.lock().unwrap().local_detail;
        if detail.sharpen.is_noop() && detail.noise_reduction.is_noop() && !local_detail {
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
            local: [f32::from(local_detail), 0.0, 0.0, 0.0],
        };
        gpu.queue
            .write_buffer(&self.combine_buf, 0, bytemuck::bytes_of(&cu));

        let mask_state = self.mask_state.lock().unwrap();
        let atlas_view = match &mask_state.frame {
            Some(f) => &f.atlas.array_view,
            None => &mask_state.dummy.array_view,
        };
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
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: wgpu::BindingResource::TextureView(atlas_view),
                },
                wgpu::BindGroupEntry {
                    binding: 6,
                    resource: self.mask_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 7,
                    resource: wgpu::BindingResource::Sampler(&self.mask_sampler),
                },
            ],
        });
        drop(mask_state);
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
    // #380 effects -- field order mirrors `present_sample.wgsl`'s `Uniforms`.
    n_a: f32,
    n_b: f32,
    n_c: f32,
    n_d: f32,
    n_tx: f32,
    n_ty: f32,
    crop_w: f32,
    crop_h: f32,
    v_amount: f32,
    v_mid: f32,
    v_feather: f32,
    v_round: f32,
    v_highlights: f32,
    v_style: u32,
    g_amount: f32,
    g_size: f32,
    g_rough: f32,
    g_seed: u32,
    flags: u32,
    pad: u32,
}

/// What the geometry pass needs to apply the post-crop effects (#380): the params plus the crop it
/// is relative to. Set once per render -- it does not change per export tile, which is exactly what
/// keeps a tile's pattern identical to the whole frame's.
#[derive(Debug, Clone, Copy)]
struct EffectsState {
    params: EffectsParams,
    /// Source coordinate -> crop-normalized `(u, v)` (`effects::crop_norm`).
    norm: Affine2D,
    crop_w: f32,
    crop_h: f32,
}

pub struct CropKernel {
    pipeline: wgpu::ComputePipeline,
    transform: std::sync::Mutex<Affine2D>,
    effects: std::sync::Mutex<Option<EffectsState>>,
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
            effects: std::sync::Mutex::new(None),
            uniform_buf,
        }
    }

    /// Records this render's post-crop effects (#380): `crop_transform` is the same output ->
    /// source transform [`Self::set_transform`] gets for an untiled render (a tiled one calls
    /// `set_transform` per tile but this once), and `crop_w`/`crop_h` the crop rect's size. A noop
    /// `params`, or a degenerate crop, clears the effects so the pass runs the exact pre-#380 path.
    pub fn set_effects(
        &self,
        params: &EffectsParams,
        crop_transform: Affine2D,
        crop_w: f32,
        crop_h: f32,
    ) {
        let params = params.sanitized();
        let state = if params.is_noop() {
            None
        } else {
            crate::effects::crop_norm(crop_transform, crop_w, crop_h).map(|norm| EffectsState {
                params,
                norm,
                crop_w,
                crop_h,
            })
        };
        *self.effects.lock().unwrap() = state;
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
        let fx = *self.effects.lock().unwrap();
        let mut u = PresentUniforms {
            a: transform.a,
            b: transform.b,
            c: transform.c,
            d: transform.d,
            tx: transform.tx,
            ty: transform.ty,
            out_width: output.extent.width,
            out_height: output.extent.height,
            ..Zeroable::zeroed()
        };
        if let Some(fx) = fx {
            let e = fx.params;
            u.n_a = fx.norm.a;
            u.n_b = fx.norm.b;
            u.n_c = fx.norm.c;
            u.n_d = fx.norm.d;
            u.n_tx = fx.norm.tx;
            u.n_ty = fx.norm.ty;
            u.crop_w = fx.crop_w;
            u.crop_h = fx.crop_h;
            u.v_amount = e.vignette_amount;
            u.v_mid = e.vignette_midpoint;
            u.v_feather = e.vignette_feather;
            u.v_round = e.vignette_roundness;
            u.v_highlights = e.vignette_highlights;
            u.v_style = match e.vignette_style {
                VignetteStyle::HighlightPriority => 0,
                VignetteStyle::ColorPriority => 1,
                VignetteStyle::PaintOverlay => 2,
            };
            u.g_amount = e.grain_amount;
            u.g_size = e.grain_size;
            u.g_rough = e.grain_roughness;
            u.g_seed = e.grain_seed;
            u.flags = u32::from(e.vignette_active()) | (u32::from(e.grain_active()) << 1);
        }
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
            PRESENCE,
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
        assert_eq!(presence_stage().kind(), StageKind::Live);
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
            dng_opcode_list3: None,
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
            dng_opcode_list3: None,
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
            dng_opcode_list3: None,
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
    fn smooth_table(hue_div: usize, sat_div: usize, val_div: usize, amp: f32) -> HueSatMap {
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
    }

    fn synthetic_profile() -> nicti_calico::dcp::DcpProfile {
        use nicti_calico::dcp::{DcpProfile, TableEncoding};
        let d50 = [0.9642, 1.0, 0.8249];
        let fm = [[d50[0], 0.0, 0.0], [0.0, d50[1], 0.0], [0.0, 0.0, d50[2]]];
        let identity = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
        let smooth = smooth_table;
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
            tone_curve_points: Some(vec![
                (0.0, 0.0),
                (0.12, 0.07),
                (0.5, 0.6),
                (0.85, 0.93),
                (1.0, 1.0),
            ]),
            baseline_exposure_offset: -0.3,
            hue_sat_map_encoding: TableEncoding::Linear,
            look_table_encoding: TableEncoding::Srgb,
            default_black_render: nicti_calico::dcp::BlackRender::Auto,
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
        let look = nicti_calico::xmp_profile::LookProfile {
            name: "synthetic look".into(),
            look_table: smooth_table(6, 3, 3, 5.0),
            encoding: TableEncoding::Srgb,
            unsupported_settings: vec![],
        };
        let solution = Arc::new(synthetic_profile().solve(gains).with_look(&look));
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
        stripped.look_profile = None;
        stripped.tone_lut = Arc::new(nicti_calico::tonecurve::ToneCurveLut::from_curve(
            &nicti_calico::tonecurve::ToneCurve::identity(),
        ));
        stripped.baseline_exposure_multiplier = 1.0;
        let mut biggest_effect = 0.0f32;

        let tone = ToneParams::default();
        let lut = color::build_tone_curve_lut(&ToneCurveParams::default());
        let hsl = HslParams::default();
        for (i, px) in input_data.iter().enumerate() {
            let mut rgb = solution.apply_cpu_toned([px[0], px[1], px[2]]);
            let plain = stripped.apply_cpu_toned([px[0], px[1], px[2]]);
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

    /// The profile tone's edge cases against the CPU twin (#321): a profile with no curve (the ACR
    /// default), channels above 1 and exact ties for the max/min channel.
    #[test]
    fn profile_tone_edge_inputs_with_the_default_curve_match_the_cpu_reference() {
        let Some(gpu) = test_gpu() else { return };
        let extent = crate::frame::Extent {
            width: 3,
            height: 2,
        };
        let input_data = vec![
            [1.8, 0.9, 0.2, 1.0],     // clips above 1 after the matrix
            [0.4, 0.4, 0.1, 1.0],     // tied maxima
            [0.1, 0.4, 0.1, 1.0],     // tied minima
            [0.3, 0.3, 0.3, 1.0],     // neutral
            [0.0, 0.0, 0.0, 1.0],     // black
            [0.02, 0.01, 0.015, 1.0], // deep shadow, where the sqrt-space table matters
        ];
        let input = crate::test_util::upload_frame(&gpu, extent, &input_data);
        let output = FrameTexture::new(&gpu, extent);
        let mut profile = synthetic_profile();
        profile.tone_curve_points = None;
        profile.hue_sat_map1 = None;
        profile.hue_sat_map2 = None;
        profile.look_table = None;
        profile.baseline_exposure_offset = 0.0;
        let solution = Arc::new(profile.solve([1.0; 3]));
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

        let tone = ToneParams::default();
        let lut = color::build_tone_curve_lut(&ToneCurveParams::default());
        let hsl = HslParams::default();
        let untoned = {
            let mut s = (*solution).clone();
            s.tone_lut = Arc::new(nicti_calico::tonecurve::ToneCurveLut::from_curve(
                &nicti_calico::tonecurve::ToneCurve::identity(),
            ));
            s
        };
        let mut biggest_tone_effect = 0.0f32;
        for (i, px) in input_data.iter().enumerate() {
            let mut rgb = solution.apply_cpu_toned([px[0], px[1], px[2]]);
            let plain = untoned.apply_cpu_toned([px[0], px[1], px[2]]);
            for c in 0..3 {
                biggest_tone_effect = biggest_tone_effect.max((rgb[c] - plain[c]).abs());
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
            biggest_tone_effect > 0.02,
            "the default curve barely changed the output ({biggest_tone_effect})"
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

    /// #432: the fused live pass's point curves (shader) match `color::apply_point_curve` (CPU twin)
    /// after the parametric curve, and an identity curve set leaves the pixel untouched.
    #[test]
    fn point_curves_in_the_live_pass_match_the_cpu_twin_and_identity_is_a_noop() {
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
        let kernel = LiveSuffixKernel::new(&gpu);
        let run = |point_curve: PointCurveParams| {
            let output = FrameTexture::new(&gpu, extent);
            kernel.set_params(
                &gpu,
                &LiveParams {
                    point_curve,
                    ..Default::default()
                },
            );
            let mut encoder = gpu
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
            kernel.encode(&gpu, &mut encoder, &input, &output);
            gpu.queue.submit(Some(encoder.finish()));
            crate::test_util::read_frame(&gpu, &output)
        };
        let curves = PointCurveParams {
            master: vec![[0.0, 0.0], [0.5, 0.65], [1.0, 1.0]],
            red: vec![[0.0, 0.1], [1.0, 0.9]],
            green: vec![],
            blue: vec![[0.0, 0.0], [0.3, 0.2], [0.7, 0.8], [1.0, 1.0]],
        };
        let luts = color::build_point_curve_luts(&curves).expect("non-identity");
        let baseline = run(PointCurveParams::default());
        let actual = run(curves);
        let identity = run(PointCurveParams {
            master: vec![[0.0, 0.0], [1.0, 1.0]],
            ..Default::default()
        });
        assert_eq!(
            baseline, identity,
            "an identity curve must be a bit-exact no-op"
        );
        assert_ne!(baseline, actual, "the curves must change the image");
        let lut = color::build_tone_curve_lut(&ToneCurveParams::default());
        for (i, px) in input_data.iter().enumerate() {
            let mut rgb = color::mat3_apply(color::mat3_identity(), [px[0], px[1], px[2]]);
            rgb = color::apply_tone(rgb, &ToneParams::default());
            rgb = color::apply_tone_curve(rgb, &lut);
            rgb = color::apply_point_curve(rgb, &luts);
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

    /// #432: Color Grading + Point Color in the fused live pass (shader) match
    /// `oklab::OkLabOps::apply` (CPU twin), and neutral params are a bit-exact no-op.
    #[test]
    fn oklab_ops_in_the_live_pass_match_the_cpu_twin_and_neutral_is_a_noop() {
        use crate::coat::{ColorGradeParams, GradeWheel, PointColorParams, PointColorSample};
        let Some(gpu) = test_gpu() else { return };
        let extent = crate::frame::Extent {
            width: 2,
            height: 2,
        };
        let input_data = vec![
            [0.2, 0.3, 0.1, 1.0],
            [0.5, 0.05, 0.4, 1.0],
            [0.6, 0.55, 0.5, 1.0],
            [0.02, 0.02, 0.03, 1.0],
        ];
        let input = crate::test_util::upload_frame(&gpu, extent, &input_data);
        let kernel = LiveSuffixKernel::new(&gpu);
        let run = |color_grade: ColorGradeParams, point_color: PointColorParams| {
            let output = FrameTexture::new(&gpu, extent);
            kernel.set_params(
                &gpu,
                &LiveParams {
                    color_grade,
                    point_color,
                    ..Default::default()
                },
            );
            let mut encoder = gpu
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
            kernel.encode(&gpu, &mut encoder, &input, &output);
            gpu.queue.submit(Some(encoder.finish()));
            crate::test_util::read_frame(&gpu, &output)
        };
        let grade = ColorGradeParams {
            shadows: GradeWheel {
                hue: 230.0,
                sat: 0.8,
                lum: -0.2,
            },
            midtones: GradeWheel {
                hue: 30.0,
                sat: 0.3,
                lum: 0.1,
            },
            highlights: GradeWheel {
                hue: 60.0,
                sat: 0.5,
                lum: 0.0,
            },
            global: GradeWheel {
                hue: 300.0,
                sat: 0.1,
                lum: 0.0,
            },
            blending: 0.6,
            balance: 0.2,
        };
        // Sample the second pixel's own colour so Point Color has a real, nearby target.
        let tone_free = |px: [f32; 3]| {
            let lab = crate::oklab::lab_from_prophoto(px);
            PointColorSample {
                lum: lab[0],
                chroma: lab[1].hypot(lab[2]),
                hue: lab[2].atan2(lab[1]).to_degrees().rem_euclid(360.0),
                hue_shift: 0.3,
                sat_shift: 0.4,
                lum_shift: 0.2,
                variance: 0.2,
                ..Default::default()
            }
        };
        let mut points = PointColorParams {
            count: 1,
            ..Default::default()
        };
        points.samples[0] = tone_free([0.5, 0.05, 0.4]);

        let baseline = run(ColorGradeParams::default(), PointColorParams::default());
        let neutral_wheels = ColorGradeParams {
            shadows: GradeWheel {
                hue: 123.0,
                sat: 0.0,
                lum: 0.0,
            },
            ..Default::default()
        };
        assert_eq!(
            baseline,
            run(neutral_wheels, PointColorParams::default()),
            "neutral wheels must be a bit-exact no-op"
        );
        let actual = run(grade, points);
        assert_ne!(
            baseline, actual,
            "grading + point colour must change the image"
        );
        let ops = crate::oklab::OkLabOps::new(&grade, &points);
        let lut = color::build_tone_curve_lut(&ToneCurveParams::default());
        for (i, px) in input_data.iter().enumerate() {
            let mut rgb = [px[0], px[1], px[2]];
            rgb = color::apply_tone(rgb, &ToneParams::default());
            rgb = color::apply_tone_curve(rgb, &lut);
            rgb = color::apply_hsl(rgb, &HslParams::default());
            rgb = ops.apply(rgb);
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

    /// Runs the live suffix over `data` with defringe on and off, checks every pixel of both against
    /// the CPU twin (`color::defringe_pixel` after the matrix, then the neutral remainder of the
    /// chain), and returns `(on, off)` for outcome assertions.
    fn defringe_live_pass_matches_the_twin(
        gpu: &GpuContext,
        extent: crate::frame::Extent,
        data: &[[f32; 4]],
        matrix: color::Mat3,
        defringe: DefringeParams,
    ) -> (Vec<[f32; 4]>, Vec<[f32; 4]>) {
        let (w, h) = (extent.width as usize, extent.height as usize);
        let input = crate::test_util::upload_frame(gpu, extent, data);
        let quantised = crate::test_util::read_frame(gpu, &input);
        let kernel = LiveSuffixKernel::new(gpu);
        let run = |defringe: DefringeParams| {
            kernel.set_params(
                gpu,
                &LiveParams {
                    working_space_matrix: matrix,
                    defringe,
                    ..Default::default()
                },
            );
            let output = FrameTexture::new(gpu, extent);
            let mut encoder = gpu
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
            kernel.encode(gpu, &mut encoder, &input, &output);
            gpu.queue.submit(Some(encoder.finish()));
            crate::test_util::read_frame(gpu, &output)
        };
        let on = run(defringe);
        let off = run(DefringeParams::default());

        let lut = color::build_tone_curve_lut(&ToneCurveParams::default());
        let r = color::defringe_radius(extent.width.max(extent.height));
        let at = |x: i32, y: i32| {
            let x = x.clamp(0, w as i32 - 1) as usize;
            let y = y.clamp(0, h as i32 - 1) as usize;
            let p = quantised[y * w + x];
            color::mat3_apply(matrix, [p[0], p[1], p[2]])
        };
        let tail = |rgb: [f32; 3]| {
            let rgb = color::apply_tone(rgb, &ToneParams::default());
            let rgb = color::apply_tone_curve(rgb, &lut);
            let rgb = color::apply_vibrance(rgb, 0.0);
            color::apply_hsl(rgb, &HslParams::default())
        };
        for y in 0..h {
            for x in 0..w {
                let centre = at(x as i32, y as i32);
                let taps =
                    color::DEFRINGE_TAPS.map(|(dx, dy)| at(x as i32 + dx * r, y as i32 + dy * r));
                let expected_on = tail(color::defringe_pixel(centre, &taps, &defringe));
                let expected_off = tail(centre);
                for c in 0..3 {
                    assert!(
                        (on[y * w + x][c] - expected_on[c]).abs() < 0.01,
                        "defringe ON ({x},{y}) channel {c}: gpu={} cpu={} (radius {r})",
                        on[y * w + x][c],
                        expected_on[c]
                    );
                    assert!(
                        (off[y * w + x][c] - expected_off[c]).abs() < 0.01,
                        "defringe OFF ({x},{y}) channel {c}: gpu={} cpu={}",
                        off[y * w + x][c],
                        expected_off[c]
                    );
                }
            }
        }
        (on, off)
    }

    fn chroma_of(p: [f32; 4]) -> f32 {
        let l = 0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2];
        (p[0] - l).abs() + (p[1] - l).abs() + (p[2] - l).abs()
    }

    /// #428: the fused live pass's defringe (shader) matches `color::defringe_pixel` (CPU twin) at
    /// *partial* strength (so a wrong strength constant cannot hide), through a non-identity camera
    /// matrix and full-width hue windows, and does what it is for.
    #[test]
    fn defringe_in_the_live_pass_matches_the_cpu_twin_and_removes_the_fringe() {
        let Some(gpu) = test_gpu() else { return };
        let extent = crate::frame::Extent {
            width: 48,
            height: 32,
        };
        let (w, h) = (extent.width as usize, extent.height as usize);
        let mut data = vec![[0.05, 0.05, 0.05, 1.0]; w * h];
        for y in 0..h {
            for x in 24..w {
                data[y * w + x] = [0.8, 0.8, 0.8, 1.0];
            }
        }
        for y in 0..8 {
            data[y * w + 23] = [0.45, 0.2, 0.55, 1.0]; // purple fringe
        }
        for y in 24..32 {
            data[y * w + 23] = [0.25, 0.6, 0.2, 1.0]; // green fringe
        }
        for y in 12..18 {
            for x in 4..10 {
                data[y * w + x] = [0.45, 0.2, 0.55, 1.0]; // a flat purple object
            }
        }
        let cam_mul = [1.8, 1.0, 1.3, 1.0];
        let cam_xyz = [
            0.55, 0.2, 0.1, 0.2, 0.7, 0.15, 0.05, 0.1, 0.85, 0.0, 0.0, 0.0,
        ];
        let matrix = color::camera_to_working_space_matrix(cam_mul, &cam_xyz, &WbParams::default());
        let defringe = DefringeParams {
            purple_amount: 0.6,
            purple_hue_lo: 0.0,
            purple_hue_hi: 1.0,
            green_amount: 0.8,
            green_hue_lo: 0.0,
            green_hue_hi: 1.0,
        };
        let (on, off) = defringe_live_pass_matches_the_twin(&gpu, extent, &data, matrix, defringe);

        // Fringes lose chroma in proportion to the amount (a 0.6 amount keeps ~40 %, 0.8 ~20 %).
        let (p_on, p_off) = (chroma_of(on[3 * w + 23]), chroma_of(off[3 * w + 23]));
        let (g_on, g_off) = (chroma_of(on[27 * w + 23]), chroma_of(off[27 * w + 23]));
        assert!(
            p_on < 0.6 * p_off && p_on > 0.1 * p_off,
            "purple {p_off} -> {p_on}"
        );
        assert!(
            g_on < 0.4 * g_off && g_on > 0.02 * g_off,
            "green {g_off} -> {g_on}"
        );
        // The flat object's interior (all its taps land inside it) is untouched; its border pixels
        // sit beside the dark background, which is what a fringe looks like.
        for y in 13..17 {
            for x in 5..9 {
                assert_eq!(on[y * w + x], off[y * w + x], "flat object at ({x},{y})");
            }
        }
    }

    /// The same check where the other one cannot reach: a frame wide enough that the tap radius is
    /// 2 (long edge 4000), an identity matrix so the pixels' hues are known, and LRC's *default*
    /// hue windows with their shoulders.
    #[test]
    fn defringe_in_the_live_pass_matches_the_twin_at_radius_two_with_default_windows() {
        let Some(gpu) = test_gpu() else { return };
        let extent = crate::frame::Extent {
            width: 4000,
            height: 24,
        };
        let (w, h) = (extent.width as usize, extent.height as usize);
        assert_eq!(color::defringe_radius(extent.width), 2);
        let mut data = vec![[0.05, 0.05, 0.05, 1.0]; w * h];
        for y in 0..h {
            for x in 2000..w {
                data[y * w + x] = [0.8, 0.8, 0.8, 1.0];
            }
        }
        // Hue ~292 (inside the default purple window 276..324) and ~127 (inside green 108..132),
        // one pixel off the edge at radius 2 and one a pixel further than any tap reaches.
        for y in 0..8 {
            data[y * w + 1999] = [0.55, 0.2, 0.6, 1.0];
            // A wide purple block far from the edge: its interior is not an edge pixel.
            for x in 1960..1980 {
                data[y * w + x] = [0.55, 0.2, 0.6, 1.0];
            }
        }
        for y in 16..24 {
            data[y * w + 1999] = [0.2, 0.6, 0.25, 1.0];
            for x in 1960..1980 {
                data[y * w + x] = [0.2, 0.6, 0.25, 1.0];
            }
        }
        let defringe = DefringeParams {
            purple_amount: 0.7,
            green_amount: 0.7,
            ..Default::default()
        };
        let (on, off) = defringe_live_pass_matches_the_twin(
            &gpu,
            extent,
            &data,
            color::mat3_identity(),
            defringe,
        );
        for (x, y) in [(1999usize, 3usize), (1999, 19)] {
            let (a, b) = (chroma_of(on[y * w + x]), chroma_of(off[y * w + x]));
            assert!(
                a < 0.5 * b,
                "fringe at ({x},{y}) kept its chroma: {b} -> {a}"
            );
        }
        // The blocks' interiors (every tap inside them) are not edge pixels: untouched.
        for (x, y) in [(1970usize, 3usize), (1970, 19)] {
            assert_eq!(on[y * w + x], off[y * w + x], "block interior ({x},{y})");
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

    /// #380: the geometry pass's vignette + grain match `effects::apply_effects` for every style,
    /// through a real crop rect and straighten rotation (so the crop-normalized coordinates are
    /// exercised, not just an identity transform).
    #[test]
    fn present_sample_effects_match_the_cpu_reference() {
        use crate::coat::{EffectsParams, VignetteStyle};
        use crate::geometry::{affine_for_crop, sample_bilinear, CropRect};
        let Some(gpu) = test_gpu() else { return };
        let extent = crate::frame::Extent {
            width: 64,
            height: 48,
        };
        let input_data: Vec<[f32; 4]> = (0..extent.width * extent.height)
            .map(|i| {
                let (x, y) = ((i % extent.width) as f32, (i / extent.width) as f32);
                [0.05 + x / 80.0, 0.1 + y / 90.0, 0.6 - x / 200.0, 1.0]
            })
            .collect();
        let input = crate::test_util::upload_frame(&gpu, extent, &input_data);
        let rect = CropRect {
            x: 8.0,
            y: 6.0,
            width: 40.0,
            height: 30.0,
        };
        let transform = affine_for_crop(rect, 6.0);
        let norm = crate::effects::crop_norm(transform, rect.width, rect.height).unwrap();
        let out_extent = crate::frame::Extent {
            width: 40,
            height: 30,
        };
        // (style, highlights, roundness, vignette on, grain on): both bits together, each alone,
        // and the squarish superellipse branch (negative roundness).
        for (style, highlights, roundness, vignette, grain) in [
            (VignetteStyle::HighlightPriority, 0.6, 0.4, true, true),
            (VignetteStyle::ColorPriority, 0.0, -0.6, true, true),
            (VignetteStyle::PaintOverlay, 0.0, 0.0, true, true),
            (VignetteStyle::HighlightPriority, 0.0, 0.0, false, true),
            (VignetteStyle::ColorPriority, 0.0, -1.0, true, false),
        ] {
            let effects = EffectsParams {
                vignette_amount: if vignette { -0.7 } else { 0.0 },
                vignette_midpoint: 0.35,
                vignette_feather: 0.6,
                vignette_roundness: roundness,
                vignette_highlights: highlights,
                vignette_style: style,
                grain_amount: if grain { 0.8 } else { 0.0 },
                grain_size: 0.5,
                grain_roughness: 0.7,
                grain_seed: 4242,
            };
            let kernel = CropKernel::new(&gpu);
            kernel.set_transform(transform);
            kernel.set_effects(&effects, transform, rect.width, rect.height);
            let output = FrameTexture::new(&gpu, out_extent);
            let mut encoder = gpu
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
            kernel.encode(&gpu, &mut encoder, &input, &output);
            gpu.queue.submit(Some(encoder.finish()));
            let actual = crate::test_util::read_frame(&gpu, &output);

            let mut worst = 0.0f32;
            let mut changed = 0usize;
            for oy in 0..out_extent.height {
                for ox in 0..out_extent.width {
                    let base = sample_bilinear(
                        &input_data,
                        (extent.width, extent.height),
                        &transform,
                        (ox, oy),
                    );
                    let s = transform.apply((ox as f32 + 0.5, oy as f32 + 0.5));
                    let want = crate::effects::apply_effects(
                        [base[0], base[1], base[2]],
                        norm.apply(s),
                        (rect.width, rect.height),
                        &effects,
                    );
                    let got = actual[(oy * out_extent.width + ox) as usize];
                    for c in 0..3 {
                        worst = worst.max((got[c] - want[c]).abs());
                    }
                    if (want[0] - base[0]).abs() > 1e-3 {
                        changed += 1;
                    }
                }
            }
            assert!(worst < 0.01, "{style:?}: GPU vs CPU differ by {worst}");
            assert!(
                changed > 600,
                "{style:?}: the effects must actually change pixels"
            );
        }
    }

    /// #380: a screen-size preview (output -> source transform scaled, as `eyeshine::screen_geometry`
    /// does) with the *unscaled* crop bound for the effects matches a full-size render's vignette,
    /// because the effects read the source position, not the output pixel.
    #[test]
    fn a_scaled_preview_shows_the_same_vignette_as_the_full_size_render() {
        use crate::coat::EffectsParams;
        let Some(gpu) = test_gpu() else { return };
        let extent = crate::frame::Extent {
            width: 40,
            height: 30,
        };
        let flat: Vec<[f32; 4]> = vec![[0.5, 0.4, 0.3, 1.0]; 40 * 30];
        let input = crate::test_util::upload_frame(&gpu, extent, &flat);
        let effects = EffectsParams {
            vignette_amount: -0.9,
            vignette_midpoint: 0.2,
            ..EffectsParams::default()
        };
        let render = |out: crate::frame::Extent, transform: Affine2D| {
            let kernel = CropKernel::new(&gpu);
            kernel.set_transform(transform);
            // Always the unscaled crop (the whole 40x30 frame, identity).
            kernel.set_effects(&effects, Affine2D::IDENTITY, 40.0, 30.0);
            let output = FrameTexture::new(&gpu, out);
            let mut encoder = gpu
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
            kernel.encode(&gpu, &mut encoder, &input, &output);
            gpu.queue.submit(Some(encoder.finish()));
            crate::test_util::read_frame(&gpu, &output)
        };
        let full = render(extent, Affine2D::IDENTITY);
        let half = render(
            crate::frame::Extent {
                width: 20,
                height: 15,
            },
            Affine2D {
                a: 2.0,
                d: 2.0,
                ..Affine2D::IDENTITY
            },
        );
        let mut worst = 0.0f32;
        for y in 0..15usize {
            for x in 0..20usize {
                let avg: f32 = [(0, 0), (1, 0), (0, 1), (1, 1)]
                    .iter()
                    .map(|(dx, dy)| full[(2 * y + dy) * 40 + 2 * x + dx][0])
                    .sum::<f32>()
                    / 4.0;
                worst = worst.max((half[y * 20 + x][0] - avg).abs());
            }
        }
        assert!(
            worst < 0.02,
            "preview vs full-size vignette differ by {worst}"
        );
        // Not vacuous: the corner really is vignetted.
        assert!(half[0][0] < 0.5 * 0.6);
    }

    /// #380: with the effects off the pass is exactly the pre-#380 sample.
    #[test]
    fn present_sample_with_noop_effects_is_the_plain_sample() {
        let Some(gpu) = test_gpu() else { return };
        let extent = crate::frame::Extent {
            width: 8,
            height: 8,
        };
        let data: Vec<[f32; 4]> = (0..64).map(|i| [i as f32 * 0.01, 0.2, 0.3, 1.0]).collect();
        let input = crate::test_util::upload_frame(&gpu, extent, &data);
        let run = |with_noop_effects: bool| {
            let kernel = CropKernel::new(&gpu);
            kernel.set_transform(Affine2D::crop(1.0, 1.0));
            if with_noop_effects {
                kernel.set_effects(
                    &EffectsParams::default(),
                    Affine2D::crop(1.0, 1.0),
                    6.0,
                    6.0,
                );
            }
            let output = FrameTexture::new(&gpu, extent);
            let mut encoder = gpu
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
            kernel.encode(&gpu, &mut encoder, &input, &output);
            gpu.queue.submit(Some(encoder.finish()));
            crate::test_util::read_frame(&gpu, &output)
        };
        assert_eq!(run(false), run(true));
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
                point_curve: PointCurveParams::default(),
                color_grade: Default::default(),
                point_color: Default::default(),
                vibrance,
                presence: PresenceParams::default(),
                defringe: DefringeParams::default(),
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
