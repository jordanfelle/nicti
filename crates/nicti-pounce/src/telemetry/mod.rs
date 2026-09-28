//! Hardware telemetry feeding both Pounce's own VRAM-budget admission control (`admission.rs`)
//! and the standalone bottleneck indicator (#70, `nicti_pounce::hackles`), which shares this
//! source rather than having its own. CPU/RAM via `sysinfo` (already a workspace dependency,
//! `spikes/homing` set precedent for this exact crate). VRAM: DXGI (`IDXGIAdapter3::
//! QueryVideoMemoryInfo`), not `nvml-wrapper` -- ADR-0054 evaluated both and picked DXGI because
//! it's vendor-neutral (works on AMD/Intel, not just NVIDIA) and the v1 target is Windows-only
//! anyway (ADR-0015), so there's no cross-platform cost to being Windows-specific here that NVML
//! would avoid; `nvml-wrapper` stays a documented fallback for a future non-NVIDIA verification
//! pass, not implemented here.
//!
//! GPU busy% and disk busy% (#70, ADR-0070) follow the same vendor-neutral reasoning: both come
//! from Windows PDH counters (`telemetry::pdh`) rather than `nvml-wrapper`, so a non-NVIDIA GPU
//! still gets a real reading instead of "n/a". Windows-only code is unverified in this sandbox (no
//! GPU adapter enumerable under WSL) -- same caveat `spikes/homing` already documents for its own
//! Windows-only volume-identity code.
//!
//! `HostTelemetrySource`/`VramSource` are unchanged from `spikes/crouch::telemetry`.
//! `TelemetrySampler` now owns a background thread (ADR-0070): querying the GPU Engine wildcard
//! counter can enumerate many instances and take tens of milliseconds, and `DxgiVramSource`
//! creates a fresh DXGI factory on every call -- neither should run on the UI thread inside
//! `activity::show`, which used to poll this directly once per frame. The thread samples at most
//! once per `min_interval` and calls back into the UI to request a repaint, since the app
//! otherwise only repaints on input or a Pounce job change.

pub mod pdh;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

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

/// GPU-busy% and disk-busy% (#70/ADR-0070). Each field is `None` when that reading isn't
/// available yet -- either the platform can't provide it, or (PDH rate counters specifically)
/// the source needs a second `query()` call before a real delta exists. Never fabricated as 0.
#[derive(Debug, Clone, Copy, Default)]
pub struct LoadReading {
    pub gpu_busy_percent: Option<f32>,
    pub disk_busy_percent: Option<f32>,
}

/// Vendor-neutral GPU/disk load query, mirroring [`VramSource`]'s honesty convention. Unlike
/// `VramSource::query`, this takes `&mut self` -- a real implementation (`pdh::PdhLoadSource`)
/// holds an open query handle across calls, since PDH rate counters need state between
/// collections to compute a delta.
pub trait LoadSource: Send {
    fn query(&mut self) -> LoadReading;
}

/// A source that never resolves -- the honest default in this sandbox and on non-Windows
/// targets, and the fallback if PDH itself fails to open (e.g. those counters don't exist on
/// this Windows build).
pub struct UnavailableLoadSource;

impl LoadSource for UnavailableLoadSource {
    fn query(&mut self) -> LoadReading {
        LoadReading::default()
    }
}

/// Returns the real PDH load source on Windows (falling back to [`UnavailableLoadSource`] if PDH
/// setup fails), [`UnavailableLoadSource`] everywhere else.
pub fn default_load_source() -> Box<dyn LoadSource> {
    #[cfg(windows)]
    {
        match pdh::PdhLoadSource::new() {
            Some(source) => Box::new(source),
            None => Box::new(UnavailableLoadSource),
        }
    }
    #[cfg(not(windows))]
    {
        Box::new(UnavailableLoadSource)
    }
}

/// One combined CPU/RAM/VRAM/GPU-busy/disk-busy reading.
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    pub host: HostTelemetry,
    pub vram: Option<VramUsage>,
    pub load: LoadReading,
}

