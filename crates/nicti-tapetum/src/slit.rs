//! The lens stage (#428): the baked `nicti.lens` node, a real resample pass instead of the old
//! passthrough.
//!
//! Named for the cat's slit pupil, which narrows to correct for what the lens lets in.
//!
//! The warp/CA/vignette model is adapted from storytold/lightcraft@265248c
//! `crates/pipeline/src/{optics,geometry}.rs`, Copyright (c) 2026 ArtCraft Team and the LightCraft contributors, MIT OR Apache-2.0
//! (see `docs/licensing.md`).
//! Changes: one camera-RGB pass (LightCraft warps Rec.2020), DNG coefficients per colour plane.
//!
//! It runs on the demosaiced, normalised, still camera-RGB frame -- *before* the camera-to-XYZ
//! matrix mixes the channels (ADR-0044; `render-graph` topic) -- because per-channel lateral CA
//! correction and the DNG per-plane warp are defined on those channels. Crop's own resample runs
//! after the live suffix, in ProPhoto, so it cannot host this; lens needs its own pass.
//!
//! What it applies, in `shaders/slit.wgsl` (and, as the test reference, [`reference`]):
//! 1. the DNG-embedded `WarpRectilinear`, per colour plane (when the file carries a profile);
//! 2. automatic lateral CA: red/blue magnified about the optical centre by the estimated
//!    `1 + alpha` ([`nicti_iris::lateral_ca`]), skipped when the embedded warp already differs per
//!    plane (that *is* the CA correction -- applying both would correct twice);
//! 3. the DNG-embedded `FixVignetteRadial` gain.
//!
//! The estimate and the profile are pure functions of the decoded file, so they are not part of
//! the cache key: the lens node hashes only [`LensParams`], chained from the decode identity.
//! `IMPL_VERSION` stays 0 on purpose -- bumping it would re-key every photo's lens node (and, via
//! `nicti.neutral`, orphan every on-disk AI alpha) although a NEF with default params still
//! renders exactly the old passthrough pixels. Bump it when the *algorithm* changes.

use bytemuck::Zeroable;
use serde_json::Value;
use wgpu::util::DeviceExt;

use nicti_claw::Module;
use nicti_cornea::LinearFrame;
use nicti_iris::dng::DngEmbedded;
use nicti_iris::lateral_ca::{self, RgbU16};
use nicti_iris::{LensCorrection, LensModel, LensSource};

use crate::coat::{self, LensParams};
use crate::frame::{Extent, FrameTexture};
use crate::gpu::{make_compute_pipeline, GpuContext};
use crate::graph::StageKind;
use crate::renderer::BakedExec;
use crate::stages::{PassthroughExec, LENS};
use crate::RenderStage;

/// Estimates already computed, keyed by [`ca_key`]. `LensExec::encode` runs on every baked-cache
/// miss of the lens node (an eviction under memory pressure, say), and the estimate is a pure
/// function of the frame, so redoing ~0.2-0.4 s of CPU work each time would stall the render path.
static CA_MEMO: std::sync::Mutex<Vec<(u64, [f64; 2])>> = std::sync::Mutex::new(Vec::new());
const CA_MEMO_ENTRIES: usize = 8;

/// Counts real estimator runs (not memo hits), for the test that proves the memo works.
#[cfg(test)]
static CA_RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// A cheap content fingerprint of `frame` (dimensions, levels and ~4096 sampled pixels) plus the
/// centre the estimate is taken about.
fn ca_key(frame: &LinearFrame, center: Option<[f64; 2]>) -> u64 {
    let mut h = blake3::Hasher::new();
    h.update(&frame.width.to_le_bytes());
    h.update(&frame.height.to_le_bytes());
    h.update(&frame.black.to_le_bytes());
    h.update(&frame.maximum.to_le_bytes());
    for c in center.unwrap_or([f64::NAN; 2]) {
        h.update(&c.to_bits().to_le_bytes());
    }
    // Whole RGB pixels, not raw indices: a stride that is a multiple of 3 would sample one colour
    // channel only and let two frames differing in the others share a key.
    let stride = (frame.pixels.len() / 3 / 1366).max(1);
    for px in frame.pixels.as_chunks::<3>().0.iter().step_by(stride) {
        for v in px {
            h.update(&v.to_le_bytes());
        }
    }
    u64::from_le_bytes(h.finalize().as_bytes()[..8].try_into().unwrap())
}

