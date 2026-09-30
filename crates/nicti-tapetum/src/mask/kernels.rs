//! The mask GPU kernels (#49): gradients, the brush stroke fold, component compose and the atlas
//! pack. Each is one persistent pipeline built once in [`MaskKernels::new`] -- never rebuilt per
//! call (a pipeline compile inside a hot path measured ~1000x too slow in this repo's research) --
//! and every dispatch gets its *own* small uniform buffer, because `queue.write_buffer` calls all
//! land before a render's single `submit`, so two dispatches sharing one buffer would both read
//! the last write (the #46 gotcha).
//!
//! Masks live in single-channel `R32Float` [`FieldTexture`]s. Inputs are always read as sampled
//! `texture_2d` + `textureLoad` and outputs are write-only storage, never a read-mode storage
//! texture (ADR-0051: that pattern produced garbage on the RTX 5080 under Dx12). Every kernel has a
//! CPU twin in `raster`/`compose` that the tests in this file check it against.

use wgpu::util::DeviceExt;

use super::params::{MaskSource, Op};
use super::raster::{self, Dab};
use super::Field;
use crate::gpu::{make_compute_pipeline, GpuContext};

/// Brush dabs are binned into tiles of this many pixels per side (matches `mask_brush.wgsl`).
pub const BRUSH_TILE: usize = 64;

/// A single-channel `f32` mask texture.
pub struct FieldTexture {
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    pub width: u32,
    pub height: u32,
}

impl FieldTexture {
    pub const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R32Float;

    /// A zero-initialized field (a fresh wgpu texture is zeroed).
    pub fn new(gpu: &GpuContext, width: u32, height: u32) -> Self {
        let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("nicti-tapetum mask field"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: Self::FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::STORAGE_BINDING
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self {
            texture,
            view,
            width,
            height,
        }
    }

    /// Uploads a CPU field (e.g. a baked AI alpha) as a new texture.
    pub fn upload(gpu: &GpuContext, field: &Field) -> Self {
        let t = Self::new(gpu, field.width as u32, field.height as u32);
        gpu.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &t.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(&field.data),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(field.width as u32 * 4),
                rows_per_image: Some(field.height as u32),
            },
            wgpu::Extent3d {
                width: t.width,
                height: t.height,
                depth_or_array_layers: 1,
            },
        );
        t
    }

    /// Reads the field back to the CPU (tests, and the engine's CPU fallbacks).
    pub fn read(&self, gpu: &GpuContext) -> Field {
        let unpadded = self.width * 4;
        let padded = unpadded.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let staging = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("mask field readback"),
            size: u64::from(padded) * u64::from(self.height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("mask field readback"),
            });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &staging,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(self.height),
                },
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
        gpu.queue.submit(Some(encoder.finish()));
        let slice = staging.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        gpu.device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("device poll failed");
        let raw = slice.get_mapped_range().expect("readback not mapped");
        let mut data = Vec::with_capacity((self.width * self.height) as usize);
        for y in 0..self.height {
            let start = (y * padded) as usize;
            let row: &[f32] = bytemuck::cast_slice(&raw[start..start + unpadded as usize]);
            data.extend_from_slice(row);
        }
        drop(raw);
        staging.unmap();
        Field {
            width: self.width as usize,
            height: self.height as usize,
            data,
        }
    }

    pub fn byte_size(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height) * 4
    }
}

/// One stroke's dabs, binned into [`BRUSH_TILE`]-pixel tiles of its bounding box so a pixel visits
/// only the dabs that can touch its tile.
#[derive(Debug, Clone, PartialEq)]
pub struct BinnedStroke {
    /// Two `vec4`s per dab: `(cx, cy, radius, feather)` then `(flow, 0, 0, 0)`.
    pub dabs: Vec<[f32; 4]>,
    /// `tiles + 1` offsets into `tile_dabs` (CSR layout).
    pub tile_offsets: Vec<u32>,
    pub tile_dabs: Vec<u32>,
    /// Bounding box `(x0, y0, width, height)` in pixels.
    pub bbox: (u32, u32, u32, u32),
    pub tiles_x: u32,
}

