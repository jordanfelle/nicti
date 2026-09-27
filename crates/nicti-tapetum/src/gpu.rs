//! The shared `wgpu::Device`/`Queue` Tapetum's render stages dispatch against. ADR-0016 requires
//! exactly one shared device for both compute and display, Vulkan by default (Dx12 lacks
//! `SHADER_F16`), and requesting `adapter.limits()` rather than wgpu's conservative 256MB default
//! (a full-res 45MP frame is well over that). Adapted from `spikes/glint/src/gpu.rs`'s
//! `GpuContext` (kept as a copy there, not a dependency -- spikes don't depend on production
//! crates or each other).

/// Which backend to prefer when more than one adapter is available. `Auto` tries Vulkan first,
/// then Dx12, then whatever else wgpu finds -- ADR-0016's own order. `NICTI_WGPU_BACKEND`
/// (`vulkan`/`dx12`/`gl`) overrides this for a manual comparison run; an unrecognized value is
/// ignored (falls back to `Auto`) rather than treated as an error, since this is a debugging knob,
/// not a documented public contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GpuPreference {
    #[default]
    Auto,
    Backend(wgpu::Backend),
}

impl GpuPreference {
    /// Reads `NICTI_WGPU_BACKEND` if set, otherwise `Auto`.
    pub fn from_env() -> Self {
        match std::env::var("NICTI_WGPU_BACKEND").as_deref() {
            Ok("vulkan") => GpuPreference::Backend(wgpu::Backend::Vulkan),
            Ok("dx12") => GpuPreference::Backend(wgpu::Backend::Dx12),
            Ok("gl") => GpuPreference::Backend(wgpu::Backend::Gl),
            _ => GpuPreference::Auto,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GpuError {
    #[error("no wgpu adapter is available (backend preference: {0:?})")]
    NoAdapter(GpuPreference),
    #[error("failed to request a device from the adapter: {0}")]
    RequestDevice(#[from] wgpu::RequestDeviceError),
}

/// The device Tapetum's render stages share -- one per process, per ADR-0016 (compute and
/// display both go through this same device/queue, never a second one).
pub struct GpuContext {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub adapter_name: String,
    pub backend: wgpu::Backend,
    pub limits: wgpu::Limits,
    pub features: wgpu::Features,
    pub timestamp_period_ns: f32,
    /// True for a CPU-backed adapter (e.g. lavapipe/WARP) -- correct for tests and CI, but never
    /// meaningful for a real timing measurement (ADR-0016's own hardware-identity caveat).
    pub is_software: bool,
}

impl GpuContext {
    /// Picks the best adapter per `pref` (or `NICTI_WGPU_BACKEND` if `pref` is `Auto` and the env
    /// var is set) and requests a device against it with the adapter's own limits and whatever of
    /// `TIMESTAMP_QUERY`/`SHADER_F16` it supports.
    pub fn new(pref: GpuPreference) -> Result<Self, GpuError> {
        let pref = match pref {
            GpuPreference::Auto => GpuPreference::from_env(),
            explicit => explicit,
        };
        // `PRIMARY` (Vulkan/Metal/Dx12/BrowserWebGpu) excludes Gl, which wgpu classifies as
        // `SECONDARY` -- enumerating only `PRIMARY` would mean an explicit `NICTI_WGPU_BACKEND=gl`
        // request could never find an adapter even on a host with a working GL driver. An
        // explicit backend request enumerates only that backend; `Auto` stays `PRIMARY`-only (it
        // never falls back to Gl on its own).
        let backends = match pref {
            GpuPreference::Backend(b) => wgpu::Backends::from(b),
            GpuPreference::Auto => wgpu::Backends::PRIMARY,
        };
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapters = pollster::block_on(instance.enumerate_adapters(backends));

        let adapter = match pref {
            // An explicit backend request must be honored exactly or fail -- silently falling
            // back to some other backend would mean a caller (or `NICTI_WGPU_BACKEND`) asking
            // for Dx12 on a Vulkan-only box gets a Vulkan context back with no indication its
            // request was ignored.
            GpuPreference::Backend(b) => adapters
                .iter()
                .find(|a| a.get_info().backend == b)
                .cloned()
                .ok_or(GpuError::NoAdapter(pref))?,
            GpuPreference::Auto => [wgpu::Backend::Vulkan, wgpu::Backend::Dx12]
                .into_iter()
                .find_map(|backend| adapters.iter().find(|a| a.get_info().backend == backend))
                .or_else(|| adapters.first())
                .cloned()
                .ok_or(GpuError::NoAdapter(pref))?,
        };

        Self::from_adapter(adapter)
    }

    fn from_adapter(adapter: wgpu::Adapter) -> Result<Self, GpuError> {
        let descriptor = device_descriptor_for(&adapter);
        let (device, queue) = pollster::block_on(adapter.request_device(&descriptor))?;
        Ok(Self::from_parts(&adapter, device, queue))
    }

    /// Builds a `GpuContext` around a `wgpu::Device`/`Queue` this crate didn't create itself --
    /// the case a host application (`nicti-pelt`'s eframe window) that owns its own device needs,
    /// so Tapetum's compute dispatches and the host's display draws share exactly one device
    /// (ADR-0016's "one shared device for compute and display"), rather than Tapetum silently
    /// creating a second one nothing else ever uses. The caller is responsible for having
    /// requested `device` with (at least) [`device_descriptor_for`]'s own features/limits --
    /// `eframe`'s `WgpuConfiguration::wgpu_setup` accepts a `device_descriptor` closure for
    /// exactly this, see `nicti-pelt`'s own setup.
    pub fn from_device(adapter: &wgpu::Adapter, device: wgpu::Device, queue: wgpu::Queue) -> Self {
        Self::from_parts(adapter, device, queue)
    }

    fn from_parts(adapter: &wgpu::Adapter, device: wgpu::Device, queue: wgpu::Queue) -> Self {
        let info = adapter.get_info();
        let features = device.features();
        let limits = device.limits();
        let timestamp_period_ns = queue.get_timestamp_period();
        Self {
            device,
            queue,
            adapter_name: info.name,
            backend: info.backend,
            limits,
            features,
            timestamp_period_ns,
            is_software: info.device_type == wgpu::DeviceType::Cpu,
        }
    }

    pub fn supports_timestamps(&self) -> bool {
        self.features.contains(wgpu::Features::TIMESTAMP_QUERY)
    }

    pub fn supports_f16(&self) -> bool {
        self.features.contains(wgpu::Features::SHADER_F16)
    }
}

/// The `wgpu::DeviceDescriptor` `GpuContext::new` requests from `adapter`: the adapter's own
/// limits (not wgpu's conservative default, which caps well under a full-res 45MP frame) plus
/// `TIMESTAMP_QUERY`/`SHADER_F16` when the adapter supports them. Exposed so a host that builds
/// its own device against the same adapter (`nicti-pelt`'s eframe window, via
/// `WgpuConfiguration::wgpu_setup`'s `device_descriptor` closure) requests the identical
/// descriptor `GpuContext::from_device` then wraps -- without this, eframe's own default
/// descriptor (`Limits::default()`, no optional features) would silently undersize the device
/// Tapetum then has to render full-resolution frames through.
pub fn device_descriptor_for(adapter: &wgpu::Adapter) -> wgpu::DeviceDescriptor<'static> {
    let features = adapter.features();
    let mut required_features = wgpu::Features::empty();
    if features.contains(wgpu::Features::TIMESTAMP_QUERY) {
        required_features |= wgpu::Features::TIMESTAMP_QUERY;
    }
    if features.contains(wgpu::Features::SHADER_F16) {
        required_features |= wgpu::Features::SHADER_F16;
    }
    wgpu::DeviceDescriptor {
        label: Some("nicti-tapetum device"),
        required_features,
        required_limits: adapter.limits(),
        ..Default::default()
    }
}

/// wgpu's per-dimension workgroup-dispatch limit (commonly 65535 on native backends). A 1D
/// dispatch of `ceil(pixel_count / workgroup_size)` overflows this well before a full-res 45MP
/// frame, so every real dispatch must split into a 2D grid instead (ADR-0016). Shared here so
/// every stage's `EncodePass` uses the same, already-hardware-verified splitting logic rather
/// than re-deriving it (`spikes/glint`/`spikes/loaf` each had their own copy).
const MAX_WORKGROUPS_PER_DIM: u32 = 65535;

/// Splits `total_workgroups` into an (x, y) dispatch grid that respects
/// `MAX_WORKGROUPS_PER_DIM` in both dimensions.
pub fn workgroup_grid(total_workgroups: u32) -> (u32, u32) {
    if total_workgroups <= MAX_WORKGROUPS_PER_DIM {
        (total_workgroups, 1)
    } else {
        let x = MAX_WORKGROUPS_PER_DIM;
        let y = total_workgroups.div_ceil(x);
        assert!(
            y <= MAX_WORKGROUPS_PER_DIM,
            "workload too large for a single 2D dispatch grid"
        );
        (x, y)
    }
}

/// Builds a compute pipeline from inline WGSL, with an auto-derived bind group layout (`layout:
/// None`). Callers build this once (in their own `*Kernel::new`) and reuse it across many
/// `encode`/dispatch calls -- rebuilding a pipeline (which recompiles the shader) inside a hot
/// path measured ~1000x too slow in this repo's own spike research (`spikes/loaf`/`spikes/glint`).
pub fn make_compute_pipeline(
    device: &wgpu::Device,
    wgsl: &str,
    entry_point: &str,
) -> wgpu::ComputePipeline {
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(entry_point),
        source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(wgsl)),
    });
    device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some(entry_point),
        layout: None,
        module: &module,
        entry_point: Some(entry_point),
        compilation_options: wgpu::PipelineCompilationOptions::default(),
        cache: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_workload_stays_1d() {
        assert_eq!(workgroup_grid(100), (100, 1));
        assert_eq!(
            workgroup_grid(MAX_WORKGROUPS_PER_DIM),
            (MAX_WORKGROUPS_PER_DIM, 1)
        );
    }

