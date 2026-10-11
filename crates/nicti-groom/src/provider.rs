//! Execution-provider selection for the removal sessions (#322, ADR-0050/0051).
//!
//! The runtime decides: a CUDA build of ONNX Runtime (the optional NVIDIA GPU pack, #345, or a
//! `NICTI_ORT_DYLIB` pointing at one) runs MobileSAM and LaMa on the CUDA provider; the CPU build
//! -- or a GPU provider that can't register or load a model -- runs on the CPU. `NICTI_ORT_EP`
//! overrides, as for BiRefNet.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use nicti_haw::ExecutionProvider;
use ort::session::Session;

use crate::{ort_err, RemovalError};

/// What one removal claims from Pounce's VRAM budget while the sessions are on a GPU provider.
///
/// **TEMPORARY ESTIMATE (#322), not a measurement**: MobileSAM's encoder (1024-px frame, ~40 MB of weights) plus
/// LaMa fp32 (512x512, ~200 MB of weights, FFC spectral activations) with the bounded CUDA arena
/// from `nicti_haw::session_builder`. It is deliberately generous; replace it with the peak
/// measured on the RTX 5080 reference machine (the way BiRefNet's `GPU_VRAM_BYTES` was, #345).
pub const GPU_VRAM_BYTES_ESTIMATE: u64 = 3 << 30;

static DECLARED_VRAM: AtomicU64 = AtomicU64::new(0);

/// What a removal job declares in its `JobSpec`: [`GPU_VRAM_BYTES_ESTIMATE`] once the sessions are running
/// on a GPU provider, else 0 (the CPU provider uses no VRAM, and nothing is loaded before the
/// first removal -- so the first GPU removal's own load runs under a 0 claim, as BiRefNet's does).
/// Process-wide, last writer wins: it is not shared with BiRefNet's own claim. Lock-free because `RemoveJob::spec` runs on the UI thread while a removal may
/// hold the backend.
pub fn declared_vram_bytes() -> u64 {
    DECLARED_VRAM.load(Ordering::Relaxed)
}

pub(crate) fn vram_for(active: ExecutionProvider) -> u64 {
    match active {
        ExecutionProvider::Cpu => 0,
        _ => GPU_VRAM_BYTES_ESTIMATE,
    }
}

pub(crate) fn declare(active: ExecutionProvider) {
    DECLARED_VRAM.store(vram_for(active), Ordering::Relaxed);
}

/// The provider a runtime library should be asked for (`NICTI_ORT_EP` wins).
pub fn requested_for(dylib: &Path) -> ExecutionProvider {
    ExecutionProvider::resolve(ExecutionProvider::for_runtime(dylib))
}

/// Opens `model` on `requested`, falling back to the CPU provider if the GPU provider can't
/// register or the model won't load on it. Returns the session and the provider really in use.
pub(crate) fn open_session(
    model: &Path,
    dylib: &Path,
    requested: ExecutionProvider,
) -> Result<(Session, ExecutionProvider), RemovalError> {
    if !model.is_file() {
        return Err(RemovalError::ModelNotFound(model.to_path_buf()));
    }
    nicti_haw::ensure_ort_environment(dylib).map_err(ort_err)?;
    let nicti_haw::ConfiguredBuilder {
        mut builder,
        active,
    } = nicti_haw::session_builder(requested).map_err(ort_err)?;
    match builder.commit_from_file(model) {
        Ok(session) => Ok((session, active)),
        Err(e) if active != ExecutionProvider::Cpu => {
            eprintln!(
                "nicti-groom: {} failed to load on {active:?} ({e}); using the CPU",
                model.display()
            );
            open_session(model, dylib, ExecutionProvider::Cpu)
        }
        Err(e) => Err(ort_err(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_gpu_provider_claims_vram() {
        assert_eq!(vram_for(ExecutionProvider::Cpu), 0);
        assert_eq!(vram_for(ExecutionProvider::Cuda), GPU_VRAM_BYTES_ESTIMATE);
        assert_eq!(
            vram_for(ExecutionProvider::DirectMl),
            GPU_VRAM_BYTES_ESTIMATE
        );
    }

    #[test]
    fn a_runtime_without_the_cuda_provider_asks_for_the_cpu() {
        // `NICTI_ORT_EP` is process-wide; skip rather than race a developer's override.
        if std::env::var_os("NICTI_ORT_EP").is_some() {
            return;
        }
        let dir = std::env::temp_dir().join("nicti-groom-provider-cpu");
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(
            requested_for(&dir.join("libonnxruntime.so")),
            ExecutionProvider::Cpu
        );
    }

    #[test]
    fn a_missing_model_is_reported_before_any_provider_work() {
        let r = open_session(
            Path::new("/nonexistent/m.onnx"),
            Path::new("/nonexistent/libonnxruntime.so"),
            ExecutionProvider::Cpu,
        );
        assert!(matches!(r, Err(RemovalError::ModelNotFound(_))));
    }
}
