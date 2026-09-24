//! wgpu device/adapter setup, kernel dispatch, and GPU-timestamp-based timing shared by every
//! test and the `glint` benchmark binary.
//!
//! Scoping note: kernels here operate on `array<vec4<f32>>` storage **buffers**, not storage
//! **textures**. The ADR-0005 decision rule cares about compute throughput and dispatch
//! overhead, which a buffer kernel exercises identically to a texture kernel; texture-specific
//! concerns (sampling, mipmaps, format-feature negotiation) are Tapetum's (#44) problem, not
//! this ticket's. `SHADER_F16` is still probed directly (see `tests/features_and_limits.rs`),
//! just not woven into the main throughput kernels.

use std::borrow::Cow;

use bytemuck::{Pod, Zeroable};

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct LiveChainParams {
    pub wb_gain: [f32; 3],
    pub exposure_stops: f32,
    pub vibrance: f32,
    pub _pad: [f32; 3],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct TileBlendParams {
    pub width: u32,
    pub seam_start: u32,
    pub seam_width: u32,
    pub _pad: u32,
}

pub const LIVE_CHAIN_WGSL: &str = include_str!("../shaders/live_chain.wgsl");
pub const TILE_BLEND_WGSL: &str = include_str!("../shaders/tile_blend.wgsl");
pub const F16_PROBE_WGSL: &str = include_str!("../shaders/f16_probe.wgsl");

pub const WORKGROUP_SIZE: u32 = 64;

/// wgpu's per-dimension dispatch limit (`maxComputeWorkgroupsPerDimension` in the WebGPU spec,
/// commonly 65535 on native backends) -- a single-dimension dispatch of `ceil(count/64)`
/// workgroups overflows this well before reaching the hero scenario's 45MP frame (45M/64 ~=
/// 710k workgroups), a real bug this crate hit on first real-hardware run (Dx12 validation
/// rejected the dispatch outright). Split into a 2D grid instead; shaders recover the flat
/// pixel index via `@builtin(num_workgroups)`.
const MAX_WORKGROUPS_PER_DIM: u32 = 65535;

/// Splits `total_workgroups` into an (x, y) dispatch grid that respects
/// `MAX_WORKGROUPS_PER_DIM` in both dimensions.
fn workgroup_grid(total_workgroups: u32) -> (u32, u32) {
    if total_workgroups <= MAX_WORKGROUPS_PER_DIM {
        (total_workgroups, 1)
    } else {
        let x = MAX_WORKGROUPS_PER_DIM;
        let y = total_workgroups.div_ceil(x);
        assert!(
            y <= MAX_WORKGROUPS_PER_DIM,
            "workload too large for a single 2D dispatch grid"
        );
        (x, y)
    }
}

/// Which wgpu backend a `GpuContext` was created against — used to label results and to run the
/// same kernel across every backend the adapter enumeration finds, per the ADR-0005 decision
/// rule's "works on both D3D12 and Vulkan backends" requirement.
pub struct GpuContext {
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub backend: wgpu::Backend,
    pub adapter_name: String,
    pub timestamp_period_ns: f32,
}

impl GpuContext {
    /// Enumerates every adapter wgpu can see across all compiled-in backends. On Windows this is
    /// (at least) Vulkan and Dx12 against the same physical GPU; in WSL without a GPU-backed
    /// Vulkan ICD, this falls back to lavapipe (software) — fine for correctness, not for
    /// throughput numbers (see `docs/adr/0005-gpu-compute-api.md`'s hardware-identity caveat).
    pub fn enumerate() -> Vec<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        pollster::block_on(instance.enumerate_adapters(wgpu::Backends::PRIMARY))
            .into_iter()
            .filter_map(Self::from_adapter)
            .collect()
    }

    fn from_adapter(adapter: wgpu::Adapter) -> Option<Self> {
        let info = adapter.get_info();
        let features = adapter.features();
        let mut required_features = wgpu::Features::empty();
        if features.contains(wgpu::Features::TIMESTAMP_QUERY) {
            required_features |= wgpu::Features::TIMESTAMP_QUERY;
        }
        if features.contains(wgpu::Features::SHADER_F16) {
            required_features |= wgpu::Features::SHADER_F16;
        }

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("glint device"),
            required_features,
            required_limits: adapter.limits(),
            ..Default::default()
        }))
        .ok()?;

        let timestamp_period_ns = queue.get_timestamp_period();

        Some(Self {
            adapter,
            device,
            queue,
            backend: info.backend,
            adapter_name: info.name,
            timestamp_period_ns,
        })
    }

    pub fn supports_timestamps(&self) -> bool {
        self.device
            .features()
            .contains(wgpu::Features::TIMESTAMP_QUERY)
    }

    pub fn supports_f16(&self) -> bool {
        self.device.features().contains(wgpu::Features::SHADER_F16)
    }
}