    #[test]
    fn oversized_workload_splits_into_2d() {
        let (x, y) = workgroup_grid(MAX_WORKGROUPS_PER_DIM + 1);
        assert_eq!(x, MAX_WORKGROUPS_PER_DIM);
        assert_eq!(y, 2);
        assert!(x as u64 * y as u64 >= (MAX_WORKGROUPS_PER_DIM as u64 + 1));
    }

    #[test]
    fn hero_scenario_workgroup_count_fits_in_2d_grid() {
        let total = 45_000_000u32.div_ceil(64);
        let (x, y) = workgroup_grid(total);
        assert!(x <= MAX_WORKGROUPS_PER_DIM);
        assert!(y <= MAX_WORKGROUPS_PER_DIM);
        assert!(x as u64 * y as u64 >= total as u64);
    }

    /// Real GPU test: skips (doesn't fail) with no adapter available, matching this repo's
    /// existing convention for GPU-touching tests in a sandbox without a GPU-backed Vulkan ICD.
    #[test]
    fn gpu_context_requests_device_successfully() {
        match GpuContext::new(GpuPreference::Auto) {
            Ok(ctx) => {
                assert!(!ctx.adapter_name.is_empty());
            }
            Err(GpuError::NoAdapter(_)) => {
                eprintln!("no wgpu adapter available in this environment, skipping");
            }
            Err(e) => panic!("unexpected GPU error: {e}"),
        }
    }

