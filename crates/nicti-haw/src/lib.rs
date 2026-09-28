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
//! records the first successfully-*committed* dylib path in one process-global lock, and a later
//! call requesting a genuinely different path gets a typed [`OrtInitError::PathMismatch`] before
//! ever reaching `ort::Session::builder()`, rather than silently reusing the wrong environment.
//!
//! **Only a successful commit is recorded, not a failed attempt** -- deliberately not a plain
//! `OnceLock<Result<PathBuf, String>>` (an earlier draft of this crate used exactly that, and an
//! adversarial review caught the regression it introduces): with a shared `OnceLock`, one crate
//! calling first with a bad path (a typo, a missing file) would permanently cache that failure for
//! every one of the six crates, forever -- worse than the six-separate-`OnceLock`s status quo this
//! crate replaces, where a bad path in one crate never touched the other five. `ort::init_from`
//! failing means `commit()` was never reached, so `ort`'s own process-global environment was never
//! actually set -- a later caller with a good path can and should still succeed. See
//! `ensure_committed`'s tests for both directions (a failed first attempt doesn't block a later
//! good one; a *successful* first commit does still gate a later, different path).
//!
//! **Known limitation** (unchanged from the six original copies): `ort`'s public API exposes no
//! way to inspect which dylib path the winning environment actually loaded, so this can only
//! compare against the path *this crate itself* saw requested first -- it can't detect a mismatch
//! against an `ort` environment some other, non-`nicti-haw` caller committed directly. That's not
//! a real gap today: every `ort`/`load-dynamic` caller in this workspace goes through this crate.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

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

/// Pure comparison, split out from [`ensure_committed`] so it's unit-testable without a real
/// ONNX Runtime dylib on disk.
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

/// The actual state-machine logic, parameterized over `commit` so it's unit-testable without a
/// real ONNX Runtime dylib on disk: a fake `commit` closure can simulate a failure followed by a
/// success, which needs `dylib_path` to stay unresolved (not cached) across that first failed
/// call -- see this module's tests. The real [`ensure_ort_environment`] below is a thin wrapper
/// passing the real `ort::init_from`/`commit()` pair in as `commit`.
///
/// `committed` is locked for the duration of one call, including the (real, dlopen-doing) `commit`
/// closure -- serializing concurrent first-callers is the right tradeoff here: this only runs
/// once per process in practice (every caller after the first hits the cheap `Some(..)` branch
/// without invoking `commit` at all), so contention is a non-issue.
fn ensure_committed(
    committed: &Mutex<Option<PathBuf>>,
    dylib_path: &Path,
    commit: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<(), OrtInitError> {
    let mut guard = committed
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match guard.as_ref() {
        Some(existing) => check_path(existing, dylib_path),
        None => match commit(dylib_path) {
            Ok(()) => {
                *guard = Some(dylib_path.to_path_buf());
                Ok(())
            }
            // Deliberately not stored in `guard` -- see this module's doc comment on why a
            // failed attempt must stay retryable by a later caller with a different (correct)
            // path, rather than permanently poisoning every caller in the process.
            Err(e) => Err(OrtInitError::Ort(e)),
        },
    }
}

/// Initializes the global `ort` environment exactly once per process, from whichever caller gets
/// there first, and validates every later call (from this crate or a different one) requests that
/// same `dylib_path`. `dylib_path` points at the ONNX Runtime shared library itself
/// (`libonnxruntime.so`/`.dylib`/`onnxruntime.dll`), a *different* file from any caller's own
/// `.onnx` model file.
pub fn ensure_ort_environment(dylib_path: &Path) -> Result<(), OrtInitError> {
    static COMMITTED: Mutex<Option<PathBuf>> = Mutex::new(None);
    ensure_committed(&COMMITTED, dylib_path, |path| {
        let builder =
            ort::init_from(path.to_string_lossy().into_owned()).map_err(|e| e.to_string())?;
        builder.commit();
        Ok(())
    })
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

    #[test]
    fn a_failed_first_attempt_does_not_poison_a_later_successful_one() {
        let committed: Mutex<Option<PathBuf>> = Mutex::new(None);

        let first = ensure_committed(&committed, Path::new("/bad.so"), |_| {
            Err("dlopen failed".to_string())
        });
        assert!(matches!(first, Err(OrtInitError::Ort(msg)) if msg == "dlopen failed"));

        let second = ensure_committed(&committed, Path::new("/good.so"), |_| Ok(()));
        assert!(
            second.is_ok(),
            "a later caller with a correct path must not inherit an earlier caller's failure"
        );
    }

    #[test]
    fn a_successful_commit_rejects_a_later_different_path() {
        let committed: Mutex<Option<PathBuf>> = Mutex::new(None);
        ensure_committed(&committed, Path::new("/good.so"), |_| Ok(())).expect("first call");

        let err = ensure_committed(&committed, Path::new("/other.so"), |_| Ok(()))
            .expect_err("a different path must be rejected once one has already committed");
        assert!(matches!(err, OrtInitError::PathMismatch { .. }));
    }

    #[test]
    fn a_successful_commit_accepts_a_later_same_path_without_recommitting() {
        let committed: Mutex<Option<PathBuf>> = Mutex::new(None);
        ensure_committed(&committed, Path::new("/good.so"), |_| Ok(())).expect("first call");

        // If this ran the `commit` closure again, the panic below would fail the test -- proving
        // a same-path second call reuses the recorded state instead of recommitting.
        let second = ensure_committed(&committed, Path::new("/good.so"), |_| {
            panic!("must not recommit for a matching path")
        });
        assert!(second.is_ok());
    }
}
