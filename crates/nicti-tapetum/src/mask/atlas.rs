//! The mask atlas: every active correction's finished composite, four per `Rgba16Float` layer (one
//! per channel), so the fused live shader reads up to four masks with one bilinear sample.
//!
//! `Rgba16Float` (not `R32Float`) because it is a *filterable* core format, which `R32Float` is
//! not -- the live shader samples the atlas bilinearly so a mask extent smaller than the frame
//! (capped at 4096) still gives smooth edges.

use std::sync::Arc;

use super::kernels::FieldTexture;
use super::local::LocalUniform;
use crate::frame::FrameTexture;
use crate::gpu::GpuContext;

/// Channels (corrections) per atlas layer.
pub const CHANNELS: usize = 4;

pub struct Atlas {
    pub texture: wgpu::Texture,
    /// The whole array, for sampling in the live shader.
    pub array_view: wgpu::TextureView,
    /// One 2D view per layer, for `mask_pack`'s write-only storage binding.
    pub layer_views: Vec<wgpu::TextureView>,
    pub width: u32,
    pub height: u32,
    pub layers: u32,
}

impl Atlas {
    pub const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

    /// An atlas with room for `corrections` masks (at least one layer, so an empty atlas is still
    /// a valid binding).
    pub fn new(gpu: &GpuContext, width: u32, height: u32, corrections: usize) -> Self {
        let layers = corrections.div_ceil(CHANNELS).max(1) as u32;
        let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("nicti-tapetum mask atlas"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: layers,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: Self::FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::STORAGE_BINDING
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let array_view = texture.create_view(&wgpu::TextureViewDescriptor {
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        });
        let layer_views = (0..layers)
            .map(|layer| {
                texture.create_view(&wgpu::TextureViewDescriptor {
                    dimension: Some(wgpu::TextureViewDimension::D2),
                    base_array_layer: layer,
                    array_layer_count: Some(1),
                    ..Default::default()
                })
            })
            .collect();
        Self {
            texture,
            array_view,
            layer_views,
            width,
            height,
            layers,
        }
    }

    pub fn byte_size(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height) * u64::from(self.layers) * 8
    }
}

/// The cached per-photo inputs of the *spatial* local adjustments (see `bases`). Each is present
/// only when some active correction needs it -- an image with only exposure masks builds none.
#[derive(Clone, Default)]
pub struct Bases {
    /// `Rgba16Float` `(fine band, mid band, baked perceptual luma, 0)` for clarity/texture,
    /// sampled bilinearly.
    pub bands: Option<Arc<FrameTexture>>,
    /// `R32Float` dehaze transmission, read with `textureLoad`.
    pub haze: Option<Arc<FieldTexture>>,
    /// Airlight colour in the baked (camera-linear) space.
    pub airlight: [f32; 3],
}

/// Everything the live shader needs to apply this render's local corrections: the atlas, the
/// per-correction uniforms (in atlas channel order) and the spatial bases.
#[derive(Clone)]
pub struct MaskFrame {
    pub atlas: Arc<Atlas>,
    pub uniforms: Vec<LocalUniform>,
    pub bases: Bases,
}
