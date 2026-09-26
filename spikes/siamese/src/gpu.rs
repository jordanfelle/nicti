//! wgpu compute-shader twins of `geometry.rs`'s rasterizers, `compose.rs`'s per-component compose
//! step, and a masked-adjust-apply kernel -- the parts of #48's design that run per-frame on GPU
//! at #44's interactive budget, not just at bake time. Same `GpuContext`/dispatch-boilerplate
//! pattern as `spikes/calico/src/gpu.rs`/`spikes/glint/src/gpu.rs` (this spike doesn't depend on
//! either). See each shader file's own header comment for the kernel it implements.

use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;

const WORKGROUP_SIZE: u32 = 64;
const MAX_WORKGROUPS_PER_DIM: u32 = 65535;

fn workgroup_grid(total_workgroups: u32) -> (u32, u32) {
    if total_workgroups <= MAX_WORKGROUPS_PER_DIM {
        (total_workgroups.max(1), 1)
    } else {
        let x = MAX_WORKGROUPS_PER_DIM;
        (x, total_workgroups.div_ceil(x))
    }
}

pub struct GpuContext {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub backend: wgpu::Backend,
    pub adapter_name: String,
}

impl GpuContext {
    /// Enumerates every adapter wgpu can see. In WSL without a GPU-backed Vulkan ICD this falls
    /// back to lavapipe (software) -- fine for correctness parity, not for performance numbers
    /// (same caveat as glint/calico's own `enumerate`, ADR-0005).
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
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("siamese device"),
            required_features: wgpu::Features::empty(),
            required_limits: adapter.limits(),
            ..Default::default()
        }))
        .ok()?;
        Some(GpuContext {
            device,
            queue,
            backend: info.backend,
            adapter_name: info.name,
        })
    }
}

fn read_back_f32(
    ctx: &GpuContext,
    mut encoder: wgpu::CommandEncoder,
    buf: &wgpu::Buffer,
    len: usize,
) -> Vec<f32> {
    let size = (len * std::mem::size_of::<f32>()) as u64;
    let staging = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("siamese readback"),
        size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_buffer_to_buffer(buf, 0, &staging, 0, size);
    ctx.queue.submit(Some(encoder.finish()));

    let slice = staging.slice(..);
    slice.map_async(wgpu::MapMode::Read, |_| {});
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("device poll failed");
    let data = slice
        .get_mapped_range()
        .expect("buffer mapping failed")
        .to_vec();
    staging.unmap();
    bytemuck::cast_slice(&data).to_vec()
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct LinearGradientParams {
    width: u32,
    height: u32,
    invert: u32,
    _pad: u32,
    p0x: f32,
    p0y: f32,
    p1x: f32,
    p1y: f32,
}

const LINEAR_GRADIENT_WGSL: &str = include_str!("../shaders/gradient_linear.wgsl");

pub fn run_rasterize_linear_gradient(
    ctx: &GpuContext,
    width: usize,
    height: usize,
    p0: (f32, f32),
    p1: (f32, f32),
    invert: bool,
) -> Vec<f32> {
    let params = LinearGradientParams {
        width: width as u32,
        height: height as u32,
        invert: invert as u32,
        _pad: 0,
        p0x: p0.0,
        p0y: p0.1,
        p1x: p1.0,
        p1y: p1.1,
    };
    run_field_kernel(
        ctx,
        LINEAR_GRADIENT_WGSL,
        "rasterize_linear_gradient",
        &params,
        width * height,
    )
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct RadialGradientParams {
    width: u32,
    height: u32,
    invert: u32,
    _pad: u32,
    center_x: f32,
    center_y: f32,
    radius_x: f32,
    radius_y: f32,
    angle: f32,
    feather: f32,
    _pad2: f32,
    _pad3: f32,
}

const RADIAL_GRADIENT_WGSL: &str = include_str!("../shaders/gradient_radial.wgsl");

#[allow(clippy::too_many_arguments)]
pub fn run_rasterize_radial_gradient(
    ctx: &GpuContext,
    width: usize,
    height: usize,
    center: (f32, f32),
    radii: (f32, f32),
    angle: f32,
    feather: f32,
    invert: bool,
) -> Vec<f32> {
    let params = RadialGradientParams {
        width: width as u32,
        height: height as u32,
        invert: invert as u32,
        _pad: 0,
        center_x: center.0,
        center_y: center.1,
        radius_x: radii.0,
        radius_y: radii.1,
        angle,
        feather,
        _pad2: 0.0,
        _pad3: 0.0,
    };
    run_field_kernel(
        ctx,
        RADIAL_GRADIENT_WGSL,
        "rasterize_radial_gradient",
        &params,
        width * height,
    )
}

/// Shared dispatch for the single-uniform-plus-single-output-field kernels (both gradient
/// rasterizers) -- binding 0 is the uniform params, binding 1 is the `f32` output field.
fn run_field_kernel<P: Pod>(
    ctx: &GpuContext,
    wgsl: &str,
    entry_point: &str,
    params: &P,
    pixel_count: usize,
) -> Vec<f32> {
    let device = &ctx.device;
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(entry_point),
        source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(wgsl)),
    });
    let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some(entry_point),
        layout: None,
        module: &module,
        entry_point: Some(entry_point),
        compilation_options: wgpu::PipelineCompilationOptions::default(),
        cache: None,
    });

    let params_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("params"),
        contents: bytemuck::bytes_of(params),
        usage: wgpu::BufferUsages::UNIFORM,
    });
    let out_size = (pixel_count * std::mem::size_of::<f32>()) as u64;
    let out_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("out_field"),
        size: out_size,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });

    let layout = pipeline.get_bind_group_layout(0);
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("field kernel bind group"),
        layout: &layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: params_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: out_buf.as_entire_binding(),
            },
        ],
    });

    let (wg_x, wg_y) = workgroup_grid((pixel_count as u32).div_ceil(WORKGROUP_SIZE));
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("siamese encoder"),
    });
    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some(entry_point),
            timestamp_writes: None,
        });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.dispatch_workgroups(wg_x, wg_y, 1);
    }
    read_back_f32(ctx, encoder, &out_buf, pixel_count)
}

