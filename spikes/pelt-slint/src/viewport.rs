//! Live-chain compute + convert-to-Rgba8Unorm for the `pelt-slint` spike. Unlike `pelt-egui`
//! (blit into a texture egui itself samples) and `pelt-iced` (draw into iced's own render pass),
//! Slint's integration point is `slint::Image::try_from(wgpu::Texture)` -- it takes ownership of
//! a texture and imports it into the scene directly, GPU-resident, no CPU readback (see
//! `i-slint-core`'s `wgpu_30.rs`: only `Rgba8Unorm`/`Rgba8UnormSrgb` with
//! `TEXTURE_BINDING | RENDER_ATTACHMENT` usage are accepted). `live_chain` itself still writes
//! `[f32; 4]` into a storage buffer (identical to `pelt-egui`/`pelt-iced`/`spikes/glint`); a
//! second render pass (the same fullscreen-triangle shape as the other two spikes' `quad.wgsl`)
//! reads that buffer's copied-out texture and writes tonemapped Rgba8Unorm, which is the texture
//! actually handed to Slint. Built once against the exact `wgpu::Device`/`Queue` Slint itself
//! renders with, obtained via `slint::Window::set_rendering_notifier`'s `RenderingSetup` state --
//! see `main.rs`. Uses `wgpu = "30"`, matching ADR-0005 exactly (Slint's own `femtovg-wgpu`
//! feature is literally named `wgpu-30`, see `docs/adr/0006-gui-framework.md`).

use pelt::live_chain::{LiveChainParams, LIVE_CHAIN_WGSL};

const QUAD_WGSL: &str = include_str!("../shaders/quad.wgsl");
const WORKGROUP_SIZE: u32 = 64;

pub struct ViewportRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    compute_pipeline: wgpu::ComputePipeline,
    render_pipeline: wgpu::RenderPipeline,
    input_buf: wgpu::Buffer,
    output_buf: wgpu::Buffer,
    params_buf: wgpu::Buffer,
    compute_bind_group: wgpu::BindGroup,
    intermediate_texture: wgpu::Texture,
    render_bind_group: wgpu::BindGroup,
    width: u32,
    height: u32,
    pixel_count: u32,
}

impl ViewportRenderer {
    pub fn new(device: wgpu::Device, queue: wgpu::Queue, width: u32, height: u32) -> Self {
        let pixel_count = width * height;
        let pixel_bytes = (pixel_count as u64) * 16;

        let compute_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pelt-slint live_chain"),
            source: wgpu::ShaderSource::Wgsl(LIVE_CHAIN_WGSL.into()),
        });
        let compute_pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("pelt-slint live_chain pipeline"),
            layout: None,
            module: &compute_module,
            entry_point: Some("live_chain"),
            compilation_options: Default::default(),
            cache: None,
        });

        let input_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pelt-slint live_chain input"),
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
            label: Some("pelt-slint live_chain output"),
            size: pixel_bytes,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let params_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pelt-slint live_chain params"),
            size: std::mem::size_of::<LiveChainParams>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let compute_bgl = compute_pipeline.get_bind_group_layout(0);
        let compute_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pelt-slint live_chain bind group"),
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

        // Intermediate: the raw live_chain output copied out of the storage buffer into a
        // sampleable texture. Not the texture handed to Slint -- see `intermediate_to_scene`.
        let intermediate_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("pelt-slint intermediate texture"),
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
        let intermediate_view =
            intermediate_texture.create_view(&wgpu::TextureViewDescriptor::default());

        let render_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pelt-slint quad"),
            source: wgpu::ShaderSource::Wgsl(QUAD_WGSL.into()),
        });
        let render_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("pelt-slint quad pipeline"),
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
                // Rgba8Unorm + TEXTURE_BINDING | RENDER_ATTACHMENT is one of only two formats
                // slint::Image::try_from(wgpu::Texture) accepts (i-slint-core's wgpu_30.rs).
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8Unorm,
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
            label: Some("pelt-slint quad bind group"),
            layout: &render_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&intermediate_view),
            }],
        });

        Self {
            device,
            queue,
            compute_pipeline,
            render_pipeline,
            input_buf,
            output_buf,
            params_buf,
            compute_bind_group,
            intermediate_texture,
            render_bind_group,
            width,
            height,
            pixel_count,
        }
    }

    /// Dispatches `live_chain` with `params`, then renders the result into a fresh
    /// `Rgba8Unorm` texture (`TEXTURE_BINDING | RENDER_ATTACHMENT`) ready for
    /// `slint::Image::try_from`. A new output texture per call, since `Image::try_from` takes
    /// ownership -- acceptable at this spike's frame rate, but a real Tapetum integration would
    /// want a small ring of reusable textures instead of an allocation per frame.
    pub fn render_frame(&self, params: LiveChainParams) -> wgpu::Texture {
        self.queue
            .write_buffer(&self.params_buf, 0, bytemuck::bytes_of(&params));

        let scene_texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("pelt-slint scene texture"),
            size: wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let scene_view = scene_texture.create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("pelt-slint frame encoder"),
            });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("pelt-slint live_chain pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.compute_pipeline);
            pass.set_bind_group(0, &self.compute_bind_group, &[]);
            let total_workgroups = self.pixel_count.div_ceil(WORKGROUP_SIZE);
            pass.dispatch_workgroups(total_workgroups, 1, 1);
        }
        let bytes_per_row = self.width * 16;
        encoder.copy_buffer_to_texture(
            wgpu::TexelCopyBufferInfo {
                buffer: &self.output_buf,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(self.height),
                },
            },
            self.intermediate_texture.as_image_copy(),
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
        {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("pelt-slint scene pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &scene_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            render_pass.set_pipeline(&self.render_pipeline);
            render_pass.set_bind_group(0, &self.render_bind_group, &[]);
            render_pass.draw(0..3, 0..1);
        }
        self.queue.submit(Some(encoder.finish()));

        let _ = &self.input_buf; // uploaded once at construction; kept alive here.
        scene_texture
    }
}
