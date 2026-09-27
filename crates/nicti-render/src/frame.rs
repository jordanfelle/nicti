//! GPU-resident frame textures. Every intermediate render-graph output is an `Rgba16Float`
//! texture -- a core WebGPU format needing no optional feature (unlike a raw f16 storage
//! *buffer*, which would need `SHADER_F16`), so it works identically on Vulkan, Dx12, WARP and
//! lavapipe (ADR-0016). A stage reads its input via `textureLoad` and writes via a write-only
//! `rgba16float` storage-texture binding.

use std::collections::HashMap;

use crate::gpu::GpuContext;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Extent {
    pub width: u32,
    pub height: u32,
}

/// One GPU-resident render-graph frame: a texture plus the view stages bind against.
pub struct FrameTexture {
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    pub extent: Extent,
}

impl FrameTexture {
    pub const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

    /// Allocates a fresh `Rgba16Float` texture, usable as a storage-texture bind (a stage's
    /// output), a sampled/storage-texture bind (a later stage's input), and a copy source (for
    /// readback into a golden-image comparison or an export encoder).
    pub fn new(gpu: &GpuContext, extent: Extent) -> Self {
        let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("nicti-render frame"),
            size: wgpu::Extent3d {
                width: extent.width,
                height: extent.height,
                depth_or_array_layers: 1,
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
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self {
            texture,
            view,
            extent,
        }
    }

    /// Byte size of this frame's pixel data (8 bytes/pixel: 4 x f16) -- the same figure ADR-0044
    /// budgets cache tiers against (a full-res 8280x5520 frame is ~349MB).
    pub fn byte_size(&self) -> u64 {
        u64::from(self.extent.width) * u64::from(self.extent.height) * 8
    }
}

/// Recycles `FrameTexture`s by extent so a stage that needs a fresh output texture doesn't pay a
/// full GPU allocation on every single dispatch -- the same shape gets reused once its previous
/// occupant is dropped and returned here. A free-list per extent, not a byte budget: unlike
/// `cache::Tier` (which caches *content*, keyed by a render-graph cache key, and must evict on a
/// byte budget), this pool only ever holds textures nothing is using yet, so there's no content
/// to evict -- just don't grow it past what callers actually return.
#[derive(Default)]
pub struct FramePool {
    free: HashMap<Extent, Vec<FrameTexture>>,
}

impl FramePool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reuses a previously `release`d texture of the same extent if one is free, otherwise
    /// allocates a fresh one.
    pub fn acquire(&mut self, gpu: &GpuContext, extent: Extent) -> FrameTexture {
        self.free
            .get_mut(&extent)
            .and_then(Vec::pop)
            .unwrap_or_else(|| FrameTexture::new(gpu, extent))
    }

    /// Returns a texture to the pool for a future `acquire` of the same extent.
    pub fn release(&mut self, frame: FrameTexture) {
        self.free.entry(frame.extent).or_default().push(frame);
    }

    /// Number of free textures currently pooled, for tests.
    pub fn free_count(&self, extent: Extent) -> usize {
        self.free.get(&extent).map_or(0, Vec::len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu::GpuPreference;

    fn test_gpu() -> Option<GpuContext> {
        match GpuContext::new(GpuPreference::Auto) {
            Ok(ctx) => Some(ctx),
            Err(_) => {
                eprintln!("no wgpu adapter available in this environment, skipping");
                None
            }
        }
    }

    #[test]
    fn byte_size_matches_the_documented_full_res_estimate() {
        let Some(gpu) = test_gpu() else { return };
        let extent = Extent {
            width: 8280,
            height: 5520,
        };
        let frame = FrameTexture::new(&gpu, extent);
        let full_res_mb = frame.byte_size() as f64 / (1024.0 * 1024.0);
        assert!(
            (340.0..360.0).contains(&full_res_mb),
            "full-res frame should be ~349MB, got {full_res_mb:.1}MB"
        );
    }

    #[test]
    fn pool_reuses_a_released_texture_of_the_same_extent() {
        let Some(gpu) = test_gpu() else { return };
        let mut pool = FramePool::new();
        let extent = Extent {
            width: 64,
            height: 64,
        };
        let a = pool.acquire(&gpu, extent);
        assert_eq!(pool.free_count(extent), 0);
        pool.release(a);
        assert_eq!(pool.free_count(extent), 1);
        let _b = pool.acquire(&gpu, extent);
        assert_eq!(
            pool.free_count(extent),
            0,
            "acquire should have taken the pooled texture"
        );
    }

    #[test]
    fn pool_never_hands_out_a_different_extents_texture() {
        let Some(gpu) = test_gpu() else { return };
        let mut pool = FramePool::new();
        let small = Extent {
            width: 32,
            height: 32,
        };
        let big = Extent {
            width: 128,
            height: 128,
        };
        let small_tex = pool.acquire(&gpu, small);
        pool.release(small_tex);
        assert_eq!(pool.free_count(small), 1);
        assert_eq!(pool.free_count(big), 0);
        let acquired_big = pool.acquire(&gpu, big);
        assert_eq!(acquired_big.extent, big);
        assert_eq!(
            pool.free_count(small),
            1,
            "the small pooled texture must be untouched"
        );
    }
}
