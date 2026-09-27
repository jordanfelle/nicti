//! Test-only GPU texture readback/upload helpers, shared by every module's GPU-vs-CPU parity
//! tests (`color`/`geometry`'s pure-math references need real pixel data to compare against).

use half::f16;

use crate::frame::{Extent, FrameTexture};
use crate::gpu::GpuContext;

const BYTES_PER_PIXEL: u32 = 8; // Rgba16Float: 4 channels x 2 bytes

/// WebGPU requires a buffer<->texture copy's row stride to be a multiple of
/// `wgpu::COPY_BYTES_PER_ROW_ALIGNMENT` (256) -- real textures are almost always wide enough for
/// `width * BYTES_PER_PIXEL` to already satisfy that, but this crate's own tiny test fixtures
/// (2x2, 4x4) are not, so every upload/readback here must pad each row out explicitly.
fn padded_bytes_per_row(width: u32) -> u32 {
    let unpadded = width * BYTES_PER_PIXEL;
    unpadded.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT
}

/// Uploads `data` (row-major RGBA f32, `extent.width * extent.height` entries) into a fresh
/// `Rgba16Float` texture.
pub fn upload_frame(gpu: &GpuContext, extent: Extent, data: &[[f32; 4]]) -> FrameTexture {
    assert_eq!(data.len(), (extent.width * extent.height) as usize);
    let frame = FrameTexture::new(gpu, extent);
    let unpadded_bpr = extent.width * BYTES_PER_PIXEL;
    let padded_bpr = padded_bytes_per_row(extent.width);
    let mut padded = vec![0u8; (padded_bpr * extent.height) as usize];
    for y in 0..extent.height {
        let row_start = (y * extent.width) as usize;
        let row = &data[row_start..row_start + extent.width as usize];
        let row_bytes: Vec<u8> = row
            .iter()
            .flat_map(|px| px.iter().flat_map(|&c| f16::from_f32(c).to_le_bytes()))
            .collect();
        assert_eq!(row_bytes.len(), unpadded_bpr as usize);
        let dst_start = (y * padded_bpr) as usize;
        padded[dst_start..dst_start + row_bytes.len()].copy_from_slice(&row_bytes);
    }
    gpu.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &frame.texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &padded,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(padded_bpr),
            rows_per_image: Some(extent.height),
        },
        wgpu::Extent3d {
            width: extent.width,
            height: extent.height,
            depth_or_array_layers: 1,
        },
    );
    frame
}

/// Reads a `FrameTexture` back to row-major RGBA f32 (converting from the texture's native f16).
pub fn read_frame(gpu: &GpuContext, frame: &FrameTexture) -> Vec<[f32; 4]> {
    let extent = frame.extent;
    let unpadded_bpr = extent.width * BYTES_PER_PIXEL;
    let padded_bpr = padded_bytes_per_row(extent.width);
    let buffer_size = u64::from(padded_bpr) * u64::from(extent.height);
    let staging = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("test readback"),
        size: buffer_size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("test readback encoder"),
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
