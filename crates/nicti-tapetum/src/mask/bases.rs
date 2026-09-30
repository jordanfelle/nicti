//! The cached inputs of the *spatial* local adjustments (#49): clarity, texture and dehaze.
//!
//! These need neighbouring pixels, so they can't be a per-pixel formula in the live shader the way
//! exposure is. Instead the expensive part is precomputed once per (baked frame, extent) and cached
//! ([`crate::mask::engine::MaskEngine`]), and the shader only *applies* it, scaled by the stacked
//! mask weights -- so a slider drag is still a uniform write.
//!
//! - **Bands** (clarity, texture): the baked perceptual luminance `g` is smoothed twice with a
//!   *self-guided* guided filter (edge-preserving, so no halos): a fine radius and a coarse one. The
//!   fine band `g - base_fine` is texture; the mid band `base_fine - base_coarse` is clarity. The
//!   bands are read from the *baked* (as-shot) frame, so a global white-balance or tone drag never
//!   rebuilds them.
//! - **Dehaze** (dark-channel prior, He et al. 2009): estimate an airlight colour `A`, take the
//!   per-window minimum of `min_c(I_c / A_c)` (the dark channel), turn it into a transmission
//!   `t = 1 - omega * dark`, and refine `t` with the guided filter so it follows edges.
//!
//! Everything runs at the *bases extent* (the mask extent capped at 2048 on the long edge): the
//! bands are smooth by construction and the transmission is smoother still, so a lower resolution
//! costs nothing visible and keeps the box filters cheap.

use wgpu::util::DeviceExt;

use super::guided::guided_refine;
use super::local::HAZE_OMEGA;
use super::Field;
use crate::frame::FrameTexture;
use crate::gpu::{make_compute_pipeline, GpuContext};

/// Longest edge the bases are built at.
pub const BASES_LONG_EDGE: u32 = 2048;
/// Fine (texture) smoothing radius as a fraction of the long edge (2048 px -> ~6 px).
pub const FINE_RADIUS_FRACTION: f32 = 0.003;
/// Coarse (clarity) smoothing radius as a fraction of the long edge (2048 px -> ~25 px).
pub const COARSE_RADIUS_FRACTION: f32 = 0.012;
/// Regularizers of the two smoothings, in perceptual-luma variance. Detail whose local variance is
/// well below `eps` is smoothed away (and so lands in the band); stronger structure is an edge and
/// is preserved. Tuned against a step edge and mid-scale detail: fine 1e-3 (std ~0.03) captures
/// real skin/fabric texture; coarse 2e-3 keeps a strong edge (a 0.17 step is variance ~7e-3) from
/// being smoothed into the band and boosted into a halo, while ~0.03-amplitude mid-scale detail
/// (variance ~4e-4) still lands in it. A larger coarse eps was tried first and haloed by 25 %.
pub const FINE_EPS: f32 = 1e-3;
pub const COARSE_EPS: f32 = 2e-3;
/// Dark-channel window radius as a fraction of the long edge.
pub const DARK_RADIUS_FRACTION: f32 = 0.008;
/// Regularizer for refining the transmission.
pub const TRANSMISSION_EPS: f32 = 1e-3;
/// Longest edge of the airlight-estimation thumbnail.
pub const THUMB_LONG_EDGE: u32 = 128;

/// The extent the bases are built at, for a given mask extent.
pub fn bases_extent(mask: (u32, u32)) -> (u32, u32) {
    let long = mask.0.max(mask.1);
    if long <= BASES_LONG_EDGE {
        return mask;
    }
    let s = BASES_LONG_EDGE as f32 / long as f32;
    (
        ((mask.0 as f32 * s).round() as u32).max(1),
        ((mask.1 as f32 * s).round() as u32).max(1),
    )
}

/// The thumbnail extent used to estimate the airlight for a frame of `w x h`.
pub fn thumb_extent(w: u32, h: u32) -> (u32, u32) {
    let long = w.max(h);
    if long <= THUMB_LONG_EDGE {
        return (w.max(1), h.max(1));
    }
    let s = THUMB_LONG_EDGE as f32 / long as f32;
    (
        ((w as f32 * s).round() as u32).max(1),
        ((h as f32 * s).round() as u32).max(1),
    )
}

