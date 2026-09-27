//! Cross-API contention: background SCUNet-tile inference via `ort`/CUDA (ADR-0040's real
//! denoise stage, 44.8ms p50 at 256px on the RTX 5080) running concurrently with a foreground
//! `wgpu`/Vulkan dispatch, both against the same physical GPU but through separate driver
//! contexts. Vulkan and CUDA don't share a submission queue the way two `wgpu::Device`s on the
//! same adapter might, so this measures whether the GPU's own hardware scheduler time-slices
//! fairly between them, or whether one starves the other -- unlike `gpu_contend.rs`'s same-API
//! case, this isn't something either API's own docs promise an answer to.
//!
//! Only the scaffolding needed to keep a session busy is duplicated here (not `spikes/rods`'s
//! full tiling/blending pipeline -- that's answering a quality question, this answers a
//! contention-timing one). Same `ort`/`load-dynamic` initialization pattern as
//! `spikes/rods/src/ai.rs`/`spikes/groom/src/ai.rs`.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use ort::session::Session;
use ort::value::Tensor;

#[derive(Debug, thiserror::Error)]
pub enum OrtContendError {
    #[error("model file not found: {0}")]
    ModelNotFound(std::path::PathBuf),
    #[error("ONNX Runtime error: {0}")]
    Ort(String),
}

fn ort_err(e: impl std::fmt::Display) -> OrtContendError {
    OrtContendError::Ort(e.to_string())
}

