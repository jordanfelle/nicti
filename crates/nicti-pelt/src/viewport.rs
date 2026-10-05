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
use half::f16;
use nicti_calico::space::OutputSpace;
use nicti_calico::transform::{DisplayKind, DisplayTransform, Lut3d, MatrixTrc};
use nicti_tapetum::frame::FrameTexture;

const DISPLAY_WGSL: &str = include_str!("../shaders/display.wgsl");

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct DisplayUniforms {
    col0: [f32; 4],
    col1: [f32; 4],
    col2: [f32; 4],
    /// Soft proof: linear ProPhoto -> proof-space linear RGB (`p*`) and back (`q*`), same
    /// column-per-vec4 layout as `col*`.
    pcol0: [f32; 4],
    pcol1: [f32; 4],
    pcol2: [f32; 4],
    qcol0: [f32; 4],
    qcol1: [f32; 4],
    qcol2: [f32; 4],
    /// Screen-UV-to-texture-UV scale/offset (#31 phase 3) -- see `display.wgsl`'s own header
    /// comment for the derivation. Placed before the scalar tail, matching WGSL's own layout, so
    /// neither side needs an alignment-bump pad (both `[f32; 2]`/`vec2<f32>` already land on an
    /// 8-byte-aligned offset here without one).
    view_scale: [f32; 2],
    view_offset: [f32; 2],
    /// 1 when the render target is an `*Srgb` format (see `display.wgsl`'s `target_srgb`).
    target_srgb: u32,
    /// 0 = exact matrix + transfer function, 1 = baked 3D LUT (`display.wgsl`'s `mode`).
    mode: u32,
    /// Mode 0 transfer function: 0 = sRGB curve, 1 = Adobe RGB gamma.
    trc: u32,
    /// 1 = tint out-of-proof-gamut pixels.
    gamut_warn: u32,
    /// 1 = run the proof stage (`p*`/`q*`).
    proof_enabled: u32,
    _pad: [u32; 3],
}

/// Built once against eframe's shared device (`PeltApp::new`), then reused every frame. The
/// display transform (ADR-0042) starts as exact sRGB and is swapped via
/// [`Self::set_display_transform`] when the monitor profile or proofing changes -- display-only,
/// so it never touches Tapetum's caches (the working space is always linear ProPhoto, see
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
    /// The display/proof LUT (ADR-0042): a 1^3 dummy in mode 0, else `LUT_SIZE`^3 Rgba16Float.
    lut_view: wgpu::TextureView,
    lut_sampler: wgpu::Sampler,
    /// Encode table for a matrix/TRC monitor (mode 2): a 1x1 dummy otherwise.
    trc_view: wgpu::TextureView,
    bind_group: Option<wgpu::BindGroup>,
    target_srgb: bool,
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
        // shader hands it linear values in that case (double-gamma otherwise), matching
        // `nicti_tapetum::geometry::output_encode`'s CPU reference (same calico matrix, #318).
        let target_srgb = target_format.is_srgb();
        let lut_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("nicti-pelt display LUT sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let uniforms = uniforms_for(
            &DisplayTransform::exact(OutputSpace::Srgb),
            false,
            target_srgb,
        );

        let mut this = Self {
            pipeline,
            uniform_buf,
            uniforms,
            sampler,
            lut_view: dummy_lut_view(device),
            lut_sampler,
            trc_view: dummy_trc_view(device),
            bind_group: None,
            target_srgb,
        };
        this.uniforms = uniforms;
        this
    }

    /// Swaps in a new display/proof transform (ADR-0042). Display-only: nothing here touches
    /// Tapetum's render graph or caches, so a monitor change or a proofing toggle costs no bake
    /// work. `view_scale`/`view_offset` are preserved (they are overwritten per-frame anyway).
    pub fn set_display_transform(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        transform: &DisplayTransform,
        gamut_warn: bool,
    ) {
        let mut u = uniforms_for(transform, gamut_warn, self.target_srgb);
        u.view_scale = self.uniforms.view_scale;
        u.view_offset = self.uniforms.view_offset;
        self.uniforms = u;
        self.lut_view = match &transform.kind {
            DisplayKind::Lut(lut) => upload_lut(device, queue, lut),
            _ => dummy_lut_view(device),
        };
        self.trc_view = match &transform.kind {
            DisplayKind::MatrixTrc(m) => upload_trc(device, queue, m),
            _ => dummy_trc_view(device),
        };
    }
}