fn make_compute_pipeline(
    device: &wgpu::Device,
    wgsl: &str,
    entry_point: &str,
) -> wgpu::ComputePipeline {
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(entry_point),
        source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(wgsl)),
    });
    device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some(entry_point),
        layout: None,
        module: &module,
        entry_point: Some(entry_point),
        compilation_options: wgpu::PipelineCompilationOptions::default(),
        cache: None,
    })
}

fn storage_buffer(
    device: &wgpu::Device,
    label: &str,
    contents: &[u8],
    usage: wgpu::BufferUsages,
) -> wgpu::Buffer {
    use wgpu::util::DeviceExt;
    device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some(label),
        contents,
        usage,
    })
}

/// Runs `live_chain` once over `pixels` (RGBA, alpha ignored/passed through) and returns the
/// output in the same layout. `timing_ns` is `Some` iff the adapter supports `TIMESTAMP_QUERY`.
pub fn run_live_chain(
    ctx: &GpuContext,
    pixels: &[[f32; 4]],
    params: LiveChainParams,
) -> (Vec<[f32; 4]>, Option<f64>) {
    let device = &ctx.device;
    let pipeline = make_compute_pipeline(device, LIVE_CHAIN_WGSL, "live_chain");

    let input_buf = storage_buffer(
        device,
        "live_chain input",
        bytemuck::cast_slice(pixels),
        wgpu::BufferUsages::STORAGE,
    );
    let output_size = std::mem::size_of_val(pixels) as u64;
    let output_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("live_chain output"),
        size: output_size,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let params_buf = storage_buffer(
        device,
        "live_chain params",
        bytemuck::bytes_of(&params),
        wgpu::BufferUsages::UNIFORM,
    );

    let bind_group_layout = pipeline.get_bind_group_layout(0);
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("live_chain bind group"),
        layout: &bind_group_layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: input_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: output_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: params_buf.as_entire_binding(),
            },
        ],
    });

    let (wg_x, wg_y) = workgroup_grid((pixels.len() as u32).div_ceil(WORKGROUP_SIZE));
    let (elapsed_ns, readback) = dispatch_and_read(
        ctx,
        &pipeline,
        &bind_group,
        (wg_x, wg_y),
        &output_buf,
        output_size,
    );

    let out: &[[f32; 4]] = bytemuck::cast_slice(&readback);
    (out.to_vec(), elapsed_ns)
}

/// A `live_chain` pipeline + buffers built once and re-dispatched many times via
/// [`LiveChainKernel::dispatch`], for measuring genuine per-dispatch cost. `run_live_chain`
/// rebuilds a fresh pipeline and buffers (including a fresh host->device upload) on every call,
/// which is fine for a single one-shot measurement (correctness, or a GPU-timestamped throughput
/// run, where only the in-GPU-timeline duration between the two timestamp writes is measured —
/// pipeline/buffer setup happens entirely outside that window) but wrong for
/// `tests/dispatch_overhead.rs`'s wall-clock-timed loop: without this type, that loop was
/// re-paying pipeline compilation and a full input-buffer upload on every iteration and
/// mislabeling the result as "per-dispatch CPU overhead" (caught in adversarial review before
/// merge). This type is what makes that isolation actually true.
pub struct LiveChainKernel {
    pipeline: wgpu::ComputePipeline,
    input_buf: wgpu::Buffer,
    output_buf: wgpu::Buffer,
    params_buf: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    pixel_count: usize,
    output_size: u64,
}

