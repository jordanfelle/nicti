//! One process-wide `GpuContext` for every GPU test in this crate.
//!
//! Creating a wgpu adapter/device per test segfaults (`SIGSEGV`) intermittently once several GPU
//! tests run in parallel -- the same failure `nicti-tapetum`'s `test_util::shared_test_gpu`
//! documents (#45 PR4), reliably absent under `--test-threads=1`, so concurrent device creation
//! is the trigger. `wgpu::Device`/`Queue` are `Send + Sync` and built for concurrent use, so the
//! tests still run their actual GPU work in parallel against the one shared device.

use std::sync::{Arc, OnceLock};

use nicti_tapetum::gpu::{GpuContext, GpuPreference};

static SHARED: OnceLock<Option<Arc<GpuContext>>> = OnceLock::new();

/// The shared context, or `None` (with a note on stderr) if there is no adapter -- the caller
/// skips its test, matching the rest of the repo's GPU-test convention.
pub fn shared() -> Option<Arc<GpuContext>> {
    SHARED
        .get_or_init(|| match GpuContext::new(GpuPreference::Auto) {
            Ok(ctx) => Some(Arc::new(ctx)),
            Err(_) => {
                eprintln!("no wgpu adapter available in this environment, skipping");
                None
            }
        })
        .clone()
}
