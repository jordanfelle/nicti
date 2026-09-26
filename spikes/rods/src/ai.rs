//! Path B's AI denoise stage (ADR-0023): a tiled `ort` wrapper around an NCHW-float,
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
//! named input, one named output) -- CPU execution provider only for now (this WSL sandbox has
//! no CUDA/TensorRT installed, see the plan's P0; CUDA/TensorRT EP registration is P5's
//! Windows-native job, gated behind this crate's own `cuda`/`tensorrt` features once wired).

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

impl TiledDenoiser {
    pub fn load(model_path: &Path, ort_dylib_path: &Path) -> Result<Self, AiDenoiseError> {
        if !model_path.is_file() {
            return Err(AiDenoiseError::ModelNotFound(model_path.to_path_buf()));
        }
        ensure_ort_environment(ort_dylib_path)?;
        let session = Session::builder()
            .map_err(ort_err)?
            .commit_from_file(model_path)
            .map_err(ort_err)?;
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
    pub fn denoise(
        &mut self,
        rgb_hwc: &[f32],
        width: u32,
        height: u32,
        config: TileConfig,
    ) -> Result<Vec<f32>, AiDenoiseError> {
        let pixels = width as usize * height as usize;
        anyhow_ensure_len(rgb_hwc.len(), pixels * 3)?;

        let stride = config.tile.saturating_sub(config.overlap).max(1);
        let mut accum = vec![0.0f64; pixels * 3];
        let mut weight = vec![0.0f64; pixels];

        let mut y = 0u32;
        loop {
            let tile_h = config.tile.min(height - y);
            let mut x = 0u32;
            loop {
                let tile_w = config.tile.min(width - x);

                let mut tile_data = vec![0.0f32; tile_w as usize * tile_h as usize * 3];
                for row in 0..tile_h {
                    let src_start = (((y + row) * width + x) * 3) as usize;
                    let src_end = src_start + tile_w as usize * 3;
                    let dst_start = (row * tile_w * 3) as usize;
                    tile_data[dst_start..dst_start + tile_w as usize * 3]
                        .copy_from_slice(&rgb_hwc[src_start..src_end]);
                }

                let denoised = self.denoise_tile(&tile_data, tile_w, tile_h)?;

                for row in 0..tile_h {
                    for col in 0..tile_w {
                        let w = feather_weight(col, tile_w, config.overlap)
                            * feather_weight(row, tile_h, config.overlap);
                        let global_idx = ((y + row) * width + (x + col)) as usize;
                        let local_idx = (row * tile_w + col) as usize;
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
        );
        assert!(matches!(result, Err(AiDenoiseError::ModelNotFound(_))));
    }
}