impl LiveChainKernel {
    pub fn new(ctx: &GpuContext, pixel_count: usize) -> Self {
        let device = &ctx.device;
        let pipeline = make_compute_pipeline(device, LIVE_CHAIN_WGSL, "live_chain");

        let input_size = (pixel_count * std::mem::size_of::<[f32; 4]>()) as u64;
        let input_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("live_chain kernel input"),
            size: input_size,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let output_size = input_size;
        let output_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("live_chain kernel output"),
            size: output_size,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let params_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("live_chain kernel params"),
            size: std::mem::size_of::<LiveChainParams>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group_layout = pipeline.get_bind_group_layout(0);
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("live_chain kernel bind group"),
            layout: &bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: input_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: output_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: params_buf.as_entire_binding(),
                },
            ],
        });

        Self {
            pipeline,
            input_buf,
            output_buf,
            params_buf,
            bind_group,
            pixel_count,
            output_size,
        }
    }

    /// Re-uploads `pixels` and `params` into the pre-built buffers (no new pipeline, no new
    /// buffer allocation) and dispatches once. This is the actual per-dispatch cost:
    /// `queue.write_buffer` x2 + submit + poll + readback.
    pub fn dispatch(
        &self,
        ctx: &GpuContext,
        pixels: &[[f32; 4]],
        params: LiveChainParams,
    ) -> (Vec<[f32; 4]>, Option<f64>) {
        assert_eq!(
            pixels.len(),
            self.pixel_count,
            "LiveChainKernel is sized for a fixed pixel count"
        );
        ctx.queue
            .write_buffer(&self.input_buf, 0, bytemuck::cast_slice(pixels));
        ctx.queue
            .write_buffer(&self.params_buf, 0, bytemuck::bytes_of(&params));

        let (wg_x, wg_y) = workgroup_grid((self.pixel_count as u32).div_ceil(WORKGROUP_SIZE));
        let (elapsed_ns, readback) = dispatch_and_read(
            ctx,
            &self.pipeline,
            &self.bind_group,
            (wg_x, wg_y),
            &self.output_buf,
            self.output_size,
        );

        let out: &[[f32; 4]] = bytemuck::cast_slice(&readback);
        (out.to_vec(), elapsed_ns)
    }
}

/// Runs `tile_blend` once, blending `tile_a`/`tile_b` (same length) along a seam of `seam_width`
/// centered on `params.seam_start`.
pub fn run_tile_blend(
    ctx: &GpuContext,
    tile_a: &[[f32; 4]],
    tile_b: &[[f32; 4]],
    params: TileBlendParams,
) -> (Vec<[f32; 4]>, Option<f64>) {
    assert_eq!(
        tile_a.len(),
        tile_b.len(),
        "tile_blend requires equal-length tiles"
    );
    let device = &ctx.device;
    let pipeline = make_compute_pipeline(device, TILE_BLEND_WGSL, "tile_blend");

    let a_buf = storage_buffer(
        device,
        "tile_a",
        bytemuck::cast_slice(tile_a),
        wgpu::BufferUsages::STORAGE,
    );
    let b_buf = storage_buffer(
        device,
        "tile_b",
        bytemuck::cast_slice(tile_b),
        wgpu::BufferUsages::STORAGE,
    );
    let output_size = std::mem::size_of_val(tile_a) as u64;
    let output_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("tile_blend output"),
        size: output_size,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let params_buf = storage_buffer(
        device,
        "tile_blend params",
        bytemuck::bytes_of(&params),
        wgpu::BufferUsages::UNIFORM,
    );

    let bind_group_layout = pipeline.get_bind_group_layout(0);
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("tile_blend bind group"),
        layout: &bind_group_layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: a_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: b_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: output_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: params_buf.as_entire_binding(),
            },
        ],
    });

    let (wg_x, wg_y) = workgroup_grid((tile_a.len() as u32).div_ceil(WORKGROUP_SIZE));
    let (elapsed_ns, readback) = dispatch_and_read(
        ctx,
        &pipeline,
        &bind_group,
        (wg_x, wg_y),
        &output_buf,
        output_size,
    );
    let out: &[[f32; 4]] = bytemuck::cast_slice(&readback);
    (out.to_vec(), elapsed_ns)
}