fn radius(long: usize, fraction: f32) -> usize {
    ((long as f32 * fraction).round() as usize).max(2)
}

pub fn fine_radius(w: usize, h: usize) -> usize {
    radius(w.max(h), FINE_RADIUS_FRACTION)
}
pub fn coarse_radius(w: usize, h: usize) -> usize {
    radius(w.max(h), COARSE_RADIUS_FRACTION)
}
pub fn dark_radius(w: usize, h: usize) -> usize {
    radius(w.max(h), DARK_RADIUS_FRACTION)
}

/// CPU reference for the two detail bands of a luminance field `g`: `(fine, mid)`.
pub fn bands_cpu(g: &Field) -> (Field, Field) {
    let base_fine = guided_refine(g, g, fine_radius(g.width, g.height), FINE_EPS);
    let base_coarse = guided_refine(g, g, coarse_radius(g.width, g.height), COARSE_EPS);
    let sub = |a: &Field, b: &Field| Field {
        width: a.width,
        height: a.height,
        data: a.data.iter().zip(&b.data).map(|(x, y)| x - y).collect(),
    };
    (sub(g, &base_fine), sub(&base_fine, &base_coarse))
}

/// Estimates the airlight from a (small) camera-linear RGBA thumbnail: among the brightest 0.1 %
/// of the dark channel (at least four pixels), the colour of the brightest pixel -- the standard
/// dark-channel-prior recipe. Falls back to the frame's brightest pixel for a degenerate image, and
/// never returns a component below 1e-3 (it is divided by later).
pub fn estimate_airlight(thumb: &[[f32; 4]], w: usize, h: usize) -> [f32; 3] {
    let n = (w * h).min(thumb.len());
    if n == 0 {
        return [1.0; 3];
    }
    let dark = |p: &[f32; 4]| p[0].min(p[1]).min(p[2]).max(0.0);
    let bright = |p: &[f32; 4]| p[0] + p[1] + p[2];
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| dark(&thumb[b]).total_cmp(&dark(&thumb[a])));
    let top = (n / 1000).max(4).min(n);
    let best = order[..top]
        .iter()
        .copied()
        .max_by(|&a, &b| bright(&thumb[a]).total_cmp(&bright(&thumb[b])))
        .unwrap_or(0);
    let p = thumb[best];
    [p[0].max(1e-3), p[1].max(1e-3), p[2].max(1e-3)]
}

/// CPU reference for the dehaze transmission at `ew x eh`: per source tap `min_c(I_c / A_c)`,
/// bilinearly combined, min-filtered over the dark-channel window, converted to
/// `t = clamp(1 - omega * dark)`, then refined with the guided filter against `guide` (the
/// luminance at the same extent).
pub fn transmission_cpu(
    frame: &[[f32; 4]],
    frame_w: usize,
    frame_h: usize,
    airlight: [f32; 3],
    guide: &Field,
) -> Field {
    let (ew, eh) = (guide.width, guide.height);
    let a = airlight.map(|c| c.max(1e-4));
    let taps = Field {
        width: frame_w,
        height: frame_h,
        data: frame
            .iter()
            .map(|p| {
                (p[0].max(0.0) / a[0])
                    .min(p[1].max(0.0) / a[1])
                    .min(p[2].max(0.0) / a[2])
            })
            .collect(),
    };
    let mut cand = Field::new(ew, eh, 0.0);
    for y in 0..eh {
        for x in 0..ew {
            cand.data[y * ew + x] = super::guided::sample_mapped(&taps, x, y, ew, eh);
        }
    }
    let r = dark_radius(ew, eh);
    let mut rows = Field::new(ew, eh, 0.0);
    for y in 0..eh {
        for x in 0..ew {
            let (lo, hi) = (x.saturating_sub(r), (x + r).min(ew - 1));
            rows.data[y * ew + x] = (lo..=hi)
                .map(|i| cand.data[y * ew + i])
                .fold(f32::INFINITY, f32::min);
        }
    }
    let mut t = Field::new(ew, eh, 0.0);
    for y in 0..eh {
        for x in 0..ew {
            let (lo, hi) = (y.saturating_sub(r), (y + r).min(eh - 1));
            let dark = (lo..=hi)
                .map(|i| rows.data[i * ew + x])
                .fold(f32::INFINITY, f32::min);
            t.data[y * ew + x] = (1.0 - HAZE_OMEGA * dark).clamp(0.0, 1.0);
        }
    }
    guided_refine(&t, guide, dark_radius(ew, eh), TRANSMISSION_EPS)
}

