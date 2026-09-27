//! GPU resize candidate: a separable Lanczos3 compute pass over a linear-light `vec4<f32>`
//! storage buffer (RGBA, alpha unused/zero) -- `shaders/lanczos_resize.wgsl`. Weight computation
//! (`compute_taps`) happens on the CPU; only the weighted-sum application runs on the GPU. This
//! module is adapted from `spikes/loaf`'s wgpu setup pattern (`GpuContext`, `workgroup_grid`,
//! persistent pipelines) -- copied, not depended on, since spikes don't depend on each other.
//!
//! **Correctness-tested only in this sandbox** (WSL sees no NVIDIA Vulkan ICD, so `wgpu` falls
//! back to `llvmpipe` software rendering here -- confirmed via `crouch`'s own adapter-enumeration
//! log, same constraint `.claude/rules/gpu-gui-and-healing/REFERENCE.md` already documents for
//! this environment). Real RTX 5080 timing needs the cross-compile-to-Windows-and-run-via-WSL-
//! interop path that ADR-0044/0054 used -- **not reached this pass**, see the ADR's "what wasn't
//! reachable" section and the follow-up issue.

use std::borrow::Cow;

use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;

use crate::resize::{srgb_eotf, srgb_oetf};

pub const WGSL: &str = include_str!("../shaders/lanczos_resize.wgsl");
const WORKGROUP_SIZE: u32 = 64;
/// Must match `shaders/lanczos_resize.wgsl`'s own `MAX_TAPS` constant and `array<_, 32>` sizes --
/// there is no shared source of truth between WGSL and Rust here, so the two are kept in sync by
/// hand. Sized for interior tap count `~= 6*scale + 1` at a downscale ratio (`src_len/dst_len`)
/// up to ~15x on one axis -- verified empirically after the original value of 32 (~5.3x) turned
/// out to fail on this repo's own ordinary export presets (a 45MP 8280px-wide source down to a
/// 1024-1536px long edge is an 5.4-8.1x ratio, adversarial review caught `compute_taps` silently
/// erroring on exactly these ordinary sizes, not just extreme ones).
const MAX_TAPS: usize = 96;
/// wgpu's per-dimension dispatch limit (same constant/citation as `spikes/loaf`/`spikes/crouch`).
const MAX_WORKGROUPS_PER_DIM: u32 = 65535;

fn workgroup_grid(total_workgroups: u32) -> (u32, u32) {
    if total_workgroups <= MAX_WORKGROUPS_PER_DIM {
        (total_workgroups.max(1), 1)
    } else {
        let x = MAX_WORKGROUPS_PER_DIM;
        let y = total_workgroups.div_ceil(x);
        (x, y)
    }
}

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct TapSet {
    count: u32,
    idx: [i32; MAX_TAPS],
    weight: [f32; MAX_TAPS],
}

