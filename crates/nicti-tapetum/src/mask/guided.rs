//! Edge-aware refinement of a low-resolution alpha (#49, ADR-0048): the fast guided filter of He &
//! Sun ("Fast Guided Filter", 2015), guided by the photo's own luminance.
//!
//! A segmentation model returns its alpha at model resolution (~1024 px). Bilinearly upsampling it
//! to the mask extent gives a soft halo that ignores the image; the guided filter instead fits, per
//! low-res window, a linear map `alpha ~= a * luminance + b` and applies it to the *full-res*
//! luminance, so the mask edge snaps to real edges in the photo. The expensive part (statistics,
//! box filters) runs at the alpha's resolution, and only a bilinear upsample of `(a, b)` plus one
//! multiply-add runs at full resolution.
//!
//! This file holds the CPU reference (`guided_refine`) and the guide-luminance helper; the GPU
//! twin is [`GuidedKernels`], checked against it. Both follow the same rules, deliberately:
//! - bilinear sampling maps a target pixel centre `(x + 0.5) / W * w - 0.5` into the source and
//!   clamps to its edge;
//! - box filters average over the *valid* pixels only (no dark border);
//! - the guide is perceptual luminance, `clamp(Y, 0, 1) ^ (1 / 2.2)` from the frame's RGB.

use wgpu::util::DeviceExt;

use super::kernels::FieldTexture;
use super::Field;
use crate::gpu::{make_compute_pipeline, GpuContext};

/// Guided-filter window radius as a fraction of the alpha's long edge (1024 px -> 8 px).
pub const RADIUS_FRACTION: f32 = 1.0 / 128.0;
/// Smallest window radius in low-res pixels.
pub const MIN_RADIUS: usize = 2;
/// Regularizer: how much luminance variance counts as an edge worth following. Guide is 0..1.
pub const EPS: f32 = 2e-3;

/// The window radius (low-res pixels) for an alpha of `width x height`.
pub fn radius_for(width: usize, height: usize) -> usize {
    ((width.max(height) as f32 * RADIUS_FRACTION).round() as usize).max(MIN_RADIUS)
}

/// Perceptual luminance of a linear RGB pixel, the guide the refine follows.
pub fn guide_luminance(rgb: [f32; 3]) -> f32 {
    let y = 0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2];
    y.clamp(0.0, 1.0).powf(1.0 / 2.2)
}

/// Bilinear sample of `field` at the source position that target pixel `(x, y)` of a
/// `target_w x target_h` grid maps to, clamped to the edge.
pub fn sample_mapped(field: &Field, x: usize, y: usize, target_w: usize, target_h: usize) -> f32 {
    let fx = (x as f32 + 0.5) / target_w as f32 * field.width as f32 - 0.5;
    let fy = (y as f32 + 0.5) / target_h as f32 * field.height as f32 - 0.5;
    let (x0, y0) = (fx.floor(), fy.floor());
    let (tx, ty) = (fx - x0, fy - y0);
    let at = |ix: f32, iy: f32| {
        let cx = (ix.max(0.0) as usize).min(field.width - 1);
        let cy = (iy.max(0.0) as usize).min(field.height - 1);
        field.data[cy * field.width + cx]
    };
    let top = at(x0, y0) * (1.0 - tx) + at(x0 + 1.0, y0) * tx;
    let bottom = at(x0, y0 + 1.0) * (1.0 - tx) + at(x0 + 1.0, y0 + 1.0) * tx;
    top * (1.0 - ty) + bottom * ty
}

/// Guide luminance at `mask_w x mask_h` from an RGBA frame (`frame_w x frame_h`, row-major),
/// bilinear-sampled at each mask pixel's centre.
pub fn guide_from_frame(
    frame: &[[f32; 4]],
    frame_w: usize,
    frame_h: usize,
    mask_w: usize,
    mask_h: usize,
) -> Field {
    let luma = Field {
        width: frame_w,
        height: frame_h,
        data: frame
            .iter()
            .map(|p| guide_luminance([p[0], p[1], p[2]]))
            .collect(),
    };
    // Sampling luminance after mapping (rather than luminance of a mapped colour) keeps the CPU and
    // GPU twins identical: the GPU kernel also converts each source texel first.
    let mut out = Field::new(mask_w, mask_h, 0.0);
    for y in 0..mask_h {
        for x in 0..mask_w {
            out.data[y * mask_w + x] = sample_mapped(&luma, x, y, mask_w, mask_h);
        }
    }
    out
}

