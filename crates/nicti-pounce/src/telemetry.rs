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
//!
//! `HostTelemetrySource`/`VramSource` are unchanged from `spikes/crouch::telemetry`.
//! `TelemetrySampler` is new: a throttled (<=1 sample/s) wrapper the activity panel polls every
//! frame without hammering `sysinfo`/DXGI on every repaint.

use std::time::{Duration, Instant};

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
pub trait VramSource: Send + Sync {
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

/// Returns the real DXGI VRAM source on Windows, [`UnavailableVramSource`] everywhere else --
/// the one place a caller (the activity panel, #70) needs to pick between them.
pub fn default_vram_source() -> Box<dyn VramSource> {
    #[cfg(windows)]
    {
        Box::new(windows_impl::DxgiVramSource)
    }
    #[cfg(not(windows))]
    {
        Box::new(UnavailableVramSource)
    }
}

/// One combined CPU/RAM/VRAM reading.
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    pub host: HostTelemetry,
    pub vram: Option<VramUsage>,
}

/// Wraps [`HostTelemetrySource`] and a [`VramSource`], throttled to at most one real sample per
/// `min_interval` -- a caller polling this once per UI frame (60+ times/s) shouldn't hit
/// `sysinfo`/DXGI that often. Returns the last real sample on every call inside the window.
pub struct TelemetrySampler {
    host: HostTelemetrySource,
    vram: Box<dyn VramSource>,
    min_interval: Duration,
    last: Option<(Instant, Sample)>,
}

impl TelemetrySampler {
    pub fn new(vram: Box<dyn VramSource>, min_interval: Duration) -> Self {
        TelemetrySampler {
            host: HostTelemetrySource::new(),
            vram,
            min_interval,
            last: None,
        }
    }

    pub fn sample(&mut self) -> Sample {
        if let Some((at, sample)) = self.last {
            if at.elapsed() < self.min_interval {
                return sample;
            }
        }
        let sample = Sample {
            host: self.host.sample(),
            vram: self.vram.query(),
        };
        self.last = Some((Instant::now(), sample));
        sample
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

    #[test]
    fn sampler_reuses_the_last_sample_inside_the_window() {
        let mut sampler =
            TelemetrySampler::new(Box::new(UnavailableVramSource), Duration::from_secs(3600));
        let first = sampler.sample();
        let second = sampler.sample();
        assert_eq!(
            first.host.total_memory_bytes,
            second.host.total_memory_bytes
        );
    }
}
