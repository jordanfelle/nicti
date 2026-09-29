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
    /// Screen-UV-to-texture-UV scale/offset (#31 phase 3) -- see `display.wgsl`'s own header
    /// comment for the derivation. Placed before the scalar tail, matching WGSL's own layout, so
    /// neither side needs an alignment-bump pad (both `[f32; 2]`/`vec2<f32>` already land on an
    /// 8-byte-aligned offset here without one).
    view_scale: [f32; 2],
    view_offset: [f32; 2],
    apply_oetf: u32,
    _pad: [u32; 3],
}

/// Built once against eframe's shared device (`PeltApp::new`), then reused every frame. The
/// working-space -> sRGB matrix and whether to apply the OETF in-shader are both fixed at
/// construction (the matrix never changes -- the working space is always linear ProPhoto, see
/// `nicti_tapetum::color`'s own doc comment; the target format doesn't change after the window is
/// created); `view_scale`/`view_offset` (#31 phase 3) default to `[1,1]`/`[0,0]` here and are
/// overwritten by whatever the current `ViewportCallback` carries on every `prepare` call, so a
/// caller (the Develop tab) that never sets them keeps exactly the old always-1:1-stretch
/// behavior. Only `bind_group` is rebuilt every render, since the `FrameTexture` it points at can
/// change between renders (a fresh texture from `Renderer`'s own cache).
pub struct ViewportResources {
    pipeline: wgpu::RenderPipeline,
    uniform_buf: wgpu::Buffer,
    uniforms: DisplayUniforms,
    sampler: wgpu::Sampler,
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
        // Bilinear + clamp-to-edge: correct for both the Develop tab's always-1:1 mapping (no
        // resampling actually happens there, see `display.wgsl`'s header comment) and the Loupe
        // view's "Fit"/zoomed sampling. Clamp-to-edge is moot in practice -- the shader's own
        // explicit `[0,1]` bounds check (letterbox background) means a sampled UV is never
        // actually outside range -- but is the least-surprising default for a `Sampler` regardless.
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("nicti-pelt display sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
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
            view_scale: [1.0, 1.0],
            view_offset: [0.0, 0.0],
            apply_oetf: apply_oetf as u32,
            _pad: [0; 3],
        };

        Self {
            pipeline,
            uniform_buf,
            uniforms,
            sampler,
            bind_group: None,
        }
    }
}

/// One frame's paint: which `FrameTexture` to display, and (#31 phase 3) at what screen-UV-to-
/// texture-UV scale/offset -- see `display.wgsl`'s own header comment for the derivation.
/// Constructed fresh each `ui()` call with the develop view's current render output;
/// `view_scale`/`view_offset` default to `[1,1]`/`[0,0]` via [`Self::identity`], reproducing the
/// old always-1:1-stretch mapping exactly, for a caller (the Develop tab) that doesn't need zoom.
pub struct ViewportCallback {
    pub frame: Arc<FrameTexture>,
    pub view_scale: [f32; 2],
    pub view_offset: [f32; 2],
}

impl ViewportCallback {
    pub fn identity(frame: Arc<FrameTexture>) -> Self {
        Self {
            frame,
            view_scale: [1.0, 1.0],
            view_offset: [0.0, 0.0],
        }
    }
}

/// The `view_scale` for "Fit": the source image maximized within `rect_size`, aspect-correct, no
/// cropping -- the axis whose own aspect is the tighter constraint gets `1.0` (no letterbox), the
/// other gets scaled up so the excess maps outside `[0,1]` texture UV (rendered as the letterbox
/// background by `display.wgsl`'s own bounds check) rather than the image being cropped or
/// stretched. Pure and GPU-independent -- see the `tests` module below for the derivation's actual
/// worked cases.
pub fn fit_scale(rect_size: (f32, f32), tex_size: (f32, f32)) -> [f32; 2] {
    let rect_aspect = rect_size.0 / rect_size.1;
    let tex_aspect = tex_size.0 / tex_size.1;
    [
        (rect_aspect / tex_aspect).max(1.0),
        (tex_aspect / rect_aspect).max(1.0),
    ]
}