/// Mean over the `(2r+1)^2` window clipped to the image, per pixel -- a separable box filter with
/// valid-pixel normalization, on `channels` interleaved values.
fn box_mean(data: &[f32], w: usize, h: usize, channels: usize, r: usize) -> Vec<f32> {
    let pass = |src: &[f32], horizontal: bool| -> Vec<f32> {
        let mut out = vec![0.0f32; src.len()];
        for y in 0..h {
            for x in 0..w {
                let (lo, hi, at) = if horizontal {
                    (x.saturating_sub(r), (x + r).min(w - 1), x)
                } else {
                    (y.saturating_sub(r), (y + r).min(h - 1), y)
                };
                let _ = at;
                for c in 0..channels {
                    let mut sum = 0.0f32;
                    for i in lo..=hi {
                        let idx = if horizontal { y * w + i } else { i * w + x };
                        sum += src[idx * channels + c];
                    }
                    out[(y * w + x) * channels + c] = sum / (hi - lo + 1) as f32;
                }
            }
        }
        out
    };
    pass(&pass(data, true), false)
}

/// Refines `alpha` (low-res) to the guide's extent, following its edges. `guide` is the photo's
/// luminance at the output extent; the statistics run at `alpha`'s resolution.
pub fn guided_refine(alpha: &Field, guide: &Field, radius: usize, eps: f32) -> Field {
    let (lw, lh) = (alpha.width, alpha.height);
    // Guide at alpha resolution.
    let mut stats = vec![0.0f32; lw * lh * 4];
    for y in 0..lh {
        for x in 0..lw {
            let i = sample_mapped(guide, x, y, lw, lh);
            let p = alpha.data[y * lw + x];
            let o = (y * lw + x) * 4;
            stats[o] = i;
            stats[o + 1] = p;
            stats[o + 2] = i * i;
            stats[o + 3] = i * p;
        }
    }
    let mean = box_mean(&stats, lw, lh, 4, radius);
    let mut ab = vec![0.0f32; lw * lh * 2];
    for px in 0..lw * lh {
        let (mi, mp, mii, mip) = (
            mean[px * 4],
            mean[px * 4 + 1],
            mean[px * 4 + 2],
            mean[px * 4 + 3],
        );
        let var = mii - mi * mi;
        let cov = mip - mi * mp;
        let a = cov / (var + eps);
        ab[px * 2] = a;
        ab[px * 2 + 1] = mp - a * mi;
    }
    let mab = box_mean(&ab, lw, lh, 2, radius);
    let a_field = Field {
        width: lw,
        height: lh,
        data: (0..lw * lh).map(|i| mab[i * 2]).collect(),
    };
    let b_field = Field {
        width: lw,
        height: lh,
        data: (0..lw * lh).map(|i| mab[i * 2 + 1]).collect(),
    };
    let mut out = Field::new(guide.width, guide.height, 0.0);
    for y in 0..guide.height {
        for x in 0..guide.width {
            let a = sample_mapped(&a_field, x, y, guide.width, guide.height);
            let b = sample_mapped(&b_field, x, y, guide.width, guide.height);
            out.data[y * guide.width + x] =
                (a * guide.data[y * guide.width + x] + b).clamp(0.0, 1.0);
        }
    }
    out
}

/// Plain bilinear upsample of `alpha` to `w x h` -- the baseline the guided refine must beat.
pub fn bilinear_upsample(alpha: &Field, w: usize, h: usize) -> Field {
    let mut out = Field::new(w, h, 0.0);
    for y in 0..h {
        for x in 0..w {
            out.data[y * w + x] = sample_mapped(alpha, x, y, w, h);
        }
    }
    out
}

/// A float RGBA texture used for the filter's intermediate statistics (`Rgba32Float`: variance
/// `E[I^2] - E[I]^2` cancels badly in half precision).
struct StatTexture {
    view: wgpu::TextureView,
    texture: wgpu::Texture,
}

