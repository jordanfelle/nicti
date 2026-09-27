//! Output-side tiling for the full-res path (#45 PR4): splits a large render into GPU-manageable
//! chunks by re-running only the geometry (crop/present-sample) stage repeatedly over the one
//! already-baked+live-suffixed full-frame [`FrameTexture`] -- decode/demosaic/denoise/lens/heal
//! and the fused live dispatch each still run exactly once per render, regardless of tile count
//! (that's the whole point of the baked/live cache: a full-res frame is GPU-resident once). Only
//! the final geometry sample -- which already reads its own dynamic output extent per-dispatch,
//! see [`crate::stages::CropKernel`]'s `PresentUniforms::out_width`/`out_height` -- runs once per
//! tile, each with the overall render's own output->source transform composed with that tile's
//! own output-space origin (see [`compose_tile_transform`]), so a fractional pan/zoom still
//! samples exactly where an untiled render would have.
//!
//! Generalized from `spikes/rods/src/ai.rs`'s `TileConfig`/`build_padded_tile`/`feather_weight`
//! (an AI-denoise tiling scheme, overlapping+feathered because every tile there is independently
//! re-run through a model whose output can differ tile-to-tile at the seam). Render tiling here
//! is simpler: `present_sample.wgsl`'s bilinear sample is a **pointwise** operation (each output
//! pixel depends only on up to 4 known-identical source texels, never on neighboring *output*
//! pixels), so tiles never need blending -- only a 1-texel halo so a tile's edge pixels can still
//! bilinearly sample past their own core boundary. Core-crop (discard the halo, keep only each
//! tile's non-overlapping core region) is therefore always correct here, unlike an AI model's
//! genuinely lossy-at-the-seam output.

use std::sync::Arc;

use crate::frame::{read_frame, Extent, FrameTexture};
use crate::geometry::Affine2D;
use crate::gpu::GpuContext;
use crate::renderer::GeometryExec;
use crate::stages::CropKernel;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    fn right(&self) -> u32 {
        self.x + self.width
    }

    fn bottom(&self) -> u32 {
        self.y + self.height
    }
}

/// One planned tile: `core` is this tile's exclusive, non-overlapping slice of the output (every
/// tile's `core` union covers the requested ROI exactly once); `padded` is `core` expanded by the
/// chain's halo on every side and clamped to the source image's extent (never negative, never
/// past the source) -- the actual region rendered, so `core`'s edge pixels have real neighbors to
/// bilinearly sample rather than clamping to `core`'s own boundary as if it were the image edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tile {
    pub core: Rect,
    pub padded: Rect,
}

#[derive(Debug, Clone, Copy)]
pub struct TileBudget {
    /// Largest a padded tile's width or height may be -- typically
    /// `wgpu::Limits::max_texture_dimension_2d`, capped further (e.g. to 4096) to keep a single
    /// tile's compute dispatch well under the frame-budget target.
    pub max_dim: u32,
    /// Largest a padded tile's readback staging buffer may be, in bytes (`width * height *
    /// bytes_per_pixel`, `FrameTexture` is always `Rgba16Float` = 8 bytes/pixel) -- bounds actual
    /// host-visible memory use, distinct from `max_dim` (a very wide-but-short tile can satisfy
    /// `max_dim` while still being too large in total bytes).
    pub max_staging_bytes: u64,
    /// The wall-clock budget one tile's render+readback should land under (ADR-0054's 16ms hard
    /// limit, with headroom) -- consumed by [`calibrate_core_size`], not by `plan` itself (`plan`
    /// has no timing information yet; calibration only has an effect across repeated `plan`
    /// calls, e.g. between undo states at different tile sizes).
    pub target_chunk_ms: f32,
}

const BYTES_PER_PIXEL: u64 = 8; // Rgba16Float

fn core_size_from_budget(halo_px: u32, budget: TileBudget) -> u32 {
    let cap_from_dim = budget.max_dim.saturating_sub(2 * halo_px).max(1);
    let max_padded_dim_from_bytes =
        ((budget.max_staging_bytes / BYTES_PER_PIXEL) as f64).sqrt() as u32;
    let cap_from_bytes = max_padded_dim_from_bytes.saturating_sub(2 * halo_px).max(1);
    cap_from_dim.min(cap_from_bytes)
}

fn expand_and_clamp(core: Rect, halo_px: u32, extent: Extent) -> Rect {
    let x0 = core.x.saturating_sub(halo_px);
    let y0 = core.y.saturating_sub(halo_px);
    let x1 = core.right().saturating_add(halo_px).min(extent.width);
    let y1 = core.bottom().saturating_add(halo_px).min(extent.height);
    Rect {
        x: x0,
        y: y0,
        width: x1 - x0,
        height: y1 - y0,
    }
}

