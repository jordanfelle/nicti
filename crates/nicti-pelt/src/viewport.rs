//! Paints a Tapetum `FrameTexture` into an egui panel via `egui_wgpu::CallbackTrait` -- the
//! pattern ADR-0068 names (`spikes/pelt-egui/src/viewport.rs`'s `prepare`/`paint` split, itself
//! mirroring upstream egui's `custom3d_wgpu` demo), adapted to bind a real GPU-resident texture
//! (`FrameTexture::view`) instead of that spike's own buffer-to-`Rgba32Float`-texture copy. The
//! render pipeline's color-target format is threaded through from `RenderState::target_format`
//! (never hardcoded -- a surface reports `Rgba8Unorm` or `Bgra8Unorm*Srgb` depending on platform/
//! driver, and a mismatched pipeline target format is a wgpu validation panic).

use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use egui_wgpu::{CallbackResources, CallbackTrait};
use nicti_tapetum::color;
use nicti_tapetum::frame::FrameTexture;

const DISPLAY_WGSL: &str = include_str!("../shaders/display.wgsl");

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct DisplayUniforms {
    col0: [f32; 4],
    col1: [f32; 4],
    col2: [f32; 4],
    apply_oetf: u32,
    _pad: [u32; 3],
}

/// Built once against eframe's shared device (`PeltApp::new`), then reused every frame. The
/// working-space -> sRGB matrix and whether to apply the OETF in-shader are both fixed at
/// construction (the matrix never changes -- the working space is always linear ProPhoto, see
/// `nicti_tapetum::color`'s own doc comment; the target format doesn't change after the window is
/// created) and written into `uniform_buf` on the very first `prepare` call. Only `bind_group` is
/// rebuilt every render, since the `FrameTexture` it points at can change between renders (a
/// fresh texture from `Renderer`'s own cache).
pub struct ViewportResources {
    pipeline: wgpu::RenderPipeline,
    uniform_buf: wgpu::Buffer,
    uniforms: DisplayUniforms,
    bind_group: Option<wgpu::BindGroup>,
}

impl ViewportResources {
    pub fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("nicti-pelt display"),
            source: wgpu::ShaderSource::Wgsl(DISPLAY_WGSL.into()),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("nicti-pelt display pipeline"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
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
        let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("nicti-pelt display uniforms"),
            size: std::mem::size_of::<DisplayUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // A target format's own `*Srgb` variant already applies the sRGB OETF on write -- the
        // shader must skip applying it a second time (double-gamma), matching
        // `nicti_tapetum::geometry::output_encode`'s CPU reference only when the target format
        // itself is non-sRGB.
        let apply_oetf = !target_format.is_srgb();
        let matrix = color::prophoto_to_srgb_linear_matrix();
        let uniforms = DisplayUniforms {
            col0: [matrix[0][0], matrix[1][0], matrix[2][0], 0.0],
            col1: [matrix[0][1], matrix[1][1], matrix[2][1], 0.0],
            col2: [matrix[0][2], matrix[1][2], matrix[2][2], 0.0],
            apply_oetf: apply_oetf as u32,
            _pad: [0; 3],
        };

        Self {
            pipeline,
            uniform_buf,
            uniforms,
            bind_group: None,
        }
    }
}

/// One frame's paint: which `FrameTexture` to display. Constructed fresh each `ui()` call with
/// the develop view's current render output.
pub struct ViewportCallback {
    pub frame: Arc<FrameTexture>,
}

impl CallbackTrait for ViewportCallback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen_descriptor: &egui_wgpu::ScreenDescriptor,
        _encoder: &mut wgpu::CommandEncoder,
        callback_resources: &mut CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let Some(res) = callback_resources.get_mut::<ViewportResources>() else {
            return Vec::new();
        };

        queue.write_buffer(&res.uniform_buf, 0, bytemuck::bytes_of(&res.uniforms));

        let bind_group_layout = res.pipeline.get_bind_group_layout(0);
        res.bind_group = Some(device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("nicti-pelt display bind group"),
            layout: &bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&self.frame.view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: res.uniform_buf.as_entire_binding(),
                },
            ],
        }));

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
        let Some(bind_group) = res.bind_group.as_ref() else {
            return;
        };
        render_pass.set_pipeline(&res.pipeline);
        render_pass.set_bind_group(0, bind_group, &[]);
        render_pass.draw(0..3, 0..1);
    }
}
