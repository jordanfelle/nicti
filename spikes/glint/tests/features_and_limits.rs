//! Probes the ADR-0005 decision rule's clause (c): required features/limits work on every
//! backend this machine exposes. `SHADER_F16`, `TIMESTAMP_QUERY`, and a >=363 MB buffer
//! allocation (the 45 MP RGBA16F intermediate size from Tapetum's design, ADR-0004 §context) --
//! see `gpu.rs`'s scoping note for why this is buffer-based rather than storage-texture-based.

use std::borrow::Cow;

use glint::gpu::{GpuContext, F16_PROBE_WGSL};

const HERO_INTERMEDIATE_BYTES: u64 = 45_000_000 * 8; // 45 MP * RGBA16F (8 bytes/pixel).

#[test]
fn timestamp_query_availability_reported_per_backend() {
    let contexts = GpuContext::enumerate();
    if contexts.is_empty() {
        eprintln!("features_and_limits: no wgpu adapter available, skipping");
        return;
    }
    for ctx in &contexts {
        println!(
            "backend={:?} adapter={} timestamp_query={} shader_f16={}",
            ctx.backend,
            ctx.adapter_name,
            ctx.supports_timestamps(),
            ctx.supports_f16()
        );
    }
}

#[test]
fn hero_intermediate_allocation_within_reported_limits() {
    let contexts = GpuContext::enumerate();
    if contexts.is_empty() {
        eprintln!("features_and_limits: no wgpu adapter available, skipping");
        return;
    }
    for ctx in &contexts {
        let limits = ctx.device.limits();
        println!(
            "backend={:?} max_buffer_size={} max_storage_buffer_binding_size={} hero_bytes={}",
            ctx.backend,
            limits.max_buffer_size,
            limits.max_storage_buffer_binding_size,
            HERO_INTERMEDIATE_BYTES
        );
        // The default WebGPU-portable limit (256 MB) is smaller than a 45MP RGBA16F frame;
        // requesting `adapter.limits()` (this crate's device-request policy, see gpu.rs) must
        // actually raise the total-allocation ceiling on native backends, or the render graph
        // can't hold one full-res intermediate at all. This is exactly the check ADR-0005's
        // decision rule cares about, so it's a hard assertion.
        assert!(
            limits.max_buffer_size >= HERO_INTERMEDIATE_BYTES,
            "backend {:?}: max_buffer_size {} is below the 45MP RGBA16F hero-scenario intermediate size {}",
            ctx.backend,
            limits.max_buffer_size,
            HERO_INTERMEDIATE_BYTES
        );
        // `max_storage_buffer_binding_size` is a separate, often much smaller cap (llvmpipe here
        // reports 128 MiB against a 360 MB hero frame) -- real NVIDIA hardware typically allows a
        // far larger single binding, but this is reported, not asserted, since a binding-size
        // shortfall is addressable by design (row-chunked bindings, or storage textures instead
        // of one monolithic buffer) rather than a hard blocker. See ADR-0005's hardware-identity
        // caveat: this number must be re-checked on the actual reference machine, not assumed
        // from software-Vulkan numbers.
        if limits.max_storage_buffer_binding_size < HERO_INTERMEDIATE_BYTES {
            eprintln!(
                "backend {:?}: max_storage_buffer_binding_size {} is below the hero-frame size {} -- a single monolithic storage buffer binding won't hold one full-res intermediate on this adapter",
                ctx.backend, limits.max_storage_buffer_binding_size, HERO_INTERMEDIATE_BYTES
            );
        }
    }
}

#[test]
fn shader_f16_probe_where_supported() {
    let contexts = GpuContext::enumerate();
    if contexts.is_empty() {
        eprintln!("features_and_limits: no wgpu adapter available, skipping");
        return;
    }
    for ctx in &contexts {
        if !ctx.supports_f16() {
            eprintln!(
                "backend {:?}: SHADER_F16 not supported, skipping probe",
                ctx.backend
            );
            continue;
        }
        let module = ctx
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("f16_probe"),
                source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(F16_PROBE_WGSL)),
            });
        // Creation succeeding (module compiles/validates against this backend) is the assertion;
        // wgpu's shader-module creation panics/logs a validation error on failure, which fails
        // the test via wgpu's default panic-on-validation-error behavior in debug builds.
        drop(module);
    }
}