pub struct TilePlanner;

impl TilePlanner {
    /// Plans a non-overlapping grid of tiles covering `roi` exactly once (`roi` must lie within
    /// `extent` -- every real caller's ROI is either the whole frame or a viewport rect already
    /// clamped to it). `halo_px` is the sum of every stage's `halo_px(params)` across the chain
    /// (today, just the geometry pass's own 1-texel bilinear neighbor read; a future stage with a
    /// larger spatial footprint, e.g. a box filter, would add to it) -- see this module's own
    /// doc comment for why render tiling only needs a halo, never feathered blending.
    pub fn plan(extent: Extent, roi: Rect, halo_px: u32, budget: TileBudget) -> Vec<Tile> {
        if roi.width == 0 || roi.height == 0 {
            return Vec::new();
        }
        let core_size = core_size_from_budget(halo_px, budget);
        let mut tiles = Vec::new();
        let mut y = roi.y;
        while y < roi.bottom() {
            let core_h = core_size.min(roi.bottom() - y);
            let mut x = roi.x;
            while x < roi.right() {
                let core_w = core_size.min(roi.right() - x);
                let core = Rect {
                    x,
                    y,
                    width: core_w,
                    height: core_h,
                };
                let padded = expand_and_clamp(core, halo_px, extent);
                tiles.push(Tile { core, padded });
                x += core_size;
            }
            y += core_size;
        }
        tiles
    }
}

/// Given how long a chunk of `measured_dim` (its padded tile's largest side) actually took,
/// returns the next core tile dimension to try so a future chunk lands near `target_ms` --
/// GPU compute cost scales with pixel count (area), not linear dimension, so the correction
/// factor is a square root, not a direct ratio. Clamped to `[1, max_dim]`. A non-positive
/// `measured_ms` (a bad/zero timer read) is treated as "no information," returning `measured_dim`
/// unchanged rather than dividing by zero or producing a nonsensical scale.
pub fn calibrate_core_size(
    measured_dim: u32,
    measured_ms: f32,
    target_ms: f32,
    max_dim: u32,
) -> u32 {
    if measured_ms <= 0.0 {
        return measured_dim.min(max_dim).max(1);
    }
    let scale = (target_ms / measured_ms).sqrt();
    let next = (measured_dim as f32 * scale).round().max(1.0) as u32;
    next.min(max_dim)
}

/// Where a completed tile's rendered pixels go. `core` is the tile's own non-overlapping output
/// region (see [`Tile::core`]); `pixels` is row-major RGBA f32 over exactly `core.width *
/// core.height` entries (the halo region is never passed here -- [`TiledRender::step`] discards
/// it after reading back the padded tile).
pub trait TileSink {
    fn write_tile(&mut self, core: Rect, pixels: &[[f32; 4]]);
}

/// The simplest `TileSink`: assembles the full frame in host memory. A file-encoder sink (writing
/// each tile straight to a TIFF/PNG's own scanline range without ever holding the whole frame in
/// RAM) is #57's scope.
pub struct MemorySink {
    pub extent: Extent,
    pub pixels: Vec<[f32; 4]>,
}

impl MemorySink {
    pub fn new(extent: Extent) -> Self {
        Self {
            extent,
            pixels: vec![[0.0; 4]; (extent.width * extent.height) as usize],
        }
    }
}

