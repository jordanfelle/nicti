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

pub struct HealKernel {
    extract_dst: wgpu::ComputePipeline,
    extract_guidance: wgpu::ComputePipeline,
    jacobi: wgpu::ComputePipeline,
    composite: wgpu::ComputePipeline,
}

impl HealKernel {
    pub fn new(gpu: &GpuContext) -> Self {
        let src = include_str!("../shaders/heal.wgsl");
        Self {
            extract_dst: make_compute_pipeline(&gpu.device, src, "extract_dst"),
            extract_guidance: make_compute_pipeline(&gpu.device, src, "extract_guidance"),
            jacobi: make_compute_pipeline(&gpu.device, src, "jacobi"),
            composite: make_compute_pipeline(&gpu.device, src, "composite"),
        }
    }

    /// Records every applicable Clone/Heal spot in `spots` onto `frame`, in list order. `frame`
    /// must already hold the upstream stage's output.
    pub fn encode_spots(
        &self,
        gpu: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        frame: &FrameTexture,
        spots: &[Spot],
    ) {
        let work: Vec<(&Spot, SpotGeometry)> = spots
            .iter()
            .filter(|s| matches!(s.kind, SpotKind::Clone | SpotKind::Heal))
            .filter_map(|s| spot_geometry(s).map(|g| (s, g)))
            .collect();
        let Some(max_side) = work.iter().map(|(_, g)| g.side).max() else {
            return;
        };
        let scratch = Extent {
            width: max_side as u32,
            height: max_side as u32,
        };
        let patch_a = FrameTexture::new(gpu, scratch);
        let patch_b = FrameTexture::new(gpu, scratch);
        let guidance = FrameTexture::new(gpu, scratch);
        let result = FrameTexture::new(gpu, scratch);

        for (spot, g) in work {
            let uniforms = SpotUniforms {
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
            };
            // A dedicated uniform buffer per spot: every `write`/init lands before the render's
            // single `queue.submit`, so a shared buffer would leave every spot reading the last
            // spot's values (the #46 gotcha in `render-graph`'s REFERENCE.md).
            let ubuf = gpu
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("heal spot uniforms"),
                    contents: bytemuck::bytes_of(&uniforms),
                    usage: wgpu::BufferUsages::UNIFORM,
                });
            let groups = (g.side as u32).div_ceil(8);

            self.dispatch(
                gpu,
                encoder,
                &self.extract_dst,
                &[(0, &frame.view), (1, &patch_a.view)],
                &ubuf,
                groups,
            );
            self.dispatch(
                gpu,
                encoder,
                &self.extract_guidance,
                &[(0, &frame.view), (1, &guidance.view)],
                &ubuf,
                groups,
            );

            let solved = if spot.kind == SpotKind::Heal {
                let iterations = jacobi_iterations(g.side);
                let a_to_b = self.bind(
                    gpu,
                    &self.jacobi,
                    &[(0, &patch_a.view), (1, &patch_b.view), (2, &guidance.view)],
                    &ubuf,
                );
                let b_to_a = self.bind(
                    gpu,
                    &self.jacobi,
                    &[(0, &patch_b.view), (1, &patch_a.view), (2, &guidance.view)],
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
                    &patch_a
                } else {
                    &patch_b
                }
            } else {
                &guidance
            };

            self.dispatch(
                gpu,
                encoder,
                &self.composite,
                &[(0, &frame.view), (1, &result.view), (2, &solved.view)],
                &ubuf,
                groups,
            );

            // Copy the in-bounds part of the composited patch back into the frame.
            let x0 = g.center.0 - g.half;
            let y0 = g.center.1 - g.half;
            let cx0 = x0.max(0);
            let cy0 = y0.max(0);
            let cx1 = (x0 + g.side).min(frame.extent.width as i32);
            let cy1 = (y0 + g.side).min(frame.extent.height as i32);
            if cx1 > cx0 && cy1 > cy0 {
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
        }
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
            .encode_spots(gpu, encoder, output, &self.params.spots);
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
