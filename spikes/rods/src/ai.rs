//! Path B's AI denoise stage (ADR-0024): a tiled `ort` wrapper around an NCHW-float,
//! dynamic-shape ONNX denoiser (NAFNet-SIDD, SCUNet, or any model sharing that input/output
//! contract -- confirmed for both via `onnxruntime`'s own input/output metadata before writing
//! this). Runs on the fixed display-encoded sRGB image (`display::to_display_srgb`'s output),
//! not on linear camera RGB: these candidates are SIDD/synthetic-noise trained on gamma-encoded,
//! WB-applied, phone-ISP-style images, and running them on linear light would be badly
//! out-of-distribution (see the plan's own note on this). Scoring stays in that same fixed
//! display encoding too, so this is an apples-to-apples addition to the classic-demosaic-only
//! baseline `rods compare` already measures, not a second color pipeline.
//!
//! Same `ort`/`load-dynamic` scaffolding pattern as `spikes/groom/src/ai.rs` (one model, one
//! named input, one named output). Execution provider is selectable ([`ExecutionProviderKind`])
//! -- CPU by default, CUDA/TensorRT on request (Windows-native only in practice, since WSL has no
//! CUDA/TensorRT installed at all, see the plan's P0). `ort` falls back to CPU by itself if a
//! requested EP fails to initialize (a missing/incompatible driver), so requesting CUDA/TensorRT
//! is always safe to try -- callers just shouldn't trust the timing number if the fallback fired
//! silently underneath them without checking `Session::execution_providers` or similar.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use ort::session::Session;
use ort::value::Tensor;

#[derive(Debug, thiserror::Error)]
pub enum AiDenoiseError {
    #[error("model file not found: {0}")]
    ModelNotFound(PathBuf),
    #[error("ONNX Runtime error: {0}")]
    Ort(String),
    #[error("model output length {actual} doesn't match expected {expected} (width*height*3)")]
    UnexpectedOutputLength { actual: usize, expected: usize },
    #[error(
        "invalid TileConfig {{ tile: {tile}, overlap: {overlap} }}: tile must be nonzero and \
         overlap must be strictly less than tile"
    )]
    InvalidTileConfig { tile: u32, overlap: u32 },
}

fn ort_err(e: impl std::fmt::Display) -> AiDenoiseError {
    AiDenoiseError::Ort(e.to_string())
}

/// Same reasoning as groom's `ensure_ort_environment`: the `ort` environment is process-global
/// and `load-dynamic` requires this to run before any other `ort` API call.
fn ensure_ort_environment(dylib_path: &Path) -> Result<(), AiDenoiseError> {
    static INIT: OnceLock<Result<(), String>> = OnceLock::new();
    let result = INIT.get_or_init(|| {
        let builder =
            ort::init_from(dylib_path.to_string_lossy().into_owned()).map_err(|e| e.to_string())?;
        if builder.commit() {
            Ok(())
        } else {
            Err("ort environment commit() returned false".to_string())
        }
    });
    result.clone().map_err(AiDenoiseError::Ort)
}

/// A tiled ONNX denoiser: NCHW float32 input/output, dynamic spatial dims, one named input
/// tensor ("input"), one named output tensor ("output") -- confirmed for both NAFNet-SIDD and
/// SCUNet's `deepghs/image_restoration` ONNX exports via their own `onnxruntime` input/output
/// metadata (`docs/licensing.md` has the model rows).
pub struct TiledDenoiser {
    session: Session,
}

/// How the image is split into overlapping tiles before inference, and how the overlap is
/// blended back together. `tile` and `overlap` are both in pixels.
#[derive(Debug, Clone, Copy)]
pub struct TileConfig {
    pub tile: u32,
    pub overlap: u32,
}

impl Default for TileConfig {
    /// The plan's own P3 default: 512-1024px tiles, 32-64px overlap. Picks the smaller/smaller
    /// end of both ranges -- a safer default for an unknown model's VRAM footprint; callers
    /// comparing throughput at a larger tile size pass their own `TileConfig`.
    fn default() -> Self {
        TileConfig {
            tile: 512,
            overlap: 32,
        }
    }
}