/// One dab, GPU-side layout -- see `shaders/brush.wgsl`'s `GpuDab`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct GpuDab {
    pub center_x: f32,
    pub center_y: f32,
    pub radius: f32,
    pub feather: f32,
    pub flow: f32,
    pub stroke_id: f32,
    pub erase: f32,
    pub _pad: f32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct Dims {
    width: u32,
    height: u32,
    _pad0: u32,
    _pad1: u32,
}

const BRUSH_WGSL: &str = include_str!("../shaders/brush.wgsl");

/// Flattens `geometry::Stroke`s into the `GpuDab` list `run_rasterize_brush` expects, assigning
/// each stroke a sequential `stroke_id` in list order -- the ordering the WGSL kernel relies on to
/// fold add/erase strokes sequentially, matching `Geometry::Brush`'s own "strokes applied in list
/// order" CPU semantics.
pub fn dabs_from_strokes(strokes: &[crate::geometry::Stroke]) -> Vec<GpuDab> {
    strokes
        .iter()
        .enumerate()
        .flat_map(|(stroke_id, stroke)| {
            stroke.dabs.iter().map(move |dab| GpuDab {
                center_x: dab.center.0,
                center_y: dab.center.1,
                radius: dab.radius,
                feather: dab.feather,
                flow: dab.flow,
                stroke_id: stroke_id as f32,
                erase: if stroke.erase { 1.0 } else { 0.0 },
                _pad: 0.0,
            })
        })
        .collect()
}

/// `dabs` must already be sorted by `stroke_id` ascending -- see `shaders/brush.wgsl`'s header
/// comment for why the kernel relies on that ordering rather than re-sorting on GPU.
pub fn run_rasterize_brush(
    ctx: &GpuContext,
    width: usize,
    height: usize,
    dabs: &[GpuDab],
) -> Vec<f32> {
    let device = &ctx.device;
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("rasterize_brush"),
        source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(BRUSH_WGSL)),
    });
    let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("rasterize_brush"),
        layout: None,
        module: &module,
        entry_point: Some("rasterize_brush"),
        compilation_options: wgpu::PipelineCompilationOptions::default(),
        cache: None,
    });

    let dims = Dims {
        width: width as u32,
        height: height as u32,
        _pad0: 0,
        _pad1: 0,
    };
    let dims_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("dims"),
        contents: bytemuck::bytes_of(&dims),
        usage: wgpu::BufferUsages::UNIFORM,
    });
    // wgpu storage buffers must be non-empty; a mask with no strokes yet still needs a valid
    // (unread past arrayLength=0) buffer to bind.
    let dabs_or_placeholder: Vec<GpuDab> = if dabs.is_empty() {
        vec![GpuDab {
            center_x: 0.0,
            center_y: 0.0,
            radius: 0.0,
            feather: 0.0,
            flow: 0.0,
            stroke_id: -1.0,
            erase: 0.0,
            _pad: 0.0,
        }]
    } else {
        dabs.to_vec()
    };
    let dabs_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("dabs"),
        contents: bytemuck::cast_slice(&dabs_or_placeholder),
        usage: wgpu::BufferUsages::STORAGE,
    });
    let pixel_count = width * height;
    let out_size = (pixel_count * std::mem::size_of::<f32>()) as u64;
    let out_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("out_field"),
        size: out_size,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });

    let layout = pipeline.get_bind_group_layout(0);
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("brush bind group"),
        layout: &layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: dims_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: dabs_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: out_buf.as_entire_binding(),
            },
        ],
    });

    let (wg_x, wg_y) = workgroup_grid((pixel_count as u32).div_ceil(WORKGROUP_SIZE));
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("siamese encoder"),
    });
    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("rasterize_brush"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.dispatch_workgroups(wg_x, wg_y, 1);
    }
    // An empty `dabs` still dispatches against the placeholder (stroke_id -1.0, matched by no
    // pixel), so the GPU path is exercised the same way for an empty mask as a populated one.
    read_back_f32(ctx, encoder, &out_buf, pixel_count)
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct ComposeParams {
    width: u32,
    height: u32,
    invert: u32,
    op: u32,
    opacity: f32,
    _pad0: f32,
    _pad1: f32,
    _pad2: f32,
}