fn estimate_ca_memoised(frame: &LinearFrame, center: Option<[f64; 2]>) -> [f64; 2] {
    let key = ca_key(frame, center);
    if let Some((_, v)) = CA_MEMO.lock().unwrap().iter().find(|(k, _)| *k == key) {
        return *v;
    }
    #[cfg(test)]
    CA_RUNS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let img = RgbU16 {
        width: frame.width as usize,
        height: frame.height as usize,
        pixels: &frame.pixels,
        black: frame.black as f32,
        white: frame.maximum as f32,
    };
    let v = lateral_ca::estimate(&img, center);
    let mut memo = CA_MEMO.lock().unwrap();
    memo.push((key, v));
    if memo.len() > CA_MEMO_ENTRIES {
        memo.remove(0);
    }
    v
}

/// See the module doc: 0 keeps every pre-#428 document's lens key valid.
pub const IMPL_VERSION: u32 = 0;

/// Upper bound on the vignette gain (the lower is 0): see `slit.wgsl`.
pub const MAX_VIGNETTE_GAIN: f32 = 16.0;

/// Largest |alpha| honored. Real lateral CA is well under 0.5 %; this bounds what a bad estimate
/// (or a hostile profile) can do to the image.
pub const MAX_CA_ALPHA: f64 = 0.02;
/// Below this magnitude both scales count as zero (no CA pass).
const CA_EPSILON: f64 = 1.0e-6;

pub struct LensStage;

impl Module for LensStage {
    fn id(&self) -> &str {
        LENS
    }
    fn schema_version(&self) -> u32 {
        1
    }
    fn migrate_params(&self, _from_version: u32, params: Value) -> Option<Value> {
        Some(params)
    }
}

impl RenderStage for LensStage {
    fn kind(&self) -> StageKind {
        StageKind::Baked
    }
    fn default_params(&self) -> Value {
        coat::default_value::<LensParams>()
    }
    fn impl_version(&self) -> u32 {
        IMPL_VERSION
    }
}

/// The resolved warp, in pixels of the frame it will run on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WarpPlan {
    /// `[kr0, kr1, kr2, kr3, kt0, kt1]` for R, G, B (a one-plane profile fills all three).
    pub planes: [[f32; 6]; 3],
    pub center: [f32; 2],
    /// Centre-to-farthest-corner distance in pixels.
    pub radius: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VignettePlan {
    pub k: [f32; 5],
    pub center: [f32; 2],
    pub radius: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CaPlan {
    /// Red and blue scales.
    pub alpha: [f32; 2],
    pub center: [f32; 2],
}

/// Everything the pass does to one frame, resolved from the params, the file and its extent.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LensPlan {
    pub warp: Option<WarpPlan>,
    pub vignette: Option<VignettePlan>,
    pub ca: Option<CaPlan>,
}

impl LensPlan {
    /// True when the pass would copy the frame unchanged (the exec then does a plain texture copy).
    pub fn is_identity(&self) -> bool {
        self.warp.is_none() && self.vignette.is_none() && self.ca.is_none()
    }