fn dispatch_and_read(
    ctx: &GpuContext,
    pipeline: &wgpu::ComputePipeline,
    bind_group: &wgpu::BindGroup,
    (workgroups_x, workgroups_y): (u32, u32),
    output_buf: &wgpu::Buffer,
    output_size: u64,
) -> (Option<f64>, Vec<u8>) {
    let device = &ctx.device;
    let queue = &ctx.queue;
    let use_timestamps = ctx.supports_timestamps();

    let query_set = use_timestamps.then(|| {
        device.create_query_set(&wgpu::QuerySetDescriptor {
            label: Some("glint timestamps"),
            ty: wgpu::QueryType::Timestamp,
            count: 2,
        })
    });
    let resolve_buf = use_timestamps.then(|| {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("timestamp resolve"),
            size: 16,
            usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        })
    });
    let timestamp_readback = use_timestamps.then(|| {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("timestamp readback"),
            size: 16,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        })
    });

    let staging_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("output readback"),
        size: output_size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("glint encoder"),
    });
    let timestamp_writes = query_set
        .as_ref()
        .map(|qs| wgpu::ComputePassTimestampWrites {
            query_set: qs,
            beginning_of_pass_write_index: Some(0),
            end_of_pass_write_index: Some(1),
        });
    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("glint pass"),
            timestamp_writes,
        });
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, bind_group, &[]);
        pass.dispatch_workgroups(workgroups_x, workgroups_y, 1);
    }
    if let (Some(qs), Some(resolve)) = (&query_set, &resolve_buf) {
        encoder.resolve_query_set(qs, 0..2, resolve, 0);
        encoder.copy_buffer_to_buffer(resolve, 0, timestamp_readback.as_ref().unwrap(), 0, 16);
    }
    encoder.copy_buffer_to_buffer(output_buf, 0, &staging_buf, 0, output_size);
    queue.submit(Some(encoder.finish()));

    let output_slice = staging_buf.slice(..);
    output_slice.map_async(wgpu::MapMode::Read, |_| {});
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("device poll failed");
    let data = output_slice
        .get_mapped_range()
        .expect("output buffer not mapped")
        .to_vec();
    staging_buf.unmap();

    let elapsed_ns = timestamp_readback.map(|ts_buf| {
        let ts_slice = ts_buf.slice(..);
        ts_slice.map_async(wgpu::MapMode::Read, |_| {});
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("device poll failed");
        let raw = ts_slice
            .get_mapped_range()
            .expect("timestamp buffer not mapped");
        let timestamps: &[u64] = bytemuck::cast_slice(&raw);
        let (start, end) = (timestamps[0], timestamps[1]);
        drop(raw);
        ts_buf.unmap();
        (end - start) as f64 * ctx.timestamp_period_ns as f64
    });

    (elapsed_ns, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_workload_stays_1d() {
        assert_eq!(workgroup_grid(100), (100, 1));
        assert_eq!(
            workgroup_grid(MAX_WORKGROUPS_PER_DIM),
            (MAX_WORKGROUPS_PER_DIM, 1)
        );
    }

    #[test]
    fn oversized_workload_splits_into_2d() {
        let (x, y) = workgroup_grid(MAX_WORKGROUPS_PER_DIM + 1);
        assert_eq!(x, MAX_WORKGROUPS_PER_DIM);
        assert_eq!(y, 2);
        assert!(x as u64 * y as u64 >= (MAX_WORKGROUPS_PER_DIM as u64 + 1));
    }

    #[test]
    fn hero_scenario_workgroup_count_fits_in_2d_grid() {
        // 45MP / WORKGROUP_SIZE, the exact case that overflowed a 1D dispatch on real hardware.
        let total = 45_000_000u32.div_ceil(WORKGROUP_SIZE);
        let (x, y) = workgroup_grid(total);
        assert!(x <= MAX_WORKGROUPS_PER_DIM);
        assert!(y <= MAX_WORKGROUPS_PER_DIM);
        assert!(x as u64 * y as u64 >= total as u64);
    }
}