    /// Regression test for `nicti-pelt`'s device-sharing path: a device requested via
    /// `device_descriptor_for` and wrapped with `from_device` must report the same
    /// features/limits `from_adapter` itself would have gotten from the *same* adapter -- if
    /// these diverged, a host (eframe) building its own device this way could silently hand
    /// Tapetum an undersized device (missing `SHADER_F16`, or wgpu's conservative default limits
    /// instead of the adapter's own), passing every test here but failing only on a real
    /// full-res frame.
    ///
    /// Deliberately compares against `from_adapter` on the *same* enumerated adapter instance,
    /// not `GpuContext::new(GpuPreference::Auto)` -- on a multi-adapter machine, `new`'s own
    /// backend-preference selection isn't guaranteed to land on the same physical adapter this
    /// test enumerates first, which would make a mismatch mean nothing (two different adapters
    /// legitimately have different limits) rather than proving `from_device` itself is correct
    /// (caught in CodeRabbit's review of this PR).
    #[test]
    fn from_device_reports_the_same_features_and_limits_as_from_adapter() {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::PRIMARY));
        let Some(adapter) = adapters.into_iter().next() else {
            eprintln!("no wgpu adapter available in this environment, skipping");
            return;
        };

        let descriptor = device_descriptor_for(&adapter);
        let (device, queue) = pollster::block_on(adapter.request_device(&descriptor))
            .expect("requesting a device with device_descriptor_for's own descriptor must succeed");
        let via_from_device = GpuContext::from_device(&adapter, device, queue);

        let via_from_adapter =
            GpuContext::from_adapter(adapter).expect("requesting a second device must succeed");

        assert_eq!(via_from_device.features, via_from_adapter.features);
        assert_eq!(
            via_from_device.limits.max_storage_buffer_binding_size,
            via_from_adapter.limits.max_storage_buffer_binding_size
        );
        assert_eq!(
            via_from_device.limits.max_texture_dimension_2d,
            via_from_adapter.limits.max_texture_dimension_2d
        );
    }

    /// An explicit `GpuPreference::Backend` for a backend with no enumerated adapter must error,
    /// not silently pick some other backend instead (the regression this test guards: the
    /// `Auto` case's `.or_else(|| adapters.first())` fallback used to apply to the explicit-
    /// backend case too).
    #[test]
    fn explicit_backend_preference_errors_if_that_backend_has_no_adapter() {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::PRIMARY));
        let present: std::collections::HashSet<wgpu::Backend> =
            adapters.iter().map(|a| a.get_info().backend).collect();
        let Some(absent) = [
            wgpu::Backend::Vulkan,
            wgpu::Backend::Dx12,
            wgpu::Backend::Metal,
            wgpu::Backend::Gl,
        ]
        .into_iter()
        .find(|b| !present.contains(b)) else {
            eprintln!("every backend has an adapter in this environment, skipping");
            return;
        };

        match GpuContext::new(GpuPreference::Backend(absent)) {
            Err(GpuError::NoAdapter(GpuPreference::Backend(b))) => assert_eq!(b, absent),
            Err(e) => panic!("expected NoAdapter, got {e}"),
            Ok(_) => panic!("expected an error requesting a backend with no adapter"),
        }
    }

    /// Regression test: `wgpu::Backends::PRIMARY` excludes Gl (`SECONDARY`), so enumerating only
    /// `PRIMARY` -- as this code used to do unconditionally -- would mean an explicit
    /// `GpuPreference::Backend(Gl)` could never find an adapter even on a host with a working GL
    /// driver. `new()` now enumerates `Backends::from(b)` for an explicit request instead.
    #[test]
    fn gl_backend_is_excluded_from_primary_but_included_when_explicitly_requested() {
        assert!(!wgpu::Backends::PRIMARY.contains(wgpu::Backends::GL));
        assert!(wgpu::Backends::from(wgpu::Backend::Gl).contains(wgpu::Backends::GL));
    }
}