    /// Builds the plan from a profile and a CA estimate for a frame of `extent`.
    ///
    /// `ca_center` is the optical centre as a fraction of the image (`None` = the middle).
    pub fn new(
        model: Option<&LensModel>,
        ca_alpha: [f64; 2],
        ca_center: Option<[f64; 2]>,
        extent: Extent,
    ) -> Self {
        let (w, h) = (f64::from(extent.width), f64::from(extent.height));
        let px = |c: [f64; 2]| [c[0] * w, c[1] * h];
        let warp = model.and_then(|m| m.warp.as_ref()).map(|warp| {
            let c = px(warp.center);
            let planes = std::array::from_fn(|ch| warp.plane(ch).map(|v| v as f32));
            WarpPlan {
                planes,
                center: [c[0] as f32, c[1] as f32],
                radius: nicti_iris::farthest_corner(w, h, c[0], c[1]) as f32,
            }
        });
        let vignette = model.and_then(|m| m.vignette.as_ref()).map(|v| {
            let c = px(v.center);
            VignettePlan {
                k: v.k.map(|k| k as f32),
                center: [c[0] as f32, c[1] as f32],
                radius: nicti_iris::farthest_corner(w, h, c[0], c[1]) as f32,
            }
        });
        let alpha = ca_alpha.map(|a| {
            if a.is_finite() && a.abs() >= CA_EPSILON {
                a.clamp(-MAX_CA_ALPHA, MAX_CA_ALPHA)
            } else {
                0.0
            }
        });
        let ca = (alpha != [0.0; 2]).then(|| {
            let c = px(ca_center.unwrap_or([0.5, 0.5]));
            CaPlan {
                alpha: alpha.map(|a| a as f32),
                center: [c[0] as f32, c[1] as f32],
            }
        });
        Self { warp, vignette, ca }
    }

    /// The plan for `frame` under `params`: parses the DNG profile and, when asked and not already
    /// handled by the profile, estimates lateral CA from the pixels. CPU work -- the exec only runs
    /// it on a baked-cache miss.
    pub fn resolve(params: &LensParams, frame: &LinearFrame, extent: Extent) -> Self {
        let model = if params.embedded_profile {
            DngEmbedded.model(&LensSource {
                make: &frame.make,
                model: &frame.model,
                dng_opcode_list3: frame.dng_opcode_list3.as_deref(),
            })
        } else {
            None
        };
        let profile_corrects_ca = model
            .as_ref()
            .and_then(|m| m.warp.as_ref())
            .is_some_and(|w| w.corrects_lateral_ca());
        let (alpha, center) = if params.remove_ca
            && !profile_corrects_ca
            && extent.width == frame.width
            && extent.height == frame.height
        {
            let center = model
                .as_ref()
                .and_then(|m| m.warp.as_ref())
                .map(|w| w.center);
            (estimate_ca_memoised(frame, center), center)
        } else {
            ([0.0; 2], None)
        };
        Self::new(model.as_ref(), alpha, center, extent)
    }

    fn uniforms(&self, extent: Extent) -> Uniforms {
        let mut u = Uniforms {
            dims: [extent.width as f32, extent.height as f32, 0.0, 0.0],
            ..Uniforms::zeroed()
        };
        if let Some(w) = &self.warp {
            u.dims[2] = 1.0;
            u.warp_c = [w.center[0], w.center[1], w.radius, 0.0];
            for (ch, p) in w.planes.iter().enumerate() {
                u.warp[2 * ch] = [p[0], p[1], p[2], p[3]];
                u.warp[2 * ch + 1] = [p[4], p[5], 0.0, 0.0];
            }
        }
        if let Some(v) = &self.vignette {
            u.dims[3] = 1.0;
            u.vig_c = [v.center[0], v.center[1], v.radius, 0.0];
            u.vig_k = [[v.k[0], v.k[1], v.k[2], v.k[3]], [v.k[4], 0.0, 0.0, 0.0]];
        }
        if let Some(c) = &self.ca {
            u.ca = [c.alpha[0], c.alpha[1], c.center[0], c.center[1]];
        }
        u
    }
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    dims: [f32; 4],
    warp_c: [f32; 4],
    vig_c: [f32; 4],
    ca: [f32; 4],
    warp: [[f32; 4]; 6],
    vig_k: [[f32; 4]; 2],
}

/// Owns the compute pipeline (built once, reused across photos and renders).
pub struct LensKernel {
    pipeline: wgpu::ComputePipeline,
}

impl LensKernel {
    pub fn new(gpu: &GpuContext) -> Self {
        Self {
            pipeline: make_compute_pipeline(
                &gpu.device,
                include_str!("../shaders/slit.wgsl"),
                "main",
            ),
        }
    }

