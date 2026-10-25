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

    /// Like [`query`](Self::query) but for the adapter with this PCI vendor/device id -- the one
    /// wgpu actually rendered on, which on a hybrid machine is not necessarily DXGI's adapter 0.
    /// `None` if no such adapter is queryable. The default ignores the ids (a source with only
    /// one adapter to report).
    fn query_for(&self, _vendor_id: u32, _device_id: u32) -> Option<VramUsage> {
        self.query()
    }
}

#[cfg(windows)]
pub mod windows_impl {
    //! `IDXGIFactory1::EnumAdapters1` + `IDXGIAdapter3::QueryVideoMemoryInfo`
    //! (`DXGI_MEMORY_SEGMENT_GROUP_LOCAL`, i.e. dedicated VRAM). Unverified in this sandbox --
    //! see this module's parent doc comment.

    use super::{VramSource, VramUsage};
    use windows::core::Interface;
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIAdapter1, IDXGIAdapter3, IDXGIFactory1,
        DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_QUERY_VIDEO_MEMORY_INFO,
    };

    pub struct DxgiVramSource;

    /// `QueryVideoMemoryInfo` for one adapter's dedicated (local) segment.
    unsafe fn usage_of(adapter: &IDXGIAdapter1) -> Option<VramUsage> {
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

    impl VramSource for DxgiVramSource {
        fn query(&self) -> Option<VramUsage> {
            unsafe {
                let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
                let adapter = factory.EnumAdapters1(0).ok()?;
                usage_of(&adapter)
            }
        }

        fn query_for(&self, vendor_id: u32, device_id: u32) -> Option<VramUsage> {
            unsafe {
                let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
                let mut index = 0;
                // `EnumAdapters1` errors (DXGI_ERROR_NOT_FOUND) past the last adapter.
                while let Ok(adapter) = factory.EnumAdapters1(index) {
                    index += 1;
                    if let Ok(desc) = adapter.GetDesc1() {
                        if desc.VendorId == vendor_id && desc.DeviceId == device_id {
                            return usage_of(&adapter);
                        }
                    }
                }
                None
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

/// Percent of the OS-granted dedicated-VRAM budget the GPU lane's admission control may reserve;
/// the rest is held back for the live Develop render, the compositor and other processes.
/// A hypothesis (#255), not a measurement -- re-tune on real hardware.
pub const GPU_LANE_BUDGET_PERCENT: u64 = 50;

/// The lane budget when no VRAM telemetry resolves (non-Windows, software adapter, DXGI failure).
/// A hypothesis (#255), not a measurement: sized so one 45 MP render fits with headroom.
pub const GPU_LANE_FALLBACK_BUDGET_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Floor under a *measured* lane budget, so a nearly-full GPU doesn't shrink it below what a
/// small render needs (such a render then waits for the lane rather than being dropped).
pub const GPU_LANE_MIN_BUDGET_BYTES: u64 = 512 * 1024 * 1024;

/// Where [`gpu_lane_budget`]'s number came from, so a caller can tell a measured budget from the
/// fallback and never present the latter as measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetSource {
    /// A share of the adapter's real DXGI budget.
    Measured,
    /// The unmeasured fallback constant.
    Fallback,
}

/// The GPU lane's total VRAM admission budget for Pounce (ADR-0054 decision rule #5), from the
/// adapter's reported usage when there is one.
pub fn gpu_lane_budget(usage: Option<VramUsage>) -> (u64, BudgetSource) {
    match usage {
        Some(u) if u.budget_bytes > 0 => (
            ((u128::from(u.budget_bytes) * u128::from(GPU_LANE_BUDGET_PERCENT) / 100) as u64)
                .max(GPU_LANE_MIN_BUDGET_BYTES),
            BudgetSource::Measured,
        ),
        _ => (GPU_LANE_FALLBACK_BUDGET_BYTES, BudgetSource::Fallback),
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

impl Sample {
    // The readout strings the activity panel draws, kept here so the repaint gate
    // ([`Self::differs_visibly`]) can never drift from what is on screen.

    pub fn cpu_text(&self) -> String {
        format!("CPU {:.0}%", self.host.cpu_usage_percent)
    }

    pub fn ram_text(&self) -> String {
        format!(
            "RAM {:.1}/{:.1} GB",
            self.host.used_memory_bytes as f64 / 1e9,
            self.host.total_memory_bytes as f64 / 1e9
        )
    }

    pub fn vram_text(&self) -> String {
        match self.vram {
            Some(v) => format!(
                "VRAM {:.2}/{:.2} GB",
                v.used_bytes as f64 / 1e9,
                v.budget_bytes as f64 / 1e9
            ),
            None => "VRAM: n/a".to_string(),
        }
    }

    pub fn gpu_text(&self) -> String {
        match self.load.gpu_busy_percent {
            Some(pct) => format!("GPU {pct:.0}%"),
            None => "GPU: n/a".to_string(),
        }
    }

    pub fn disk_text(&self) -> String {
        match self.load.disk_busy_percent {
            Some(pct) => format!("Disk {pct:.0}%"),
            None => "Disk: n/a".to_string(),
        }
    }

    /// Would drawing `self` instead of `previous` change a pixel (#512)? True when any readout
    /// text differs, or when a CPU/GPU/Disk reading is at or above the busy threshold and has
    /// moved at all: the bottleneck verdict compares the raw percentages there (hysteresis,
    /// picking the busiest), so two samples with identical text can still classify differently.
    /// While everything is calm the verdict is `Idle` whatever the exact values.
    pub fn differs_visibly(&self, previous: &Sample) -> bool {
        !self.changed_fields(previous).is_empty()
    }

    /// Which readouts make [`Self::differs_visibly`] true: `cpu`/`ram`/`vram`/`gpu`/`disk` for a
    /// changed text, `busy-raw` for a moved reading at or above the busy threshold.
    pub fn changed_fields(&self, previous: &Sample) -> Vec<&'static str> {
        let mut out = Vec::new();
        for (name, now, before) in [
            ("cpu", self.cpu_text(), previous.cpu_text()),
            ("ram", self.ram_text(), previous.ram_text()),
            ("vram", self.vram_text(), previous.vram_text()),
            ("gpu", self.gpu_text(), previous.gpu_text()),
            ("disk", self.disk_text(), previous.disk_text()),
        ] {
            if now != before {
                out.push(name);
            }
        }
        let raw = |s: &Sample| {
            [
                Some(s.host.cpu_usage_percent),
                s.load.gpu_busy_percent,
                s.load.disk_busy_percent,
            ]
        };
        if raw(self)
            .into_iter()
            .zip(raw(previous))
            .any(|(now, before)| {
                now != before
                    && [now, before]
                        .into_iter()
                        .flatten()
                        .any(|p| !crate::hackles::is_calm(p))
            })
        {
            out.push("busy-raw");
        }
        out
    }
}

/// Whether the sampler thread calls back for `sample`, given the last one it reported.
fn should_report(only_on_change: bool, reported: Option<&Sample>, sample: &Sample) -> bool {
    let report = !only_on_change || reported.is_none_or(|r| sample.differs_visibly(r));
    if only_on_change && report && std::env::var_os("NICTI_REPAINT_TRACE").is_some() {
        // Diagnostic for #512: which readout made this sample worth a frame.
        if let Some(r) = reported {
            eprintln!("telemetry-gate: changed {:?}", sample.changed_fields(r));
        }
    }
    report
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
        Self::spawn_inner(vram, load, min_interval, false, on_sample)
    }

    /// Like [`Self::spawn`], but `on_change` fires only for the first sample and for samples that
    /// [`Sample::differs_visibly`] from the last one reported. An idle window then isn't woken
    /// twice a second to redraw identical text (#512); `sample()` still always returns the newest.
    pub fn spawn_on_change(
        vram: Box<dyn VramSource>,
        load: Box<dyn LoadSource>,
        min_interval: Duration,
        on_change: impl Fn() + Send + 'static,
    ) -> Self {
        Self::spawn_inner(vram, load, min_interval, true, on_change)
    }

    fn spawn_inner(
        vram: Box<dyn VramSource>,
        load: Box<dyn LoadSource>,
        min_interval: Duration,
        only_on_change: bool,
        on_sample: impl Fn() + Send + 'static,
    ) -> Self {
        let latest = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let latest_thread = Arc::clone(&latest);
        let stop_thread = Arc::clone(&stop);

        let handle = thread::spawn(move || {
            let mut host = HostTelemetrySource::new();
            let mut load = load;
            let mut reported: Option<Sample> = None;

            while !stop_thread.load(Ordering::Relaxed) {
                let sample = Sample {
                    host: host.sample(),
                    vram: vram.query(),
                    load: load.query(),
                };
                // Recovers from a poisoned lock rather than panicking: the mutex only ever
                // guards a plain assignment (no invariant that could be left broken by a
                // panic mid-write), so a stale poison flag from some earlier panic shouldn't
                // permanently crash every future sample on this thread or every future
                // `sample()` call on the UI thread.
                *latest_thread
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(sample);
                if should_report(only_on_change, reported.as_ref(), &sample) {
                    reported = Some(sample);
                    on_sample();
                }

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
    /// Recovers from a poisoned lock rather than panicking, for the same reason `spawn`'s
    /// own write side does.
    pub fn sample(&self) -> Option<Sample> {
        *self
            .latest
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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
    fn lane_budget_falls_back_when_telemetry_is_unavailable() {
        assert_eq!(
            gpu_lane_budget(None),
            (GPU_LANE_FALLBACK_BUDGET_BYTES, BudgetSource::Fallback)
        );
        let zero = VramUsage {
            used_bytes: 0,
            budget_bytes: 0,
        };
        assert_eq!(gpu_lane_budget(Some(zero)).1, BudgetSource::Fallback);
    }

    #[test]
    fn lane_budget_is_a_share_of_the_measured_budget() {
        let gib = 1024 * 1024 * 1024;
        let usage = VramUsage {
            used_bytes: gib,
            budget_bytes: 8 * gib,
        };
        assert_eq!(
            gpu_lane_budget(Some(usage)),
            (4 * gib, BudgetSource::Measured)
        );
    }

    #[test]
    fn lane_budget_never_drops_below_the_floor() {
        let usage = VramUsage {
            used_bytes: 0,
            budget_bytes: 64 * 1024 * 1024,
        };
        assert_eq!(
            gpu_lane_budget(Some(usage)),
            (GPU_LANE_MIN_BUDGET_BYTES, BudgetSource::Measured)
        );
    }

    #[test]
    fn query_for_defaults_to_query() {
        assert_eq!(UnavailableVramSource.query_for(0x10de, 0x2704), None);
    }

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

    fn sample_of(cpu: f32, used_gb: f64, gpu: Option<f32>) -> Sample {
        Sample {
            host: HostTelemetry {
                cpu_usage_percent: cpu,
                used_memory_bytes: (used_gb * 1e9) as u64,
                total_memory_bytes: 32_000_000_000,
            },
            vram: None,
            load: LoadReading {
                gpu_busy_percent: gpu,
                disk_busy_percent: None,
            },
        }
    }

    #[test]
    fn a_change_below_the_displayed_precision_is_not_visible_while_calm() {
        let base = sample_of(12.2, 8.00, Some(3.1));
        assert!(!base.differs_visibly(&base));
        // Same "CPU 12%", same "RAM 8.0", same "GPU 3%": nothing to redraw.
        assert!(!sample_of(12.4, 8.02, Some(3.3)).differs_visibly(&base));
        // Each readout flipping its rounded text is.
        assert!(sample_of(12.6, 8.00, Some(3.1)).differs_visibly(&base));
        assert!(sample_of(12.2, 8.20, Some(3.1)).differs_visibly(&base));
        assert!(sample_of(12.2, 8.00, Some(3.6)).differs_visibly(&base));
        assert!(sample_of(12.2, 8.00, None).differs_visibly(&base));
    }

    #[test]
    fn a_busy_reading_repaints_on_any_movement_because_the_verdict_uses_raw_values() {
        // "CPU 60%" both times, but 59.6 is calm and 60.2 is busy: the verdict changes.
        assert!(sample_of(60.2, 8.0, None).differs_visibly(&sample_of(59.6, 8.0, None)));
        // Busy and drifting inside one rounded value: still repaints (hysteresis compares raw).
        assert!(sample_of(70.3, 8.0, None).differs_visibly(&sample_of(70.1, 8.0, None)));
        // A busy GPU counts the same, whatever the CPU does.
        assert!(sample_of(5.0, 8.0, Some(90.3)).differs_visibly(&sample_of(5.0, 8.0, Some(90.1))));
    }

    #[test]
    fn the_change_gate_reports_the_first_sample_then_only_visible_changes() {
        let a = sample_of(12.2, 8.0, None);
        let same_looking = sample_of(12.3, 8.0, None);
        let different = sample_of(40.0, 8.0, None);
        // Not gated: every sample reports, as `spawn` always did.
        assert!(should_report(false, Some(&a), &same_looking));
        // Gated: first sample yes, an identical-looking one no, a changed one yes.
        assert!(should_report(true, None, &a));
        assert!(!should_report(true, Some(&a), &same_looking));
        assert!(should_report(true, Some(&a), &different));
    }

    #[test]
    fn spawn_on_change_reports_at_least_the_first_sample() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_cb = Arc::clone(&calls);
        let sampler = TelemetrySampler::spawn_on_change(
            Box::new(UnavailableVramSource),
            Box::new(UnavailableLoadSource),
            Duration::from_millis(20),
            move || {
                calls_cb.fetch_add(1, Ordering::Relaxed);
            },
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while calls.load(Ordering::Relaxed) < 1 && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        drop(sampler);
        assert!(calls.load(Ordering::Relaxed) >= 1);
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
        // Poll with a generous deadline rather than one fixed sleep-then-snapshot window: a fixed
        // 200ms window flaked on Windows CI (same root cause as the neighboring
        // sampler_calls_on_sample_after_each_real_sample test's comment -- `cargo test` runs every
        // test's own background sampler thread concurrently, and a loaded/virtualized runner can
        // delay a fresh thread's first scheduling past 200ms).
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut saw_sample = sampler.sample().is_some();
        while !saw_sample && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
            saw_sample = sampler.sample().is_some();
        }
        assert!(saw_sample, "expected a sample within 5s of spawning");
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
        // Poll with a generous deadline rather than one fixed sleep-then-snapshot window: a
        // fixed 150ms window flaked to 0 samples on Windows CI (`cargo test` runs every test's
        // own background sampler thread concurrently, and a loaded/virtualized runner can delay
        // a fresh thread's first scheduling past 150ms). Waiting for *at least* 2 samples, however
        // long that actually takes up to a generous cap, checks the same "fires repeatedly"
        // behavior without being sensitive to how fast the CI host schedules threads today.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while calls.load(Ordering::Relaxed) < 2 && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let final_count = calls.load(Ordering::Relaxed);
        drop(sampler);
        assert!(
            final_count >= 2,
            "expected at least 2 samples within 5s at a 20ms interval, got {}",
            final_count
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