/// Bins a stroke's dabs. `None` when no dab touches the frame.
pub fn bin_stroke(dabs: &[Dab], width: usize, height: usize) -> Option<BinnedStroke> {
    let (x0, y0, x1, y1) = raster::stroke_box(dabs, width, height)?;
    let (bw, bh) = (x1 - x0, y1 - y0);
    let tiles_x = bw.div_ceil(BRUSH_TILE);
    let tiles_y = bh.div_ceil(BRUSH_TILE);
    let tile_count = tiles_x * tiles_y;

    // Which tiles each dab touches, using the exact pixel box the CPU splat uses.
    let tile_range = |d: &Dab| -> Option<(usize, usize, usize, usize)> {
        let (lx, ly, hx, hy) = raster::dab_box(d, width, height)?;
        Some((
            (lx - x0) / BRUSH_TILE,
            (ly - y0) / BRUSH_TILE,
            (hx - 1 - x0) / BRUSH_TILE,
            (hy - 1 - y0) / BRUSH_TILE,
        ))
    };
    let mut counts = vec![0u32; tile_count + 1];
    for d in dabs {
        if let Some((tx0, ty0, tx1, ty1)) = tile_range(d) {
            for ty in ty0..=ty1 {
                for tx in tx0..=tx1 {
                    counts[ty * tiles_x + tx + 1] += 1;
                }
            }
        }
    }
    for i in 0..tile_count {
        counts[i + 1] += counts[i];
    }
    let mut cursor = counts.clone();
    let mut tile_dabs = vec![0u32; counts[tile_count] as usize];
    for (i, d) in dabs.iter().enumerate() {
        if let Some((tx0, ty0, tx1, ty1)) = tile_range(d) {
            for ty in ty0..=ty1 {
                for tx in tx0..=tx1 {
                    let slot = &mut cursor[ty * tiles_x + tx];
                    tile_dabs[*slot as usize] = i as u32;
                    *slot += 1;
                }
            }
        }
    }
    let mut packed = Vec::with_capacity(dabs.len() * 2);
    for d in dabs {
        packed.push([d.cx, d.cy, d.radius, d.feather]);
        packed.push([d.flow, 0.0, 0.0, 0.0]);
    }
    Some(BinnedStroke {
        dabs: packed,
        tile_offsets: counts,
        tile_dabs,
        bbox: (x0 as u32, y0 as u32, bw as u32, bh as u32),
        tiles_x: tiles_x as u32,
    })
}

/// The persistent mask pipelines.
pub struct MaskKernels {
    linear: wgpu::ComputePipeline,
    radial: wgpu::ComputePipeline,
    brush: wgpu::ComputePipeline,
    compose: wgpu::ComputePipeline,
    pack: wgpu::ComputePipeline,
    range: wgpu::ComputePipeline,
    /// A 1x1 zero field bound to unused `mask_pack` channels.
    zero: FieldTexture,
}

fn uniform(gpu: &GpuContext, label: &str, data: &[f32]) -> wgpu::Buffer {
    gpu.device
        .create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some(label),
            contents: bytemuck::cast_slice(data),
            usage: wgpu::BufferUsages::UNIFORM,
        })
}

/// A storage buffer that is never zero-sized (wgpu rejects an empty binding).
fn storage<T: bytemuck::Pod>(gpu: &GpuContext, label: &str, data: &[T]) -> wgpu::Buffer {
    let padding = [0u8; 16];
    let bytes: &[u8] = if data.is_empty() {
        &padding
    } else {
        bytemuck::cast_slice(data)
    };
    gpu.device
        .create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some(label),
            contents: bytes,
            usage: wgpu::BufferUsages::STORAGE,
        })
}

fn dispatch(
    encoder: &mut wgpu::CommandEncoder,
    label: &str,
    pipeline: &wgpu::ComputePipeline,
    bind_group: &wgpu::BindGroup,
    width: u32,
    height: u32,
) {
    let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
        label: Some(label),
        timestamp_writes: None,
    });
    pass.set_pipeline(pipeline);
    pass.set_bind_group(0, bind_group, &[]);
    pass.dispatch_workgroups(width.div_ceil(8), height.div_ceil(8), 1);
}

fn entry(binding: u32, resource: wgpu::BindingResource<'_>) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry { binding, resource }
}

