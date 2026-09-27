//! wgpu device/adapter setup and kernel dispatch, adapted (copied, not depended-on -- spikes don't
//! depend on each other) from `spikes/glint/src/gpu.rs` (#16/ADR-0005): the same `GpuContext`
//! enumeration, `workgroup_grid` 2D-dispatch-limit fix, and GPU-timestamp dispatch/readback
//! machinery, extended with three kernels specific to Tapetum's design: `live_suffix` (the fused
//! live-stage chain, decision rule #2), `present_sample` (the crop/geometry sample pass, decision
//! rule #3), and `box_filter` (the mask-refine primitive, `refine.rs`'s GPU twin).

use std::borrow::Cow;

use bytemuck::{Pod, Zeroable};

pub const LIVE_SUFFIX_WGSL: &str = include_str!("../shaders/live_suffix.wgsl");
pub const PRESENT_SAMPLE_WGSL: &str = include_str!("../shaders/present_sample.wgsl");
pub const BOX_FILTER_WGSL: &str = include_str!("../shaders/box_filter.wgsl");

pub const WORKGROUP_SIZE: u32 = 64;

/// wgpu's per-dimension dispatch limit -- see `spikes/glint/src/gpu.rs`'s identical constant and
/// its citation (a real bug hit on first real-hardware run, Dx12 validation rejected a 1D dispatch
/// at hero-scenario resolution).
const MAX_WORKGROUPS_PER_DIM: u32 = 65535;

