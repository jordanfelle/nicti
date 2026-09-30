//! Heal/remove stage (#51, ADR-0050): clone stamp and gradient-domain (Poisson) spot heal on the
//! GPU, promoted from `spikes/groom`. A `Baked` stage sitting after lens correction and before the
//! live suffix (ADR-0044), so it operates on linear camera RGB.
//!
//! [`HealStage`] is the registry-facing `RenderStage` (kind, default params, cache contribution);
//! [`HealKernel`] owns the compute pipelines (built once, like every other `*Kernel` in
//! `stages.rs`); [`HealExec`] is the per-render `BakedExec` that carries one render's
//! [`HealParams`]. `shaders/heal.wgsl` documents the per-spot pass sequence.
//!
//! **Semantics shared by the shader and the CPU reference (`reference`, test-only):**
//! - a spot's centers are rounded to whole pixels; its patch is `side = 2 * ceil(radius) + 1`
//!   pixels square, centered on the rounded center;
//! - reads outside the frame clamp to the nearest edge pixel (the spike zero-filled instead, which
//!   would have poisoned a heal near a border with black);
//! - only patch pixels that fall inside the frame are written back;
//! - spots apply in list order, each seeing the previous spots' output.

use serde_json::Value;
use wgpu::util::DeviceExt;

use nicti_claw::Module;

use crate::coat::{self, HealParams, Spot, SpotKind};
use crate::frame::{Extent, FrameTexture};
use crate::gpu::{make_compute_pipeline, GpuContext};
use crate::graph::StageKind;
use crate::renderer::BakedExec;
use crate::stages::{PassthroughExec, HEAL};
use crate::RenderStage;

/// Bump when the shader or the spot semantics change in a way that must invalidate cached output
/// even though the params schema didn't.
pub const IMPL_VERSION: u32 = 1;

/// Largest destination radius honored, in pixels: bounds a single spot's patch (and so its scratch
/// textures and Jacobi cost) no matter what a hand-edited or imported document asks for.
pub const MAX_RADIUS: f32 = 512.0;

/// Jacobi sweeps for a patch of `side` pixels. ADR-0050 measured 50 sweeps on a 41-px patch; a
/// larger patch needs proportionally more for the boundary condition to propagate inward.
pub fn jacobi_iterations(side: i32) -> u32 {
    (side.max(0) as u32).clamp(50, 400)
}

/// A spot's resolved integer geometry -- everything the shader needs, computed once on the CPU so
/// the shader and the reference can never disagree on rounding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpotGeometry {
    pub center: (i32, i32),
    pub src_center: (i32, i32),
    pub half: i32,
    pub side: i32,
}

/// Resolves `spot` to integer geometry, or `None` when it can't be applied on the GPU: a
/// non-finite or non-positive radius, a non-finite center/offset, or a Clone/Heal spot with no
/// `source_offset`. (`Remove` spots are not GPU clone/heal work; they composite a pre-inpainted
/// patch instead.)
pub fn spot_geometry(spot: &Spot) -> Option<SpotGeometry> {
    if !(spot.radius.is_finite() && spot.radius > 0.0) {
        return None;
    }
    let (cx, cy) = spot.center;
    if !(cx.is_finite() && cy.is_finite()) {
        return None;
    }
    let (ox, oy) = spot.source_offset?;
    if !(ox.is_finite() && oy.is_finite()) {
        return None;
    }
    let half = spot.radius.min(MAX_RADIUS).ceil() as i32;
    let center = (cx.round() as i32, cy.round() as i32);
    Some(SpotGeometry {
        center,
        src_center: ((cx + ox).round() as i32, (cy + oy).round() as i32),
        half,
        side: 2 * half + 1,
    })
}

/// The destination-circle radius actually applied (clamped to [`MAX_RADIUS`]).
fn applied_radius(spot: &Spot) -> f32 {
    spot.radius.min(MAX_RADIUS)
}

pub struct HealStage;

impl Module for HealStage {
    fn id(&self) -> &str {
        HEAL
    }
    fn schema_version(&self) -> u32 {
        1
    }
    fn migrate_params(&self, _from_version: u32, params: Value) -> Option<Value> {
        Some(params)
    }
}

impl RenderStage for HealStage {
    fn kind(&self) -> StageKind {
        StageKind::Baked
    }
    fn default_params(&self) -> Value {
        coat::default_value::<HealParams>()
    }
    fn impl_version(&self) -> u32 {
        IMPL_VERSION
    }
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct SpotUniforms {
    center: [i32; 2],
    src_center: [i32; 2],
    side: i32,
    half: i32,
    radius: f32,
    feather: f32,
    opacity: f32,
    frame_w: i32,
    frame_h: i32,
    _pad: u32,
}

/// The largest AI-removal patch side honored: a 1025-px LaMa crop plus its context margin, doubled
/// for headroom. Bounds the scratch textures a malformed patch could demand.
pub const MAX_PATCH_SIDE: u32 = 2049;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PatchError {
    #[error("removal patch side {0} must be odd and between 1 and {MAX_PATCH_SIDE}")]
    BadSide(u32),
    #[error("removal patch has {got} pixels, expected {expected}")]
    WrongPixelCount { got: usize, expected: usize },
}

/// A finished AI removal (#51): a square patch of already-inpainted pixels, in the same space as
/// the heal stage's input (linear camera RGB), ready to be blended over the frame.
///
/// `pixels` is row-major `side * side`; `.rgb` is the fill and `.a` the fill weight in `[0, 1]`
/// (the object mask, already feathered -- so the shader needs no radius or feather of its own).
/// The patch is centered on `center`: patch pixel `(px, py)` covers frame pixel
/// `(center.0 + px - side/2, center.1 + py - side/2)`. Producing a *square* patch is the
/// producer's job (pad with zero weight); it keeps the GPU side to one shape.
#[derive(Debug, Clone, PartialEq)]
pub struct RemovalPatch {
    pub center: (i32, i32),
    pub side: u32,
    pub pixels: Vec<[f32; 4]>,
}

impl RemovalPatch {
    pub fn new(center: (i32, i32), side: u32, pixels: Vec<[f32; 4]>) -> Result<Self, PatchError> {
        if side == 0 || side.is_multiple_of(2) || side > MAX_PATCH_SIDE {
            return Err(PatchError::BadSide(side));
        }
        let expected = side as usize * side as usize;
        if pixels.len() != expected {
            return Err(PatchError::WrongPixelCount {
                got: pixels.len(),
                expected,
            });
        }
        Ok(Self {
            center,
            side,
            pixels,
        })
    }