/// The `view_scale` for "100%": exactly one texture texel maps to one screen pixel on each axis.
pub fn one_to_one_scale(rect_size_px: (f32, f32), tex_size_px: (f32, f32)) -> [f32; 2] {
    [
        rect_size_px.0 / tex_size_px.0,
        rect_size_px.1 / tex_size_px.1,
    ]
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

        let mut uniforms = res.uniforms;
        uniforms.view_scale = self.view_scale;
        uniforms.view_offset = self.view_offset;
        queue.write_buffer(&res.uniform_buf, 0, bytemuck::bytes_of(&uniforms));

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
                    resource: wgpu::BindingResource::Sampler(&res.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
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

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_tapetum::frame::Extent;
    use nicti_tapetum::gpu::{GpuContext, GpuPreference};

    /// Regression test for a real bug caught in an earlier PR's own adversarial review: an
    /// earlier version of `display.wgsl`'s `Uniforms` struct trailed with a `_pad: vec3<u32>`,
    /// which WGSL's uniform-address-space layout rules give align 16 (same as any vecN) --
    /// pushing the shader's *reflected* struct size 16 bytes past `DisplayUniforms`'s actual
    /// `repr(C)` layout (plain `[u32; 3]` has no such alignment bump). Since the pipeline's bind
    /// group layout is auto-derived (`layout: None`) from that reflected shader struct,
    /// `create_bind_group`'s min-binding-size validation against a mismatched-size buffer would
    /// panic the first time the Develop tab actually painted -- a path no unit test previously
    /// exercised (`nicti-pelt` had no tests at all). This builds the exact same pipeline/buffer/
    /// bind-group `ViewportCallback::prepare` builds (three bindings now, #31 phase 3 added the
    /// sampler), against a real device, so a future layout drift between the Rust struct and the
    /// WGSL struct fails here instead of only in a live app.
    #[test]
    fn bind_group_creation_matches_the_shaders_reflected_uniform_layout() {
        let Some(gpu) = GpuContext::new(GpuPreference::Auto).ok() else {
            eprintln!("no wgpu adapter available in this environment, skipping");
            return;
        };

        let target_format = wgpu::TextureFormat::Rgba8Unorm;
        let mut resources = ViewportResources::new(&gpu.device, target_format);
        let frame = Arc::new(FrameTexture::new(
            &gpu,
            Extent {
                width: 4,
                height: 4,
            },
        ));

        gpu.queue.write_buffer(
            &resources.uniform_buf,
            0,
            bytemuck::bytes_of(&resources.uniforms),
        );
        let bind_group_layout = resources.pipeline.get_bind_group_layout(0);
        resources.bind_group = Some(gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("test bind group"),
            layout: &bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&frame.view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&resources.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: resources.uniform_buf.as_entire_binding(),
                },
            ],
        }));

        // wgpu's default uncaptured-error handler panics synchronously on a validation error
        // (e.g. the min-binding-size mismatch this test guards against) during device.poll --
        // reaching this line at all, with a real bind group produced, is the assertion.
        gpu.device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("device poll failed");
        assert!(resources.bind_group.is_some());
    }

    #[test]
    fn fit_scale_is_identity_when_rect_and_image_aspect_match() {
        assert_eq!(fit_scale((800.0, 600.0), (1600.0, 1200.0)), [1.0, 1.0]);
    }

    #[test]
    fn fit_scale_letterboxes_the_narrower_axis_for_a_wider_rect() {
        // Rect is wider (2:1) than the image (4:3, ~1.33:1) -- image height fills the rect, width
        // is letterboxed, so X gets scaled up (bars appear outside [0,1] on X) and Y stays 1.0.
        let scale = fit_scale((1000.0, 500.0), (400.0, 300.0));
        assert!((scale[0] - (2.0 / (4.0 / 3.0))).abs() < 1e-6);
        assert_eq!(scale[1], 1.0);
    }

    #[test]
    fn fit_scale_letterboxes_the_narrower_axis_for_a_taller_rect() {
        // Rect is narrower/taller than the image -- symmetric case, Y gets scaled up instead.
        let scale = fit_scale((300.0, 1000.0), (400.0, 300.0));
        assert_eq!(scale[0], 1.0);
        assert!((scale[1] - ((4.0 / 3.0) / (300.0 / 1000.0))).abs() < 1e-6);
    }

    #[test]
    fn one_to_one_scale_is_the_pixel_ratio_per_axis() {
        assert_eq!(
            one_to_one_scale((800.0, 600.0), (1600.0, 1200.0)),
            [0.5, 0.5]
        );
        assert_eq!(
            one_to_one_scale((1600.0, 1200.0), (1600.0, 1200.0)),
            [1.0, 1.0]
        );
    }
}
