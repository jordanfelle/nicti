//! Shared, process-wide `ort` (ONNX Runtime, `load-dynamic`) environment init.
//!
//! Six throwaway spikes (`spikes/groom`, `spikes/siamese`, `spikes/crouch`, `spikes/rods`,
//! `spikes/litter`, `spikes/rosette`) each used to carry their own crate-local copy of
//! `ensure_ort_environment`, gated by their own crate-local `OnceLock`. That was enough to dedupe
//! repeated calls *within* one crate, but `ort`'s own environment is a single process-global --
//! see `ort::EnvironmentBuilder::commit`'s own doc comment (2.0.0-rc.13): returning `false` means
//! "an environment has already been configured", not a failure. Before #179's fix, a crate-local
//! copy treated that `false` as a permanent cached error. #179 fixed that, but left a second gap
//! (#229, this crate): if two of those six spikes ever run `ensure_ort_environment` with a
//! *different* dylib path in the same process, whichever call loses the race silently proceeds
//! against whichever environment the winner actually loaded -- e.g. `crouch`/`rods` requesting a
//! CUDA/TensorRT build could silently fall back to a CPU-only build `groom` happened to load
//! first, with no error and no visible signal.
//!
//! This crate is the single process-wide caller every one of those six now goes through: it
//! records the first successfully-committed dylib path in one process-global [`OnceLock`], and a
//! later call requesting a genuinely different path gets a typed [`OrtInitError::PathMismatch`]
//! before ever reaching `ort::Session::builder()`, rather than silently reusing the wrong
//! environment.
//!
//! **Known limitation** (unchanged from the six original copies): `ort`'s public API exposes no
//! way to inspect which dylib path the winning environment actually loaded, so this can only
//! compare against the path *this crate itself* saw requested first -- it can't detect a mismatch
//! against an `ort` environment some other, non-`nicti-haw` caller committed directly. That's not
//! a real gap today: every `ort`/`load-dynamic` caller in this workspace goes through this crate.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[derive(Debug, thiserror::Error)]
pub enum OrtInitError {
    #[error("ONNX Runtime error: {0}")]
    Ort(String),
    #[error(
        "ONNX Runtime environment already initialized from {existing} by an earlier caller in \
         this process, but {requested} was requested -- ort's environment is process-global and \
         can't be reconfigured once committed"
    )]
    PathMismatch {
        existing: PathBuf,
        requested: PathBuf,
    },
}

/// Pure comparison, split out from [`ensure_ort_environment`] so it's unit-testable without a
/// real ONNX Runtime dylib on disk (every other test here needs `NICTI_TEST_ORT_DYLIB` and is
/// `#[ignore]`d, same posture as each spike's own real-model tests).
fn check_path(committed: &Path, requested: &Path) -> Result<(), OrtInitError> {
    if committed == requested {
        Ok(())
    } else {
        Err(OrtInitError::PathMismatch {
            existing: committed.to_path_buf(),
            requested: requested.to_path_buf(),
        })
    }
}

/// Initializes the global `ort` environment exactly once per process, from whichever caller gets
/// there first, and validates every later call (from this crate or a different one) requests that
/// same `dylib_path`. `dylib_path` points at the ONNX Runtime shared library itself
/// (`libonnxruntime.so`/`.dylib`/`onnxruntime.dll`), a *different* file from any caller's own
/// `.onnx` model file.
pub fn ensure_ort_environment(dylib_path: &Path) -> Result<(), OrtInitError> {
    static INIT: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    let result = INIT.get_or_init(|| {
        let builder =
            ort::init_from(dylib_path.to_string_lossy().into_owned()).map_err(|e| e.to_string())?;
        builder.commit();
        Ok(dylib_path.to_path_buf())
    });

    match result {
        Ok(committed_path) => check_path(committed_path, dylib_path),
        Err(e) => Err(OrtInitError::Ort(e.clone())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_path_is_ok() {
        assert!(check_path(
            Path::new("/opt/ort/libonnxruntime.so"),
            Path::new("/opt/ort/libonnxruntime.so")
        )
        .is_ok());
    }

    #[test]
    fn different_path_is_a_typed_mismatch_error() {
        let err = check_path(
            Path::new("/opt/ort/libonnxruntime.so"),
            Path::new("/opt/ort/other.so"),
        )
        .expect_err("different paths must be rejected");
        match err {
            OrtInitError::PathMismatch {
                existing,
                requested,
            } => {
                assert_eq!(existing, Path::new("/opt/ort/libonnxruntime.so"));
                assert_eq!(requested, Path::new("/opt/ort/other.so"));
            }
            other => panic!("expected PathMismatch, got {other:?}"),
        }
    }
}