    /// Content hash: what identifies "this exact fill" in the heal stage's cache key.
    pub fn content_hash(&self) -> blake3::Hash {
        let mut h = blake3::Hasher::new();
        h.update(&self.center.0.to_le_bytes());
        h.update(&self.center.1.to_le_bytes());
        h.update(&self.side.to_le_bytes());
        h.update(bytemuck::cast_slice(&self.pixels));
        h.finalize()
    }
}

/// Finished removals, keyed by [`spot_key`]. Owned by whoever runs the removal jobs (the Develop
/// view); the render only reads it.
pub type RemovalSet = std::collections::HashMap<String, std::sync::Arc<RemovalPatch>>;

/// A stable identity for a spot: the canonical hash of its full definition (kind, geometry and
/// mask recipe), so editing a spot yields a new key and its old patch can never be reused for it.
pub fn spot_key(spot: &Spot) -> String {
    nicti_pawprint::hash_value(spot)
        .expect("a Spot serializes without nulls (Option fields skip when None)")
        .to_hex()
        .to_string()
}

/// Stamps which removals are ready into the document's heal entry, as an extra `"removals"` map
/// (`spot key -> patch content hash`). [`HealParams`] ignores the unknown field when parsing, but
/// `apply_document` hashes the whole entry, so a patch arriving -- or being replaced -- changes the
/// heal stage's cache key and rebakes it, through the normal path rather than a side channel. A
/// no-op when the document has no heal entry or no removal is ready for one of its spots.
pub fn stamp_removal_state(doc: &mut nicti_pawprint::EditDocument, removals: &RemovalSet) {
    let Some(entry) = doc.stages.get_mut(HEAL) else {
        return;
    };
    let params: HealParams = coat::parse(&entry.params);
    let ready: std::collections::BTreeMap<String, String> = params
        .spots
        .iter()
        .filter(|s| s.kind == SpotKind::Remove)
        .filter_map(|s| {
            let key = spot_key(s);
            removals
                .get(&key)
                .map(|p| (key, p.content_hash().to_hex().to_string()))
        })
        .collect();
    if ready.is_empty() {
        return;
    }
    if let Value::Object(map) = &mut entry.params {
        map.insert(
            "removals".to_owned(),
            serde_json::to_value(ready).expect("a string map serializes"),
        );
    }
}

/// The scratch textures one `encode_spots` call reuses across its spots.
struct Scratch {
    patch_a: FrameTexture,
    patch_b: FrameTexture,
    guidance: FrameTexture,
    result: FrameTexture,
}

pub struct HealKernel {
    extract_dst: wgpu::ComputePipeline,
    extract_guidance: wgpu::ComputePipeline,
    jacobi: wgpu::ComputePipeline,
    composite: wgpu::ComputePipeline,
    composite_patch: wgpu::ComputePipeline,
}

impl HealKernel {
    pub fn new(gpu: &GpuContext) -> Self {
        let src = include_str!("../shaders/heal.wgsl");
        Self {
            extract_dst: make_compute_pipeline(&gpu.device, src, "extract_dst"),
            extract_guidance: make_compute_pipeline(&gpu.device, src, "extract_guidance"),
            jacobi: make_compute_pipeline(&gpu.device, src, "jacobi"),
            composite: make_compute_pipeline(&gpu.device, src, "composite"),
            composite_patch: make_compute_pipeline(&gpu.device, src, "composite_patch"),
        }
    }

    /// Records every applicable spot in `spots` onto `frame`, in list order: Clone/Heal spots run
    /// on the GPU, and a Remove spot composites its pre-inpainted patch from `removals` (a Remove
    /// spot with no ready patch is skipped, i.e. passes through). `frame` must already hold the
    /// upstream stage's output.
    pub fn encode_spots(
        &self,
        gpu: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        frame: &FrameTexture,
        spots: &[Spot],
        removals: &RemovalSet,
    ) {
        enum Op<'a> {
            Classic(&'a Spot, SpotGeometry),
            Patch(&'a Spot, &'a RemovalPatch),
        }
        let ops: Vec<Op> = spots
            .iter()
            .filter_map(|s| match s.kind {
                SpotKind::Clone | SpotKind::Heal => spot_geometry(s).map(|g| Op::Classic(s, g)),
                SpotKind::Remove => removals
                    .get(&spot_key(s))
                    .map(|patch| Op::Patch(s, patch.as_ref())),
            })
            .collect();
        let Some(max_side) = ops
            .iter()
            .map(|op| match op {
                Op::Classic(_, g) => g.side,
                Op::Patch(_, p) => p.side as i32,
            })
            .max()
        else {
            return;
        };
        let scratch = Extent {
            width: max_side as u32,
            height: max_side as u32,
        };
        let scratch_bufs = Scratch {
            patch_a: FrameTexture::new(gpu, scratch),
            patch_b: FrameTexture::new(gpu, scratch),
            guidance: FrameTexture::new(gpu, scratch),
            result: FrameTexture::new(gpu, scratch),
        };

        for op in ops {
            match op {
                Op::Classic(spot, g) => {
                    self.encode_classic(gpu, encoder, frame, &scratch_bufs, spot, g)
                }
                Op::Patch(spot, patch) => {
                    self.encode_patch(gpu, encoder, frame, &scratch_bufs, spot, patch)
                }
            }
        }
    }

