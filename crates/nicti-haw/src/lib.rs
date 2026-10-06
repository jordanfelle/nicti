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

/// (Windows only.) ONNX Runtime loads a GPU provider's own dependencies (cuDNN, cuBLAS) by plain `LoadLibrary`
/// name, which on Windows searches the application directory and `PATH` -- *not* the directory the
/// provider DLL itself sits in. Putting the runtime's directory on `PATH` (once, before the first
/// session, under `ensure_committed`'s lock) lets a self-contained GPU pack folder work without
/// the CUDA toolkit being installed (#345).
#[cfg(windows)]
fn prepend_to_path(dir: Option<&Path>) {
    let Some(dir) = dir.filter(|d| !d.as_os_str().is_empty()) else {
        return;
    };
    let mut paths = vec![dir.to_path_buf()];
    if let Some(existing) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&existing));
    }
    if let Ok(joined) = std::env::join_paths(paths) {
        std::env::set_var("PATH", joined);
    }
}

/// Elsewhere the dynamic loader's search path (`LD_LIBRARY_PATH`, rpath) is what matters, and
/// mutating the environment of a running multi-threaded process is a data race.
#[cfg(not(windows))]
fn prepend_to_path(_dir: Option<&Path>) {}

/// Initializes the global `ort` environment exactly once per process, from whichever caller gets
/// there first, and validates every later call (from this crate or a different one) requests that
/// same `dylib_path`. `dylib_path` points at the ONNX Runtime shared library itself
/// (`libonnxruntime.so`/`.dylib`/`onnxruntime.dll`), a *different* file from any caller's own
/// `.onnx` model file.
pub fn ensure_ort_environment(dylib_path: &Path) -> Result<(), OrtInitError> {
    static COMMITTED: Mutex<Option<PathBuf>> = Mutex::new(None);
    ensure_committed(&COMMITTED, dylib_path, |path| {
        prepend_to_path(path.parent());
        let builder =
            ort::init_from(path.to_string_lossy().into_owned()).map_err(|e| e.to_string())?;
        builder.commit();
        Ok(())
    })
}

/// Which ONNX Runtime execution provider a session runs on (#345).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionProvider {
    Cpu,
    /// Any DirectX 12 GPU (needs the DirectML build of ONNX Runtime plus `DirectML.dll`).
    DirectMl,
    /// NVIDIA GPUs only (needs the CUDA build of ONNX Runtime plus the CUDA/cuDNN runtime).
    Cuda,
}

impl ExecutionProvider {
    /// Parses the `NICTI_ORT_EP` override value (`cpu` / `directml` / `cuda`, case-insensitive).
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "cpu" => Some(Self::Cpu),
            "directml" | "dml" => Some(Self::DirectMl),
            "cuda" => Some(Self::Cuda),
            _ => None,
        }
    }

    /// The provider a runtime library can offer: CUDA if the CUDA provider DLL/SO sits beside
    /// `dylib` (a GPU build), else CPU.
    pub fn for_runtime(dylib: &Path) -> Self {
        let has = |name: &str| dylib.parent().is_some_and(|dir| dir.join(name).is_file());
        if has("onnxruntime_providers_cuda.dll") || has("libonnxruntime_providers_cuda.so") {
            Self::Cuda
        } else {
            Self::Cpu
        }
    }

    /// `requested`, unless `NICTI_ORT_EP` names a different provider (benchmarking and
    /// troubleshooting: force `cpu` to rule the GPU out).
    pub fn resolve(requested: Self) -> Self {
        std::env::var("NICTI_ORT_EP")
            .ok()
            .and_then(|v| Self::parse(&v))
            .unwrap_or(requested)
    }
}

/// A session builder together with the execution provider that is *actually* active.
pub struct ConfiguredBuilder {
    pub builder: ort::session::builder::SessionBuilder,
    pub active: ExecutionProvider,
}

/// Starts a session builder on `requested`, falling back to the CPU provider if the GPU provider
/// fails to register (missing DLL, no suitable device, ...). Unlike a plain
/// `with_execution_providers`, which silently falls back inside ONNX Runtime, the caller learns
/// which provider is live via [`ConfiguredBuilder::active`] and the fallback is logged.
pub fn session_builder(requested: ExecutionProvider) -> Result<ConfiguredBuilder, OrtInitError> {
    use ort::ep::ExecutionProviderDispatch;
    use ort::session::Session;

    fn ort_err<R>(e: ort::Error<R>) -> OrtInitError {
        OrtInitError::Ort(e.to_string())
    }
    let gpu: Option<ExecutionProviderDispatch> = match requested {
        ExecutionProvider::Cpu => None,
        ExecutionProvider::DirectMl => {
            Some(ort::ep::DirectML::default().build().error_on_failure())
        }
        // The CUDA EP's defaults (power-of-two arena growth, exhaustive cuDNN algorithm search with
        // the largest workspace) took ~12 GB of VRAM for one BiRefNet bake (#345): bound them.
        ExecutionProvider::Cuda => Some(
            ort::ep::CUDA::default()
                .with_arena_extend_strategy(ort::ep::ArenaExtendStrategy::SameAsRequested)
                .with_conv_algorithm_search(ort::ep::cuda::ConvAlgorithmSearch::Heuristic)
                .with_conv_max_workspace(false)
                .build()
                .error_on_failure(),
        ),
    };
    if let Some(ep) = gpu {
        let attempt = Session::builder().map_err(ort_err)?;
        // DirectML requires sequential execution and no memory-pattern optimisation.
        let attempt = if requested == ExecutionProvider::DirectMl {
            let attempt = attempt.with_memory_pattern(false).map_err(ort_err)?;
            attempt.with_parallel_execution(false).map_err(ort_err)?
        } else {
            attempt
        };
        match attempt.with_execution_providers([ep]) {
            Ok(builder) => {
                return Ok(ConfiguredBuilder {
                    builder,
                    active: requested,
                })
            }
            Err(e) => eprintln!(
                "nicti-haw: {requested:?} execution provider unavailable ({e}); falling back to CPU"
            ),
        }
    }
    Ok(ConfiguredBuilder {
        builder: Session::builder().map_err(ort_err)?,
        active: ExecutionProvider::Cpu,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_execution_provider_names() {
        assert_eq!(
            ExecutionProvider::parse("CPU"),
            Some(ExecutionProvider::Cpu)
        );
        assert_eq!(
            ExecutionProvider::parse(" directml "),
            Some(ExecutionProvider::DirectMl)
        );
        assert_eq!(
            ExecutionProvider::parse("cuda"),
            Some(ExecutionProvider::Cuda)
        );
        assert_eq!(ExecutionProvider::parse("tensorrt"), None);
    }

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