/// Uniforms for `transform`. Pure (no GPU) so the mapping is unit-testable.
fn uniforms_for(
    transform: &DisplayTransform,
    gamut_warn: bool,
    target_srgb: bool,
) -> DisplayUniforms {
    // The display matrix: the built-in space's, the monitor's own (mode 2), or (unused in LUT
    // mode) sRGB's so the struct is always well-formed.
    let (space, mode, matrix) = match &transform.kind {
        DisplayKind::Space(s) => (*s, 0, s.from_working_f32()),
        DisplayKind::MatrixTrc(m) => (OutputSpace::Srgb, 2, m.from_working),
        DisplayKind::Lut(_) => (OutputSpace::Srgb, 1, OutputSpace::Srgb.from_working_f32()),
    };
    let cols = |m: [[f32; 3]; 3]| {
        [
            [m[0][0], m[1][0], m[2][0], 0.0],
            [m[0][1], m[1][1], m[2][1], 0.0],
            [m[0][2], m[1][2], m[2][2], 0.0],
        ]
    };
    let [col0, col1, col2] = cols(matrix);
    // Identity when not proofing (never read: `proof_enabled` is 0).
    let (p, q) = match transform.proof {
        Some(ps) => (ps.from_working_f32(), ps.to_working_f32()),
        None => ([[0.0; 3]; 3], [[0.0; 3]; 3]),
    };
    let [pcol0, pcol1, pcol2] = cols(p);
    let [qcol0, qcol1, qcol2] = cols(q);
    DisplayUniforms {
        col0,
        col1,
        col2,
        pcol0,
        pcol1,
        pcol2,
        qcol0,
        qcol1,
        qcol2,
        view_scale: [1.0, 1.0],
        view_offset: [0.0, 0.0],
        target_srgb: target_srgb as u32,
        mode,
        trc: (mode == 0 && space == OutputSpace::AdobeRgb) as u32,
        gamut_warn: (gamut_warn && transform.proof.is_some()) as u32,
        proof_enabled: transform.proof.is_some() as u32,
        _pad: [0; 3],
    }
}

