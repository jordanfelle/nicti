//! Hardware telemetry feeding both Pounce's own VRAM-budget admission control (`admission.rs`)
//! and the standalone bottleneck indicator (#70, which explicitly shares this source rather than
//! having its own). CPU/RAM via `sysinfo` (already a workspace dependency, `spikes/homing` set
//! precedent for this exact crate). VRAM: DXGI (`IDXGIAdapter3::QueryVideoMemoryInfo`), not
//! `nvml-wrapper` -- ADR-0054 evaluated both and picked DXGI because it's vendor-neutral (works
//! on AMD/Intel, not just NVIDIA) and the v1 target is Windows-only anyway (ADR-0015), so there's
//! no cross-platform cost to being Windows-specific here that NVML would avoid; `nvml-wrapper`
//! stays a documented fallback for a future non-NVIDIA verification pass, not implemented here.
//! Windows-only code is unverified in this sandbox (no GPU adapter enumerable under WSL) --
//! same caveat `spikes/homing` already documents for its own Windows-only volume-identity code.

use sysinfo::System;

#[derive(Debug, Clone, Copy)]
pub struct HostTelemetry {
    pub cpu_usage_percent: f32,
    pub used_memory_bytes: u64,
    pub total_memory_bytes: u64,
}

pub struct HostTelemetrySource {
    system: System,
}

impl HostTelemetrySource {
    pub fn new() -> Self {
        let mut system = System::new();
        system.refresh_cpu_usage();
        system.refresh_memory();
        HostTelemetrySource { system }
    }

    /// Refreshes and returns current CPU/RAM readings. `sysinfo`'s own docs require a delay
    /// between two `refresh_cpu_usage` calls before the percentage is meaningful -- callers
    /// polling this repeatedly (as a real telemetry indicator would) get a real number from the
    /// second call onward; a single call right after construction reports 0%.
    pub fn sample(&mut self) -> HostTelemetry {
        self.system.refresh_cpu_usage();
        self.system.refresh_memory();
        HostTelemetry {
            cpu_usage_percent: self.system.global_cpu_usage(),
            used_memory_bytes: self.system.used_memory(),
            total_memory_bytes: self.system.total_memory(),
        }
    }
}

impl Default for HostTelemetrySource {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VramUsage {
    pub used_bytes: u64,
    pub budget_bytes: u64,
}

/// Vendor-neutral VRAM query. `None` when no adapter is queryable (this sandbox: no GPU under
/// WSL) or on a non-Windows target -- callers must treat this as "telemetry unavailable", never
/// substitute a fabricated number, per this repo's own measured-vs-hypothesis convention.
pub trait VramSource {
    fn query(&self) -> Option<VramUsage>;
}

#[cfg(windows)]
pub mod windows_impl {
    //! `IDXGIFactory1::EnumAdapters1` + `IDXGIAdapter3::QueryVideoMemoryInfo`
    //! (`DXGI_MEMORY_SEGMENT_GROUP_LOCAL`, i.e. dedicated VRAM). Unverified in this sandbox --
    //! see this module's parent doc comment.

    use super::{VramSource, VramUsage};
    use windows::core::Interface;
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIAdapter3, IDXGIFactory1, DXGI_MEMORY_SEGMENT_GROUP_LOCAL,
        DXGI_QUERY_VIDEO_MEMORY_INFO,
    };

    pub struct DxgiVramSource;

    impl VramSource for DxgiVramSource {
        fn query(&self) -> Option<VramUsage> {
            unsafe {
                let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
                let adapter = factory.EnumAdapters1(0).ok()?;
                let adapter3: IDXGIAdapter3 = adapter.cast().ok()?;

                let mut info = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
                adapter3
                    .QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut info)
                    .ok()?;

                Some(VramUsage {
                    used_bytes: info.CurrentUsage,
                    budget_bytes: info.Budget,
                })
            }
        }
    }
}

/// A source that never resolves -- the honest default in this sandbox and on non-Windows
/// targets, rather than fabricating a reading.
pub struct UnavailableVramSource;

impl VramSource for UnavailableVramSource {
    fn query(&self) -> Option<VramUsage> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_source_reports_none() {
        assert_eq!(UnavailableVramSource.query(), None);
    }

    #[test]
    fn host_telemetry_sample_reports_nonzero_total_memory() {
        let mut source = HostTelemetrySource::new();
        let sample = source.sample();
        assert!(sample.total_memory_bytes > 0);
    }
}