    /// Records the resample of `input` into `output` (same extent). The uniform buffer is created
    /// per call, so two lens passes recorded into one encoder never share (and overwrite) state.
    pub fn encode(
        &self,
        gpu: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        input: &FrameTexture,
        output: &FrameTexture,
        plan: &LensPlan,
    ) {
        let uniforms = plan.uniforms(output.extent);
        let buf = gpu
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("slit uniforms"),
                contents: bytemuck::bytes_of(&uniforms),
                usage: wgpu::BufferUsages::UNIFORM,
            });
        let layout = self.pipeline.get_bind_group_layout(0);
        let bind_group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("slit bind group"),
            layout: &layout,
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
                    resource: buf.as_entire_binding(),
                },
            ],
        });
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("slit"),
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

/// Whether `frame` carries a usable embedded lens profile (a DNG whose `OpcodeList3` has a
/// warp or vignette), so a UI can offer the switch only where it does something.
pub fn has_embedded_profile(frame: &LinearFrame) -> bool {
    DngEmbedded
        .model(&LensSource {
            make: &frame.make,
            model: &frame.model,
            dng_opcode_list3: frame.dng_opcode_list3.as_deref(),
        })
        .is_some()
}

/// One render's lens pass: the persistent kernel, the document's params and the source frame.
pub struct LensExec<'a> {
    pub kernel: &'a LensKernel,
    pub params: &'a LensParams,
    pub frame: &'a LinearFrame,
}

impl BakedExec for LensExec<'_> {
    fn encode(
        &self,
        gpu: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        input: Option<&FrameTexture>,
        output: &FrameTexture,
    ) {
        let input = input.expect("the lens stage always has an upstream baked node");
        let plan = LensPlan::resolve(self.params, self.frame, output.extent);
        if plan.is_identity() {
            PassthroughExec.encode(gpu, encoder, Some(input), output);
        } else {
            self.kernel.encode(gpu, encoder, input, output, &plan);
        }
    }
}

/// CPU twin of `slit.wgsl`, in the same f32 arithmetic. The test reference: every kernel here is
/// proven against it on a real adapter, and it is what outcome tests measure through.
#[cfg(test)]
pub(crate) mod reference {
    use super::*;

    fn load(img: &[[f32; 4]], e: Extent, x: i32, y: i32) -> [f32; 3] {
        let x = x.clamp(0, e.width as i32 - 1) as usize;
        let y = y.clamp(0, e.height as i32 - 1) as usize;
        let p = img[y * e.width as usize + x];
        [p[0], p[1], p[2]]
    }

    fn bilinear(img: &[[f32; 4]], e: Extent, p: [f32; 2]) -> [f32; 3] {
        let (qx, qy) = (p[0] - 0.5, p[1] - 0.5);
        let (fx, fy) = (qx.floor(), qy.floor());
        let (tx, ty) = (qx - fx, qy - fy);
        let (ix, iy) = (fx as i32, fy as i32);
        let a = load(img, e, ix, iy);
        let b = load(img, e, ix + 1, iy);
        let c = load(img, e, ix, iy + 1);
        let d = load(img, e, ix + 1, iy + 1);
        std::array::from_fn(|i| {
            let top = a[i] + (b[i] - a[i]) * tx;
            let bot = c[i] + (d[i] - c[i]) * tx;
            top + (bot - top) * ty
        })
    }

    fn warp_source(plan: &LensPlan, ch: usize, p: [f32; 2]) -> [f32; 2] {
        let Some(w) = &plan.warp else { return p };
        let (dx, dy) = (
            (p[0] - w.center[0]) / w.radius,
            (p[1] - w.center[1]) / w.radius,
        );
        let [k0, k1, k2, k3, t0, t1] = w.planes[ch];
        let r2 = dx * dx + dy * dy;
        let f = k0 + r2 * (k1 + r2 * (k2 + r2 * k3));
        let sx = f * dx + 2.0 * t0 * dx * dy + t1 * (r2 + 2.0 * dx * dx);
        let sy = f * dy + t0 * (r2 + 2.0 * dy * dy) + 2.0 * t1 * dx * dy;
        [w.center[0] + sx * w.radius, w.center[1] + sy * w.radius]
    }

    fn ca_source(plan: &LensPlan, which: usize, s: [f32; 2]) -> [f32; 2] {
        let Some(c) = &plan.ca else { return s };
        let k = 1.0 + c.alpha[which];
        [
            c.center[0] + (s[0] - c.center[0]) * k,
            c.center[1] + (s[1] - c.center[1]) * k,
        ]
    }