impl StatTexture {
    fn new(gpu: &GpuContext, w: u32, h: u32) -> Self {
        let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("guided filter stats"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba32Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::STORAGE_BINDING,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self { view, texture }
    }
}

/// The guided-filter pipelines, built once.
pub struct GuidedKernels {
    guide_luma: wgpu::ComputePipeline,
    prep: wgpu::ComputePipeline,
    box_pass: wgpu::ComputePipeline,
    coeffs: wgpu::ComputePipeline,
    apply: wgpu::ComputePipeline,
}

fn uniform(gpu: &GpuContext, data: &[f32]) -> wgpu::Buffer {
    gpu.device
        .create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("guided uniforms"),
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

impl GuidedKernels {
    pub fn new(gpu: &GpuContext) -> Self {
        let d = &gpu.device;
        Self {
            guide_luma: make_compute_pipeline(
                d,
                include_str!("../../shaders/guide_luma.wgsl"),
                "guide_luma",
            ),
            prep: make_compute_pipeline(
                d,
                include_str!("../../shaders/guided_prep.wgsl"),
                "guided_prep",
            ),
            box_pass: make_compute_pipeline(
                d,
                include_str!("../../shaders/guided_box.wgsl"),
                "guided_box",
            ),
            coeffs: make_compute_pipeline(
                d,
                include_str!("../../shaders/guided_coeffs.wgsl"),
                "guided_coeffs",
            ),
            apply: make_compute_pipeline(
                d,
                include_str!("../../shaders/guided_apply.wgsl"),
                "guided_apply",
            ),
        }
    }

    /// Writes the guide luminance of `frame` (an `Rgba16Float` frame view of `frame_w x frame_h`)
    /// into `out`, resampled to `out`'s extent.
    pub fn guide_luma(
        &self,
        gpu: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        frame: &wgpu::TextureView,
        frame_extent: (u32, u32),
        out: &FieldTexture,
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
            label: Some("guide_luma"),
            layout: &self.guide_luma.get_bind_group_layout(0),
            entries: &[
                entry(0, wgpu::BindingResource::TextureView(frame)),
                entry(1, wgpu::BindingResource::TextureView(&out.view)),
                entry(2, u.as_entire_binding()),
            ],
        });
        dispatch(
            encoder,
            "guide_luma",
            &self.guide_luma,
            &bg,
            out.width,
            out.height,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn box_filter(
        &self,
        gpu: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        src: &StatTexture,
        dst: &StatTexture,
        scratch: &StatTexture,
        size: (u32, u32),
        radius: u32,
    ) {
        for (from, to, dir) in [(src, scratch, 0.0f32), (scratch, dst, 1.0f32)] {
            let u = uniform(gpu, &[size.0 as f32, size.1 as f32, radius as f32, dir]);
            let bg = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("guided_box"),
                layout: &self.box_pass.get_bind_group_layout(0),
                entries: &[
                    entry(0, wgpu::BindingResource::TextureView(&from.view)),
                    entry(1, wgpu::BindingResource::TextureView(&to.view)),
                    entry(2, u.as_entire_binding()),
                ],
            });
            dispatch(encoder, "guided_box", &self.box_pass, &bg, size.0, size.1);
        }
    }