pub fn workgroup_grid(total_workgroups: u32) -> (u32, u32) {
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

pub struct GpuContext {
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub backend: wgpu::Backend,
    pub adapter_name: String,
    pub timestamp_period_ns: f32,
}

impl GpuContext {
    /// Enumerates every adapter wgpu can see. In this sandbox (WSL, no NVIDIA Vulkan ICD) this is
    /// lavapipe/llvmpipe software rendering only -- fine for the GPU-parity correctness tests,
    /// useless for the decision rule's real timing numbers (see `.claude/rules/gpu-gui-and-healing
    /// /REFERENCE.md`'s cross-compile-to-Windows-and-run-via-WSL-interop path for those).
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

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("loaf device"),
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
            label: Some("loaf timestamps"),
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
        label: Some("loaf encoder"),
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
            label: Some("loaf pass"),
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

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct LiveSuffixParams {
    pub wb_r: f32,
    pub wb_g: f32,
    pub wb_b: f32,
    pub exposure_stops: f32,
    pub vibrance: f32,
    pub _pad0: f32,
    pub _pad1: f32,
    pub _pad2: f32,
}

/// Runs `live_suffix` once over `pixels` (RGBA). Decision rule #2's fused-live-suffix timing.
pub fn run_live_suffix(
    ctx: &GpuContext,
    pixels: &[[f32; 4]],
    params: LiveSuffixParams,
) -> (Vec<[f32; 4]>, Option<f64>) {
    let device = &ctx.device;
    let pipeline = make_compute_pipeline(device, LIVE_SUFFIX_WGSL, "live_suffix");

    let input_buf = storage_buffer(
        device,
        "live_suffix input",
        bytemuck::cast_slice(pixels),
        wgpu::BufferUsages::STORAGE,
    );
    let output_size = std::mem::size_of_val(pixels) as u64;
    let output_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("live_suffix output"),
        size: output_size,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let params_buf = storage_buffer(
        device,
        "live_suffix params",
        bytemuck::bytes_of(&params),
        wgpu::BufferUsages::UNIFORM,
    );

    let bind_group_layout = pipeline.get_bind_group_layout(0);
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("live_suffix bind group"),
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

/// A `live_suffix` pipeline + buffers built once and re-dispatched many times, matching
/// `spikes/glint/src/gpu.rs::LiveChainKernel`'s own reasoning exactly: `run_live_suffix` rebuilds
/// a fresh pipeline (including shader-module creation, which some drivers compile lazily on first
/// dispatch rather than at module-creation time) and fresh buffers on every call, which is fine
/// for a one-shot correctness check but wrong for a repeated-call timing loop -- `bin/loaf.rs`'s
/// first `bench` pass called `run_live_suffix` inside `Protocol::run`'s 1-warmup+5-measured loop
/// and measured ~400ms p50 on the reference RTX 5080, roughly 1000x ADR-0005's own 0.326ms p95
/// live-chain figure at a comparable resolution -- caught by comparing against that ADR's number
/// rather than trusting the first result at face value. This type is what makes "warm, steady-
/// state per-frame cost" (what decision rule #2 actually asks for) the thing actually measured.
pub struct LiveSuffixKernel {
    pipeline: wgpu::ComputePipeline,
    input_buf: wgpu::Buffer,
    output_buf: wgpu::Buffer,
    params_buf: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    pixel_count: usize,
    output_size: u64,
}

impl LiveSuffixKernel {
    pub fn new(ctx: &GpuContext, pixel_count: usize) -> Self {
        let device = &ctx.device;
        let pipeline = make_compute_pipeline(device, LIVE_SUFFIX_WGSL, "live_suffix");

        let input_size = (pixel_count * std::mem::size_of::<[f32; 4]>()) as u64;
        let input_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("live_suffix kernel input"),
            size: input_size,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let output_size = input_size;
        let output_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("live_suffix kernel output"),
            size: output_size,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let params_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("live_suffix kernel params"),
            size: std::mem::size_of::<LiveSuffixParams>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group_layout = pipeline.get_bind_group_layout(0);
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("live_suffix kernel bind group"),
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

    pub fn dispatch(
        &self,
        ctx: &GpuContext,
        pixels: &[[f32; 4]],
        params: LiveSuffixParams,
    ) -> (Vec<[f32; 4]>, Option<f64>) {
        assert_eq!(
            pixels.len(),
            self.pixel_count,
            "LiveSuffixKernel is sized for a fixed pixel count"
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

// WGSL layout note: a `vec3<f32>` uniform member is padded so its *start* offset is 16-byte
// aligned, but its own size stays 12 bytes -- there's no padding after the *last* vec3 before a
// following scalar unless that scalar itself needs more than 4-byte alignment. `m0` needs a pad
// field after it (`m1` is also a vec3, needing 16-byte alignment), but `m1` does not, since
// `source_width` (a `u32`, 4-byte alignment) starts right after `m1`'s 12 real bytes at offset 28.
// Getting this wrong (adding a pad after `m1` too) shifts every field after it by 4 bytes,
// silently corrupting `source_width`/`source_height`/`out_width`/`out_height` -- caught by
// `tests/gpu_parity.rs::present_sample_gpu_matches_cpu_reference_for_a_crop` reading back all
// zeros (every invocation's `i >= total` guard tripped on a garbage `total`).
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct PresentSampleParams {
    pub m0: [f32; 3],
    pub _pad_m0: f32,
    pub m1: [f32; 3],
    pub source_width: u32,
    pub source_height: u32,
    pub out_width: u32,
    pub out_height: u32,
    /// Trailing pad so this struct's total size (48 bytes) matches WGSL's own struct size, which
    /// rounds up to the struct's alignment (16, driven by the two `vec3<f32>` members) -- without
    /// this, Rust's repr(C) layout (max member align 4) would size the struct at 44 bytes, one
    /// `u32` short of what a uniform-buffer binding validates the buffer against.
    pub _pad_end: u32,
}

/// Runs `present_sample` once: the crop/geometry pass. Decision rule #3's zero-upstream-dispatch
/// claim is a structural property of `graph::RenderGraph` (tested there); this function's own job
/// is only to measure this one kernel's own dispatch cost against the 16.7ms frame budget.
pub fn run_present_sample(
    ctx: &GpuContext,
    source: &[[f32; 4]],
    source_width: u32,
    source_height: u32,
    transform: &crate::geometry::Affine2D,
    out_width: u32,
    out_height: u32,
) -> (Vec<[f32; 4]>, Option<f64>) {
    let device = &ctx.device;
    let pipeline = make_compute_pipeline(device, PRESENT_SAMPLE_WGSL, "present_sample");

    let params = PresentSampleParams {
        m0: transform.m[0],
        _pad_m0: 0.0,
        m1: transform.m[1],
        source_width,
        source_height,
        out_width,
        out_height,
        _pad_end: 0,
    };

    let input_buf = storage_buffer(
        device,
        "present_sample source",
        bytemuck::cast_slice(source),
        wgpu::BufferUsages::STORAGE,
    );
    let output_len = (out_width * out_height) as usize;
    let output_size = (output_len * std::mem::size_of::<[f32; 4]>()) as u64;
    let output_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("present_sample output"),
        size: output_size,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let params_buf = storage_buffer(
        device,
        "present_sample params",
        bytemuck::bytes_of(&params),
        wgpu::BufferUsages::UNIFORM,
    );

    let bind_group_layout = pipeline.get_bind_group_layout(0);
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("present_sample bind group"),
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

    let (wg_x, wg_y) = workgroup_grid((output_len as u32).div_ceil(WORKGROUP_SIZE));
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

/// Persistent `present_sample` pipeline + buffers -- see `LiveSuffixKernel`'s doc comment for why
/// this exists (the same "rebuild everything, including shader compilation, on every timed call"
/// bug applies identically here).
pub struct PresentSampleKernel {
    pipeline: wgpu::ComputePipeline,
    input_buf: wgpu::Buffer,
    output_buf: wgpu::Buffer,
    params_buf: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    source_len: usize,
    out_width: u32,
    out_height: u32,
    output_size: u64,
}

impl PresentSampleKernel {
    pub fn new(ctx: &GpuContext, source_len: usize, out_width: u32, out_height: u32) -> Self {
        let device = &ctx.device;
        let pipeline = make_compute_pipeline(device, PRESENT_SAMPLE_WGSL, "present_sample");

        let input_size = (source_len * std::mem::size_of::<[f32; 4]>()) as u64;
        let input_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("present_sample kernel source"),
            size: input_size,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let output_len = (out_width * out_height) as usize;
        let output_size = (output_len * std::mem::size_of::<[f32; 4]>()) as u64;
        let output_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("present_sample kernel output"),
            size: output_size,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let params_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("present_sample kernel params"),
            size: std::mem::size_of::<PresentSampleParams>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group_layout = pipeline.get_bind_group_layout(0);
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("present_sample kernel bind group"),
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
            source_len,
            out_width,
            out_height,
            output_size,
        }
    }

    pub fn dispatch(
        &self,
        ctx: &GpuContext,
        source: &[[f32; 4]],
        source_width: u32,
        source_height: u32,
        transform: &crate::geometry::Affine2D,
    ) -> (Vec<[f32; 4]>, Option<f64>) {
        assert_eq!(
            source.len(),
            self.source_len,
            "PresentSampleKernel is sized for a fixed source length"
        );
        let params = PresentSampleParams {
            m0: transform.m[0],
            _pad_m0: 0.0,
            m1: transform.m[1],
            source_width,
            source_height,
            out_width: self.out_width,
            out_height: self.out_height,
            _pad_end: 0,
        };
        ctx.queue
            .write_buffer(&self.input_buf, 0, bytemuck::cast_slice(source));
        ctx.queue
            .write_buffer(&self.params_buf, 0, bytemuck::bytes_of(&params));

        let output_len = (self.out_width * self.out_height) as usize;
        let (wg_x, wg_y) = workgroup_grid((output_len as u32).div_ceil(WORKGROUP_SIZE));
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

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct BoxFilterParams {
    pub width: u32,
    pub height: u32,
    pub radius: u32,
    pub _pad: u32,
}

/// Runs `box_filter` once -- the GPU twin `tests/gpu_parity.rs` checks against
/// `refine::box_filter`.
pub fn run_box_filter(
    ctx: &GpuContext,
    field: &[f32],
    width: u32,
    height: u32,
    radius: u32,
) -> (Vec<f32>, Option<f64>) {
    let device = &ctx.device;
    let pipeline = make_compute_pipeline(device, BOX_FILTER_WGSL, "box_filter");

    let params = BoxFilterParams {
        width,
        height,
        radius,
        _pad: 0,
    };

    let input_buf = storage_buffer(
        device,
        "box_filter input",
        bytemuck::cast_slice(field),
        wgpu::BufferUsages::STORAGE,
    );
    let output_size = std::mem::size_of_val(field) as u64;
    let output_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("box_filter output"),
        size: output_size,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let params_buf = storage_buffer(
        device,
        "box_filter params",
        bytemuck::bytes_of(&params),
        wgpu::BufferUsages::UNIFORM,
    );

    let bind_group_layout = pipeline.get_bind_group_layout(0);
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("box_filter bind group"),
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

    let (wg_x, wg_y) = workgroup_grid((field.len() as u32).div_ceil(WORKGROUP_SIZE));
    let (elapsed_ns, readback) = dispatch_and_read(
        ctx,
        &pipeline,
        &bind_group,
        (wg_x, wg_y),
        &output_buf,
        output_size,
    );
    let out: &[f32] = bytemuck::cast_slice(&readback);
    (out.to_vec(), elapsed_ns)
}

/// Persistent `box_filter` pipeline + buffers -- see `LiveSuffixKernel`'s doc comment for why.
pub struct BoxFilterKernel {
    pipeline: wgpu::ComputePipeline,
    input_buf: wgpu::Buffer,
    output_buf: wgpu::Buffer,
    params_buf: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    field_len: usize,
    output_size: u64,
}

impl BoxFilterKernel {
    pub fn new(ctx: &GpuContext, width: u32, height: u32) -> Self {
        let device = &ctx.device;
        let pipeline = make_compute_pipeline(device, BOX_FILTER_WGSL, "box_filter");
        let field_len = (width * height) as usize;

        let input_size = (field_len * std::mem::size_of::<f32>()) as u64;
        let input_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("box_filter kernel input"),
            size: input_size,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let output_size = input_size;
        let output_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("box_filter kernel output"),
            size: output_size,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let params_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("box_filter kernel params"),
            size: std::mem::size_of::<BoxFilterParams>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group_layout = pipeline.get_bind_group_layout(0);
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("box_filter kernel bind group"),
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
            field_len,
            output_size,
        }
    }

    pub fn dispatch(
        &self,
        ctx: &GpuContext,
        field: &[f32],
        width: u32,
        height: u32,
        radius: u32,
    ) -> (Vec<f32>, Option<f64>) {
        assert_eq!(
            field.len(),
            self.field_len,
            "BoxFilterKernel is sized for a fixed field length"
        );
        let params = BoxFilterParams {
            width,
            height,
            radius,
            _pad: 0,
        };
        ctx.queue
            .write_buffer(&self.input_buf, 0, bytemuck::cast_slice(field));
        ctx.queue
            .write_buffer(&self.params_buf, 0, bytemuck::bytes_of(&params));

        let (wg_x, wg_y) = workgroup_grid((self.field_len as u32).div_ceil(WORKGROUP_SIZE));
        let (elapsed_ns, readback) = dispatch_and_read(
            ctx,
            &self.pipeline,
            &self.bind_group,
            (wg_x, wg_y),
            &self.output_buf,
            self.output_size,
        );
        let out: &[f32] = bytemuck::cast_slice(&readback);
        (out.to_vec(), elapsed_ns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_workload_stays_1d() {
        assert_eq!(workgroup_grid(100), (100, 1));
    }

    #[test]
    fn hero_scenario_workgroup_count_fits_in_2d_grid() {
        let total = 45_000_000u32.div_ceil(WORKGROUP_SIZE);
        let (x, y) = workgroup_grid(total);
        assert!(x <= MAX_WORKGROUPS_PER_DIM);
        assert!(y <= MAX_WORKGROUPS_PER_DIM);
        assert!(x as u64 * y as u64 >= total as u64);
    }
}