/// Same reasoning as the identical copies in `spikes/rods/src/ai.rs`, `spikes/groom/src/ai.rs`,
/// `spikes/siamese/src/segment.rs`, and `spikes/litter/src/embed.rs`: the `ort` environment is
/// process-global and `load-dynamic` requires this to run before any other `ort` API call. Keep
/// all five in sync (#179).
///
/// `EnvironmentBuilder::commit()` returning `false` is not a failure: per its own doc comment
/// (ort 2.0.0-rc.13), `false` means "an environment has already been configured" -- `commit()`
/// only inserts the builder into a process-global `OnceLock`, it never calls ONNX Runtime's
/// `CreateEnv` itself, so there is no way for it to report a genuine init failure at all. A real
/// failure (bad dylib, version mismatch) surfaces from `ort::init_from` above instead, and is
/// already propagated by the `?`. So this proceeds either way once `init_from` succeeds, verified
/// in `spikes/groom/tests/ort_cross_module.rs` (exercises all five crates together). **Known
/// limitation**: `ort`'s public API exposes no way to inspect which dylib path the winning
/// environment actually loaded, so a dylib-path mismatch across callers can't be detected here.
/// Execution providers *can* be read back via `Environment::current()?.execution_providers()`,
/// but that only reflects EPs set via `EnvironmentBuilder::with_execution_providers` -- this
/// module and `rods` request EPs per-`Session` instead (`Session::builder().
/// with_execution_providers(...)`), which isn't visible on `Environment` at all. So even with
/// that getter, there's no way to learn which EP a losing caller's session actually ends up
/// using.
pub fn ensure_ort_environment(dylib_path: &Path) -> Result<(), OrtContendError> {
    static INIT: OnceLock<Result<(), String>> = OnceLock::new();
    let result = INIT.get_or_init(|| {
        let builder =
            ort::init_from(dylib_path.to_string_lossy().into_owned()).map_err(|e| e.to_string())?;
        builder.commit();
        Ok(())
    });
    result.clone().map_err(OrtContendError::Ort)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum ExecutionProviderKind {
    #[default]
    Cpu,
    Cuda,
    TensorRt,
}

/// Runs a fixed-size SCUNet/NAFNet-shaped (NCHW float32, one named input, one named output) tile
/// repeatedly -- a background chunk generator, not a correctness-checked denoiser (no output is
/// ever inspected). `tile_size` should match one of ADR-0040's own tested sizes (128/256px --
/// 512 is invalid per SCUNet's window-attention divisibility constraint, see
/// `spikes/rods/src/ai.rs`'s doc comment).
pub struct TileLoad {
    session: Session,
    tile_size: u32,
    input: Vec<f32>,
}

impl TileLoad {
    pub fn load(
        model_path: &Path,
        ort_dylib_path: &Path,
        ep: ExecutionProviderKind,
        tile_size: u32,
    ) -> Result<Self, OrtContendError> {
        if !model_path.is_file() {
            return Err(OrtContendError::ModelNotFound(model_path.to_path_buf()));
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
        let pixels = (tile_size as usize) * (tile_size as usize);
        // Real pixel values don't matter for a contention timing bench -- only shape/cost does.
        let input = vec![0.5f32; pixels * 3];
        Ok(TileLoad {
            session,
            tile_size,
            input,
        })
    }

    /// Runs exactly one tile inference, discarding the output. This is the "chunk" a real bake
    /// worker would submit per SCUNet tile -- see `job.rs`'s own doc comment on why a chunk, not
    /// a whole-image bake, is the right cancellation/contention granularity.
    pub fn run_one(&mut self) -> Result<(), OrtContendError> {
        let shape = [1usize, 3, self.tile_size as usize, self.tile_size as usize];
        let tensor = Tensor::from_array((shape, self.input.clone())).map_err(ort_err)?;
        let outputs = self.session.run(ort::inputs![tensor]).map_err(ort_err)?;
        let _ = outputs[0].try_extract_tensor::<f32>().map_err(ort_err)?;
        Ok(())
    }
}

/// Keeps a [`TileLoad`] continuously running tiles on its own thread until stopped -- the
/// `ort`/CUDA counterpart to `gpu_contend::BackgroundLoad`.
pub struct BackgroundOrtLoad {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<Result<u64, OrtContendError>>>,
}

impl BackgroundOrtLoad {
    pub fn start(mut tile_load: TileLoad) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let handle = std::thread::spawn(move || {
            let mut chunks = 0u64;
            while !thread_stop.load(Ordering::Relaxed) {
                tile_load.run_one()?;
                chunks += 1;
            }
            Ok(chunks)
        });
        BackgroundOrtLoad {
            stop,
            handle: Some(handle),
        }
    }

    pub fn stop(mut self) -> Result<u64, OrtContendError> {
        self.stop.store(true, Ordering::Relaxed);
        self.handle
            .take()
            .unwrap()
            .join()
            .expect("ort background thread panicked")
    }
}

impl Drop for BackgroundOrtLoad {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Sanity-check helper: a caller comparing a CUDA-EP timing number against the CPU-EP number for
/// the same tile size should see a dramatic speedup, matching ADR-0040's own ~36x confirmation --
/// `ort` gives no cheap way to introspect which EP actually served a call, so this is the only
/// available check against a silent CPU fallback.
pub fn suspiciously_close_to_cpu_speed(cuda_ms: f64, cpu_ms: f64) -> bool {
    cuda_ms > cpu_ms * 0.5
}

/// Counts how many `tile x tile` inference calls `TiledDenoiser::denoise`
/// (`spikes/rods/src/ai.rs`) would make to cover a `width x height` frame at this `overlap` --
/// same stride and edge-tile-clamp loop as that function, kept in lockstep with it rather than
/// re-derived from a closed-form formula, since the real loop's "last tile may overshoot the
/// overlap-derived stride" edge behavior (`if x + tile_w >= width { break }`) doesn't reduce to a
/// simple `ceil(width / stride)` once `tile > stride` (an overlap-driven stride, as used here,
/// always has this property). Used to turn an isolated per-tile timing into an estimated
/// whole-frame cost -- #205's own scope, not a duplicate of `rods`'s own full-image timing
/// (`--time` on `rods compare`), which needs a real image to run against.
pub fn tiles_for_frame(width: u32, height: u32, tile: u32, overlap: u32) -> u32 {
    assert!(tile > 0, "tile must be nonzero");
    assert!(
        overlap < tile,
        "overlap ({overlap}) must be strictly less than tile ({tile})"
    );
    let stride = tile.saturating_sub(overlap).max(1);

    let mut count = 0u32;
    let mut y = 0u32;
    loop {
        let tile_h = tile.min(height - y);
        let mut x = 0u32;
        loop {
            let tile_w = tile.min(width - x);
            count += 1;
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
    count
}

#[cfg(test)]
mod tiles_for_frame_tests {
    use super::tiles_for_frame;

    #[test]
    fn exact_multiple_no_overlap() {
        // 4 tiles tile along each axis at stride==tile, no remainder.
        assert_eq!(tiles_for_frame(1024, 1024, 256, 0), 16);
    }

    #[test]
    fn single_tile_covers_whole_frame() {
        assert_eq!(tiles_for_frame(200, 100, 256, 32), 1);
    }

    #[test]
    fn real_full_resolution_frame_128px() {
        // 6064x4040 @ 128px tile / 32px overlap (stride 96). Expected count (2646) computed
        // independently in Python against the same stride/edge-clamp rule, not by re-running this
        // function's own loop -- a bug shared between this function and a hand-inlined copy of
        // its loop would otherwise sail through undetected. Also the figure #205's own ADR/
        // research-doc writeup cites, so this doubles as a regression check on that number.
        assert_eq!(tiles_for_frame(6064, 4040, 128, 32), 2646);
    }

    #[test]
    fn real_full_resolution_frame_256px() {
        // Same independent-oracle reasoning as the 128px test above; 486 is #205's own cited
        // figure for this tile/overlap combination.
        assert_eq!(tiles_for_frame(6064, 4040, 256, 32), 486);
        // Cross-check: a larger tile with the same overlap must need no more tiles than the
        // smaller one over the same frame (coarser stride, fewer steps per axis).
        assert!(tiles_for_frame(6064, 4040, 256, 32) <= tiles_for_frame(6064, 4040, 128, 32));
    }

    #[test]
    #[should_panic(expected = "must be strictly less than tile")]
    fn overlap_not_less_than_tile_panics() {
        tiles_for_frame(100, 100, 64, 64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_not_found_is_a_clean_error() {
        let result = TileLoad::load(
            Path::new("/nonexistent/model.onnx"),
            Path::new("/nonexistent/onnxruntime.so"),
            ExecutionProviderKind::Cpu,
            256,
        );
        assert!(matches!(result, Err(OrtContendError::ModelNotFound(_))));
    }

    #[test]
    fn suspiciously_close_to_cpu_speed_flags_a_likely_fallback() {
        assert!(suspiciously_close_to_cpu_speed(40.0, 44.8));
        assert!(!suspiciously_close_to_cpu_speed(1.2, 44.8));
    }
}