const COMPOSE_WGSL: &str = include_str!("../shaders/compose.wgsl");

/// Matches `compose::Op`'s declared order: 0 = Add, 1 = Subtract, 2 = Intersect.
#[allow(clippy::too_many_arguments)]
pub fn run_compose_step(
    ctx: &GpuContext,
    width: usize,
    height: usize,
    running: &[f32],
    weight: &[f32],
    invert: bool,
    op: u32,
    opacity: f32,
) -> Vec<f32> {
    let device = &ctx.device;
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("compose_step"),
        source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(COMPOSE_WGSL)),
    });
    let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("compose_step"),
        layout: None,
        module: &module,
        entry_point: Some("compose_step"),
        compilation_options: wgpu::PipelineCompilationOptions::default(),
        cache: None,
    });

    let params = ComposeParams {
        width: width as u32,
        height: height as u32,
        invert: invert as u32,
        op,
        opacity,
        _pad0: 0.0,
        _pad1: 0.0,
        _pad2: 0.0,
    };
    let params_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("params"),
        contents: bytemuck::bytes_of(&params),
        usage: wgpu::BufferUsages::UNIFORM,
    });
    let running_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("running"),
        contents: bytemuck::cast_slice(running),
        usage: wgpu::BufferUsages::STORAGE,
    });
    let weight_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("weight"),
        contents: bytemuck::cast_slice(weight),
        usage: wgpu::BufferUsages::STORAGE,
    });
    let pixel_count = width * height;
    let out_size = (pixel_count * std::mem::size_of::<f32>()) as u64;
    let out_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("out_field"),
        size: out_size,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });

    let layout = pipeline.get_bind_group_layout(0);
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("compose bind group"),
        layout: &layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: params_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: running_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: weight_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: out_buf.as_entire_binding(),
            },
        ],
    });

    let (wg_x, wg_y) = workgroup_grid((pixel_count as u32).div_ceil(WORKGROUP_SIZE));
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("siamese encoder"),
    });
    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("compose_step"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.dispatch_workgroups(wg_x, wg_y, 1);
    }
    read_back_f32(ctx, encoder, &out_buf, pixel_count)
}

/// Matches `spikes/calico/src/gpu.rs::GpuPixel`'s shape exactly (`rgb: vec3<f32>, _pad: f32`).
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct GpuPixel {
    pub rgb: [f32; 3],
    pub _pad: f32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct AdjustParams {
    width: u32,
    height: u32,
    exposure_ev: f32,
    _pad: f32,
}

const MASKED_ADJUST_WGSL: &str = include_str!("../shaders/masked_adjust.wgsl");

pub fn run_masked_adjust_apply(
    ctx: &GpuContext,
    width: usize,
    height: usize,
    pixels: &[GpuPixel],
    mask: &[f32],
    exposure_ev: f32,
) -> Vec<GpuPixel> {
    let device = &ctx.device;
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("masked_adjust_apply"),
        source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(MASKED_ADJUST_WGSL)),
    });
    let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("masked_adjust_apply"),
        layout: None,
        module: &module,
        entry_point: Some("masked_adjust_apply"),
        compilation_options: wgpu::PipelineCompilationOptions::default(),
        cache: None,
    });

    let params = AdjustParams {
        width: width as u32,
        height: height as u32,
        exposure_ev,
        _pad: 0.0,
    };
    let params_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("params"),
        contents: bytemuck::bytes_of(&params),
        usage: wgpu::BufferUsages::UNIFORM,
    });
    let in_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("in_pixels"),
        contents: bytemuck::cast_slice(pixels),
        usage: wgpu::BufferUsages::STORAGE,
    });
    let mask_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("mask"),
        contents: bytemuck::cast_slice(mask),
        usage: wgpu::BufferUsages::STORAGE,
    });
    let out_size = std::mem::size_of_val(pixels) as u64;
    let out_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("out_pixels"),
        size: out_size,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });

    let layout = pipeline.get_bind_group_layout(0);
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("masked adjust bind group"),
        layout: &layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: params_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: in_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: mask_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: out_buf.as_entire_binding(),
            },
        ],
    });

    let pixel_count = width * height;
    let (wg_x, wg_y) = workgroup_grid((pixel_count as u32).div_ceil(WORKGROUP_SIZE));
    let staging = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: out_size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("siamese encoder"),
    });
    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("masked_adjust_apply"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.dispatch_workgroups(wg_x, wg_y, 1);
    }
    encoder.copy_buffer_to_buffer(&out_buf, 0, &staging, 0, out_size);
    ctx.queue.submit(Some(encoder.finish()));

    let slice = staging.slice(..);
    slice.map_async(wgpu::MapMode::Read, |_| {});
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("device poll failed");
    let data = slice
        .get_mapped_range()
        .expect("buffer mapping failed")
        .to_vec();
    staging.unmap();
    bytemuck::cast_slice(&data).to_vec()
}