impl TileSink for MemorySink {
    fn write_tile(&mut self, core: Rect, pixels: &[[f32; 4]]) {
        assert_eq!(pixels.len(), (core.width * core.height) as usize);
        for row in 0..core.height {
            let dst_row_start = ((core.y + row) * self.extent.width + core.x) as usize;
            let src_row_start = (row * core.width) as usize;
            self.pixels[dst_row_start..dst_row_start + core.width as usize]
                .copy_from_slice(&pixels[src_row_start..src_row_start + core.width as usize]);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileStep {
    Yield,
    Done,
}

/// Same shape as Pounce's `ChunkedJob` (a `step()` a caller drives from its own event loop, one
/// tile per call, so a slow full-res render never blocks a UI frame) -- without depending on the
/// crouch spike.
pub struct TiledRender<'a> {
    gpu: Arc<GpuContext>,
    source: &'a FrameTexture,
    geometry: &'a CropKernel,
    /// The overall output-space -> source-space transform being tiled (the crop/zoom/pan a
    /// non-tiled render would use directly) -- possibly non-integer (a fractional pan/zoom), in
    /// which case a real bilinear sample straddling a tile boundary genuinely needs its halo, not
    /// just the always-exact case a plain integer 1:1 crop would produce (see this module's own
    /// doc comment for why `present_sample.wgsl`'s pointwise sampling still means the halo alone,
    /// with no blending, is always sufficient).
    base_transform: Affine2D,
    tiles: Vec<Tile>,
    next: usize,
}

/// Composes `base` (an output-space -> source-space transform) with a plain translate by
/// `tile_origin` applied first -- i.e. `result(x, y) == base(x + tile_origin.0, y +
/// tile_origin.1)`. Lets each tile keep using its own small, tile-local (0,0)-based output
/// coordinates while still sampling exactly where the untiled `base` transform would have.
fn compose_tile_transform(base: Affine2D, tile_origin: (f32, f32)) -> Affine2D {
    let (ox, oy) = tile_origin;
    Affine2D {
        a: base.a,
        b: base.b,
        c: base.c,
        d: base.d,
        tx: base.a * ox + base.b * oy + base.tx,
        ty: base.c * ox + base.d * oy + base.ty,
    }
}

impl<'a> TiledRender<'a> {
    pub fn new(
        gpu: Arc<GpuContext>,
        source: &'a FrameTexture,
        geometry: &'a CropKernel,
        base_transform: Affine2D,
        tiles: Vec<Tile>,
    ) -> Self {
        Self {
            gpu,
            source,
            geometry,
            base_transform,
            tiles,
            next: 0,
        }
    }

    pub fn remaining(&self) -> usize {
        self.tiles.len() - self.next
    }

    /// Renders exactly one tile: retargets `geometry`'s transform to `base_transform` composed
    /// with this tile's own output-space origin (see [`compose_tile_transform`]), samples
    /// `padded` from `source`, reads it back, discards the halo, and writes only `core`'s pixels
    /// into `sink`.
    pub fn step(&mut self, sink: &mut dyn TileSink) -> TileStep {
        let Some(&tile) = self.tiles.get(self.next) else {
            return TileStep::Done;
        };

        let padded_extent = Extent {
            width: tile.padded.width,
            height: tile.padded.height,
        };
        let output = FrameTexture::new(&self.gpu, padded_extent);

        let transform = compose_tile_transform(
            self.base_transform,
            (tile.padded.x as f32, tile.padded.y as f32),
        );
        self.geometry.set_transform(transform);

        let mut encoder = self
            .gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("tile geometry pass"),
            });
        self.geometry
            .encode(&self.gpu, &mut encoder, self.source, &output);
        self.gpu.queue.submit(Some(encoder.finish()));

        let padded_pixels = read_frame(&self.gpu, &output);
        let off_x = tile.core.x - tile.padded.x;
        let off_y = tile.core.y - tile.padded.y;
        let mut core_pixels = Vec::with_capacity((tile.core.width * tile.core.height) as usize);
        for row in 0..tile.core.height {
            let src_row_start = ((off_y + row) * tile.padded.width + off_x) as usize;
            core_pixels.extend_from_slice(
                &padded_pixels[src_row_start..src_row_start + tile.core.width as usize],
            );
        }
        sink.write_tile(tile.core, &core_pixels);

        self.next += 1;
        if self.next >= self.tiles.len() {
            TileStep::Done
        } else {
            TileStep::Yield
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_util::shared_test_gpu as test_gpu;

    fn small_budget() -> TileBudget {
        TileBudget {
            max_dim: 4096,
            max_staging_bytes: 64 * 1024 * 1024,
            target_chunk_ms: 8.0,
        }
    }

    #[test]
    fn plan_tiles_cover_the_roi_exactly_once() {
        let extent = Extent {
            width: 100,
            height: 70,
        };
        let roi = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 70,
        };
        let budget = TileBudget {
            max_dim: 32,
            max_staging_bytes: u64::MAX,
            target_chunk_ms: 8.0,
        };
        let tiles = TilePlanner::plan(extent, roi, 2, budget);
        assert!(tiles.len() > 1, "budget should force a real split");

        // Build an occupancy grid over the ROI and confirm every pixel is covered by exactly one
        // tile's `core` (never zero -- a gap -- and never more than one -- an overlap).
        let mut hits = vec![0u32; (roi.width * roi.height) as usize];
        for tile in &tiles {
            for row in 0..tile.core.height {
                for col in 0..tile.core.width {
                    let x = tile.core.x + col;
                    let y = tile.core.y + row;
                    assert!(x < roi.width && y < roi.height, "core tile escapes the ROI");
                    hits[(y * roi.width + x) as usize] += 1;
                }
            }
        }
        assert!(
            hits.iter().all(|&h| h == 1),
            "every ROI pixel must be covered by exactly one core tile"
        );
    }

    #[test]
    fn plan_padded_tiles_stay_within_the_caps() {
        let extent = Extent {
            width: 200,
            height: 200,
        };
        let roi = Rect {
            x: 0,
            y: 0,
            width: 200,
            height: 200,
        };
        let budget = TileBudget {
            max_dim: 64,
            max_staging_bytes: u64::MAX,
            target_chunk_ms: 8.0,
        };
        let halo = 4;
        for tile in TilePlanner::plan(extent, roi, halo, budget) {
            assert!(tile.padded.width <= budget.max_dim);
            assert!(tile.padded.height <= budget.max_dim);
        }
    }

    #[test]
    fn plan_padded_tiles_never_exceed_the_staging_byte_budget() {
        let extent = Extent {
            width: 4096,
            height: 4096,
        };
        let roi = Rect {
            x: 0,
            y: 0,
            width: 4096,
            height: 4096,
        };
        // A tight byte budget: at most 256x256 padded pixels x 8 bytes/pixel.
        let budget = TileBudget {
            max_dim: 4096,
            max_staging_bytes: 256 * 256 * BYTES_PER_PIXEL,
            target_chunk_ms: 8.0,
        };
        for tile in TilePlanner::plan(extent, roi, 8, budget) {
            let bytes =
                u64::from(tile.padded.width) * u64::from(tile.padded.height) * BYTES_PER_PIXEL;
            assert!(
                bytes <= budget.max_staging_bytes,
                "tile {tile:?} exceeds byte budget"
            );
        }
    }

    #[test]
    fn plan_padded_tiles_clamp_to_the_source_extent_not_negative_or_past_the_edge() {
        let extent = Extent {
            width: 20,
            height: 20,
        };
        let roi = Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 20,
        };
        let budget = TileBudget {
            max_dim: 8,
            max_staging_bytes: u64::MAX,
            target_chunk_ms: 8.0,
        };
        for tile in TilePlanner::plan(extent, roi, 4, budget) {
            assert!(tile.padded.x + tile.padded.width <= extent.width);
            assert!(tile.padded.y + tile.padded.height <= extent.height);
            // core must always lie inside padded (the halo expansion is additive, never negative).
            assert!(tile.padded.x <= tile.core.x);
            assert!(tile.padded.y <= tile.core.y);
        }
    }

