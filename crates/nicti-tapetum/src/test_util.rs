//! Test-only GPU texture readback/upload helpers, shared by every module's GPU-vs-CPU parity
//! tests (`color`/`geometry`'s pure-math references need real pixel data to compare against).

use std::sync::{Arc, OnceLock};

use half::f16;

use crate::frame::{Extent, FrameTexture};
use crate::gpu::{GpuContext, GpuPreference};

const BYTES_PER_PIXEL: u32 = 8; // Rgba16Float: 4 channels x 2 bytes

static SHARED_GPU: OnceLock<Option<Arc<GpuContext>>> = OnceLock::new();

/// A single, process-wide `GpuContext`, lazily created on first use and reused by every test that
/// just needs "a working device" -- as opposed to a test that's exercising `GpuContext::new`
/// itself (`gpu::tests`'s own backend-preference tests keep constructing their own for that
/// reason). `wgpu::Device`/`Queue` are `Send + Sync` and designed for exactly this kind of
/// concurrent use across threads.
///
/// **Regression fix (#45 PR4's adversarial review)**: before this, every GPU test module in this
/// crate had its own private `test_gpu()` that called `GpuContext::new` fresh, per test. Once
/// `tile.rs` added a real batch of new GPU tests, `cargo test -p nicti-tapetum` at default (multi-
/// threaded) parallelism started segfaulting (`SIGSEGV`) in this sandbox 3 of 5 runs -- reliably
/// clean under `--test-threads=1`, confirming concurrent adapter/device requests were the trigger,
/// not a logic bug. Sharing one device removes that concurrent-creation pressure entirely while
/// still letting every test's actual GPU work (pipelines/buffers/dispatches) run genuinely in
/// parallel against the one shared, thread-safe device.
pub fn shared_test_gpu() -> Option<Arc<GpuContext>> {
    SHARED_GPU
        .get_or_init(|| match GpuContext::new(GpuPreference::Auto) {
            Ok(ctx) => Some(Arc::new(ctx)),
            Err(_) => {
                eprintln!("no wgpu adapter available in this environment, skipping");
                None
            }
        })
        .clone()
}

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

/// Reads a `FrameTexture` back to row-major RGBA f32 -- re-exported from `frame::read_frame`
/// (the real production readback path) rather than kept as a second, test-only implementation.
/// **Regression fix (#45 PR4's adversarial review)**: `frame::read_frame` was promoted out of a
/// test-only duplicate here, but every GPU parity test in `stages.rs` still called this module's
/// own copy directly, so the newly-promoted production function had zero test coverage and the
/// two implementations could silently drift apart. Re-exporting closes that gap: every caller
/// (test or production) now goes through the exact same code.
pub use crate::frame::read_frame;