    fn vignette_gain(plan: &LensPlan, s: [f32; 2]) -> f32 {
        let Some(v) = &plan.vignette else { return 1.0 };
        let (dx, dy) = (
            (s[0] - v.center[0]) / v.radius,
            (s[1] - v.center[1]) / v.radius,
        );
        let r2 = dx * dx + dy * dy;
        (1.0 + r2 * (v.k[0] + r2 * (v.k[1] + r2 * (v.k[2] + r2 * (v.k[3] + r2 * v.k[4])))))
            .clamp(0.0, super::MAX_VIGNETTE_GAIN)
    }

    pub(crate) fn apply(img: &[[f32; 4]], e: Extent, plan: &LensPlan) -> Vec<[f32; 4]> {
        let mut out = Vec::with_capacity(img.len());
        for y in 0..e.height {
            for x in 0..e.width {
                let p = [x as f32 + 0.5, y as f32 + 0.5];
                let sr = ca_source(plan, 0, warp_source(plan, 0, p));
                let sg = warp_source(plan, 1, p);
                let sb = ca_source(plan, 1, warp_source(plan, 2, p));
                let gain = vignette_gain(plan, sg);
                out.push([
                    bilinear(img, e, sr)[0] * gain,
                    bilinear(img, e, sg)[1] * gain,
                    bilinear(img, e, sb)[2] * gain,
                    1.0,
                ]);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{read_frame, shared_test_gpu, upload_frame};
    use nicti_iris::{Vignette, Warp};

    fn ext(w: u32, h: u32) -> Extent {
        Extent {
            width: w,
            height: h,
        }
    }

    /// A frame with structure at every scale, so a wrong sample position can't hide.
    fn textured(e: Extent) -> Vec<[f32; 4]> {
        (0..e.width * e.height)
            .map(|i| {
                let (x, y) = ((i % e.width) as f32, (i / e.width) as f32);
                [
                    0.2 + 0.5 * ((x * 0.31).sin() * (y * 0.17).cos()).abs(),
                    0.1 + 0.6 * ((x * 0.11 + y * 0.23).sin()).abs(),
                    0.7 - 0.4 * ((x * 0.07).cos() * (y * 0.29).sin()).abs(),
                    1.0,
                ]
            })
            .collect()
    }

    fn full_model() -> LensModel {
        LensModel {
            warp: Some(Warp {
                planes: vec![
                    [1.0010, 0.012, -0.004, 0.0, 0.0003, -0.0002],
                    [1.0, 0.010, -0.004, 0.0, 0.0003, -0.0002],
                    [0.9990, 0.008, -0.004, 0.0, 0.0003, -0.0002],
                ],
                center: [0.52, 0.48],
            }),
            vignette: Some(Vignette {
                k: [0.35, -0.12, 0.04, 0.0, 0.0],
                center: [0.5, 0.5],
            }),
        }
    }

    #[test]
    fn identity_plan_is_detected_and_everything_else_is_not() {
        let e = ext(64, 48);
        assert!(LensPlan::new(None, [0.0; 2], None, e).is_identity());
        assert!(LensPlan::new(None, [1.0e-9, -1.0e-9], None, e).is_identity());
        assert!(!LensPlan::new(None, [0.003, 0.0], None, e).is_identity());
        assert!(!LensPlan::new(Some(&full_model()), [0.0; 2], None, e).is_identity());
    }

    #[test]
    fn a_hostile_ca_estimate_is_clamped_or_dropped() {
        let e = ext(64, 48);
        let p = LensPlan::new(None, [5.0, -5.0], None, e);
        assert_eq!(
            p.ca.unwrap().alpha,
            [MAX_CA_ALPHA as f32, -(MAX_CA_ALPHA as f32)]
        );
        assert!(LensPlan::new(None, [f64::NAN, f64::INFINITY], None, e).is_identity());
    }

    #[test]
    fn plan_geometry_matches_the_iris_f64_math() {
        let e = ext(600, 400);
        let model = full_model();
        let plan = LensPlan::new(Some(&model), [0.0; 2], None, e);
        let w = plan.warp.unwrap();
        let warp = model.warp.as_ref().unwrap();
        let (cx, cy) = (0.52 * 600.0, 0.48 * 400.0);
        let m = nicti_iris::farthest_corner(600.0, 400.0, cx, cy);
        assert!((f64::from(w.radius) - m).abs() < 1e-3);
        // Same corrected->source mapping as `Warp::source_offset`, in the plan's pixel units.
        let reference = warp.source_offset(0, 120.0, -80.0, m);
        let (dx, dy) = (120.0f32 / w.radius, -80.0f32 / w.radius);
        let [k0, k1, k2, k3, t0, t1] = w.planes[0];
        let r2 = dx * dx + dy * dy;
        let f = k0 + r2 * (k1 + r2 * (k2 + r2 * k3));
        let sx = (f * dx + 2.0 * t0 * dx * dy + t1 * (r2 + 2.0 * dx * dx)) * w.radius;
        let sy = (f * dy + t0 * (r2 + 2.0 * dy * dy) + 2.0 * t1 * dx * dy) * w.radius;
        assert!(
            (f64::from(sx) - reference.0).abs() < 1e-2,
            "{sx} {}",
            reference.0
        );
        assert!(
            (f64::from(sy) - reference.1).abs() < 1e-2,
            "{sy} {}",
            reference.1
        );
        // A one-plane profile serves every channel.
        let one = LensModel {
            warp: Some(Warp {
                planes: vec![[1.0, 0.1, 0.0, 0.0, 0.0, 0.0]],
                center: [0.5, 0.5],
            }),
            vignette: None,
        };
        let p = LensPlan::new(Some(&one), [0.0; 2], None, e).warp.unwrap();
        assert_eq!(p.planes[0], p.planes[2]);
    }

    #[test]
    fn the_reference_with_an_identity_warp_returns_the_input() {
        let e = ext(32, 24);
        let img = textured(e);
        let model = LensModel {
            warp: Some(Warp {
                planes: vec![[1.0, 0.0, 0.0, 0.0, 0.0, 0.0]],
                center: [0.5, 0.5],
            }),
            vignette: None,
        };
        let plan = LensPlan::new(Some(&model), [0.0; 2], None, e);
        let out = reference::apply(&img, e, &plan);
        for (a, b) in img.iter().zip(&out) {
            for c in 0..3 {
                assert!((a[c] - b[c]).abs() < 1e-5);
            }
        }
    }

    #[test]
    fn vignette_gain_brightens_the_corner_by_the_polynomial() {
        let e = ext(64, 64);
        let flat = vec![[0.5, 0.5, 0.5, 1.0]; (e.width * e.height) as usize];
        let model = LensModel {
            warp: None,
            vignette: Some(Vignette {
                k: [0.4, 0.0, 0.0, 0.0, 0.0],
                center: [0.5, 0.5],
            }),
        };
        let out = reference::apply(&flat, e, &LensPlan::new(Some(&model), [0.0; 2], None, e));
        let mid = out[(32 * 64 + 32) as usize][1];
        let corner = out[0][1];
        assert!((mid - 0.5).abs() < 0.01, "centre stays put: {mid}");
        // The corner pixel centre sits at r just under 1, so the gain is just under 1.4.
        assert!(
            corner > 0.5 * 1.35 && corner < 0.5 * 1.4 + 1e-4,
            "corner {corner}"
        );
    }

    #[test]
    fn ca_correction_realigns_red_and_blue_with_green() {
        // The outcome, not the formula: red/blue planes magnified by the planted CA come back into
        // register with green after the pass, measured as the plane-to-plane error.
        use nicti_iris::lateral_ca::{estimate, RgbU16};
        let (w, h) = (600usize, 400usize);
        let cell = w as f64 / 10.0;
        let val = |x: f64, y: f64| -> f64 {
            let (fx, fy) = ((x / cell).fract() - 0.5, (y / cell).fract() - 0.5);
            0.05 + 0.85 * ((fx.hypot(fy) * cell - cell * 0.28) / 1.2).clamp(-0.5, 0.5) + 0.425
        };
        let (cx, cy) = (w as f64 / 2.0, h as f64 / 2.0);
        let (ar, ab) = (0.004, -0.003);
        let mut px16 = Vec::with_capacity(w * h * 3);
        let mut img = Vec::with_capacity(w * h);
        for y in 0..h {
            for x in 0..w {
                let (x, y) = (x as f64 + 0.5, y as f64 + 0.5);
                let at = |a: f64| val(cx + (x - cx) / (1.0 + a), cy + (y - cy) / (1.0 + a));
                let (r, g, b) = (at(ar), at(0.0), at(ab));
                for v in [r, g, b] {
                    px16.push((v * 65535.0).round() as u16);
                }
                img.push([r as f32, g as f32, b as f32, 1.0]);
            }
        }
        let e = ext(w as u32, h as u32);
        let alpha = estimate(
            &RgbU16 {
                width: w,
                height: h,
                pixels: &px16,
                black: 0.0,
                white: 65535.0,
            },
            None,
        );
        let plan = LensPlan::new(None, alpha, None, e);
        assert!(
            !plan.is_identity(),
            "the planted CA must be found: {alpha:?}"
        );
        let out = reference::apply(&img, e, &plan);
        let err = |px: &[[f32; 4]], ch: usize| -> f64 {
            // Ignore the border band the resample clamps.
            let mut s = 0.0;
            for y in 20..h - 20 {
                for x in 20..w - 20 {
                    let p = px[y * w + x];
                    s += f64::from(p[ch] - p[1]).powi(2);
                }
            }
            s
        };
        for ch in [0, 2] {
            let (before, after) = (err(&img, ch), err(&out, ch));
            assert!(
                after < 0.25 * before,
                "channel {ch}: misregistration {before:.3} -> {after:.3}"
            );
        }
    }

    #[test]
    fn gpu_matches_the_cpu_reference() {
        let Some(gpu) = shared_test_gpu() else { return };
        let e = ext(96, 64);
        let model = full_model();
        let cases = [
            // Warp + vignette + CA together.
            LensPlan::new(Some(&model), [0.004, -0.003], Some([0.52, 0.48]), e),
            // CA only, off-centre.
            LensPlan::new(None, [0.006, 0.0], Some([0.4, 0.55]), e),
            // Vignette only.
            LensPlan::new(
                Some(&LensModel {
                    warp: None,
                    vignette: model.vignette.clone(),
                }),
                [0.0; 2],
                None,
                e,
            ),
            // A hostile vignette whose polynomial goes strongly negative: clamped, never NaN.
            LensPlan::new(
                Some(&LensModel {
                    warp: None,
                    vignette: Some(Vignette {
                        k: [-50.0, 30.0, 0.0, 0.0, 0.0],
                        center: [0.5, 0.5],
                    }),
                }),
                [0.0; 2],
                None,
                e,
            ),
            // Warp only.
            LensPlan::new(
                Some(&LensModel {
                    warp: model.warp.clone(),
                    vignette: None,
                }),
                [0.0; 2],
                None,
                e,
            ),
        ];
        let kernel = LensKernel::new(&gpu);
        for (i, plan) in cases.iter().enumerate() {
            let input = upload_frame(&gpu, e, &textured(e));
            // The f16-quantised input is what the GPU actually reads, so the reference must too.
            let quantised = read_frame(&gpu, &input);
            let output = FrameTexture::new(&gpu, e);
            let mut encoder = gpu
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
            kernel.encode(&gpu, &mut encoder, &input, &output, plan);
            gpu.queue.submit(Some(encoder.finish()));
            let actual = read_frame(&gpu, &output);
            let expected = reference::apply(&quantised, e, plan);
            for (px, (a, b)) in actual.iter().zip(&expected).enumerate() {
                for c in 0..3 {
                    assert!(
                        a[c].is_finite() && a[c] >= 0.0,
                        "case {i} pixel {px} channel {c} is {}",
                        a[c]
                    );
                    assert!(
                        (a[c] - b[c]).abs() < 4.0e-3,
                        "case {i} pixel ({}, {}) channel {c}: gpu={} cpu={}",
                        px as u32 % e.width,
                        px as u32 / e.width,
                        a[c],
                        b[c]
                    );
                }
            }
        }
    }

    #[test]
    fn exec_without_a_profile_or_ca_is_a_bit_exact_copy() {
        let Some(gpu) = shared_test_gpu() else { return };
        let e = ext(40, 30);
        let data = textured(e);
        let input = upload_frame(&gpu, e, &data);
        let output = FrameTexture::new(&gpu, e);
        let frame = nicti_cornea::LinearFrame {
            make: "T".into(),
            model: "S".into(),
            width: e.width,
            height: e.height,
            black: 0,
            maximum: 65535,
            cam_mul: [1.0; 4],
            pre_mul: [1.0; 4],
            cam_xyz: [0.0; 12],
            cblack: [0; 4],
            pixels: vec![0; (e.width * e.height * 3) as usize],
            dng_opcode_list3: None,
        };
        let kernel = LensKernel::new(&gpu);
        let params = LensParams::default();
        let exec = LensExec {
            kernel: &kernel,
            params: &params,
            frame: &frame,
        };
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        exec.encode(&gpu, &mut encoder, Some(&input), &output);
        gpu.queue.submit(Some(encoder.finish()));
        assert_eq!(read_frame(&gpu, &input), read_frame(&gpu, &output));
    }

    #[test]
    fn the_ca_estimate_runs_once_per_frame_not_once_per_cache_miss() {
        use std::sync::atomic::Ordering;
        let (w, h) = (96usize, 64usize);
        let frame = nicti_cornea::LinearFrame {
            make: "T".into(),
            model: "S".into(),
            width: w as u32,
            height: h as u32,
            black: 0,
            maximum: 65535,
            cam_mul: [1.0; 4],
            pre_mul: [1.0; 4],
            cam_xyz: [0.0; 12],
            cblack: [0; 4],
            // A pixel pattern unique to this test, so no other test can prime its memo entry.
            pixels: (0..w * h * 3).map(|i| (i * 7919 % 60001) as u16).collect(),
            dng_opcode_list3: None,
        };
        let params = LensParams {
            remove_ca: true,
            embedded_profile: true,
        };
        let e = ext(w as u32, h as u32);
        let before = CA_RUNS.load(Ordering::SeqCst);
        let a = LensPlan::resolve(&params, &frame, e);
        let b = LensPlan::resolve(&params, &frame, e);
        let runs = CA_RUNS.load(Ordering::SeqCst) - before;
        assert_eq!(a, b);
        assert_eq!(runs, 1, "the second resolve must hit the memo");
        // A different photo is a different key.
        let mut other = frame.clone();
        other.pixels[0] = other.pixels[0].wrapping_add(1);
        other.pixels[1] = 12345;
        let _ = LensPlan::resolve(&params, &other, e);
        assert_eq!(CA_RUNS.load(Ordering::SeqCst) - before, 2);
    }

    #[test]
    fn resolve_applies_a_dng_profile_and_honours_the_switch() {
        let e = ext(64, 48);
        let blob = nicti_iris::dng::write_opcode_list3(&full_model());
        let frame = nicti_cornea::LinearFrame {
            make: "T".into(),
            model: "S".into(),
            width: e.width,
            height: e.height,
            black: 0,
            maximum: 65535,
            cam_mul: [1.0; 4],
            pre_mul: [1.0; 4],
            cam_xyz: [0.0; 12],
            cblack: [0; 4],
            pixels: vec![0; (e.width * e.height * 3) as usize],
            dng_opcode_list3: Some(blob),
        };
        let on = LensPlan::resolve(&LensParams::default(), &frame, e);
        assert!(on.warp.is_some() && on.vignette.is_some());
        // The profile's planes differ, so it already corrects CA: auto-CA must not stack on top.
        let both = LensPlan::resolve(
            &LensParams {
                remove_ca: true,
                embedded_profile: true,
            },
            &frame,
            e,
        );
        assert!(both.ca.is_none(), "no double CA correction");
        let off = LensPlan::resolve(
            &LensParams {
                remove_ca: false,
                embedded_profile: false,
            },
            &frame,
            e,
        );
        assert!(off.is_identity());
    }
}