    #[allow(clippy::too_many_arguments)]
    /// Refines `alpha` (low-res `R32Float`) into `out` (mask extent), following `guide` (the guide
    /// luminance at `out`'s extent, from [`Self::guide_luma`]).
    pub fn refine(
        &self,
        gpu: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        alpha: &FieldTexture,
        guide: &FieldTexture,
        out: &FieldTexture,
        radius: usize,
        eps: f32,
    ) {
        let size = (alpha.width, alpha.height);
        let stats = StatTexture::new(gpu, size.0, size.1);
        let means = StatTexture::new(gpu, size.0, size.1);
        let scratch = StatTexture::new(gpu, size.0, size.1);
        let coeffs = StatTexture::new(gpu, size.0, size.1);
        let coeff_means = StatTexture::new(gpu, size.0, size.1);

        let u = uniform(
            gpu,
            &[
                guide.width as f32,
                guide.height as f32,
                size.0 as f32,
                size.1 as f32,
            ],
        );
        let bg = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("guided_prep"),
            layout: &self.prep.get_bind_group_layout(0),
            entries: &[
                entry(0, wgpu::BindingResource::TextureView(&guide.view)),
                entry(1, wgpu::BindingResource::TextureView(&alpha.view)),
                entry(2, wgpu::BindingResource::TextureView(&stats.view)),
                entry(3, u.as_entire_binding()),
            ],
        });
        dispatch(encoder, "guided_prep", &self.prep, &bg, size.0, size.1);

        self.box_filter(gpu, encoder, &stats, &means, &scratch, size, radius as u32);

        let u = uniform(gpu, &[size.0 as f32, size.1 as f32, eps, 0.0]);
        let bg = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("guided_coeffs"),
            layout: &self.coeffs.get_bind_group_layout(0),
            entries: &[
                entry(0, wgpu::BindingResource::TextureView(&means.view)),
                entry(1, wgpu::BindingResource::TextureView(&coeffs.view)),
                entry(2, u.as_entire_binding()),
            ],
        });
        dispatch(encoder, "guided_coeffs", &self.coeffs, &bg, size.0, size.1);

        self.box_filter(
            gpu,
            encoder,
            &coeffs,
            &coeff_means,
            &scratch,
            size,
            radius as u32,
        );

        let u = uniform(
            gpu,
            &[
                out.width as f32,
                out.height as f32,
                size.0 as f32,
                size.1 as f32,
            ],
        );
        let bg = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("guided_apply"),
            layout: &self.apply.get_bind_group_layout(0),
            entries: &[
                entry(0, wgpu::BindingResource::TextureView(&guide.view)),
                entry(1, wgpu::BindingResource::TextureView(&coeff_means.view)),
                entry(2, wgpu::BindingResource::TextureView(&out.view)),
                entry(3, u.as_entire_binding()),
            ],
        });
        dispatch(
            encoder,
            "guided_apply",
            &self.apply,
            &bg,
            out.width,
            out.height,
        );
        // The stat textures are dropped at the end of this call; wgpu keeps them alive until the
        // submitted work completes.
        let _ = (&stats.texture, &means.texture, &scratch.texture);
        let _ = (&coeffs.texture, &coeff_means.texture);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Extent;
    use crate::test_util::{shared_test_gpu, upload_frame};

    /// A guide with a sharp vertical step at `edge_x`, at `w x h`.
    fn step_guide(w: usize, h: usize, edge_x: usize) -> Field {
        Field {
            width: w,
            height: h,
            data: (0..w * h)
                .map(|i| if i % w < edge_x { 0.15 } else { 0.85 })
                .collect(),
        }
    }

    #[test]
    fn the_radius_scales_with_the_alpha_and_has_a_floor() {
        assert_eq!(radius_for(1024, 768), 8);
        assert_eq!(radius_for(16, 16), MIN_RADIUS);
    }

    #[test]
    fn guide_luminance_is_monotone_clamped_and_finite() {
        assert_eq!(guide_luminance([0.0; 3]), 0.0);
        assert_eq!(guide_luminance([5.0; 3]), 1.0);
        assert_eq!(guide_luminance([-1.0; 3]), 0.0);
        assert!(guide_luminance([0.2; 3]) < guide_luminance([0.4; 3]));
    }

    #[test]
    fn a_constant_alpha_stays_constant() {
        let guide = step_guide(64, 48, 30);
        let alpha = Field::new(16, 12, 0.6);
        let out = guided_refine(&alpha, &guide, 2, EPS);
        assert!(out.data.iter().all(|&v| (v - 0.6).abs() < 1e-4));
    }

    /// The point of the filter: a coarse alpha whose edge is misplaced by a low-res pixel snaps to
    /// the guide's real edge, where a plain bilinear upsample leaves it soft and misplaced.
    #[test]
    fn guided_refine_snaps_a_soft_edge_to_the_guides_real_edge() {
        let (w, h) = (128usize, 32usize);
        let edge = 70usize;
        let truth = Field {
            width: w,
            height: h,
            data: (0..w * h)
                .map(|i| if i % w < edge { 0.0 } else { 1.0 })
                .collect(),
        };
        let guide = step_guide(w, h, edge);
        // A 32x8 alpha of the same edge: each low-res pixel covers 4 full-res pixels, so the edge
        // at 70 falls mid-pixel and the alpha is blurred across it.
        let low = {
            let (lw, lh) = (32usize, 8usize);
            let mut f = Field::new(lw, lh, 0.0);
            for y in 0..lh {
                for x in 0..lw {
                    let covered = (x * 4..x * 4 + 4).filter(|&px| px >= edge).count();
                    f.data[y * lw + x] = covered as f32 / 4.0;
                }
            }
            f
        };
        let error = |f: &Field| -> f32 {
            f.data
                .iter()
                .zip(&truth.data)
                .map(|(a, b)| (a - b).abs())
                .sum::<f32>()
                / f.data.len() as f32
        };
        let bilinear = bilinear_upsample(&low, w, h);
        let refined = guided_refine(&low, &guide, 2, EPS);
        assert!(
            error(&refined) < error(&bilinear) * 0.6,
            "refined {} should clearly beat bilinear {}",
            error(&refined),
            error(&bilinear)
        );
        // And the refined edge is actually sharp: within two pixels of the guide's step it has
        // already reached (nearly) the extreme values.
        let row = |x: usize| refined.data[16 * w + x];
        assert!(row(edge - 3) < 0.1, "{}", row(edge - 3));
        assert!(row(edge + 3) > 0.9, "{}", row(edge + 3));
    }

    #[test]
    fn a_flat_guide_gives_the_plain_smooth_upsample_not_garbage() {
        let guide = Field::new(64, 64, 0.5);
        let mut low = Field::new(16, 16, 0.0);
        for y in 0..16 {
            for x in 8..16 {
                low.data[y * 16 + x] = 1.0;
            }
        }
        let out = guided_refine(&low, &guide, 2, EPS);
        assert!(out
            .data
            .iter()
            .all(|v| v.is_finite() && (0.0..=1.0).contains(v)));
        // Far from the transition it is unchanged.
        assert!(out.data[32 * 64 + 2] < 0.05);
        assert!(out.data[32 * 64 + 61] > 0.95);
    }

    fn textured_guide(w: usize, h: usize) -> Field {
        Field {
            width: w,
            height: h,
            data: (0..w * h)
                .map(|i| {
                    let (x, y) = ((i % w) as f32, (i / w) as f32);
                    (0.5 + 0.3 * (x * 0.13).sin() * (y * 0.09).cos()
                        + if x > w as f32 * 0.55 { 0.15 } else { 0.0 })
                    .clamp(0.0, 1.0)
                })
                .collect(),
        }
    }

    fn textured_alpha(w: usize, h: usize) -> Field {
        Field {
            width: w,
            height: h,
            data: (0..w * h)
                .map(|i| {
                    let x = (i % w) as f32 / w as f32;
                    let y = (i / w) as f32 / h as f32;
                    ((x - 0.55) * 9.0 + (y - 0.5) * 2.0).clamp(0.0, 1.0)
                })
                .collect(),
        }
    }

    fn run(gpu: &GpuContext, f: impl FnOnce(&mut wgpu::CommandEncoder)) {
        let mut enc = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        f(&mut enc);
        gpu.queue.submit(Some(enc.finish()));
    }

    #[test]
    fn the_gpu_refine_matches_the_cpu_reference() {
        let Some(gpu) = shared_test_gpu() else { return };
        let k = GuidedKernels::new(&gpu);
        // Odd, non-multiple-of-8 extents so bounds handling is exercised.
        let (w, h, lw, lh) = (173usize, 111usize, 41usize, 27usize);
        let guide = textured_guide(w, h);
        let alpha = textured_alpha(lw, lh);
        let cpu = guided_refine(&alpha, &guide, 3, EPS);

        let guide_tex = FieldTexture::upload(&gpu, &guide);
        let alpha_tex = FieldTexture::upload(&gpu, &alpha);
        let out = FieldTexture::new(&gpu, w as u32, h as u32);
        run(&gpu, |e| {
            k.refine(&gpu, e, &alpha_tex, &guide_tex, &out, 3, EPS)
        });
        let got = out.read(&gpu);
        let worst = got
            .data
            .iter()
            .zip(&cpu.data)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(worst < 2e-3, "GPU vs CPU guided refine differ by {worst}");
    }

    #[test]
    fn the_gpu_guide_luma_matches_the_cpu_and_resamples() {
        let Some(gpu) = shared_test_gpu() else { return };
        let k = GuidedKernels::new(&gpu);
        let (fw, fh, mw, mh) = (90usize, 50usize, 47usize, 26usize);
        let frame: Vec<[f32; 4]> = (0..fw * fh)
            .map(|i| {
                let (x, y) = ((i % fw) as f32 / fw as f32, (i / fw) as f32 / fh as f32);
                [x, y, (x + y) * 0.5, 1.0]
            })
            .collect();
        let tex = upload_frame(
            &gpu,
            Extent {
                width: fw as u32,
                height: fh as u32,
            },
            &frame,
        );
        let out = FieldTexture::new(&gpu, mw as u32, mh as u32);
        run(&gpu, |e| {
            k.guide_luma(&gpu, e, &tex.view, (fw as u32, fh as u32), &out)
        });
        let cpu = guide_from_frame(&frame, fw, fh, mw, mh);
        let got = out.read(&gpu);
        let worst = got
            .data
            .iter()
            .zip(&cpu.data)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        // f16 frame storage bounds the agreement.
        assert!(worst < 5e-3, "GPU vs CPU guide luma differ by {worst}");
    }
}