    #[test]
    fn plan_a_single_tile_when_the_whole_roi_fits_the_budget() {
        let extent = Extent {
            width: 50,
            height: 50,
        };
        let roi = Rect {
            x: 0,
            y: 0,
            width: 50,
            height: 50,
        };
        let tiles = TilePlanner::plan(extent, roi, 1, small_budget());
        assert_eq!(tiles.len(), 1);
        assert_eq!(tiles[0].core, roi);
    }

    #[test]
    fn plan_of_an_empty_roi_returns_no_tiles() {
        let extent = Extent {
            width: 10,
            height: 10,
        };
        let roi = Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        };
        assert!(TilePlanner::plan(extent, roi, 1, small_budget()).is_empty());
    }

    #[test]
    fn calibrate_core_size_shrinks_when_measured_time_exceeds_target() {
        let next = calibrate_core_size(1024, 32.0, 8.0, 4096);
        assert!(
            next < 1024,
            "expected a shrink toward the target, got {next}"
        );
    }

    #[test]
    fn calibrate_core_size_grows_when_measured_time_is_well_under_target() {
        let next = calibrate_core_size(256, 2.0, 8.0, 4096);
        assert!(next > 256, "expected a grow toward the target, got {next}");
    }

    #[test]
    fn calibrate_core_size_clamps_to_max_dim() {
        let next = calibrate_core_size(1024, 0.1, 8.0, 2048);
        assert!(next <= 2048);
    }

    #[test]
    fn calibrate_core_size_never_returns_zero() {
        assert!(calibrate_core_size(1, 1000.0, 1.0, 4096) >= 1);
    }

    #[test]
    fn memory_sink_places_each_tile_at_its_core_offset() {
        let extent = Extent {
            width: 4,
            height: 4,
        };
        let mut sink = MemorySink::new(extent);
        let core = Rect {
            x: 1,
            y: 1,
            width: 2,
            height: 2,
        };
        let pixels = vec![
            [1.0, 0.0, 0.0, 1.0],
            [2.0, 0.0, 0.0, 1.0],
            [3.0, 0.0, 0.0, 1.0],
            [4.0, 0.0, 0.0, 1.0],
        ];
        sink.write_tile(core, &pixels);
        assert_eq!(sink.pixels[4 + 1], [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(sink.pixels[4 + 2], [2.0, 0.0, 0.0, 1.0]);
        assert_eq!(sink.pixels[2 * 4 + 1], [3.0, 0.0, 0.0, 1.0]);
        assert_eq!(sink.pixels[2 * 4 + 2], [4.0, 0.0, 0.0, 1.0]);
        // Untouched pixels stay at the zero default.
        assert_eq!(sink.pixels[0], [0.0, 0.0, 0.0, 0.0]);
    }

    /// A smooth diagonal gradient (not a flat color) so a fractional-offset bilinear sample
    /// actually varies continuously across it -- a wrong (missing-halo) sample would read a
    /// visibly different value, not one that happens to coincide by symmetry.
    fn gradient_source(gpu: &GpuContext, extent: Extent) -> FrameTexture {
        let mut data = Vec::with_capacity((extent.width * extent.height) as usize);
        for y in 0..extent.height {
            for x in 0..extent.width {
                let v = (x + y) as f32 / (extent.width + extent.height) as f32;
                data.push([v, v * 0.5, 1.0 - v, 1.0]);
            }
        }
        crate::test_util::upload_frame(gpu, extent, &data)
    }

    #[test]
    fn tiled_render_with_a_correct_halo_matches_a_whole_frame_render() {
        let Some(gpu) = test_gpu() else { return };
        let extent = Extent {
            width: 24,
            height: 24,
        };
        let source = gradient_source(&gpu, extent);

        // A fractional pan (not integer-aligned) so bilinear sampling genuinely straddles tile
        // boundaries -- the case a halo exists to serve.
        let base_transform = Affine2D {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            tx: 0.37,
            ty: -0.21,
        };

        let whole_kernel = CropKernel::new(&gpu);
        whole_kernel.set_transform(base_transform);
        let whole_output = FrameTexture::new(&gpu, extent);
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        GeometryExec::encode(&whole_kernel, &gpu, &mut encoder, &source, &whole_output);
        gpu.queue.submit(Some(encoder.finish()));
        let whole_pixels = read_frame(&gpu, &whole_output);

        let roi = Rect {
            x: 0,
            y: 0,
            width: extent.width,
            height: extent.height,
        };
        let budget = TileBudget {
            max_dim: 8,
            max_staging_bytes: u64::MAX,
            target_chunk_ms: 8.0,
        };
        let tiles = TilePlanner::plan(extent, roi, 2, budget);
        assert!(
            tiles.len() > 1,
            "test should actually exercise multiple tiles"
        );

        let tile_kernel = CropKernel::new(&gpu);
        let mut renderer = TiledRender::new(
            std::sync::Arc::clone(&gpu),
            &source,
            &tile_kernel,
            base_transform,
            tiles,
        );
        let mut sink = MemorySink::new(extent);
        loop {
            if renderer.step(&mut sink) == TileStep::Done {
                break;
            }
        }

        for (i, (a, b)) in whole_pixels.iter().zip(sink.pixels.iter()).enumerate() {
            for c in 0..4 {
                assert!(
                    (a[c] - b[c]).abs() < 1e-4,
                    "pixel {i} channel {c}: whole={} tiled={}",
                    a[c],
                    b[c]
                );
            }
        }
    }

    // No "halo=0 produces visible seams" regression test exists here, unlike
    // `spikes/rods`'s AI-tiling seam tests -- and deliberately so, not an oversight. Checked by
    // actually writing one and finding it never fails: `CropKernel::encode`'s `input` is always
    // `self.source`, the *complete, un-windowed* source texture, for every tile (see `step`'s own
    // body) -- `present_sample.wgsl`'s sample is never restricted to a tile-local buffer that
    // could be missing a neighbor pixel, so a halo of 0 is observably identical to any other halo
    // for this specific stage. The halo mechanism (`Tile::padded`, `TilePlanner::plan`'s
    // `halo_px` parameter) still needs to exist and stay correct -- a future stage that *does*
    // tile its own input (an AI/box-filter stage reading a genuinely windowed per-tile buffer,
    // `BlendMode::Feather`'s intended use per the original design sketch) would need it, and
    // `plan_padded_tiles_clamp_to_the_source_extent_not_negative_or_past_the_edge` above already
    // proves the padding math itself is correct independent of whether any current stage reads
    // the padding. Don't add a stage-specific seam test back in without first wiring a stage
    // whose `encode` actually reads a windowed, not whole, input texture.
}
