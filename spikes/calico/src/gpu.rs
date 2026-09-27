//! GPU port of `huesatmap.rs`'s single-map sample+apply, as a 3D-texture wgpu compute kernel --
//! the part of ADR-0038's Candidates the plan flagged as needing a real GPU-pattern validation
//! (glint's own kernels are storage-buffer-only, see `spikes/glint/src/gpu.rs`'s scoping note;
//! no 3D-texture pattern existed anywhere in this repo before this spike).
//!
//! Deliberately narrower than the full CPU `pipeline.rs`: dual-illuminant map blending and the
//! hue-wrap-aware angle interpolation at a wraparound-discontinuous hue-shift value stay CPU-only
//! (a naive GPU trilinear-filtered sample of angle values doesn't do shortest-path interpolation
//! across the 360/0 seam -- see `shaders/color.wgsl`'s header comment). The matrix stages
//! (camera->XYZ, working-space conversion) are ordinary 3x3 multiplies with no GPU-specific risk,
//! so porting the *whole* pipeline wasn't where this research's GPU-feasibility question actually
//! lived.

use std::borrow::Cow;

use bytemuck::{Pod, Zeroable};

use crate::huesatmap::HueSatMap;

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct GpuPixel {
    pub rgb: [f32; 3],
    pub _pad: f32,
}

pub const COLOR_WGSL: &str = include_str!("../shaders/color.wgsl");
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
    /// (same caveat as glint's `GpuContext::enumerate`, ADR-0016).
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
            label: Some("calico device"),
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

fn upload_hue_sat_texture(ctx: &GpuContext, map: &HueSatMap) -> (wgpu::TextureView, wgpu::Sampler) {
    // `map.data`'s natural memory order is value-outermost/hue-middle/saturation-innermost (see
    // huesatmap.rs's module doc -- this is the DNG SDK's own on-disk order, not a choice made
    // here). A `write_texture` upload is row-major with the texture's *width* varying fastest, so
    // texture width maps to saturation (not hue) to upload `map.data` byte-for-byte with no
    // transpose; the shader and sampler address modes below follow the same axis assignment.
    let size = wgpu::Extent3d {
        width: map.sat_divisions as u32,
        height: map.hue_divisions as u32,
        depth_or_array_layers: map.val_divisions as u32,
    };
    let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("hue_sat_map"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D3,
        format: wgpu::TextureFormat::Rgba16Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });

    let texels: Vec<half::f16> = map
        .data
        .iter()
        .flat_map(|entry| {
            [
                half::f16::from_f32(entry[0]),
                half::f16::from_f32(entry[1]),
                half::f16::from_f32(entry[2]),
                half::f16::from_f32(0.0),
            ]
        })
        .collect();
    let bytes: &[u8] = bytemuck::cast_slice(&texels);

    ctx.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        bytes,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(map.sat_divisions as u32 * 8), // 4 x f16 = 8 bytes/texel
            rows_per_image: Some(map.hue_divisions as u32),
        },
        size,
    );

    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let sampler = ctx.device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("hue_sat_sampler"),
        address_mode_u: wgpu::AddressMode::ClampToEdge, // saturation
        address_mode_v: wgpu::AddressMode::Repeat,      // hue wraps
        address_mode_w: wgpu::AddressMode::ClampToEdge, // value
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    (view, sampler)
}

/// Runs `apply_hue_sat_map` over `pixels` (already gamma-encoded linear-ProPhoto RGB, per
/// `pipeline.rs`'s stage order) against a single [`HueSatMap`] and returns the result in the same
/// layout.
pub fn run_apply_hue_sat_map(
    ctx: &GpuContext,
    pixels: &[GpuPixel],
    map: &HueSatMap,
) -> Vec<GpuPixel> {
    let device = &ctx.device;
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("color"),
        source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(COLOR_WGSL)),
    });
    let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("apply_hue_sat_map"),
        layout: None,
        module: &module,
        entry_point: Some("apply_hue_sat_map"),
        compilation_options: wgpu::PipelineCompilationOptions::default(),
        cache: None,
    });

    use wgpu::util::DeviceExt;
    let input_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("color input"),
        contents: bytemuck::cast_slice(pixels),
        usage: wgpu::BufferUsages::STORAGE,
    });
    let output_size = std::mem::size_of_val(pixels) as u64;
    let output_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("color output"),
        size: output_size,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let (view, sampler) = upload_hue_sat_texture(ctx, map);

    let bind_group_layout = pipeline.get_bind_group_layout(0);
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("color bind group"),
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
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: wgpu::BindingResource::Sampler(&sampler),
            },
        ],
    });

    let (wg_x, wg_y) = workgroup_grid((pixels.len() as u32).div_ceil(WORKGROUP_SIZE));
    let staging_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("color readback"),
        size: output_size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("calico encoder"),
    });
    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("apply_hue_sat_map pass"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.dispatch_workgroups(wg_x, wg_y, 1);
    }
    encoder.copy_buffer_to_buffer(&output_buf, 0, &staging_buf, 0, output_size);
    ctx.queue.submit(Some(encoder.finish()));

    let slice = staging_buf.slice(..);
    slice.map_async(wgpu::MapMode::Read, |_| {});
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("device poll failed");
    let data = slice
        .get_mapped_range()
        .expect("buffer mapping failed")
        .to_vec();
    staging_buf.unmap();

    bytemuck::cast_slice(&data).to_vec()
}