/// A float RGBA32 texture that can be read back (the airlight thumbnail).
pub struct ThumbTexture {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    pub width: u32,
    pub height: u32,
}

impl ThumbTexture {
    pub fn new(gpu: &GpuContext, width: u32, height: u32) -> Self {
        let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("dehaze thumbnail"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba32Float,
            usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC,
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

    /// Reads the thumbnail back as row-major RGBA.
    pub fn read(&self, gpu: &GpuContext) -> Vec<[f32; 4]> {
        let unpadded = self.width * 16;
        let padded = unpadded.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let staging = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("thumbnail readback"),
            size: u64::from(padded) * u64::from(self.height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut enc = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        enc.copy_texture_to_buffer(
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
        gpu.queue.submit(Some(enc.finish()));
        let slice = staging.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        gpu.device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("device poll failed");
        let raw = slice.get_mapped_range().expect("thumbnail not mapped");
        let mut out = Vec::with_capacity((self.width * self.height) as usize);
        for y in 0..self.height {
            let start = (y * padded) as usize;
            let row: &[f32] = bytemuck::cast_slice(&raw[start..start + unpadded as usize]);
            for px in row.as_chunks::<4>().0 {
                out.push([px[0], px[1], px[2], px[3]]);
            }
        }
        drop(raw);
        staging.unmap();
        out
    }
}

/// The pipelines for the bases, built once.
pub struct BasesKernels {
    combine: wgpu::ComputePipeline,
    thumb: wgpu::ComputePipeline,
    min0: wgpu::ComputePipeline,
    minpass: wgpu::ComputePipeline,
}

fn uniform(gpu: &GpuContext, data: &[f32]) -> wgpu::Buffer {
    gpu.device
        .create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("bases uniforms"),
            contents: bytemuck::cast_slice(data),
            usage: wgpu::BufferUsages::UNIFORM,
        })
}

fn entry(binding: u32, resource: wgpu::BindingResource<'_>) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry { binding, resource }
}

fn dispatch(
    encoder: &mut wgpu::CommandEncoder,
    label: &str,
    pipeline: &wgpu::ComputePipeline,
    bind_group: &wgpu::BindGroup,
    w: u32,
    h: u32,
) {
    let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
        label: Some(label),
        timestamp_writes: None,
    });
    pass.set_pipeline(pipeline);
    pass.set_bind_group(0, bind_group, &[]);
    pass.dispatch_workgroups(w.div_ceil(8), h.div_ceil(8), 1);
}

impl BasesKernels {
    pub fn new(gpu: &GpuContext) -> Self {
        let d = &gpu.device;
        Self {
            combine: make_compute_pipeline(
                d,
                include_str!("../../shaders/bases_combine.wgsl"),
                "bases_combine",
            ),
            thumb: make_compute_pipeline(
                d,
                include_str!("../../shaders/frame_thumb.wgsl"),
                "frame_thumb",
            ),
            min0: make_compute_pipeline(
                d,
                include_str!("../../shaders/dehaze_min0.wgsl"),
                "dehaze_min0",
            ),
            minpass: make_compute_pipeline(
                d,
                include_str!("../../shaders/dehaze_minpass.wgsl"),
                "dehaze_minpass",
            ),
        }
    }

