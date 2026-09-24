//! Custom wgpu viewport for the `pelt-egui` spike: a `live_chain` compute dispatch (copied from
//! `spikes/glint`, see `pelt::live_chain`) writes into a storage buffer, which is copied into an
//! `Rgba32Float` texture and drawn as a fullscreen textured triangle inside egui's own render
//! pass. This is egui's standard `egui_wgpu::CallbackTrait` pattern (mirrors the upstream
//! `custom3d_wgpu` demo) -- the device/queue used here are eframe's own, obtained once via
//! `wgpu_render_state()` and never a second wgpu::Instance/Device created by this crate.

use egui_wgpu::{CallbackResources, CallbackTrait};
use pelt::live_chain::{LiveChainParams, LIVE_CHAIN_WGSL};

const QUAD_WGSL: &str = include_str!("../shaders/quad.wgsl");

/// GPU resources for the live-chain viewport, built once against eframe's shared device and
/// re-dispatched every frame with fresh params (slider/pan state) via [`ViewportCallback`].
pub struct ViewportResources {
    compute_pipeline: wgpu::ComputePipeline,
    render_pipeline: wgpu::RenderPipeline,
    input_buf: wgpu::Buffer,
    output_buf: wgpu::Buffer,
    params_buf: wgpu::Buffer,
    compute_bind_group: wgpu::BindGroup,
    output_texture: wgpu::Texture,
    render_bind_group: wgpu::BindGroup,
    pixel_count: u32,
    width: u32,
    height: u32,
}

const WORKGROUP_SIZE: u32 = 64;

impl ViewportResources {
    pub fn new(device: &wgpu::Device, width: u32, height: u32) -> Self {
        let pixel_count = width * height;

        let compute_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pelt-egui live_chain"),
            source: wgpu::ShaderSource::Wgsl(LIVE_CHAIN_WGSL.into()),
        });
        let compute_pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("pelt-egui live_chain pipeline"),
            layout: None,
            module: &compute_module,
            entry_point: Some("live_chain"),
            compilation_options: Default::default(),
            cache: None,
        });

        let pixel_bytes = (pixel_count as u64) * 16; // [f32; 4] per pixel.
        let input_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pelt-egui live_chain input"),
            size: pixel_bytes,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let output_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pelt-egui live_chain output"),
            size: pixel_bytes,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let params_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pelt-egui live_chain params"),
            size: std::mem::size_of::<LiveChainParams>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let compute_bgl = compute_pipeline.get_bind_group_layout(0);
        let compute_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pelt-egui live_chain bind group"),
            layout: &compute_bgl,
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

        let output_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("pelt-egui viewport texture"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba32Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let texture_view = output_texture.create_view(&wgpu::TextureViewDescriptor::default());

        let render_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pelt-egui quad"),
            source: wgpu::ShaderSource::Wgsl(QUAD_WGSL.into()),
        });
        let render_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("pelt-egui quad pipeline"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &render_module,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &render_module,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Bgra8Unorm,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let render_bgl = render_pipeline.get_bind_group_layout(0);
        let render_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pelt-egui quad bind group"),
            layout: &render_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&texture_view),
            }],
        });

        Self {
            compute_pipeline,
            render_pipeline,
            input_buf,
            output_buf,
            params_buf,
            compute_bind_group,
            output_texture,
            render_bind_group,
            pixel_count,
            width,
            height,
        }
    }

    pub fn upload_input(&self, queue: &wgpu::Queue, pixels: &[[f32; 4]]) {
        queue.write_buffer(&self.input_buf, 0, bytemuck::cast_slice(pixels));
    }
}

/// One dispatch of the live-chain compute pass followed by a buffer->texture copy, run from
/// [`CallbackTrait::prepare`] every frame with the current slider/pan-derived params.
pub struct ViewportCallback {
    pub params: LiveChainParams,
}

impl CallbackTrait for ViewportCallback {
    fn prepare(
        &self,
        _device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen_descriptor: &egui_wgpu::ScreenDescriptor,
        encoder: &mut wgpu::CommandEncoder,
        callback_resources: &mut CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let Some(res) = callback_resources.get::<ViewportResources>() else {
            return Vec::new();
        };

        queue.write_buffer(&res.params_buf, 0, bytemuck::bytes_of(&self.params));

        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("pelt-egui live_chain pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&res.compute_pipeline);
            pass.set_bind_group(0, &res.compute_bind_group, &[]);
            let total_workgroups = res.pixel_count.div_ceil(WORKGROUP_SIZE);
            // Single-dimension dispatch: this viewport is 1920x1080 (~2M/64 ~= 31.6k workgroups),
            // well under wgpu's 65535-per-dimension cap, unlike the hero-scenario 45MP resolution
            // that forced the 2D grid in spikes/glint/src/gpu.rs::workgroup_grid. No such split
            // needed here.
            pass.dispatch_workgroups(total_workgroups, 1, 1);
        }

        let bytes_per_row = res.width * 16;
        encoder.copy_buffer_to_texture(
            wgpu::TexelCopyBufferInfo {
                buffer: &res.output_buf,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(res.height),
                },
            },
            res.output_texture.as_image_copy(),
            wgpu::Extent3d {
                width: res.width,
                height: res.height,
                depth_or_array_layers: 1,
            },
        );

        Vec::new()
    }

    fn paint(
        &self,
        _info: egui::epaint::PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        callback_resources: &CallbackResources,
    ) {
        let Some(res) = callback_resources.get::<ViewportResources>() else {
            return;
        };
        render_pass.set_pipeline(&res.render_pipeline);
        render_pass.set_bind_group(0, &res.render_bind_group, &[]);
        render_pass.draw(0..3, 0..1);
    }
}