    fn uniform_buffer(gpu: &GpuContext, u: &SpotUniforms) -> wgpu::Buffer {
        // A dedicated uniform buffer per spot: every `write`/init lands before the render's single
        // `queue.submit`, so a shared buffer would leave every spot reading the last spot's values
        // (the #46 gotcha in `render-graph`'s REFERENCE.md).
        gpu.device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("heal spot uniforms"),
                contents: bytemuck::bytes_of(u),
                usage: wgpu::BufferUsages::UNIFORM,
            })
    }

    fn encode_classic(
        &self,
        gpu: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        frame: &FrameTexture,
        s: &Scratch,
        spot: &Spot,
        g: SpotGeometry,
    ) {
        let ubuf = Self::uniform_buffer(
            gpu,
            &SpotUniforms {
                center: [g.center.0, g.center.1],
                src_center: [g.src_center.0, g.src_center.1],
                side: g.side,
                half: g.half,
                radius: applied_radius(spot),
                feather: spot.feather,
                opacity: spot.opacity.clamp(0.0, 1.0),
                frame_w: frame.extent.width as i32,
                frame_h: frame.extent.height as i32,
                _pad: 0,
            },
        );
        let groups = (g.side as u32).div_ceil(8);

        self.dispatch(
            gpu,
            encoder,
            &self.extract_dst,
            &[(0, &frame.view), (1, &s.patch_a.view)],
            &ubuf,
            groups,
        );
        self.dispatch(
            gpu,
            encoder,
            &self.extract_guidance,
            &[(0, &frame.view), (1, &s.guidance.view)],
            &ubuf,
            groups,
        );

        let solved = if spot.kind == SpotKind::Heal {
            let iterations = jacobi_iterations(g.side);
            let a_to_b = self.bind(
                gpu,
                &self.jacobi,
                &[
                    (0, &s.patch_a.view),
                    (1, &s.patch_b.view),
                    (2, &s.guidance.view),
                ],
                &ubuf,
            );
            let b_to_a = self.bind(
                gpu,
                &self.jacobi,
                &[
                    (0, &s.patch_b.view),
                    (1, &s.patch_a.view),
                    (2, &s.guidance.view),
                ],
                &ubuf,
            );
            for i in 0..iterations {
                let bind = if i.is_multiple_of(2) {
                    &a_to_b
                } else {
                    &b_to_a
                };
                let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                    label: Some("heal jacobi"),
                    timestamp_writes: None,
                });
                pass.set_pipeline(&self.jacobi);
                pass.set_bind_group(0, bind, &[]);
                pass.dispatch_workgroups(groups, groups, 1);
            }
            // Even sweep count ends back in `patch_a`, odd in `patch_b`.
            if iterations.is_multiple_of(2) {
                &s.patch_a
            } else {
                &s.patch_b
            }
        } else {
            &s.guidance
        };

        self.dispatch(
            gpu,
            encoder,
            &self.composite,
            &[(0, &frame.view), (1, &s.result.view), (2, &solved.view)],
            &ubuf,
            groups,
        );
        Self::copy_back(encoder, frame, &s.result, g.center, g.half, g.side);
    }

    fn encode_patch(
        &self,
        gpu: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        frame: &FrameTexture,
        s: &Scratch,
        spot: &Spot,
        patch: &RemovalPatch,
    ) {
        let side = patch.side as i32;
        let half = side / 2;
        // Upload the patch as an Rgba16Float texture (rgb = fill, a = fill weight).
        let extent = Extent {
            width: patch.side,
            height: patch.side,
        };
        let tex = FrameTexture::new(gpu, extent);
        let bytes: Vec<u8> = patch
            .pixels
            .iter()
            .flat_map(|px| {
                px.iter()
                    .flat_map(|&c| half::f16::from_f32(c).to_le_bytes())
            })
            .collect();
        gpu.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &tex.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(patch.side * 8),
                rows_per_image: Some(patch.side),
            },
            wgpu::Extent3d {
                width: patch.side,
                height: patch.side,
                depth_or_array_layers: 1,
            },
        );
        let ubuf = Self::uniform_buffer(
            gpu,
            &SpotUniforms {
                center: [patch.center.0, patch.center.1],
                src_center: [0, 0],
                side,
                half,
                radius: 0.0,
                feather: 0.0,
                opacity: spot.opacity.clamp(0.0, 1.0),
                frame_w: frame.extent.width as i32,
                frame_h: frame.extent.height as i32,
                _pad: 0,
            },
        );
        let groups = patch.side.div_ceil(8);
        self.dispatch(
            gpu,
            encoder,
            &self.composite_patch,
            &[(0, &frame.view), (1, &s.result.view), (2, &tex.view)],
            &ubuf,
            groups,
        );
        Self::copy_back(encoder, frame, &s.result, patch.center, half, side);
    }

    /// Copies the in-bounds part of a composited `side` x `side` patch (centered on `center`)
    /// from `result` back into `frame`.
    fn copy_back(
        encoder: &mut wgpu::CommandEncoder,
        frame: &FrameTexture,
        result: &FrameTexture,
        center: (i32, i32),
        half: i32,
        side: i32,
    ) {
        let x0 = center.0 - half;
        let y0 = center.1 - half;
        let cx0 = x0.max(0);
        let cy0 = y0.max(0);
        let cx1 = (x0 + side).min(frame.extent.width as i32);
        let cy1 = (y0 + side).min(frame.extent.height as i32);
        if cx1 <= cx0 || cy1 <= cy0 {
            return;
        }
        encoder.copy_texture_to_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &result.texture,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: (cx0 - x0) as u32,
                    y: (cy0 - y0) as u32,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyTextureInfo {
                texture: &frame.texture,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: cx0 as u32,
                    y: cy0 as u32,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::Extent3d {
                width: (cx1 - cx0) as u32,
                height: (cy1 - cy0) as u32,
                depth_or_array_layers: 1,
            },
        );
    }

    fn bind(
        &self,
        gpu: &GpuContext,
        pipeline: &wgpu::ComputePipeline,
        textures: &[(u32, &wgpu::TextureView)],
        uniforms: &wgpu::Buffer,
    ) -> wgpu::BindGroup {
        let mut entries: Vec<wgpu::BindGroupEntry> = textures
            .iter()
            .map(|(binding, view)| wgpu::BindGroupEntry {
                binding: *binding,
                resource: wgpu::BindingResource::TextureView(view),
            })
            .collect();
        entries.push(wgpu::BindGroupEntry {
            binding: 3,
            resource: uniforms.as_entire_binding(),
        });
        gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("heal bind group"),
            layout: &pipeline.get_bind_group_layout(0),
            entries: &entries,
        })
    }

    fn dispatch(
        &self,
        gpu: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        pipeline: &wgpu::ComputePipeline,
        textures: &[(u32, &wgpu::TextureView)],
        uniforms: &wgpu::Buffer,
        groups: u32,
    ) {
        let bind = self.bind(gpu, pipeline, textures, uniforms);
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("heal pass"),
            timestamp_writes: None,
        });
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, &bind, &[]);
        pass.dispatch_workgroups(groups, groups, 1);
    }
}