impl MaskKernels {
    pub fn new(gpu: &GpuContext) -> Self {
        let d = &gpu.device;
        Self {
            linear: make_compute_pipeline(
                d,
                include_str!("../../shaders/mask_linear.wgsl"),
                "mask_linear",
            ),
            radial: make_compute_pipeline(
                d,
                include_str!("../../shaders/mask_radial.wgsl"),
                "mask_radial",
            ),
            brush: make_compute_pipeline(
                d,
                include_str!("../../shaders/mask_brush.wgsl"),
                "mask_brush",
            ),
            compose: make_compute_pipeline(
                d,
                include_str!("../../shaders/mask_compose.wgsl"),
                "mask_compose",
            ),
            pack: make_compute_pipeline(
                d,
                include_str!("../../shaders/mask_pack.wgsl"),
                "mask_pack",
            ),
            range: make_compute_pipeline(
                d,
                include_str!("../../shaders/mask_range.wgsl"),
                "mask_range",
            ),
            zero: FieldTexture::new(gpu, 1, 1),
        }
    }

    #[allow(clippy::too_many_arguments)]
    /// A luminance- or colour-range selection of `frame` (camera-linear `Rgba16Float`) into `out`.
    /// `source` must be a range source; any other kind is a no-op. `matrix` takes camera RGB to
    /// the working space the range is measured in.
    pub fn range(
        &self,
        gpu: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        frame: &wgpu::TextureView,
        frame_extent: (u32, u32),
        out: &FieldTexture,
        source: &MaskSource,
        matrix: crate::color::Mat3,
    ) {
        let (mode, lo, hi, smooth, tolerance, samples): (f32, f32, f32, f32, f32, &[[f32; 3]]) =
            match source {
                MaskSource::LuminanceRange { lo, hi, smooth } => (0.0, *lo, *hi, *smooth, 0.0, &[]),
                MaskSource::ColorRange { samples, tolerance } => {
                    (1.0, 0.0, 0.0, 0.0, *tolerance, samples.as_slice())
                }
                _ => return,
            };
        let n = samples.len().min(super::params::MAX_COLOR_SAMPLES);
        let mut data: Vec<f32> = vec![
            mode,
            lo,
            hi,
            smooth,
            tolerance,
            n as f32,
            out.width as f32,
            out.height as f32,
            frame_extent.0 as f32,
            frame_extent.1 as f32,
            0.0,
            0.0,
            matrix[0][0],
            matrix[1][0],
            matrix[2][0],
            0.0,
            matrix[0][1],
            matrix[1][1],
            matrix[2][1],
            0.0,
            matrix[0][2],
            matrix[1][2],
            matrix[2][2],
            0.0,
        ];
        for i in 0..super::params::MAX_COLOR_SAMPLES {
            let s = samples.get(i).copied().unwrap_or([0.0; 3]);
            data.extend_from_slice(&[s[0], s[1], s[2], 0.0]);
        }
        let u = uniform(gpu, "mask_range uniforms", &data);
        let bg = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("mask_range"),
            layout: &self.range.get_bind_group_layout(0),
            entries: &[
                entry(0, wgpu::BindingResource::TextureView(frame)),
                entry(1, wgpu::BindingResource::TextureView(&out.view)),
                entry(2, u.as_entire_binding()),
            ],
        });
        dispatch(
            encoder,
            "mask_range",
            &self.range,
            &bg,
            out.width,
            out.height,
        );
    }

    /// Linear gradient into `out`, endpoints in pixels.
    pub fn linear(
        &self,
        gpu: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        out: &FieldTexture,
        p0: (f32, f32),
        p1: (f32, f32),
    ) {
        let u = uniform(
            gpu,
            "mask_linear uniforms",
            &[
                p0.0,
                p0.1,
                p1.0,
                p1.1,
                out.width as f32,
                out.height as f32,
                0.0,
                0.0,
            ],
        );
        let bg = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("mask_linear"),
            layout: &self.linear.get_bind_group_layout(0),
            entries: &[
                entry(0, wgpu::BindingResource::TextureView(&out.view)),
                entry(1, u.as_entire_binding()),
            ],
        });
        dispatch(
            encoder,
            "mask_linear",
            &self.linear,
            &bg,
            out.width,
            out.height,
        );
    }

    /// Radial gradient into `out`; centre/radii/feather in pixels.
    #[allow(clippy::too_many_arguments)]
    pub fn radial(
        &self,
        gpu: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        out: &FieldTexture,
        center: (f32, f32),
        radii: (f32, f32),
        angle_deg: f32,
        feather: f32,
    ) {
        let angle = angle_deg.to_radians();
        let mean = (radii.0 + radii.1) * 0.5;
        let feather_norm = if mean > 0.0 {
            (feather / mean).max(1e-6)
        } else {
            1e-6
        };
        let u = uniform(
            gpu,
            "mask_radial uniforms",
            &[
                center.0,
                center.1,
                radii.0,
                radii.1,
                angle.cos(),
                angle.sin(),
                feather_norm,
                0.0,
                out.width as f32,
                out.height as f32,
                0.0,
                0.0,
            ],
        );
        let bg = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("mask_radial"),
            layout: &self.radial.get_bind_group_layout(0),
            entries: &[
                entry(0, wgpu::BindingResource::TextureView(&out.view)),
                entry(1, u.as_entire_binding()),
            ],
        });
        dispatch(
            encoder,
            "mask_radial",
            &self.radial,
            &bg,
            out.width,
            out.height,
        );
    }

    /// Folds one binned stroke into the brush field: `out = fold(prev, stroke)`. `prev` is copied
    /// into `out` first (so everything outside the stroke's box is carried over), then only the
    /// box is dispatched.
    pub fn brush_stroke(
        &self,
        gpu: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        prev: &FieldTexture,
        out: &FieldTexture,
        stroke: &BinnedStroke,
        erase: bool,
    ) {
        encoder.copy_texture_to_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &prev.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyTextureInfo {
                texture: &out.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::Extent3d {
                width: prev.width,
                height: prev.height,
                depth_or_array_layers: 1,
            },
        );
        let (x0, y0, bw, bh) = stroke.bbox;
        let dabs = storage(gpu, "mask_brush dabs", &stroke.dabs);
        let offsets = storage(gpu, "mask_brush tile offsets", &stroke.tile_offsets);
        let tile_dabs = storage(gpu, "mask_brush tile dabs", &stroke.tile_dabs);
        let u = uniform(
            gpu,
            "mask_brush uniforms",
            &[
                x0 as f32,
                y0 as f32,
                bw as f32,
                bh as f32,
                stroke.tiles_x as f32,
                f32::from(erase),
                0.0,
                0.0,
            ],
        );
        let bg = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("mask_brush"),
            layout: &self.brush.get_bind_group_layout(0),
            entries: &[
                entry(0, wgpu::BindingResource::TextureView(&prev.view)),
                entry(1, wgpu::BindingResource::TextureView(&out.view)),
                entry(2, dabs.as_entire_binding()),
                entry(3, offsets.as_entire_binding()),
                entry(4, tile_dabs.as_entire_binding()),
                entry(5, u.as_entire_binding()),
            ],
        });
        dispatch(encoder, "mask_brush", &self.brush, &bg, bw, bh);
    }

    /// `acc_out = fold_step(acc_in, weight, invert, opacity, op)`, per pixel.
    #[allow(clippy::too_many_arguments)]
    pub fn compose(
        &self,
        gpu: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        acc_in: &FieldTexture,
        weight: &FieldTexture,
        acc_out: &FieldTexture,
        invert: bool,
        opacity: f32,
        op: Op,
    ) {
        let op_code = match op {
            Op::Add => 0.0,
            Op::Subtract => 1.0,
            Op::Intersect => 2.0,
        };
        let u = uniform(
            gpu,
            "mask_compose uniforms",
            &[
                f32::from(invert),
                opacity,
                op_code,
                0.0,
                acc_out.width as f32,
                acc_out.height as f32,
                0.0,
                0.0,
            ],
        );
        let bg = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("mask_compose"),
            layout: &self.compose.get_bind_group_layout(0),
            entries: &[
                entry(0, wgpu::BindingResource::TextureView(&acc_in.view)),
                entry(1, wgpu::BindingResource::TextureView(&weight.view)),
                entry(2, wgpu::BindingResource::TextureView(&acc_out.view)),
                entry(3, u.as_entire_binding()),
            ],
        });
        dispatch(
            encoder,
            "mask_compose",
            &self.compose,
            &bg,
            acc_out.width,
            acc_out.height,
        );
    }

    /// Packs up to four composites into the RGBA channels of `layer` (an `Rgba16Float` 2D view,
    /// write-only storage). Missing channels read as 0.
    pub fn pack(
        &self,
        gpu: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        fields: [Option<&FieldTexture>; 4],
        layer: &wgpu::TextureView,
        width: u32,
        height: u32,
    ) {
        let f = |i: usize| fields[i].map_or(&self.zero.view, |t| &t.view);
        let u = uniform(
            gpu,
            "mask_pack uniforms",
            &[width as f32, height as f32, 0.0, 0.0],
        );
        let bg = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("mask_pack"),
            layout: &self.pack.get_bind_group_layout(0),
            entries: &[
                entry(0, wgpu::BindingResource::TextureView(f(0))),
                entry(1, wgpu::BindingResource::TextureView(f(1))),
                entry(2, wgpu::BindingResource::TextureView(f(2))),
                entry(3, wgpu::BindingResource::TextureView(f(3))),
                entry(4, wgpu::BindingResource::TextureView(layer)),
                entry(5, u.as_entire_binding()),
            ],
        });
        dispatch(encoder, "mask_pack", &self.pack, &bg, width, height);
    }
}