/// Owns a background thread that samples host/VRAM/load telemetry at most once per
/// `min_interval` and calls `on_sample` after each real sample, so a caller (the activity panel)
/// can request a UI repaint -- the app otherwise only repaints on input or a Pounce job change,
/// so the readout would go stale during load Pounce didn't start (e.g. a GPU load driven outside
/// the scheduler, like the Develop view's continuous render). `sample()` is a non-blocking read
/// of the latest value: `None` until the first sample lands, never blocks on the query itself.
pub struct TelemetrySampler {
    latest: Arc<Mutex<Option<Sample>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl TelemetrySampler {
    pub fn spawn(
        vram: Box<dyn VramSource>,
        load: Box<dyn LoadSource>,
        min_interval: Duration,
        on_sample: impl Fn() + Send + 'static,
    ) -> Self {
        let latest = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let latest_thread = Arc::clone(&latest);
        let stop_thread = Arc::clone(&stop);

        let handle = thread::spawn(move || {
            let mut host = HostTelemetrySource::new();
            let mut load = load;

            while !stop_thread.load(Ordering::Relaxed) {
                let sample = Sample {
                    host: host.sample(),
                    vram: vram.query(),
                    load: load.query(),
                };
                *latest_thread.lock().unwrap() = Some(sample);
                on_sample();

                // Sleep in small chunks rather than one `thread::sleep(min_interval)` so
                // dropping the sampler (which sets `stop`) doesn't have to wait out a whole
                // interval before the thread notices and joins.
                let poll = Duration::from_millis(50);
                let mut waited = Duration::ZERO;
                while waited < min_interval {
                    if stop_thread.load(Ordering::Relaxed) {
                        return;
                    }
                    let chunk = poll.min(min_interval - waited);
                    thread::sleep(chunk);
                    waited += chunk;
                }
            }
        });

        TelemetrySampler {
            latest,
            stop,
            handle: Some(handle),
        }
    }

    /// Returns the latest sample without blocking. `None` before the first sample lands.
    pub fn sample(&self) -> Option<Sample> {
        *self.latest.lock().unwrap()
    }
}

impl Drop for TelemetrySampler {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn unavailable_vram_source_reports_none() {
        assert_eq!(UnavailableVramSource.query(), None);
    }

    #[test]
    fn unavailable_load_source_reports_no_readings() {
        let reading = UnavailableLoadSource.query();
        assert_eq!(reading.gpu_busy_percent, None);
        assert_eq!(reading.disk_busy_percent, None);
    }

    #[test]
    fn host_telemetry_sample_reports_nonzero_total_memory() {
        let mut source = HostTelemetrySource::new();
        let sample = source.sample();
        assert!(sample.total_memory_bytes > 0);
    }

    #[test]
    fn sampler_reports_none_before_first_sample_then_a_real_one() {
        let sampler = TelemetrySampler::spawn(
            Box::new(UnavailableVramSource),
            Box::new(UnavailableLoadSource),
            Duration::from_millis(20),
            || {},
        );
        // Immediately after spawn, the background thread may not have sampled yet.
        let mut saw_sample = sampler.sample().is_some();
        if !saw_sample {
            thread::sleep(Duration::from_millis(200));
            saw_sample = sampler.sample().is_some();
        }
        assert!(saw_sample, "expected a sample within 200ms of spawning");
    }

    #[test]
    fn sampler_calls_on_sample_after_each_real_sample() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_cb = Arc::clone(&calls);
        let sampler = TelemetrySampler::spawn(
            Box::new(UnavailableVramSource),
            Box::new(UnavailableLoadSource),
            Duration::from_millis(20),
            move || {
                calls_cb.fetch_add(1, Ordering::Relaxed);
            },
        );
        thread::sleep(Duration::from_millis(150));
        drop(sampler);
        assert!(
            calls.load(Ordering::Relaxed) >= 2,
            "expected at least 2 samples in 150ms at a 20ms interval, got {}",
            calls.load(Ordering::Relaxed)
        );
    }

    #[test]
    fn dropping_the_sampler_stops_the_background_thread_promptly() {
        let sampler = TelemetrySampler::spawn(
            Box::new(UnavailableVramSource),
            Box::new(UnavailableLoadSource),
            Duration::from_secs(3600),
            || {},
        );
        let start = std::time::Instant::now();
        drop(sampler);
        // The sampler's own poll chunk is 50ms; dropping should never wait out the (here,
        // 1-hour) min_interval.
        assert!(start.elapsed() < Duration::from_millis(500));
    }
}