/// Which execution provider to request. `ort`/onnxruntime always keeps CPU available as an
/// implicit fallback for any op an accelerated EP can't claim (or if the EP fails to initialize
/// at all, e.g. a missing driver) -- confirmed against this same onnxruntime version in the
/// plan's own P0 hello-world check, so requesting Cuda/TensorRt is always safe to try. There is
/// no cheap way to introspect which EP actually served a given `run()` call after the fact
/// (checked; `ort` 2.0.0-rc.13's `Session` doesn't expose this), so a caller trusting a CUDA/
/// TensorRT timing number should sanity-check it's dramatically faster than the CPU numbers
/// already measured -- a silent fallback would show up as suspiciously CPU-speed timing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum ExecutionProviderKind {
    #[default]
    Cpu,
    Cuda,
    TensorRt,
}

impl TiledDenoiser {
    pub fn load(
        model_path: &Path,
        ort_dylib_path: &Path,
        ep: ExecutionProviderKind,
    ) -> Result<Self, AiDenoiseError> {
        if !model_path.is_file() {
            return Err(AiDenoiseError::ModelNotFound(model_path.to_path_buf()));
        }
        ensure_ort_environment(ort_dylib_path)?;
        let builder = Session::builder().map_err(ort_err)?;
        let mut builder = match ep {
            ExecutionProviderKind::Cpu => builder,
            ExecutionProviderKind::Cuda => builder
                .with_execution_providers([ort::ep::CUDA::default().build()])
                .map_err(ort_err)?,
            ExecutionProviderKind::TensorRt => builder
                .with_execution_providers([ort::ep::TensorRT::default().build()])
                .map_err(ort_err)?,
        };
        let session = builder.commit_from_file(model_path).map_err(ort_err)?;
        Ok(TiledDenoiser { session })
    }

    /// Runs the model on one tile, no larger than the model/hardware can hold in one shot.
    /// `rgb_hwc` is interleaved RGB, `0.0..=1.0`, `width*height*3` samples.
    fn denoise_tile(
        &mut self,
        rgb_hwc: &[f32],
        width: u32,
        height: u32,
    ) -> Result<Vec<f32>, AiDenoiseError> {
        let pixels = width as usize * height as usize;
        anyhow_ensure_len(rgb_hwc.len(), pixels * 3)?;

        // HWC -> planar NCHW (batch=1), the shape every candidate model here expects.
        let mut chw = vec![0.0f32; pixels * 3];
        for (i, px) in rgb_hwc.as_chunks::<3>().0.iter().enumerate() {
            chw[i] = px[0];
            chw[pixels + i] = px[1];
            chw[pixels * 2 + i] = px[2];
        }

        let shape = [1usize, 3, height as usize, width as usize];
        let tensor = Tensor::from_array((shape, chw)).map_err(ort_err)?;
        let outputs = self.session.run(ort::inputs![tensor]).map_err(ort_err)?;
        let (_shape, data) = outputs[0].try_extract_tensor::<f32>().map_err(ort_err)?;

        if data.len() != pixels * 3 {
            return Err(AiDenoiseError::UnexpectedOutputLength {
                actual: data.len(),
                expected: pixels * 3,
            });
        }

        // Planar NCHW -> HWC, matching the caller's own layout.
        let mut out = vec![0.0f32; pixels * 3];
        for i in 0..pixels {
            out[i * 3] = data[i];
            out[i * 3 + 1] = data[pixels + i];
            out[i * 3 + 2] = data[pixels * 2 + i];
        }
        Ok(out)
    }