#[cfg(test)]
mod tests {
    use super::super::compose::fold_step;
    use super::super::params::{MaskComponent, MaskGroup, MaskSource, Stroke};
    use super::*;
    use crate::frame::{Extent, FrameTexture};
    use crate::test_util::shared_test_gpu;
    use std::sync::Arc;

    const TOL: f32 = 1e-4;

    fn gpu() -> Option<Arc<GpuContext>> {
        shared_test_gpu()
    }

    fn assert_close(a: &Field, b: &Field, what: &str) {
        assert_eq!((a.width, a.height), (b.width, b.height));
        let worst = a
            .data
            .iter()
            .zip(&b.data)
            .map(|(x, y)| (x - y).abs())
            .fold(0.0f32, f32::max);
        assert!(worst < TOL, "{what}: GPU vs CPU differ by {worst}");
    }

    fn run(gpu: &GpuContext, f: impl FnOnce(&mut wgpu::CommandEncoder)) {
        let mut enc = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        f(&mut enc);
        gpu.queue.submit(Some(enc.finish()));
    }

    #[test]
    fn the_linear_gradient_matches_the_cpu_reference() {
        let Some(gpu) = gpu() else { return };
        let k = MaskKernels::new(&gpu);
        for (w, h) in [(64usize, 48usize), (37, 91)] {
            let out = FieldTexture::new(&gpu, w as u32, h as u32);
            run(&gpu, |e| {
                k.linear(&gpu, e, &out, (5.0, 7.0), (w as f32 - 3.0, h as f32 - 9.0))
            });
            let mut cpu = Field::new(w, h, 0.0);
            for y in 0..h {
                for x in 0..w {
                    cpu.data[y * w + x] = raster::linear_weight(
                        (5.0, 7.0),
                        (w as f32 - 3.0, h as f32 - 9.0),
                        x as f32 + 0.5,
                        y as f32 + 0.5,
                    );
                }
            }
            assert_close(&out.read(&gpu), &cpu, "linear");
        }
    }