fn dummy_trc_view(device: &wgpu::Device) -> wgpu::TextureView {
    device
        .create_texture(&wgpu::TextureDescriptor {
            label: Some("nicti-pelt display encode table (dummy)"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba16Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        })
        .create_view(&wgpu::TextureViewDescriptor::default())
}

fn upload_trc(device: &wgpu::Device, queue: &wgpu::Queue, m: &MatrixTrc) -> wgpu::TextureView {
    let n = m.encode[0].len() as u32;
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("nicti-pelt display encode table"),
        size: wgpu::Extent3d {
            width: n,
            height: 1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba16Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let texels: Vec<u16> = (0..n as usize)
        .flat_map(|i| {
            [m.encode[0][i], m.encode[1][i], m.encode[2][i], 1.0]
                .map(|v| f16::from_f32(v).to_bits())
        })
        .collect();
    queue.write_texture(
        texture.as_image_copy(),
        bytemuck::cast_slice(&texels),
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(n * 8),
            rows_per_image: Some(1),
        },
        wgpu::Extent3d {
            width: n,
            height: 1,
            depth_or_array_layers: 1,
        },
    );
    texture.create_view(&wgpu::TextureViewDescriptor::default())
}

fn dummy_lut_view(device: &wgpu::Device) -> wgpu::TextureView {
    // A bound-but-unused 1^3 texture: the bind group layout is auto-derived from the shader, so
    // binding 3 must always be satisfiable even in mode 0. Never written; contents are unread.
    device
        .create_texture(&wgpu::TextureDescriptor {
            label: Some("nicti-pelt display LUT (dummy)"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D3,
            format: wgpu::TextureFormat::Rgba16Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        })
        .create_view(&wgpu::TextureViewDescriptor::default())
}

fn upload_lut(device: &wgpu::Device, queue: &wgpu::Queue, lut: &Lut3d) -> wgpu::TextureView {
    let n = lut.size as u32;
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("nicti-pelt display LUT"),
        size: wgpu::Extent3d {
            width: n,
            height: n,
            depth_or_array_layers: n,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D3,
        format: wgpu::TextureFormat::Rgba16Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let texels: Vec<u16> = lut
        .rgba
        .iter()
        .map(|&v| f16::from_f32(v).to_bits())
        .collect();
    queue.write_texture(
        texture.as_image_copy(),
        bytemuck::cast_slice(&texels),
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(n * 8),
            rows_per_image: Some(n),
        },
        wgpu::Extent3d {
            width: n,
            height: n,
            depth_or_array_layers: n,
        },
    );
    texture.create_view(&wgpu::TextureViewDescriptor::default())
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
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&res.lut_view),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::Sampler(&res.lut_sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: wgpu::BindingResource::TextureView(&res.trc_view),
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
    use nicti_calico::transform::DisplayProfile;
    use nicti_tapetum::frame::Extent;
    use nicti_tapetum::gpu::GpuContext;

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
        let Some(gpu) = crate::test_gpu::shared() else {
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
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&resources.lut_view),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::Sampler(&resources.lut_sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: wgpu::BindingResource::TextureView(&resources.trc_view),
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

    /// Draws `pixels` (linear working-space RGB, one row) through the real display pipeline into a
    /// non-sRGB `Rgba8Unorm` target and reads the bytes back.
    fn render_row(
        gpu: &GpuContext,
        resources: &mut ViewportResources,
        pixels: &[[f32; 3]],
    ) -> Vec<[u8; 4]> {
        render_row_to(gpu, resources, pixels, wgpu::TextureFormat::Rgba8Unorm)
    }

    /// As [`render_row`] but into `format`; the pipeline in `resources` must have been built for
    /// the same format.
    fn render_row_to(
        gpu: &GpuContext,
        resources: &mut ViewportResources,
        pixels: &[[f32; 3]],
        format: wgpu::TextureFormat,
    ) -> Vec<[u8; 4]> {
        let w = pixels.len() as u32;
        assert_eq!(w * 4 % 256, 0, "row bytes must satisfy copy alignment");
        let frame = Arc::new(FrameTexture::new(
            gpu,
            Extent {
                width: w,
                height: 1,
            },
        ));
        let texels: Vec<u16> = pixels
            .iter()
            .flat_map(|p| [p[0], p[1], p[2], 1.0])
            .map(|v| f16::from_f32(v).to_bits())
            .collect();
        gpu.queue.write_texture(
            frame.texture.as_image_copy(),
            bytemuck::cast_slice(&texels),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w * 8),
                rows_per_image: Some(1),
            },
            wgpu::Extent3d {
                width: w,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        gpu.queue.write_buffer(
            &resources.uniform_buf,
            0,
            bytemuck::bytes_of(&resources.uniforms),
        );
        let layout = resources.pipeline.get_bind_group_layout(0);
        let bind_group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("test bind group"),
            layout: &layout,
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
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&resources.lut_view),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::Sampler(&resources.lut_sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: wgpu::BindingResource::TextureView(&resources.trc_view),
                },
            ],
        });
        let target = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("test target"),
            size: wgpu::Extent3d {
                width: w,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("test readback"),
            size: u64::from(w * 4),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let view = target.create_view(&wgpu::TextureViewDescriptor::default());
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("test pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&resources.pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        encoder.copy_texture_to_buffer(
            target.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(w * 4),
                    rows_per_image: Some(1),
                },
            },
            wgpu::Extent3d {
                width: w,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        gpu.queue.submit([encoder.finish()]);
        let slice = readback.slice(..);
        slice.map_async(wgpu::MapMode::Read, |r| r.expect("map failed"));
        gpu.device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("device poll failed");
        let data = slice.get_mapped_range().expect("mapped range");
        data.as_chunks::<4>().0.to_vec()
    }

    /// 64 spread-out linear working-space test colors: neutrals plus saturated ramps.
    fn test_pixels() -> Vec<[f32; 3]> {
        (0..64)
            .map(|i| {
                let t = i as f32 / 63.0;
                match i % 4 {
                    0 => [t * t, t * t, t * t],
                    1 => [t * 0.6, 0.2 + t * 0.3, 0.1],
                    2 => [0.05, t * 0.7, 0.3 + t * 0.2],
                    _ => [t * 0.4 + 0.1, 0.05, t * 0.5],
                }
            })
            .collect()
    }

    fn assert_matches_cpu(gpu_px: &[[u8; 4]], transform: &DisplayTransform, gamut_warn: bool) {
        // f16 LUT storage + 8-bit rounding: a few codes.
        let warn = [0.9f32, 0.1, 0.55];
        for (i, (got, src)) in gpu_px.iter().zip(test_pixels()).enumerate() {
            let (cpu, flagged) = transform.apply(src);
            let want = if gamut_warn && flagged { warn } else { cpu };
            for c in 0..3 {
                let w = (want[c].clamp(0.0, 1.0) * 255.0).round();
                assert!(
                    (f32::from(got[c]) - w).abs() <= 3.0,
                    "pixel {i} channel {c}: gpu {got:?} vs cpu {want:?} (flagged {flagged})"
                );
            }
        }
    }

    fn gpu_or_skip() -> Option<Arc<GpuContext>> {
        crate::test_gpu::shared()
    }

    #[test]
    fn exact_mode_matches_the_cpu_reference_for_every_space() {
        let Some(gpu) = gpu_or_skip() else { return };
        for space in OutputSpace::ALL {
            let mut res = ViewportResources::new(&gpu.device, wgpu::TextureFormat::Rgba8Unorm);
            let t = DisplayTransform::exact(space);
            res.set_display_transform(&gpu.device, &gpu.queue, &t, false);
            assert_matches_cpu(&render_row(&gpu, &mut res, &test_pixels()), &t, false);
        }
    }

    #[test]
    fn proof_with_gamut_warning_matches_the_cpu_reference() {
        let Some(gpu) = gpu_or_skip() else { return };
        let t = DisplayTransform::build(
            &DisplayProfile::Space(OutputSpace::DisplayP3),
            Some(OutputSpace::Srgb),
        )
        .unwrap();
        let mut res = ViewportResources::new(&gpu.device, wgpu::TextureFormat::Rgba8Unorm);
        for gamut_warn in [false, true] {
            res.set_display_transform(&gpu.device, &gpu.queue, &t, gamut_warn);
            let px = render_row(&gpu, &mut res, &test_pixels());
            assert_matches_cpu(&px, &t, gamut_warn);
            let flagged = test_pixels().iter().filter(|p| t.apply(**p).1).count();
            assert!(
                flagged > 0 && flagged < 64,
                "test set should mix in- and out-of-gamut, got {flagged}/64"
            );
        }
    }

    fn p3_per_channel_gamma() -> nicti_calico::transform::ColorProfile {
        use nicti_calico::transform::{ColorProfile, ToneReprCurve};
        let mut p = ColorProfile::new_display_p3();
        p.cicp = None; // or moxcms would use its sRGB transfer instead of our curve
                       // Different gamma per channel so a channel mix-up in the encode table can't pass.
        p.red_trc = Some(ToneReprCurve::Parametric(vec![1.8]));
        p.green_trc = Some(ToneReprCurve::Parametric(vec![2.2]));
        p.blue_trc = Some(ToneReprCurve::Parametric(vec![2.6]));
        p
    }

    #[test]
    fn matrix_trc_monitor_matches_the_cpu_reference() {
        let Some(gpu) = gpu_or_skip() else { return };
        let monitor = DisplayProfile::Icc(Arc::new(p3_per_channel_gamma()));
        for proof in [None, Some(OutputSpace::AdobeRgb)] {
            let t = DisplayTransform::build(&monitor, proof).unwrap();
            assert!(matches!(t.kind, DisplayKind::MatrixTrc(_)));
            let mut res = ViewportResources::new(&gpu.device, wgpu::TextureFormat::Rgba8Unorm);
            res.set_display_transform(&gpu.device, &gpu.queue, &t, proof.is_some());
            assert_matches_cpu(
                &render_row(&gpu, &mut res, &test_pixels()),
                &t,
                proof.is_some(),
            );
        }
    }

    #[test]
    fn baked_lut_monitor_matches_the_cpu_reference() {
        let Some(gpu) = gpu_or_skip() else { return };
        let profile = nicti_calico::transform::ColorProfile::new_bt2020();
        for proof in [None, Some(OutputSpace::AdobeRgb)] {
            let t = DisplayTransform::with_lut(&profile, proof).unwrap();
            let mut res = ViewportResources::new(&gpu.device, wgpu::TextureFormat::Rgba8Unorm);
            res.set_display_transform(&gpu.device, &gpu.queue, &t, proof.is_some());
            assert_matches_cpu(
                &render_row(&gpu, &mut res, &test_pixels()),
                &t,
                proof.is_some(),
            );
        }
    }

    #[test]
    fn srgb_render_target_gets_the_same_bytes_as_a_unorm_one() {
        // The hardware applies the sRGB OETF on write to an `*Srgb` target, so the shader hands
        // it linear values (`target_srgb`); the final bytes must match the non-sRGB target's
        // shader-side encoding in every mode.
        let Some(gpu) = gpu_or_skip() else { return };
        let monitor =
            DisplayProfile::Icc(Arc::new(nicti_calico::transform::ColorProfile::new_bt2020()));
        let transforms = [
            DisplayTransform::exact(OutputSpace::Srgb),
            DisplayTransform::build(&monitor, None).unwrap(),
        ];
        for t in transforms {
            let mut plain = ViewportResources::new(&gpu.device, wgpu::TextureFormat::Rgba8Unorm);
            plain.set_display_transform(&gpu.device, &gpu.queue, &t, false);
            let a = render_row(&gpu, &mut plain, &test_pixels());
            let mut srgb = ViewportResources::new(&gpu.device, wgpu::TextureFormat::Rgba8UnormSrgb);
            srgb.set_display_transform(&gpu.device, &gpu.queue, &t, false);
            let b = render_row_to(
                &gpu,
                &mut srgb,
                &test_pixels(),
                wgpu::TextureFormat::Rgba8UnormSrgb,
            );
            for (i, (x, y)) in a.iter().zip(&b).enumerate() {
                for c in 0..3 {
                    assert!(
                        (i32::from(x[c]) - i32::from(y[c])).abs() <= 2,
                        "pixel {i} channel {c}: unorm {x:?} vs srgb {y:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn uniforms_select_the_right_mode_and_transfer_function() {
        let d = uniforms_for(&DisplayTransform::exact(OutputSpace::AdobeRgb), true, false);
        assert_eq!((d.mode, d.trc, d.gamut_warn, d.proof_enabled), (0, 1, 0, 0));
        let s = uniforms_for(&DisplayTransform::exact(OutputSpace::Srgb), false, true);
        assert_eq!((s.mode, s.trc, s.target_srgb), (0, 0, 1));
        let p = DisplayTransform::build(
            &DisplayProfile::Space(OutputSpace::Srgb),
            Some(OutputSpace::AdobeRgb),
        )
        .unwrap();
        let u = uniforms_for(&p, true, false);
        assert_eq!((u.proof_enabled, u.gamut_warn), (1, 1));
    }

    #[test]
    fn display_uniforms_size_is_a_multiple_of_16() {
        // WGSL uniform structs round up to their max member alignment (16 for vec4).
        assert_eq!(std::mem::size_of::<DisplayUniforms>() % 16, 0);
        assert_eq!(std::mem::size_of::<DisplayUniforms>(), 192);
    }

    #[test]
    fn display_matrix_is_the_cpu_references_matrix() {
        // #318: `geometry::output_encode` (the CPU readback reference) takes its ProPhoto -> sRGB
        // matrix from calico, the same `OutputSpace::Srgb.from_working_f32()` the display shader
        // is fed, so the two agree to f32 rounding (they used to differ by ~3e-4, moving ~1.6% of
        // pixels by one 8-bit code). Pinned on the encoded result, including an out-of-gamut input.
        let m = OutputSpace::Srgb.from_working_f32();
        let inputs = [
            [1.0, 1.0, 1.0],
            [0.18, 0.18, 0.18],
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, 0.0, 1.0],
            [0.6, 0.3, 0.1],
        ];
        for px in inputs {
            let ours: [f32; 3] = std::array::from_fn(|i| {
                nicti_tapetum::color::srgb_oetf(m[i][0] * px[0] + m[i][1] * px[1] + m[i][2] * px[2])
            });
            let theirs = nicti_tapetum::geometry::output_encode(px);
            for c in 0..3 {
                assert!(
                    (ours[c] - theirs[c]).abs() < 1e-6,
                    "{px:?}[{c}] display {} vs CPU reference {}",
                    ours[c],
                    theirs[c]
                );
            }
        }
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