    /// Packs `(g - fine, fine - coarse, g)` into `out` (an `Rgba16Float` frame at the bases extent).
    pub fn combine(
        &self,
        gpu: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        g: &super::kernels::FieldTexture,
        fine: &super::kernels::FieldTexture,
        coarse: &super::kernels::FieldTexture,
        out: &FrameTexture,
    ) {
        let u = uniform(gpu, &[g.width as f32, g.height as f32, 0.0, 0.0]);
        let bg = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("bases_combine"),
            layout: &self.combine.get_bind_group_layout(0),
            entries: &[
                entry(0, wgpu::BindingResource::TextureView(&g.view)),
                entry(1, wgpu::BindingResource::TextureView(&fine.view)),
                entry(2, wgpu::BindingResource::TextureView(&coarse.view)),
                entry(3, wgpu::BindingResource::TextureView(&out.view)),
                entry(4, u.as_entire_binding()),
            ],
        });
        dispatch(
            encoder,
            "bases_combine",
            &self.combine,
            &bg,
            g.width,
            g.height,
        );
    }

    /// Averages the frame down into `out`.
    pub fn thumb(
        &self,
        gpu: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        frame: &wgpu::TextureView,
        frame_extent: (u32, u32),
        out: &ThumbTexture,
    ) {
        let u = uniform(
            gpu,
            &[
                frame_extent.0 as f32,
                frame_extent.1 as f32,
                out.width as f32,
                out.height as f32,
            ],
        );
        let bg = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("frame_thumb"),
            layout: &self.thumb.get_bind_group_layout(0),
            entries: &[
                entry(0, wgpu::BindingResource::TextureView(frame)),
                entry(1, wgpu::BindingResource::TextureView(&out.view)),
                entry(2, u.as_entire_binding()),
            ],
        });
        dispatch(
            encoder,
            "frame_thumb",
            &self.thumb,
            &bg,
            out.width,
            out.height,
        );
    }

    /// The unrefined transmission at `out`'s extent: candidates, then the two min passes. `scratch`
    /// and `out` are `R32Float` fields of that extent.
    #[allow(clippy::too_many_arguments)]
    pub fn transmission(
        &self,
        gpu: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        frame: &wgpu::TextureView,
        frame_extent: (u32, u32),
        airlight: [f32; 3],
        scratch: &super::kernels::FieldTexture,
        out: &super::kernels::FieldTexture,
    ) {
        let (w, h) = (out.width, out.height);
        let u = uniform(
            gpu,
            &[
                airlight[0],
                airlight[1],
                airlight[2],
                0.0,
                frame_extent.0 as f32,
                frame_extent.1 as f32,
                w as f32,
                h as f32,
            ],
        );
        let bg = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("dehaze_min0"),
            layout: &self.min0.get_bind_group_layout(0),
            entries: &[
                entry(0, wgpu::BindingResource::TextureView(frame)),
                entry(1, wgpu::BindingResource::TextureView(&scratch.view)),
                entry(2, u.as_entire_binding()),
            ],
        });
        dispatch(encoder, "dehaze_min0", &self.min0, &bg, w, h);

        let r = dark_radius(w as usize, h as usize) as f32;
        // Horizontal min: scratch -> out; vertical min + transmission: out -> scratch... then the
        // result must land in `out`, so ping through a second hop is avoided by reading `out` back
        // into `scratch` after the horizontal pass.
        let hpass = uniform(
            gpu,
            &[w as f32, h as f32, r, 0.0, HAZE_OMEGA, 0.0, 0.0, 0.0],
        );
        let bg = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("dehaze_minpass h"),
            layout: &self.minpass.get_bind_group_layout(0),
            entries: &[
                entry(0, wgpu::BindingResource::TextureView(&scratch.view)),
                entry(1, wgpu::BindingResource::TextureView(&out.view)),
                entry(2, hpass.as_entire_binding()),
            ],
        });
        dispatch(encoder, "dehaze_minpass h", &self.minpass, &bg, w, h);
        // Copy `out` (horizontal result) over `scratch` so the vertical pass can write `out`.
        encoder.copy_texture_to_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &out.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyTextureInfo {
                texture: &scratch.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        let vpass = uniform(
            gpu,
            &[w as f32, h as f32, r, 1.0, HAZE_OMEGA, 0.0, 0.0, 0.0],
        );
        let bg = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("dehaze_minpass v"),
            layout: &self.minpass.get_bind_group_layout(0),
            entries: &[
                entry(0, wgpu::BindingResource::TextureView(&scratch.view)),
                entry(1, wgpu::BindingResource::TextureView(&out.view)),
                entry(2, vpass.as_entire_binding()),
            ],
        });
        dispatch(encoder, "dehaze_minpass v", &self.minpass, &bg, w, h);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// One `BasesKernels` for the whole test binary (see `kernels::tests::shared_kernels`).
    pub(crate) fn shared_kernels(gpu: &std::sync::Arc<GpuContext>) -> std::sync::Arc<BasesKernels> {
        static K: std::sync::OnceLock<std::sync::Arc<BasesKernels>> = std::sync::OnceLock::new();
        std::sync::Arc::clone(K.get_or_init(|| std::sync::Arc::new(BasesKernels::new(gpu))))
    }

    #[test]
    fn the_bases_extent_caps_the_long_edge() {
        assert_eq!(bases_extent((1000, 700)), (1000, 700));
        assert_eq!(bases_extent((4096, 2731)), (2048, 1366));
        assert_eq!(bases_extent((2731, 4096)), (1366, 2048));
        assert_eq!(thumb_extent(8000, 4000), (128, 64));
        assert_eq!(thumb_extent(10, 10), (10, 10));
    }

    #[test]
    fn radii_scale_with_the_frame_and_have_a_floor() {
        assert!(fine_radius(2048, 1365) < coarse_radius(2048, 1365));
        assert_eq!(fine_radius(64, 64), 2);
        assert!(dark_radius(2048, 1365) >= 2);
    }

    /// A flat image has no detail: both bands are zero everywhere.
    #[test]
    fn a_flat_image_has_no_bands() {
        let g = Field::new(80, 60, 0.4);
        let (fine, mid) = bands_cpu(&g);
        assert!(fine.data.iter().all(|v| v.abs() < 1e-4));
        assert!(mid.data.iter().all(|v| v.abs() < 1e-4));
    }

    /// Fine speckle lands in the fine band; a broad gradient does not.
    #[test]
    fn fine_detail_is_the_fine_band_and_broad_shading_is_neither() {
        let (w, h) = (128usize, 96usize);
        let speckle = Field {
            width: w,
            height: h,
            data: (0..w * h)
                .map(|i| {
                    let (x, y) = (i % w, i / w);
                    0.5 + if (x + y) % 2 == 0 { 0.02 } else { -0.02 }
                })
                .collect(),
        };
        let (fine, _) = bands_cpu(&speckle);
        let energy = |f: &Field| f.data.iter().map(|v| v * v).sum::<f32>() / f.data.len() as f32;
        assert!(
            energy(&fine) > 2e-4,
            "speckle must show in the fine band: {}",
            energy(&fine)
        );

        let ramp = Field {
            width: w,
            height: h,
            data: (0..w * h)
                .map(|i| 0.2 + 0.6 * (i % w) as f32 / w as f32)
                .collect(),
        };
        let (fine, mid) = bands_cpu(&ramp);
        assert!(energy(&fine) < 1e-5, "{}", energy(&fine));
        assert!(energy(&mid) < 1e-5, "{}", energy(&mid));
    }

    /// The bands are edge-preserving: a hard step must not leave a large ringing band beside it
    /// (a plain Gaussian split would).
    #[test]
    fn a_step_edge_does_not_ring_in_the_bands() {
        let (w, h) = (128usize, 32usize);
        let step = Field {
            width: w,
            height: h,
            data: (0..w * h)
                .map(|i| if i % w < w / 2 { 0.2 } else { 0.8 })
                .collect(),
        };
        let (fine, mid) = bands_cpu(&step);
        // Well away from the edge (> 2x the coarse radius) the bands are ~0, and next to it they
        // stay far below the step height.
        let r = coarse_radius(w, h);
        for x in 0..w {
            let dist = (x as i32 - (w / 2) as i32).unsigned_abs() as usize;
            let (f, m) = (fine.data[16 * w + x], mid.data[16 * w + x]);
            if dist > 2 * r {
                assert!(f.abs() < 1e-3 && m.abs() < 1e-3, "x={x}: {f} {m}");
            }
            assert!(f.abs() < 0.2 && m.abs() < 0.2, "x={x} rings: {f} {m}");
        }
    }

    fn thumb_of(pixels: &[[f32; 3]]) -> Vec<[f32; 4]> {
        pixels.iter().map(|p| [p[0], p[1], p[2], 1.0]).collect()
    }

    #[test]
    fn the_airlight_is_the_brightest_of_the_haziest_pixels() {
        // Mostly dark scene, plus a hazy patch (high dark channel) whose brightest pixel is the sky.
        let mut px = vec![[0.05, 0.04, 0.03]; 400];
        px[10] = [0.70, 0.72, 0.75]; // dense haze, lower
        px[11] = [0.82, 0.84, 0.88]; // the airlight
        px[12] = [0.95, 0.30, 0.02]; // a saturated bright red: high channel, but its dark channel is tiny
        let a = estimate_airlight(&thumb_of(&px), 20, 20);
        assert_eq!(a, [0.82, 0.84, 0.88]);
    }

    #[test]
    fn the_airlight_is_never_zero_and_handles_empty_input() {
        assert_eq!(estimate_airlight(&[], 0, 0), [1.0; 3]);
        let black = thumb_of(&[[0.0; 3]; 16]);
        let a = estimate_airlight(&black, 4, 4);
        assert!(a.iter().all(|&c| c >= 1e-3));
    }

    /// Hazy = J * t + A * (1 - t) with a uniform t: the estimated transmission must recover ~t in
    /// textured regions that contain dark pixels (the prior's own assumption).
    #[test]
    fn the_transmission_of_a_uniformly_hazy_image_is_recovered() {
        let (w, h) = (96usize, 72usize);
        let a = [0.85f32, 0.86, 0.9];
        let t_true = 0.55f32;
        // A scene with many near-black pixels in every window (shadows, dark foliage).
        let scene = |i: usize| -> [f32; 3] {
            let (x, y) = (i % w, i / w);
            if (x * 7 + y * 13) % 9 == 0 {
                [0.01, 0.01, 0.01]
            } else {
                [
                    0.2 + 0.5 * (x as f32 / w as f32),
                    0.3,
                    0.2 + 0.4 * (y as f32 / h as f32),
                ]
            }
        };
        let frame: Vec<[f32; 4]> = (0..w * h)
            .map(|i| {
                let j = scene(i);
                [
                    j[0] * t_true + a[0] * (1.0 - t_true),
                    j[1] * t_true + a[1] * (1.0 - t_true),
                    j[2] * t_true + a[2] * (1.0 - t_true),
                    1.0,
                ]
            })
            .collect();
        let guide = Field {
            width: w,
            height: h,
            data: frame
                .iter()
                .map(|p| super::super::guided::guide_luminance([p[0], p[1], p[2]]))
                .collect(),
        };
        let t = transmission_cpu(&frame, w, h, a, &guide);
        let mean = t.data.iter().sum::<f32>() / t.data.len() as f32;
        // omega < 1 keeps a little haze, so the estimate sits a touch above the truth.
        assert!(
            (mean - t_true).abs() < 0.12,
            "mean transmission {mean} vs true {t_true}"
        );
    }

    #[test]
    fn a_haze_free_image_has_transmission_near_one_when_the_airlight_is_far_from_its_pixels() {
        let (w, h) = (48usize, 48usize);
        // Saturated blue: its red channel is ~0, so the dark channel is ~0 and t ~ 1.
        let frame = vec![[0.02, 0.05, 0.6, 1.0]; w * h];
        let guide = Field::new(w, h, 0.5);
        let t = transmission_cpu(&frame, w, h, [0.9, 0.9, 0.9], &guide);
        assert!(t.data.iter().all(|&v| v > 0.9), "{}", t.data[0]);
    }
}