/// One render's heal work: copies the upstream frame through, then applies the spots on top.
pub struct HealExec<'a> {
    pub kernel: &'a HealKernel,
    pub params: &'a HealParams,
    /// Finished AI removals for the spots in `params` (empty when none are ready).
    pub removals: &'a RemovalSet,
}

impl BakedExec for HealExec<'_> {
    fn encode(
        &self,
        gpu: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        input: Option<&FrameTexture>,
        output: &FrameTexture,
    ) {
        PassthroughExec.encode(gpu, encoder, input, output);
        self.kernel
            .encode_spots(gpu, encoder, output, &self.params.spots, self.removals);
    }
}

/// Pure-CPU twin of the shader, used only to prove GPU parity.
#[cfg(test)]
pub(crate) mod reference {
    use super::*;

    pub fn feather_weight(dist: f32, radius: f32, feather: f32) -> f32 {
        if radius <= 0.0 || dist >= radius {
            return 0.0;
        }
        let feather = feather.max(0.0).min(radius);
        let inner = radius - feather;
        if dist <= inner {
            1.0
        } else {
            ((radius - dist) / (radius - inner)).clamp(0.0, 1.0)
        }
    }

    fn get_clamped(frame: &[[f32; 4]], w: i32, h: i32, x: i32, y: i32) -> [f32; 4] {
        frame[(y.clamp(0, h - 1) * w + x.clamp(0, w - 1)) as usize]
    }

    fn patch(
        frame: &[[f32; 4]],
        w: i32,
        h: i32,
        center: (i32, i32),
        g: SpotGeometry,
    ) -> Vec<[f32; 4]> {
        let mut out = Vec::with_capacity((g.side * g.side) as usize);
        for py in 0..g.side {
            for px in 0..g.side {
                out.push(get_clamped(
                    frame,
                    w,
                    h,
                    center.0 + px - g.half,
                    center.1 + py - g.half,
                ));
            }
        }
        out
    }

    /// One Jacobi sweep, identical update rule to `heal.wgsl::jacobi`.
    fn jacobi_step(
        input: &[[f32; 4]],
        guidance: &[[f32; 4]],
        side: i32,
        half: i32,
        radius: f32,
    ) -> Vec<[f32; 4]> {
        let mut out = input.to_vec();
        for y in 0..side {
            for x in 0..side {
                let i = (y * side + x) as usize;
                let (fx, fy) = ((x - half) as f32, (y - half) as f32);
                if (fx * fx + fy * fy).sqrt() >= radius {
                    continue;
                }
                let mut sum_f = [0.0f32; 3];
                let mut sum_g = [0.0f32; 3];
                let mut n = 0.0f32;
                for (nx, ny) in [(x - 1, y), (x + 1, y), (x, y - 1), (x, y + 1)] {
                    if nx < 0 || ny < 0 || nx >= side || ny >= side {
                        continue;
                    }
                    let j = (ny * side + nx) as usize;
                    for c in 0..3 {
                        sum_f[c] += input[j][c];
                        sum_g[c] += guidance[i][c] - guidance[j][c];
                    }
                    n += 1.0;
                }
                if n > 0.0 {
                    for c in 0..3 {
                        out[i][c] = (sum_f[c] + sum_g[c]) / n;
                    }
                }
            }
        }
        out
    }

    /// Blends a finished removal patch onto `frame` -- the formula `heal.wgsl::composite_patch`
    /// implements: `dst + (fill - dst) * clamp(weight) * opacity` on rgb, alpha untouched.
    pub fn apply_patch(frame: &mut [[f32; 4]], w: i32, h: i32, patch: &RemovalPatch, opacity: f32) {
        let half = patch.side as i32 / 2;
        for py in 0..patch.side as i32 {
            for px in 0..patch.side as i32 {
                let (fx, fy) = (patch.center.0 + px - half, patch.center.1 + py - half);
                if fx < 0 || fy < 0 || fx >= w || fy >= h {
                    continue;
                }
                let fill = patch.pixels[(py * patch.side as i32 + px) as usize];
                let wgt = fill[3].clamp(0.0, 1.0) * opacity.clamp(0.0, 1.0);
                let dst = &mut frame[(fy * w + fx) as usize];
                for c in 0..3 {
                    dst[c] += (fill[c] - dst[c]) * wgt;
                }
            }
        }
    }