    /// Runs the model over the whole image, tiled per `config`, with a linear-ramp feathered
    /// blend across each overlap region so tile seams don't show up as a visible or
    /// metrics-detectable artifact. `rgb_hwc` is interleaved RGB, `0.0..=1.0`.
    ///
    /// Every tile fed to the model is exactly `config.tile x config.tile`, even at the image's
    /// right/bottom edge -- found necessary, not just tidy, when a real full-resolution run
    /// surfaced SCUNet's window-attention self-attention rejecting a smaller, non-divisible edge
    /// tile with an ONNX Runtime reshape error (its internal downsampled feature map didn't
    /// divide evenly by its window size). An edge tile's out-of-bounds region is clamp-to-edge
    /// padded (replicating the source image's last real row/column, not zero-filled, so the
    /// model sees continuation rather than a hard black edge) before inference, then only the
    /// tile's real (unpadded) region is used for blending -- the padded region's output is
    /// discarded, never blended in.
    pub fn denoise(
        &mut self,
        rgb_hwc: &[f32],
        width: u32,
        height: u32,
        config: TileConfig,
    ) -> Result<Vec<f32>, AiDenoiseError> {
        let pixels = width as usize * height as usize;
        anyhow_ensure_len(rgb_hwc.len(), pixels * 3)?;

        validate_tile_config(config)?;

        let stride = config.tile.saturating_sub(config.overlap).max(1);
        let mut accum = vec![0.0f64; pixels * 3];
        let mut weight = vec![0.0f64; pixels];

        let mut y = 0u32;
        loop {
            let tile_h = config.tile.min(height - y);
            let mut x = 0u32;
            loop {
                let tile_w = config.tile.min(width - x);

                let tile_data = build_padded_tile(
                    rgb_hwc,
                    TileWindow {
                        source_width: width,
                        source_height: height,
                        x,
                        y,
                        tile_w,
                        tile_h,
                        tile_size: config.tile,
                    },
                );

                let denoised = self.denoise_tile(&tile_data, config.tile, config.tile)?;

                for row in 0..tile_h {
                    for col in 0..tile_w {
                        let w = feather_weight(col, tile_w, config.overlap)
                            * feather_weight(row, tile_h, config.overlap);
                        let global_idx = ((y + row) * width + (x + col)) as usize;
                        let local_idx = (row * config.tile + col) as usize;
                        weight[global_idx] += w;
                        for c in 0..3 {
                            accum[global_idx * 3 + c] += denoised[local_idx * 3 + c] as f64 * w;
                        }
                    }
                }

                if x + tile_w >= width {
                    break;
                }
                x += stride;
            }
            if y + tile_h >= height {
                break;
            }
            y += stride;
        }

        let mut out = vec![0.0f32; pixels * 3];
        for i in 0..pixels {
            let w = weight[i].max(1e-9);
            for c in 0..3 {
                out[i * 3 + c] = (accum[i * 3 + c] / w) as f32;
            }
        }
        Ok(out)
    }
}

/// Builds one `tile_size x tile_size` interleaved-RGB buffer starting at `(x, y)` in `rgb_hwc`
/// (an interleaved RGB image, `width*height*3` samples). The real region is `tile_w x tile_h`
/// (`<= tile_size`, smaller at the image's right/bottom edge); anything beyond that is
/// clamp-to-edge padded from the source image's own last real row/column -- see
/// [`TiledDenoiser::denoise`]'s doc comment for why (a real full-resolution run surfaced a model
/// that rejects a non-divisible tile size, including a smaller edge/remainder tile).
///
/// Pure function, no model/session involved, so this is unit-testable without a real ONNX file --
/// [`TiledDenoiser::denoise`]'s own real-model tests can't run in this sandbox (see this module's
/// own model-less test), but the padding math that feeds it can and should be checked directly.
#[derive(Debug, Clone, Copy)]
struct TileWindow {
    source_width: u32,
    source_height: u32,
    /// Top-left corner of this tile in the source image.
    x: u32,
    y: u32,
    /// The tile's real (unpadded) size -- `<= tile_size`, smaller at the image's right/bottom
    /// edge.
    tile_w: u32,
    tile_h: u32,
    tile_size: u32,
}

fn build_padded_tile(rgb_hwc: &[f32], w: TileWindow) -> Vec<f32> {
    // A zero-sized window has no last-real-row/column to clamp to (the `- 1` below would
    // underflow) -- this is a real caller bug (an empty image or an empty tile), not something
    // to silently wrap around in release builds. `assert!`, not `debug_assert!`: this guards
    // against undefined behavior (an out-of-bounds read past `rgb_hwc`'s end once the
    // wrapped-around value is `.min()`-clamped back into range), not just a logic error worth
    // catching only in debug.
    assert!(
        w.tile_w > 0 && w.tile_h > 0 && w.source_width > 0 && w.source_height > 0,
        "build_padded_tile requires a nonzero tile and source size, got {w:?}"
    );
    let mut tile_data = vec![0.0f32; w.tile_size as usize * w.tile_size as usize * 3];
    for row in 0..w.tile_size {
        let src_row = (w.y + row.min(w.tile_h - 1)).min(w.source_height - 1);
        for col in 0..w.tile_size {
            let src_col = (w.x + col.min(w.tile_w - 1)).min(w.source_width - 1);
            let src_idx = (src_row * w.source_width + src_col) as usize;
            let dst_idx = (row * w.tile_size + col) as usize;
            tile_data[dst_idx * 3..dst_idx * 3 + 3]
                .copy_from_slice(&rgb_hwc[src_idx * 3..src_idx * 3 + 3]);
        }
    }
    tile_data
}

