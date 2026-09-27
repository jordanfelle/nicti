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

const BYTES_PER_PIXEL: u32 = 8; // Rgba16Float: 4 channels x 2 bytes

/// WebGPU requires a buffer<->texture copy's row stride to be a multiple of
/// `wgpu::COPY_BYTES_PER_ROW_ALIGNMENT` (256). `pub(crate)` so `tile.rs`'s own staging-byte
/// budget math can account for the real, padded readback allocation size rather than an
/// unpadded estimate (CodeRabbit, #45 PR4).
pub(crate) fn padded_bytes_per_row(width: u32) -> u32 {
    let unpadded = width * BYTES_PER_PIXEL;
    unpadded.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT
}

/// Reads a `FrameTexture` back to row-major RGBA f32 (converting from the texture's native f16)
/// -- the shared production readback path for anything that needs a `FrameTexture`'s actual
/// pixels on the CPU (a golden-image comparison, an export encoder, `tile::TiledRender`'s own
/// per-tile readback).
pub fn read_frame(gpu: &GpuContext, frame: &FrameTexture) -> Vec<[f32; 4]> {
    use half::f16;

    let extent = frame.extent;
    let unpadded_bpr = extent.width * BYTES_PER_PIXEL;
    let padded_bpr = padded_bytes_per_row(extent.width);
    let buffer_size = u64::from(padded_bpr) * u64::from(extent.height);
    let staging = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("frame readback"),
        size: buffer_size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("frame readback encoder"),
        });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &frame.texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &staging,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_bpr),
                rows_per_image: Some(extent.height),
            },
        },
        wgpu::Extent3d {
            width: extent.width,
            height: extent.height,
            depth_or_array_layers: 1,
        },
    );
    gpu.queue.submit(Some(encoder.finish()));

    let slice = staging.slice(..);
    slice.map_async(wgpu::MapMode::Read, |_| {});
    gpu.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("device poll failed");
    let raw = slice.get_mapped_range().expect("output buffer not mapped");

    let mut out = Vec::with_capacity((extent.width * extent.height) as usize);
    for y in 0..extent.height {
        let row_start = (y * padded_bpr) as usize;
        let row = &raw[row_start..row_start + unpadded_bpr as usize];
        let u16s: &[u16] = bytemuck::cast_slice(row);
        for px in u16s.as_chunks::<4>().0 {
            out.push([
                f16::from_bits(px[0]).to_f32(),
                f16::from_bits(px[1]).to_f32(),
                f16::from_bits(px[2]).to_f32(),
                f16::from_bits(px[3]).to_f32(),
            ]);
        }
    }
    drop(raw);
    staging.unmap();
    out
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
    use crate::test_util::shared_test_gpu as test_gpu;

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