    /// Applies every Clone/Heal spot in order onto `frame` (row-major, `w` x `h`).
    pub fn apply_spots(frame: &mut [[f32; 4]], w: i32, h: i32, spots: &[Spot]) {
        for spot in spots {
            if !matches!(spot.kind, SpotKind::Clone | SpotKind::Heal) {
                continue;
            }
            let Some(g) = spot_geometry(spot) else {
                continue;
            };
            let radius = applied_radius(spot);
            let guidance = patch(frame, w, h, g.src_center, g);
            let solved = if spot.kind == SpotKind::Heal {
                let mut cur = patch(frame, w, h, g.center, g);
                for _ in 0..jacobi_iterations(g.side) {
                    cur = jacobi_step(&cur, &guidance, g.side, g.half, radius);
                }
                cur
            } else {
                guidance
            };
            let snapshot = frame.to_vec();
            for py in 0..g.side {
                for px in 0..g.side {
                    let (dx, dy) = (px - g.half, py - g.half);
                    let (fx, fy) = (g.center.0 + dx, g.center.1 + dy);
                    if fx < 0 || fy < 0 || fx >= w || fy >= h {
                        continue;
                    }
                    let dist = ((dx * dx + dy * dy) as f32).sqrt();
                    let wgt =
                        feather_weight(dist, radius, spot.feather) * spot.opacity.clamp(0.0, 1.0);
                    let dst = snapshot[(fy * w + fx) as usize];
                    let s = solved[(py * g.side + px) as usize];
                    let mut out = [0.0f32; 4];
                    for c in 0..4 {
                        out[c] = dst[c] + (s[c] - dst[c]) * wgt;
                    }
                    frame[(fy * w + fx) as usize] = out;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{read_frame, shared_test_gpu as test_gpu, upload_frame};
    use nicti_pawprint::StageEntry;

    fn synthetic_frame(w: u32, h: u32) -> Vec<[f32; 4]> {
        // A smooth ramp plus a deterministic high-frequency term, rounded through f16 the way the
        // GPU's storage format will round it, so the reference starts from the same values.
        (0..w * h)
            .map(|i| {
                let (x, y) = ((i % w) as f32, (i / w) as f32);
                let v = |a: f32| {
                    half::f16::from_f32(
                        (0.15
                            + 0.5 * (x / w as f32)
                            + 0.25 * (y / h as f32)
                            + 0.05 * ((x * a + y * 1.7).sin()))
                        .clamp(0.0, 1.0),
                    )
                    .to_f32()
                };
                [v(0.9), v(1.3), v(2.1), 1.0]
            })
            .collect()
    }

    fn run_gpu(
        gpu: &GpuContext,
        w: u32,
        h: u32,
        data: &[[f32; 4]],
        spots: &[Spot],
    ) -> Vec<[f32; 4]> {
        run_gpu_with(gpu, w, h, data, spots, &RemovalSet::new())
    }

    fn run_gpu_with(
        gpu: &GpuContext,
        w: u32,
        h: u32,
        data: &[[f32; 4]],
        spots: &[Spot],
        removals: &RemovalSet,
    ) -> Vec<[f32; 4]> {
        let extent = Extent {
            width: w,
            height: h,
        };
        let input = upload_frame(gpu, extent, data);
        let output = FrameTexture::new(gpu, extent);
        let kernel = HealKernel::new(gpu);
        let params = HealParams {
            spots: spots.to_vec(),
        };
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        HealExec {
            kernel: &kernel,
            params: &params,
            removals,
        }
        .encode(gpu, &mut encoder, Some(&input), &output);
        gpu.queue.submit(Some(encoder.finish()));
        read_frame(gpu, &output)
    }

    fn max_diff(a: &[[f32; 4]], b: &[[f32; 4]]) -> f32 {
        a.iter()
            .zip(b)
            .flat_map(|(p, q)| (0..4).map(move |c| (p[c] - q[c]).abs()))
            .fold(0.0, f32::max)
    }

    fn assert_parity(spots: Vec<Spot>) {
        let Some(gpu) = test_gpu() else { return };
        let (w, h) = (96u32, 72u32);
        let data = synthetic_frame(w, h);
        let mut expected = data.clone();
        reference::apply_spots(&mut expected, w as i32, h as i32, &spots);
        let actual = run_gpu(&gpu, w, h, &data, &spots);
        let diff = max_diff(&expected, &actual);
        assert!(diff < 0.02, "GPU vs CPU max diff {diff}");
        assert!(
            max_diff(&data, &actual) > 0.02,
            "the spots must actually change the frame, or this test proves nothing"
        );
    }

    #[test]
    fn clone_spot_matches_the_cpu_reference() {
        assert_parity(vec![Spot::clone_spot((30.0, 30.0), 9.0, (30.0, 10.0), 3.0)]);
    }

    #[test]
    fn heal_spot_matches_the_cpu_reference() {
        assert_parity(vec![Spot::heal_spot((30.0, 30.0), 9.0, (35.0, 12.0), 3.0)]);
    }

    #[test]
    fn spots_apply_in_order_and_see_earlier_spots_output() {
        assert_parity(vec![
            Spot::clone_spot((30.0, 30.0), 9.0, (30.0, 10.0), 3.0),
            // Sources from the first spot's destination.
            Spot::heal_spot((70.0, 50.0), 8.0, (-40.0, -20.0), 2.0),
        ]);
    }

    #[test]
    fn spot_overlapping_the_frame_edge_matches_the_cpu_reference() {
        assert_parity(vec![
            Spot::heal_spot((3.0, 4.0), 10.0, (40.0, 30.0), 2.0),
            Spot::clone_spot((93.0, 70.0), 10.0, (-40.0, -30.0), 2.0),
        ]);
    }

    #[test]
    fn opacity_scales_the_blend() {
        let Some(gpu) = test_gpu() else { return };
        let (w, h) = (64u32, 64u32);
        let data = synthetic_frame(w, h);
        let mut spot = Spot::clone_spot((20.0, 20.0), 6.0, (30.0, 30.0), 0.0);
        let full = run_gpu(&gpu, w, h, &data, std::slice::from_ref(&spot));
        spot.opacity = 0.5;
        let half = run_gpu(&gpu, w, h, &data, std::slice::from_ref(&spot));
        let i = (20 * w + 20) as usize;
        for c in 0..3 {
            let expect = data[i][c] + (full[i][c] - data[i][c]) * 0.5;
            assert!((half[i][c] - expect).abs() < 0.01);
        }
    }

    #[test]
    fn no_spots_is_a_passthrough() {
        let Some(gpu) = test_gpu() else { return };
        let data = synthetic_frame(32, 32);
        let out = run_gpu(&gpu, 32, 32, &data, &[]);
        assert!(max_diff(&data, &out) < 1e-3);
    }

    #[test]
    fn unappliable_spots_are_skipped_not_fatal() {
        let Some(gpu) = test_gpu() else { return };
        let data = synthetic_frame(32, 32);
        let mut no_source = Spot::heal_spot((10.0, 10.0), 4.0, (5.0, 5.0), 1.0);
        no_source.source_offset = None;
        let bad = [
            no_source,
            Spot::clone_spot((f32::NAN, 10.0), 4.0, (5.0, 5.0), 1.0),
            Spot::clone_spot((10.0, 10.0), 0.0, (5.0, 5.0), 1.0),
            Spot::clone_spot((10.0, 10.0), -3.0, (5.0, 5.0), 1.0),
        ];
        let out = run_gpu(&gpu, 32, 32, &data, &bad);
        assert!(max_diff(&data, &out) < 1e-3);
    }

    /// A `side` x `side` patch of a constant fill, with a weight ramp so partial weights matter.
    fn ramp_patch(center: (i32, i32), side: u32, fill: [f32; 3]) -> RemovalPatch {
        let pixels = (0..side * side)
            .map(|i| {
                let x = (i % side) as f32 / (side - 1) as f32;
                let f = |c: f32| half::f16::from_f32(c).to_f32();
                [f(fill[0]), f(fill[1]), f(fill[2]), f(0.25 + 0.75 * x)]
            })
            .collect();
        RemovalPatch::new(center, side, pixels).unwrap()
    }

    fn remove_spot_for_test(center: (f32, f32), opacity: f32) -> Spot {
        let mut s = Spot::remove_spot(
            center,
            9.0,
            2.0,
            coat::MaskRecipe {
                model_id: "test".into(),
                model_version: "1".into(),
                params: serde_json::json!({ "click": [center.0, center.1] }),
                seed: None,
            },
        );
        s.opacity = opacity;
        s
    }

    fn removals_for(spot: &Spot, patch: RemovalPatch) -> RemovalSet {
        let mut set = RemovalSet::new();
        set.insert(spot_key(spot), std::sync::Arc::new(patch));
        set
    }

    #[test]
    fn a_removal_patch_matches_the_cpu_blend() {
        let Some(gpu) = test_gpu() else { return };
        let (w, h) = (64u32, 48u32);
        let data = synthetic_frame(w, h);
        let spot = remove_spot_for_test((30.0, 20.0), 1.0);
        let patch = ramp_patch((30, 20), 21, [0.9, 0.1, 0.4]);
        let mut expected = data.clone();
        reference::apply_patch(&mut expected, w as i32, h as i32, &patch, 1.0);
        let actual = run_gpu_with(
            &gpu,
            w,
            h,
            &data,
            std::slice::from_ref(&spot),
            &removals_for(&spot, patch),
        );
        assert!(max_diff(&expected, &actual) < 0.01);
        assert!(
            max_diff(&data, &actual) > 0.05,
            "the patch must visibly change the frame"
        );
    }

    #[test]
    fn removal_opacity_scales_the_blend() {
        let Some(gpu) = test_gpu() else { return };
        let (w, h) = (64u32, 48u32);
        let data = synthetic_frame(w, h);
        let spot = remove_spot_for_test((30.0, 20.0), 0.5);
        let patch = ramp_patch((30, 20), 21, [0.9, 0.1, 0.4]);
        let mut expected = data.clone();
        reference::apply_patch(&mut expected, w as i32, h as i32, &patch, 0.5);
        let actual = run_gpu_with(
            &gpu,
            w,
            h,
            &data,
            std::slice::from_ref(&spot),
            &removals_for(&spot, patch),
        );
        assert!(max_diff(&expected, &actual) < 0.01);
    }

    #[test]
    fn a_removal_patch_hanging_off_the_frame_edge_is_clipped() {
        let Some(gpu) = test_gpu() else { return };
        let (w, h) = (40u32, 30u32);
        let data = synthetic_frame(w, h);
        let spot = remove_spot_for_test((2.0, 27.0), 1.0);
        let patch = ramp_patch((2, 27), 21, [0.2, 0.8, 0.5]);
        let mut expected = data.clone();
        reference::apply_patch(&mut expected, w as i32, h as i32, &patch, 1.0);
        let actual = run_gpu_with(
            &gpu,
            w,
            h,
            &data,
            std::slice::from_ref(&spot),
            &removals_for(&spot, patch),
        );
        assert!(max_diff(&expected, &actual) < 0.01);
    }

    #[test]
    fn a_removal_with_no_ready_patch_passes_through() {
        let Some(gpu) = test_gpu() else { return };
        let data = synthetic_frame(32, 32);
        let out = run_gpu(
            &gpu,
            32,
            32,
            &data,
            &[remove_spot_for_test((16.0, 16.0), 1.0)],
        );
        assert!(max_diff(&data, &out) < 1e-3);
    }

    #[test]
    fn a_patch_for_a_different_spot_is_not_applied() {
        let Some(gpu) = test_gpu() else { return };
        let data = synthetic_frame(32, 32);
        let ready = remove_spot_for_test((10.0, 10.0), 1.0);
        let asked = remove_spot_for_test((20.0, 20.0), 1.0);
        let removals = removals_for(&ready, ramp_patch((10, 10), 11, [1.0, 0.0, 0.0]));
        let out = run_gpu_with(&gpu, 32, 32, &data, &[asked], &removals);
        assert!(max_diff(&data, &out) < 1e-3);
    }

    #[test]
    fn removals_and_classic_spots_apply_in_list_order() {
        let Some(gpu) = test_gpu() else { return };
        let (w, h) = (64u32, 48u32);
        let data = synthetic_frame(w, h);
        let remove = remove_spot_for_test((30.0, 20.0), 1.0);
        let patch = ramp_patch((30, 20), 21, [0.9, 0.1, 0.4]);
        // Cloned *after* the removal, from inside the removed area: it must copy the filled pixels.
        let clone = Spot::clone_spot((50.0, 36.0), 6.0, (-20.0, -16.0), 0.0);
        let mut expected = data.clone();
        reference::apply_patch(&mut expected, w as i32, h as i32, &patch, 1.0);
        reference::apply_spots(
            &mut expected,
            w as i32,
            h as i32,
            std::slice::from_ref(&clone),
        );
        let actual = run_gpu_with(
            &gpu,
            w,
            h,
            &data,
            &[remove.clone(), clone],
            &removals_for(&remove, patch),
        );
        assert!(max_diff(&expected, &actual) < 0.02);
    }

    #[test]
    fn removal_patch_validation() {
        assert_eq!(
            RemovalPatch::new((0, 0), 4, vec![[0.0; 4]; 16]),
            Err(PatchError::BadSide(4))
        );
        assert_eq!(
            RemovalPatch::new((0, 0), 0, vec![]),
            Err(PatchError::BadSide(0))
        );
        assert_eq!(
            RemovalPatch::new((0, 0), MAX_PATCH_SIDE + 2, vec![]),
            Err(PatchError::BadSide(MAX_PATCH_SIDE + 2))
        );
        assert_eq!(
            RemovalPatch::new((0, 0), 3, vec![[0.0; 4]; 8]),
            Err(PatchError::WrongPixelCount {
                got: 8,
                expected: 9
            })
        );
        assert!(RemovalPatch::new((0, 0), 3, vec![[0.0; 4]; 9]).is_ok());
    }

    fn doc_with(spots: Vec<Spot>) -> nicti_pawprint::EditDocument {
        let mut doc = nicti_pawprint::EditDocument::default();
        doc.stages.insert(
            HEAL.to_owned(),
            StageEntry {
                schema_version: 1,
                params: serde_json::to_value(HealParams { spots }).unwrap(),
            },
        );
        doc
    }

    fn contribution(doc: &nicti_pawprint::EditDocument) -> blake3::Hash {
        HealStage.cache_contribution(&doc.stages[HEAL]).unwrap()
    }

    #[test]
    fn a_ready_patch_changes_the_heal_cache_key_and_a_new_patch_changes_it_again() {
        let spot = remove_spot_for_test((30.0, 20.0), 1.0);
        let base = doc_with(vec![spot.clone()]);

        let mut none_ready = base.clone();
        stamp_removal_state(&mut none_ready, &RemovalSet::new());
        assert_eq!(contribution(&base), contribution(&none_ready));

        let mut first = base.clone();
        stamp_removal_state(
            &mut first,
            &removals_for(&spot, ramp_patch((30, 20), 21, [0.9, 0.1, 0.4])),
        );
        assert_ne!(
            contribution(&base),
            contribution(&first),
            "a patch arriving must rebake"
        );

        let mut second = base.clone();
        stamp_removal_state(
            &mut second,
            &removals_for(&spot, ramp_patch((30, 20), 21, [0.1, 0.9, 0.4])),
        );
        assert_ne!(
            contribution(&first),
            contribution(&second),
            "a changed fill must rebake"
        );
    }

    #[test]
    fn stamping_does_not_disturb_how_the_params_parse() {
        let spot = remove_spot_for_test((30.0, 20.0), 1.0);
        let mut doc = doc_with(vec![spot.clone()]);
        stamp_removal_state(
            &mut doc,
            &removals_for(&spot, ramp_patch((30, 20), 21, [0.9, 0.1, 0.4])),
        );
        let parsed: HealParams = coat::parse(&doc.stages[HEAL].params);
        assert_eq!(parsed.spots, vec![spot]);
        // And the stamped entry is still hashable (no nulls sneaked in).
        HealStage.cache_contribution(&doc.stages[HEAL]).unwrap();
    }

    #[test]
    fn spot_keys_change_with_any_edit_to_the_spot() {
        let a = remove_spot_for_test((30.0, 20.0), 1.0);
        let mut b = a.clone();
        b.radius += 1.0;
        let mut c = a.clone();
        c.mask_recipe.as_mut().unwrap().params = serde_json::json!({ "click": [31.0, 20.0] });
        assert_ne!(spot_key(&a), spot_key(&b));
        assert_ne!(spot_key(&a), spot_key(&c));
        assert_eq!(spot_key(&a), spot_key(&a.clone()));
    }

    /// Wall-clock cost of the heal stage (frame copy + spots + GPU sync) on the machine's own
    /// adapter, per `docs/benchmarks.md`'s protocol (1 warm-up, 5 measured, p50/p95). Not run in
    /// CI. Use a persistent kernel (ADR-0016's "never rebuild a pipeline in the timing loop"), and
    /// a real adapter -- a software one (lavapipe/WARP) is refused as meaningless:
    ///
    /// ```text
    /// NICTI_WGPU_BACKEND=vulkan cargo test -p nicti-tapetum --release heal::tests::throughput \
    ///   -- --ignored --nocapture
    /// ```
    ///
    /// The "0 spots" row is the fixed cost of copying the frame, so a spot's own cost is the
    /// difference to it. This is end-to-end (submit + readback-free fence), unlike ADR-0050's
    /// kernel-only `TIMESTAMP_QUERY` proxy for the Jacobi dispatches.
    #[test]
    #[ignore = "timing measurement; needs a real GPU adapter"]
    fn throughput() {
        use crate::gpu::GpuPreference;
        let gpu = GpuContext::new(GpuPreference::Auto).expect("an adapter");
        println!(
            "adapter: {} ({:?}), software: {}",
            gpu.adapter_name, gpu.backend, gpu.is_software
        );
        assert!(
            !gpu.is_software,
            "a software adapter's timings are meaningless"
        );
        let kernel = HealKernel::new(&gpu);

        let frames = [
            ("screen 3840x2560", 3840u32, 2560u32),
            ("full 8280x5520", 8280, 5520),
        ];
        for (label, w, h) in frames {
            let extent = Extent {
                width: w,
                height: h,
            };
            // Uniform gray input: content doesn't affect the cost of any pass.
            let input = FrameTexture::new(&gpu, extent);
            let output = FrameTexture::new(&gpu, extent);
            let spot = |kind: SpotKind, i: u32, r: f32| {
                let c = (400.0 + 90.0 * i as f32, 500.0 + 40.0 * i as f32);
                let mut s = Spot::heal_spot(c, r, (300.0, 0.0), r * 0.3);
                s.kind = kind;
                s
            };
            let patch = |side: u32| {
                let px = vec![[0.5, 0.5, 0.5, 1.0]; (side * side) as usize];
                RemovalPatch::new((1000, 1000), side, px).unwrap()
            };
            let scenarios: Vec<(&str, Vec<Spot>, RemovalSet)> = vec![
                ("0 spots (frame copy only)", vec![], RemovalSet::new()),
                (
                    "1 heal r=24",
                    vec![spot(SpotKind::Heal, 0, 24.0)],
                    RemovalSet::new(),
                ),
                (
                    "10 heal r=24",
                    (0..10).map(|i| spot(SpotKind::Heal, i, 24.0)).collect(),
                    RemovalSet::new(),
                ),
                (
                    "1 heal r=100",
                    vec![spot(SpotKind::Heal, 0, 100.0)],
                    RemovalSet::new(),
                ),
                (
                    "1 heal r=300",
                    vec![spot(SpotKind::Heal, 0, 300.0)],
                    RemovalSet::new(),
                ),
                (
                    "1 clone r=100",
                    vec![spot(SpotKind::Clone, 0, 100.0)],
                    RemovalSet::new(),
                ),
                (
                    "10 clone r=24",
                    (0..10).map(|i| spot(SpotKind::Clone, i, 24.0)).collect(),
                    RemovalSet::new(),
                ),
                (
                    "1 AI patch 513x513",
                    {
                        let mut s = spot(SpotKind::Remove, 0, 100.0);
                        s.mask_recipe = Some(coat::MaskRecipe {
                            model_id: "b".into(),
                            model_version: "1".into(),
                            params: serde_json::json!({}),
                            seed: None,
                        });
                        vec![s]
                    },
                    RemovalSet::new(),
                ),
            ];
            println!("--- {label} ---");
            for (name, spots, mut removals) in scenarios {
                if name.starts_with("1 AI patch") {
                    removals.insert(spot_key(&spots[0]), std::sync::Arc::new(patch(513)));
                }
                let params = HealParams { spots };
                let run = || {
                    let t = std::time::Instant::now();
                    let mut enc = gpu
                        .device
                        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
                    HealExec {
                        kernel: &kernel,
                        params: &params,
                        removals: &removals,
                    }
                    .encode(&gpu, &mut enc, Some(&input), &output);
                    gpu.queue.submit(Some(enc.finish()));
                    gpu.device
                        .poll(wgpu::PollType::wait_indefinitely())
                        .expect("poll");
                    t.elapsed().as_secs_f64() * 1000.0
                };
                let _warmup = run();
                let mut ms: Vec<f64> = (0..5).map(|_| run()).collect();
                ms.sort_by(|a, b| a.total_cmp(b));
                println!(
                    "{name:<28} p50 {:>8.2} ms   p95 {:>8.2} ms   max {:>8.2} ms",
                    ms[2], ms[4], ms[4]
                );
            }
        }
    }

    #[test]
    fn spot_geometry_rounds_and_sizes_the_patch() {
        let g = spot_geometry(&Spot::clone_spot((10.4, 20.6), 4.2, (3.0, -2.0), 1.0)).unwrap();
        assert_eq!(g.center, (10, 21));
        assert_eq!(g.half, 5);
        assert_eq!(g.side, 11);
        assert_eq!(g.src_center, (13, 19));
    }

    #[test]
    fn oversized_radius_is_clamped() {
        let g = spot_geometry(&Spot::clone_spot((0.0, 0.0), 1e9, (1.0, 1.0), 1.0)).unwrap();
        assert_eq!(g.half, MAX_RADIUS as i32);
    }

    #[test]
    fn default_params_are_an_empty_spot_list_and_a_spot_changes_the_cache_contribution() {
        let stage = HealStage;
        assert_eq!(stage.default_params(), serde_json::json!({ "spots": [] }));
        let empty = StageEntry {
            schema_version: 1,
            params: stage.default_params(),
        };
        let with_spot = StageEntry {
            schema_version: 1,
            params: serde_json::to_value(HealParams {
                spots: vec![Spot::clone_spot((1.0, 2.0), 3.0, (4.0, 5.0), 1.0)],
            })
            .unwrap(),
        };
        assert_ne!(
            stage.cache_contribution(&empty).unwrap(),
            stage.cache_contribution(&with_spot).unwrap()
        );
    }

    #[test]
    fn a_spot_round_trips_through_json_without_null_fields() {
        let params = HealParams {
            spots: vec![Spot::clone_spot((1.0, 2.0), 3.0, (4.0, 5.0), 1.0)],
        };
        let value = serde_json::to_value(&params).unwrap();
        assert!(!value.to_string().contains("null"));
        assert_eq!(coat::parse::<HealParams>(&value), params);
        // hash_value refuses null, so this also proves the canonical hasher accepts a real spot.
        nicti_pawprint::hash_value(&value).unwrap();
    }
}