/// Rejects a degenerate `TileConfig` as a clean `Err`, rather than letting it reach the tiling
/// loop -- `tile == 0` would otherwise only surface via `build_padded_tile`'s own assert (a
/// panic, not a `Result`), and `overlap >= tile` silently degrades to an extremely slow 1px
/// stride rather than failing (see `feather_weight`'s own doc comment: correct but not what a
/// caller almost certainly meant).
fn validate_tile_config(config: TileConfig) -> Result<(), AiDenoiseError> {
    if config.tile == 0 || config.overlap >= config.tile {
        return Err(AiDenoiseError::InvalidTileConfig {
            tile: config.tile,
            overlap: config.overlap,
        });
    }
    Ok(())
}

fn anyhow_ensure_len(actual: usize, expected: usize) -> Result<(), AiDenoiseError> {
    if actual != expected {
        return Err(AiDenoiseError::UnexpectedOutputLength { actual, expected });
    }
    Ok(())
}

/// A linear-ramp feather from 0 at the tile edge to 1 at `overlap` pixels in, 1.0 elsewhere --
/// zero right at a true image border (`pos` at the tile's edge *and* that edge is the image's own
/// edge) still gets full weight via the tile-boundary check in `denoise`'s stride logic covering
/// every pixel at least once; a symmetric ramp on both edges is simplest and correct as long as
/// `overlap < tile/2`, which every sane `TileConfig` satisfies.
fn feather_weight(pos: u32, extent: u32, overlap: u32) -> f64 {
    if overlap == 0 {
        return 1.0;
    }
    let from_start = (pos + 1).min(overlap) as f64 / overlap as f64;
    let from_end = (extent - pos).min(overlap) as f64 / overlap as f64;
    from_start.min(from_end).min(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 4x3 RGB image, pixel value at (col,row) = (col*10 + row) repeated across R/G/B, so a
    /// padded/clamped sample's origin is identifiable from its value alone.
    fn test_image() -> Vec<f32> {
        let width = 4u32;
        let height = 3u32;
        let mut out = vec![0.0f32; (width * height * 3) as usize];
        for row in 0..height {
            for col in 0..width {
                let v = (col * 10 + row) as f32;
                let idx = (row * width + col) as usize;
                out[idx * 3..idx * 3 + 3].copy_from_slice(&[v, v, v]);
            }
        }
        out
    }

    fn window(x: u32, y: u32, tile_w: u32, tile_h: u32, tile_size: u32) -> TileWindow {
        TileWindow {
            source_width: 4,
            source_height: 3,
            x,
            y,
            tile_w,
            tile_h,
            tile_size,
        }
    }

    #[test]
    fn build_padded_tile_real_region_is_unpadded() {
        // The tile's real (tile_w x tile_h) region must be an exact copy of the source, whatever
        // padding happens beyond it.
        let img = test_image();
        let tile = build_padded_tile(&img, window(0, 0, 4, 3, 4));
        for row in 0..3u32 {
            for col in 0..4u32 {
                let expected = (col * 10 + row) as f32;
                let idx = (row * 4 + col) as usize;
                assert_eq!(tile[idx * 3], expected, "at ({col},{row})");
            }
        }
    }

    #[test]
    fn build_padded_tile_pads_bottom_edge_by_replicating_last_row() {
        // Image is 4x3; a 4x4 tile at (0,0) has tile_h=3 (real) but tile_size=4, so row 3 must
        // replicate row 2 (the image's real last row), not read out of bounds or zero-fill.
        let img = test_image();
        let tile = build_padded_tile(&img, window(0, 0, 4, 3, 4));
        for col in 0..4u32 {
            let expected_last_real_row = (col * 10 + 2) as f32; // row index 2 is the last real row
            let padded_idx = (3 * 4 + col) as usize; // row 3 (padded)
            assert_eq!(
                tile[padded_idx * 3],
                expected_last_real_row,
                "padded row should replicate the last real row at col {col}"
            );
        }
    }

    #[test]
    fn build_padded_tile_pads_right_edge_by_replicating_last_column() {
        // A 4-wide image with a 6-wide tile: tile_w=4 (real), tile_size=6, so columns 4/5 must
        // replicate column 3 (the image's real last column).
        let img = test_image();
        let tile = build_padded_tile(&img, window(0, 0, 4, 3, 6));
        for row in 0..3u32 {
            let expected_last_real_col = (3 * 10 + row) as f32; // col index 3 is the last real col
            for padded_col in [4u32, 5u32] {
                let padded_idx = (row * 6 + padded_col) as usize;
                assert_eq!(
                    tile[padded_idx * 3],
                    expected_last_real_col,
                    "padded col {padded_col} should replicate the last real col at row {row}"
                );
            }
        }
    }

    #[test]
    #[should_panic(expected = "requires a nonzero tile and source size")]
    fn build_padded_tile_rejects_zero_sized_window() {
        // Regression test for a real adversarial-review finding: a zero-sized window (e.g. from
        // an unvalidated `--crop 0`) used to underflow `tile_h - 1`/`tile_w - 1`, which in a
        // release build wrapped to u32::MAX, got silently `.min()`-clamped back into range, and
        // produced an out-of-bounds read into `rgb_hwc` rather than a clean panic.
        let img = test_image();
        build_padded_tile(&img, window(0, 0, 0, 0, 4));
    }

    #[test]
    fn build_padded_tile_offset_tile_reads_the_right_source_window() {
        // A tile starting at (x=1, y=1), fully interior (2 real cols, 2 real rows within a
        // 4x3 source) -- confirms x/y offsets are applied, not just the (0,0) case above.
        let img = test_image();
        let tile = build_padded_tile(&img, window(1, 1, 2, 2, 2));
        assert_eq!(tile[0], 11.0); // (col=1,row=1) in source coords: col*10 + row = 1*10+1
        assert_eq!(tile[3], (2 * 10 + 1) as f32); // (col=2,row=1)
        assert_eq!(tile[6], 12.0); // (col=1,row=2): col*10 + row = 1*10+2
        assert_eq!(tile[9], (2 * 10 + 2) as f32); // (col=2,row=2)
    }

    #[test]
    fn validate_tile_config_rejects_zero_tile() {
        let result = validate_tile_config(TileConfig {
            tile: 0,
            overlap: 0,
        });
        assert!(matches!(
            result,
            Err(AiDenoiseError::InvalidTileConfig {
                tile: 0,
                overlap: 0
            })
        ));
    }

    #[test]
    fn validate_tile_config_rejects_overlap_at_or_past_tile() {
        assert!(validate_tile_config(TileConfig {
            tile: 256,
            overlap: 256
        })
        .is_err());
        assert!(validate_tile_config(TileConfig {
            tile: 256,
            overlap: 300
        })
        .is_err());
    }

    #[test]
    fn validate_tile_config_accepts_sane_values() {
        assert!(validate_tile_config(TileConfig {
            tile: 256,
            overlap: 16
        })
        .is_ok());
    }

    #[test]
    fn feather_weight_is_one_in_the_interior() {
        assert_eq!(feather_weight(50, 100, 16), 1.0);
    }

    #[test]
    fn feather_weight_ramps_at_edges() {
        assert!(feather_weight(0, 100, 16) < 1.0);
        assert!(feather_weight(99, 100, 16) < 1.0);
        assert!(feather_weight(0, 100, 16) > 0.0);
    }

    #[test]
    fn feather_weight_handles_zero_overlap() {
        assert_eq!(feather_weight(0, 100, 0), 1.0);
        assert_eq!(feather_weight(99, 100, 0), 1.0);
    }

    #[test]
    fn model_not_found_is_a_clean_error() {
        let result = TiledDenoiser::load(
            Path::new("/nonexistent/model.onnx"),
            Path::new("/nonexistent/onnxruntime.so"),
            ExecutionProviderKind::Cpu,
        );
        assert!(matches!(result, Err(AiDenoiseError::ModelNotFound(_))));
    }
}