impl Default for TapSet {
    fn default() -> Self {
        TapSet {
            count: 0,
            idx: [0; MAX_TAPS],
            weight: [0.0; MAX_TAPS],
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct Dims {
    src_width: u32,
    src_height: u32,
    dst_width: u32,
    dst_height: u32,
}

fn lanczos3(x: f64) -> f64 {
    if x == 0.0 {
        return 1.0;
    }
    if x.abs() >= 3.0 {
        return 0.0;
    }
    let px = std::f64::consts::PI * x;
    3.0 * (px).sin() * (px / 3.0).sin() / (px * px)
}

/// One [`TapSet`] per output index along one axis. `scale = src_len / dst_len`; a scale > 1
/// (downscaling) widens the kernel support by `scale` to avoid aliasing, matching the standard
/// Lanczos-resize convention `fast_image_resize` itself follows.
///
/// Returns an error if any output pixel needs more taps than [`MAX_TAPS`] after edge-clamped
/// duplicates are merged. Interior tap count grows as `~= 6*scale + 1`, so with `MAX_TAPS=96`
/// this covers downscale ratios up to ~15x on one axis -- verified by
/// `gpu_resize_succeeds_at_ordinary_export_downscale_ratios` below at this repo's own real 45MP
/// (8280px-wide) source downsized to every long-edge preset from 1024 to 2048px (5.4x-8.1x
/// ratios). An earlier `MAX_TAPS=32` (~5.3x ceiling) silently errored on exactly these ordinary
/// sizes -- caught by adversarial review, not by the tests that existed at the time (both used
/// 4x/0.25x ratios, nowhere near the real boundary).
fn compute_taps(src_len: u32, dst_len: u32) -> anyhow::Result<Vec<TapSet>> {
    let scale = src_len as f64 / dst_len as f64;
    let filter_scale = scale.max(1.0);
    let support = 3.0 * filter_scale;

    let mut taps = Vec::with_capacity(dst_len as usize);
    for i in 0..dst_len {
        let center = (i as f64 + 0.5) * scale - 0.5;
        let lo = (center - support).floor() as i64;
        let hi = (center + support).ceil() as i64;

        // Accumulate weights per *clamped* source index -- edge taps can collapse onto the same
        // clamped index, and their weights must sum rather than overwrite.
        let mut merged: Vec<(i32, f64)> = Vec::new();
        for src in lo..=hi {
            let w = lanczos3((src as f64 - center) / filter_scale);
            if w == 0.0 {
                continue;
            }
            let clamped = src.clamp(0, src_len as i64 - 1) as i32;
            if let Some(entry) = merged.iter_mut().find(|(idx, _)| *idx == clamped) {
                entry.1 += w;
            } else {
                merged.push((clamped, w));
            }
        }

        if merged.len() > MAX_TAPS {
            anyhow::bail!(
                "output index {i} needs {} taps, exceeds MAX_TAPS={MAX_TAPS} (src_len={src_len}, dst_len={dst_len})",
                merged.len()
            );
        }

        let total: f64 = merged.iter().map(|(_, w)| w).sum();
        let mut tap_set = TapSet {
            count: merged.len() as u32,
            ..Default::default()
        };
        for (j, (idx, w)) in merged.into_iter().enumerate() {
            tap_set.idx[j] = idx;
            tap_set.weight[j] = if total != 0.0 {
                (w / total) as f32
            } else {
                0.0
            };
        }
        taps.push(tap_set);
    }
    Ok(taps)
}

/// Persistent GPU resources (device, queue, both compute pipelines) -- built once, reused across
/// calls to [`GpuLanczosResizer::resize`]. Rebuilding the pipeline per call inside a timing loop
/// is the exact anti-pattern `spikes/loaf`'s own gotcha note warns about.
pub struct GpuLanczosResizer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    horizontal_pipeline: wgpu::ComputePipeline,
    vertical_pipeline: wgpu::ComputePipeline,
}

impl GpuLanczosResizer {
    /// Requests the first available adapter (any backend) and builds both compute pipelines.
    pub fn new() -> anyhow::Result<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        }))
        .map_err(|e| anyhow::anyhow!("no wgpu adapter available: {e}"))?;

        // Default limits cap a single buffer at 256MiB -- too small for a full-res 45MP RGBA f32
        // source buffer (~731MB). Request the adapter's own (much larger) limits instead, same
        // as `spikes/loaf`/`spikes/crouch`'s `GpuContext` -- caught by a real validation error on
        // the first full-resolution run, not by inspection.
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("prey gpu_resize device"),
            required_limits: adapter.limits(),
            ..Default::default()
        }))
        .map_err(|e| anyhow::anyhow!("wgpu device request failed: {e}"))?;

        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("lanczos_resize"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(WGSL)),
        });
        let make_pipeline = |entry_point: &str| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry_point),
                layout: None,
                module: &module,
                entry_point: Some(entry_point),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                cache: None,
            })
        };

        Ok(GpuLanczosResizer {
            horizontal_pipeline: make_pipeline("resize_horizontal"),
            vertical_pipeline: make_pipeline("resize_vertical"),
            device,
            queue,
        })
    }

    pub fn adapter_backend_name(&self) -> String {
        format!("{:?}", self.device.features())
    }

    /// Resizes `src` (sRGB `u8`) to `dst_width`x`dst_height` on the GPU, converting to/from
    /// linear light the same way [`crate::resize::resize_fast_linear`] does on the CPU, so the
    /// two candidates are compared on equal footing.
    pub fn resize(
        &self,
        src: &image::RgbImage,
        dst_width: u32,
        dst_height: u32,
    ) -> anyhow::Result<image::RgbImage> {
        let (src_width, src_height) = src.dimensions();
        let src_rgba: Vec<[f32; 4]> = src
            .pixels()
            .map(|p| {
                [
                    srgb_eotf(p.0[0] as f32 / 255.0),
                    srgb_eotf(p.0[1] as f32 / 255.0),
                    srgb_eotf(p.0[2] as f32 / 255.0),
                    0.0,
                ]
            })
            .collect();

        let taps_x = compute_taps(src_width, dst_width)?;
        let taps_y = compute_taps(src_height, dst_height)?;

        let device = &self.device;
        let queue = &self.queue;

        let src_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("src_pixels"),
            contents: bytemuck::cast_slice(&src_rgba),
            usage: wgpu::BufferUsages::STORAGE,
        });
        let intermediate_len = (dst_width as usize) * (src_height as usize);
        let intermediate_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("intermediate_pixels"),
            size: (intermediate_len * std::mem::size_of::<[f32; 4]>()) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let taps_x_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("taps_x"),
            contents: bytemuck::cast_slice(&taps_x),
            usage: wgpu::BufferUsages::STORAGE,
        });
        let taps_y_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("taps_y"),
            contents: bytemuck::cast_slice(&taps_y),
            usage: wgpu::BufferUsages::STORAGE,
        });
        let dims = Dims {
            src_width,
            src_height,
            dst_width,
            dst_height,
        };
        let dims_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("dims"),
            contents: bytemuck::bytes_of(&dims),
            usage: wgpu::BufferUsages::UNIFORM,
        });

        let dst_len = (dst_width as usize) * (dst_height as usize);
        let dst_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("dst_pixels"),
            size: (dst_len * std::mem::size_of::<[f32; 4]>()) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let staging_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("staging"),
            size: (dst_len * std::mem::size_of::<[f32; 4]>()) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let horizontal_layout = self.horizontal_pipeline.get_bind_group_layout(0);
        let horizontal_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("horizontal_bind_group"),
            layout: &horizontal_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: src_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: intermediate_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: taps_x_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: dims_buf.as_entire_binding(),
                },
            ],
        });
        // The vertical entry point's storage/uniform bindings are declared at @group(1) in the
        // WGSL (a separate bind group from the horizontal pass's @group(0)), so its auto-derived
        // layout lives at index 1, not 0 -- index 0 exists but is empty for this entry point.
        let vertical_layout = self.vertical_pipeline.get_bind_group_layout(1);
        let vertical_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vertical_bind_group"),
            layout: &vertical_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: intermediate_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: dst_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: taps_y_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: dims_buf.as_entire_binding(),
                },
            ],
        });

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("gpu_resize encoder"),
        });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("horizontal pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.horizontal_pipeline);
            pass.set_bind_group(0, &horizontal_bind_group, &[]);
            let (wx, wy) = workgroup_grid((intermediate_len as u32).div_ceil(WORKGROUP_SIZE));
            pass.dispatch_workgroups(wx, wy, 1);
        }
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("vertical pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.vertical_pipeline);
            pass.set_bind_group(1, &vertical_bind_group, &[]);
            let (wx, wy) = workgroup_grid((dst_len as u32).div_ceil(WORKGROUP_SIZE));
            pass.dispatch_workgroups(wx, wy, 1);
        }
        encoder.copy_buffer_to_buffer(
            &dst_buf,
            0,
            &staging_buf,
            0,
            (dst_len * std::mem::size_of::<[f32; 4]>()) as u64,
        );
        queue.submit(Some(encoder.finish()));

        let slice = staging_buf.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| anyhow::anyhow!("device poll failed: {e:?}"))?;
        let raw = slice
            .get_mapped_range()
            .map_err(|e| anyhow::anyhow!("output buffer not mapped: {e:?}"))?;
        let out_rgba: &[[f32; 4]] = bytemuck::cast_slice(&raw);

        let out_image = image::ImageBuffer::from_fn(dst_width, dst_height, |x, y| {
            let p = out_rgba[(y * dst_width + x) as usize];
            image::Rgb([
                (srgb_oetf(p[0]) * 255.0).round().clamp(0.0, 255.0) as u8,
                (srgb_oetf(p[1]) * 255.0).round().clamp(0.0, 255.0) as u8,
                (srgb_oetf(p[2]) * 255.0).round().clamp(0.0, 255.0) as u8,
            ])
        });
        drop(raw);
        staging_buf.unmap();
        Ok(out_image)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::RgbImage;
    use nicti_prowl::golden::ssim;

    #[test]
    fn taps_sum_to_one_and_stay_in_bounds() {
        let taps = compute_taps(256, 64).unwrap();
        assert_eq!(taps.len(), 64);
        for tap_set in &taps {
            assert!(tap_set.count > 0);
            let sum: f32 = tap_set.weight[..tap_set.count as usize].iter().sum();
            assert!(
                (sum - 1.0).abs() < 1e-4,
                "weights must sum to 1.0, got {sum}"
            );
            for &idx in &tap_set.idx[..tap_set.count as usize] {
                assert!((0..256).contains(&idx));
            }
        }
    }

    #[test]
    fn upscale_taps_also_sum_to_one() {
        let taps = compute_taps(16, 64).unwrap();
        assert_eq!(taps.len(), 64);
        for tap_set in &taps {
            let sum: f32 = tap_set.weight[..tap_set.count as usize].iter().sum();
            assert!((sum - 1.0).abs() < 1e-4);
        }
    }

    #[test]
    fn compute_taps_succeeds_at_ordinary_export_downscale_ratios() {
        // Regression test for a real adversarial-review finding: MAX_TAPS=32 (~5.3x ceiling)
        // silently errored on this repo's own ordinary export long-edge presets from a real 45MP
        // (8280px-wide) source -- every one of these is a perfectly normal "web export" size, not
        // an extreme ratio. 8280/1024 ~= 8.08x is the worst of these.
        let src_width = 8280u32;
        for &dst_long_edge in &[1024u32, 1200, 1400, 1536, 2048] {
            let taps = compute_taps(src_width, dst_long_edge)
                .unwrap_or_else(|e| panic!("8280 -> {dst_long_edge} must succeed: {e}"));
            assert_eq!(taps.len(), dst_long_edge as usize);
        }
    }

    #[test]
    fn compute_taps_errors_past_max_taps_ceiling_instead_of_producing_wrong_weights() {
        // At a genuinely extreme ratio (well past MAX_TAPS=96's ~15x ceiling), compute_taps must
        // fail loudly rather than silently truncate to a wrong (non-1.0-summing) weight set.
        let result = compute_taps(10_000, 50);
        assert!(
            result.is_err(),
            "a 200x downscale ratio must error, not silently truncate"
        );
    }

    fn checkerboard(width: u32, height: u32) -> RgbImage {
        RgbImage::from_fn(width, height, |x, y| {
            if (x / 4 + y / 4) % 2 == 0 {
                image::Rgb([230, 40, 40])
            } else {
                image::Rgb([20, 20, 200])
            }
        })
    }

    #[test]
    fn gpu_resize_matches_cpu_linear_resize() {
        let resizer = match GpuLanczosResizer::new() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping gpu_resize test, no adapter available: {e}");
                return;
            }
        };
        let src = checkerboard(64, 64);
        let gpu_out = resizer.resize(&src, 16, 16).unwrap();
        let cpu_out = crate::resize::resize_fast_linear(&src, 16, 16).unwrap();
        assert_eq!(gpu_out.dimensions(), cpu_out.dimensions());
        let score = ssim(&gpu_out, &cpu_out);
        assert!(
            score > 0.95,
            "GPU and CPU linear-light Lanczos3 resize should closely agree, got ssim={score}"
        );
    }

    #[test]
    fn gpu_resize_produces_correct_dimensions_for_upscale() {
        let resizer = match GpuLanczosResizer::new() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping gpu_resize test, no adapter available: {e}");
                return;
            }
        };
        let src = checkerboard(16, 16);
        let out = resizer.resize(&src, 48, 32).unwrap();
        assert_eq!(out.dimensions(), (48, 32));
    }
}
