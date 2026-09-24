//! Custom wgpu viewport for the `pelt-iced` spike, via `iced::widget::shader`'s
//! `Primitive`/`Pipeline` traits. Same `live_chain` compute -> texture -> fullscreen-triangle
//! shape as `pelt-egui`'s `viewport.rs`, adapted to iced's split API: `Pipeline::new` builds
//! GPU state once (keyed by primitive type, iced's own caching), and `Primitive::prepare`/`draw`
//! run every frame. Unlike egui_wgpu's `CallbackTrait::prepare` (which is handed egui's shared
//! command encoder), iced's `Primitive::prepare` gets no encoder -- the compute dispatch here
//! creates and submits its own, a real, iced-imposed structural difference worth noting in
//! ADR-0006, not a design choice made by this spike.
//!
//! **wgpu version note:** this crate depends on `wgpu = "27"` (not `"30"`, unlike `pelt-egui` and
//! ADR-0005), because `iced_wgpu` 0.14.0 itself pins wgpu 27.0.1 -- see that crate's own
//! `Cargo.lock`, checked 2026-09-23. Rust's type system requires the exact same `wgpu` crate
//! version to unify `wgpu::Device`/`wgpu::RenderPass` types between this file and iced's
//! internals; a `wgpu = "30"` dependency here would simply fail to compile against iced's API,
//! not silently degrade. This is real, load-bearing evidence for ADR-0006's gate on wgpu-version
//! compatibility with ADR-0005's wgpu-30 choice, not a hypothetical.

use iced::widget::shader::{self, Viewport};
use iced::Rectangle;
use pelt::live_chain::{LiveChainParams, LIVE_CHAIN_WGSL};

const QUAD_WGSL: &str = include_str!("../shaders/quad.wgsl");
const WORKGROUP_SIZE: u32 = 64;

pub struct LiveChainPipeline {
    compute_pipeline: wgpu::ComputePipeline,
    render_pipeline: wgpu::RenderPipeline,
    input_buf: wgpu::Buffer,
    output_buf: wgpu::Buffer,
    params_buf: wgpu::Buffer,
    compute_bind_group: wgpu::BindGroup,
    output_texture: wgpu::Texture,
    render_bind_group: wgpu::BindGroup,
    width: u32,
    height: u32,
    pixel_count: u32,
}

impl shader::Pipeline for LiveChainPipeline {
    fn new(device: &wgpu::Device, queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        let width = pelt::config::VIEWPORT_WIDTH;
        let height = pelt::config::VIEWPORT_HEIGHT;
        let pixel_count = width * height;
        let pixel_bytes = (pixel_count as u64) * 16;

        let compute_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pelt-iced live_chain"),
            source: wgpu::ShaderSource::Wgsl(LIVE_CHAIN_WGSL.into()),
        });
        let compute_pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("pelt-iced live_chain pipeline"),
            layout: None,
            module: &compute_module,
            entry_point: Some("live_chain"),
            compilation_options: Default::default(),
            cache: None,
        });

        let input_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pelt-iced live_chain input"),
            size: pixel_bytes,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(
            &input_buf,
            0,
            bytemuck::cast_slice(&pelt::live_chain::generate_viewport_frame()),
        );
        let output_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pelt-iced live_chain output"),
            size: pixel_bytes,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let params_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pelt-iced live_chain params"),
            size: std::mem::size_of::<LiveChainParams>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let compute_bgl = compute_pipeline.get_bind_group_layout(0);
        let compute_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pelt-iced live_chain bind group"),
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
            label: Some("pelt-iced viewport texture"),
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
            label: Some("pelt-iced quad"),
            source: wgpu::ShaderSource::Wgsl(QUAD_WGSL.into()),
        });
        let render_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("pelt-iced quad pipeline"),
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
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let render_bgl = render_pipeline.get_bind_group_layout(0);
        let render_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pelt-iced quad bind group"),
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
            width,
            height,
            pixel_count,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct LiveChainPrimitive {
    pub params: LiveChainParams,
}

impl shader::Primitive for LiveChainPrimitive {
    type Pipeline = LiveChainPipeline;

    fn prepare(
        &self,
        pipeline: &mut Self::Pipeline,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _bounds: &Rectangle,
        _viewport: &Viewport,
    ) {
        queue.write_buffer(&pipeline.params_buf, 0, bytemuck::bytes_of(&self.params));

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("pelt-iced live_chain encoder"),
        });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("pelt-iced live_chain pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&pipeline.compute_pipeline);
            pass.set_bind_group(0, &pipeline.compute_bind_group, &[]);
            let total_workgroups = pipeline.pixel_count.div_ceil(WORKGROUP_SIZE);
            pass.dispatch_workgroups(total_workgroups, 1, 1);
        }
        let bytes_per_row = pipeline.width * 16;
        encoder.copy_buffer_to_texture(
            wgpu::TexelCopyBufferInfo {
                buffer: &pipeline.output_buf,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(pipeline.height),
                },
            },
            pipeline.output_texture.as_image_copy(),
            wgpu::Extent3d {
                width: pipeline.width,
                height: pipeline.height,
                depth_or_array_layers: 1,
            },
        );
        queue.submit(Some(encoder.finish()));
        let _ = &pipeline.input_buf; // uploaded once at Pipeline::new; kept alive here.
    }

    fn draw(&self, pipeline: &Self::Pipeline, render_pass: &mut wgpu::RenderPass<'_>) -> bool {
        render_pass.set_pipeline(&pipeline.render_pipeline);
        render_pass.set_bind_group(0, &pipeline.render_bind_group, &[]);
        render_pass.draw(0..3, 0..1);
        true
    }
}