    #[test]
    fn a_degenerate_linear_gradient_is_finite_on_the_gpu() {
        let Some(gpu) = gpu() else { return };
        let k = MaskKernels::new(&gpu);
        let out = FieldTexture::new(&gpu, 16, 16);
        run(&gpu, |e| k.linear(&gpu, e, &out, (8.0, 8.0), (8.0, 8.0)));
        assert!(out.read(&gpu).data.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn the_radial_gradient_matches_the_cpu_reference() {
        let Some(gpu) = gpu() else { return };
        let k = MaskKernels::new(&gpu);
        let (w, h) = (80usize, 60usize);
        let (center, radii, angle, feather) = ((40.0, 28.0), (25.0, 12.0), 33.0, 9.0);
        let out = FieldTexture::new(&gpu, w as u32, h as u32);
        run(&gpu, |e| {
            k.radial(&gpu, e, &out, center, radii, angle, feather)
        });
        let mut cpu = Field::new(w, h, 0.0);
        for y in 0..h {
            for x in 0..w {
                cpu.data[y * w + x] = raster::radial_weight(
                    center,
                    radii,
                    angle,
                    feather,
                    x as f32 + 0.5,
                    y as f32 + 0.5,
                );
            }
        }
        assert_close(&out.read(&gpu), &cpu, "radial");
    }

    fn stroke(points: &[[f32; 2]], radius: f32, feather: f32, flow: f32, erase: bool) -> Stroke {
        Stroke {
            points: points.to_vec(),
            radius,
            feather,
            flow,
            erase,
        }
    }

    /// Runs a brush's strokes through the GPU kernel, stroke by stroke, like the engine will.
    fn gpu_brush(
        gpu: &GpuContext,
        k: &MaskKernels,
        strokes: &[Stroke],
        w: usize,
        h: usize,
    ) -> Field {
        let mut a = FieldTexture::new(gpu, w as u32, h as u32);
        let mut b = FieldTexture::new(gpu, w as u32, h as u32);
        for s in strokes {
            let dabs = raster::dabs_for_stroke(s, w, h, raster::MAX_DABS_PER_STROKE);
            let Some(binned) = bin_stroke(&dabs, w, h) else {
                continue;
            };
            run(gpu, |e| k.brush_stroke(gpu, e, &a, &b, &binned, s.erase));
            std::mem::swap(&mut a, &mut b);
        }
        a.read(gpu)
    }

    #[test]
    fn the_brush_matches_the_cpu_reference_including_erase_and_flow() {
        let Some(gpu) = gpu() else { return };
        let k = MaskKernels::new(&gpu);
        // 300x200 so a long stroke spans several 64px tiles.
        let strokes = [
            stroke(
                &[[0.1, 0.3], [0.5, 0.6], [0.9, 0.4]],
                0.04,
                0.02,
                1.0,
                false,
            ),
            stroke(&[[0.3, 0.7]], 0.08, 0.0, 0.5, false),
            stroke(&[[0.4, 0.45], [0.6, 0.55]], 0.02, 0.01, 1.0, true),
        ];
        let cpu = raster::rasterize_brush(&strokes, 300, 200);
        assert!(
            cpu.data.iter().any(|&v| v > 0.9),
            "the reference painted nothing"
        );
        assert_close(&gpu_brush(&gpu, &k, &strokes, 300, 200), &cpu, "brush");
    }

    #[test]
    fn a_brush_stroke_off_the_frame_is_skipped_and_one_at_the_edge_is_clipped() {
        let Some(gpu) = gpu() else { return };
        let k = MaskKernels::new(&gpu);
        let strokes = [
            stroke(&[[3.0, 3.0]], 0.05, 0.0, 1.0, false), // entirely off-frame
            stroke(&[[0.0, 0.0], [1.0, 1.0]], 0.05, 0.01, 1.0, false), // hugs the corners
        ];
        let cpu = raster::rasterize_brush(&strokes, 100, 100);
        assert_close(&gpu_brush(&gpu, &k, &strokes, 100, 100), &cpu, "edge brush");
    }

    #[test]
    fn an_empty_brush_leaves_the_field_zero() {
        let Some(gpu) = gpu() else { return };
        let k = MaskKernels::new(&gpu);
        assert!(gpu_brush(&gpu, &k, &[], 32, 32)
            .data
            .iter()
            .all(|&v| v == 0.0));
    }

    #[test]
    fn binning_assigns_every_touching_dab_to_every_tile_it_touches() {
        let dabs = [
            Dab {
                cx: 10.0,
                cy: 10.0,
                radius: 6.0,
                feather: 2.0,
                flow: 1.0,
            },
            Dab {
                cx: 130.0,
                cy: 70.0,
                radius: 20.0,
                feather: 4.0,
                flow: 1.0,
            },
        ];
        let b = bin_stroke(&dabs, 200, 120).unwrap();
        let tiles = b.tile_offsets.len() - 1;
        assert_eq!(b.tile_offsets[0], 0);
        assert_eq!(*b.tile_offsets.last().unwrap() as usize, b.tile_dabs.len());
        // The big dab straddles a tile boundary: it must be listed in more than one tile.
        let big = b.tile_dabs.iter().filter(|&&i| i == 1).count();
        assert!(big >= 2, "a dab spanning tiles was binned into {big}");
        assert!(tiles >= 2);
        assert!(bin_stroke(&[], 10, 10).is_none());
    }

    fn gpu_compose(
        gpu: &GpuContext,
        k: &MaskKernels,
        comps: &[(Field, bool, f32, Op)],
        w: usize,
        h: usize,
    ) -> Field {
        let mut a = FieldTexture::new(gpu, w as u32, h as u32);
        let mut b = FieldTexture::new(gpu, w as u32, h as u32);
        for (field, invert, opacity, op) in comps {
            let weight = FieldTexture::upload(gpu, field);
            run(gpu, |e| {
                k.compose(gpu, e, &a, &weight, &b, *invert, *opacity, *op)
            });
            std::mem::swap(&mut a, &mut b);
        }
        a.read(gpu)
    }

    #[test]
    fn compose_matches_fold_step_for_every_op_invert_and_opacity() {
        let Some(gpu) = gpu() else { return };
        let k = MaskKernels::new(&gpu);
        let (w, h) = (24usize, 20usize);
        let ramp = |phase: f32| Field {
            width: w,
            height: h,
            data: (0..w * h)
                .map(|i| (((i as f32) * 0.037 + phase).sin() * 0.5 + 0.5).clamp(0.0, 1.0))
                .collect(),
        };
        let comps = vec![
            (ramp(0.0), false, 1.0, Op::Add),
            (ramp(1.3), true, 0.7, Op::Add),
            (ramp(2.1), false, 0.9, Op::Subtract),
            (ramp(0.4), true, 1.0, Op::Intersect),
            (ramp(3.3), false, 0.5, Op::Add),
        ];
        let mut cpu = Field::new(w, h, 0.0);
        for (f, invert, opacity, op) in &comps {
            for (acc, &wt) in cpu.data.iter_mut().zip(&f.data) {
                *acc = fold_step(*acc, wt, *invert, *opacity, *op);
            }
        }
        assert_close(&gpu_compose(&gpu, &k, &comps, w, h), &cpu, "compose");
    }

    #[test]
    fn a_whole_group_composed_on_the_gpu_matches_compose_on_the_cpu() {
        let Some(gpu) = gpu() else { return };
        let k = MaskKernels::new(&gpu);
        let (w, h) = (96usize, 64usize);
        let group = MaskGroup {
            components: vec![
                MaskComponent {
                    source: MaskSource::RadialGradient {
                        center: [0.5, 0.5],
                        radii: [0.3, 0.2],
                        angle_deg: 15.0,
                        feather: 0.1,
                    },
                    ..MaskComponent::default()
                },
                MaskComponent {
                    source: MaskSource::LinearGradient {
                        p0: [0.0, 0.0],
                        p1: [1.0, 0.0],
                    },
                    op: Op::Subtract,
                    opacity: 0.6,
                    ..MaskComponent::default()
                },
            ],
        };
        let cpu =
            super::super::compose::compose(&group, w, h, |s| raster::rasterize_source(s, w, h))
                .unwrap();
        let comps: Vec<_> = group
            .components
            .iter()
            .map(|c| {
                (
                    raster::rasterize_source(&c.source, w, h).unwrap(),
                    c.invert,
                    c.opacity,
                    c.op,
                )
            })
            .collect();
        assert_close(&gpu_compose(&gpu, &k, &comps, w, h), &cpu, "group");
    }

    #[test]
    fn pack_writes_one_composite_per_channel_and_zeroes_the_rest() {
        let Some(gpu) = gpu() else { return };
        let k = MaskKernels::new(&gpu);
        let (w, h) = (20usize, 12usize);
        let field = |v: f32| Field::new(w, h, v);
        let (a, b, c) = (
            FieldTexture::upload(&gpu, &field(0.25)),
            FieldTexture::upload(&gpu, &field(0.5)),
            FieldTexture::upload(&gpu, &field(1.0)),
        );
        let layer = FrameTexture::new(
            &gpu,
            Extent {
                width: w as u32,
                height: h as u32,
            },
        );
        run(&gpu, |e| {
            k.pack(
                &gpu,
                e,
                [Some(&a), Some(&b), Some(&c), None],
                &layer.view,
                w as u32,
                h as u32,
            )
        });
        let px = crate::frame::read_frame(&gpu, &layer);
        for p in px {
            assert!((p[0] - 0.25).abs() < 1e-3);
            assert!((p[1] - 0.5).abs() < 1e-3);
            assert!((p[2] - 1.0).abs() < 1e-3);
            assert_eq!(p[3], 0.0);
        }
    }

    #[test]
    fn field_upload_and_readback_round_trip_across_row_padding() {
        let Some(gpu) = gpu() else { return };
        // 37 is not a multiple of 64 floats, so the padded row stride is exercised.
        let f = Field {
            width: 37,
            height: 5,
            data: (0..37 * 5).map(|i| i as f32 * 0.01).collect(),
        };
        assert_eq!(FieldTexture::upload(&gpu, &f).read(&gpu), f);
    }

    fn range_frame(w: usize, h: usize) -> Vec<[f32; 4]> {
        (0..w * h)
            .map(|i| {
                let (x, y) = ((i % w) as f32 / w as f32, (i / w) as f32 / h as f32);
                [0.02 + 0.8 * x, 0.02 + 0.6 * y, 0.02 + 0.5 * (1.0 - x), 1.0]
            })
            .collect()
    }

    fn assert_range_matches(gpu: &Arc<GpuContext>, source: &MaskSource, what: &str) {
        let k = MaskKernels::new(gpu);
        let (fw, fh, mw, mh) = (64usize, 40usize, 33usize, 21usize);
        let matrix: crate::color::Mat3 = [[1.3, -0.2, -0.1], [-0.1, 1.2, -0.1], [0.0, -0.1, 1.1]];
        let frame = range_frame(fw, fh);
        let tex = crate::test_util::upload_frame(
            gpu,
            Extent {
                width: fw as u32,
                height: fh as u32,
            },
            &frame,
        );
        let out = FieldTexture::new(gpu, mw as u32, mh as u32);
        run(gpu, |e| {
            k.range(
                gpu,
                e,
                &tex.view,
                (fw as u32, fh as u32),
                &out,
                source,
                matrix,
            )
        });
        let cpu = raster::range_field(source, &frame, fw, fh, mw, mh, matrix).unwrap();
        // The frame is stored as f16, which bounds the agreement (and Lab amplifies it).
        let worst = out
            .read(gpu)
            .data
            .iter()
            .zip(&cpu.data)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(worst < 0.03, "{what}: GPU vs CPU range differ by {worst}");
        assert!(
            cpu.data.iter().any(|&v| v > 0.05) && cpu.data.iter().any(|&v| v < 0.95),
            "{what}: the test range selected all or nothing, so it proves little"
        );
    }

    #[test]
    fn the_luminance_range_kernel_matches_the_cpu_reference() {
        let Some(gpu) = gpu() else { return };
        assert_range_matches(
            &gpu,
            &MaskSource::LuminanceRange {
                lo: 0.35,
                hi: 0.6,
                smooth: 0.1,
            },
            "luminance",
        );
    }

    #[test]
    fn the_colour_range_kernel_matches_the_cpu_reference() {
        let Some(gpu) = gpu() else { return };
        let matrix: crate::color::Mat3 = [[1.3, -0.2, -0.1], [-0.1, 1.2, -0.1], [0.0, -0.1, 1.1]];
        // Sample a colour actually present in the frame so the range is neither empty nor total.
        let frame = range_frame(64, 40);
        let p = frame[20 * 64 + 30];
        let lab = raster::working_lab(crate::color::mat3_apply(matrix, [p[0], p[1], p[2]]));
        assert_range_matches(
            &gpu,
            &MaskSource::ColorRange {
                samples: vec![lab],
                tolerance: 25.0,
            },
            "colour",
        );
    }

    #[test]
    fn a_non_range_source_is_a_no_op_on_the_range_kernel() {
        let Some(gpu) = gpu() else { return };
        let k = MaskKernels::new(&gpu);
        let tex = crate::test_util::upload_frame(
            &gpu,
            Extent {
                width: 4,
                height: 4,
            },
            &[[0.5, 0.5, 0.5, 1.0]; 16],
        );
        let out = FieldTexture::new(&gpu, 4, 4);
        run(&gpu, |e| {
            k.range(
                &gpu,
                e,
                &tex.view,
                (4, 4),
                &out,
                &MaskSource::Brush { strokes: vec![] },
                crate::color::mat3_identity(),
            )
        });
        assert!(out.read(&gpu).data.iter().all(|&v| v == 0.0));
    }
}
