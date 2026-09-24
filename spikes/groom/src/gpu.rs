//! wgpu device/adapter setup and dispatch for the Poisson-Jacobi compute shader, following
//! `spikes/glint/src/gpu.rs`'s pattern (adapter enumeration, `SHADER_F16`/`TIMESTAMP_QUERY`
//! feature probing, the 2D dispatch-grid workaround for wgpu's 65535-per-dimension workgroup
//! limit). Simplified relative to glint in one deliberate way: no GPU-timestamp harness, since
//! this ticket's perf work is CPU-only in this sandbox (see `tests/throughput.rs`) and real GPU
//! numbers are explicitly deferred to the reference-machine follow-up (ADR-0007's Measured
//! results section).

use bytemuck::{Pod, Zeroable};

pub const POISSON_JACOBI_WGSL: &str = include_str!("../shaders/poisson_jacobi.wgsl");

pub const WORKGROUP_SIZE: u32 = 64;

/// wgpu's per-dimension dispatch limit -- see `spikes/glint`'s identical constant/comment and
/// ADR-0005's dispatch-dimensioning finding. Mirrored here rather than imported since spikes
/// don't depend on each other (see `CLAUDE.md`'s "don't build on top of a spike crate" rule).
const MAX_WORKGROUPS_PER_DIM: u32 = 65535;

fn workgroup_grid(total_workgroups: u32) -> (u32, u32) {
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

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct Params {
    width: u32,
    height: u32,
    _pad0: u32,
    _pad1: u32,
}

/// A single wgpu adapter/device/queue, following glint's `GpuContext` shape.
pub struct GpuContext {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub backend: wgpu::Backend,
    pub adapter_name: String,
}

impl GpuContext {
    /// Enumerates every adapter wgpu can see. Returns an empty `Vec` (never panics) when no
    /// adapter is available at all -- CI runners without a GPU or software Vulkan ICD hit this
    /// path, and callers (see `tests/correctness.rs`) must skip cleanly rather than fail.
    pub fn enumerate() -> Vec<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        pollster::block_on(instance.enumerate_adapters(wgpu::Backends::PRIMARY))
            .into_iter()
            .filter_map(Self::from_adapter)
            .collect()
    }

    fn from_adapter(adapter: wgpu::Adapter) -> Option<Self> {
        let info = adapter.get_info();
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("groom device"),
            required_features: wgpu::Features::empty(),
            required_limits: adapter.limits(),
            ..Default::default()
        }))
        .ok()?;
        Some(Self {
            device,
            queue,
            backend: info.backend,
            adapter_name: info.name,
        })
    }
}

fn storage_buffer(
    device: &wgpu::Device,
    label: &str,
    contents: &[u8],
    usage: wgpu::BufferUsages,
) -> wgpu::Buffer {
    use wgpu::util::DeviceExt;
    device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some(label),
        contents,
        usage,
    })
}

/// Runs `iterations` Jacobi sweeps of the Poisson solve on the GPU, ping-ponging between two
/// storage buffers (one dispatch per iteration, alternating which buffer is `in`/`out`) -- the
/// GPU-side twin of `cpu_reference::poisson_jacobi_cpu`. `guidance`/`initial`/`mask` must all be
/// `width * height` long; `mask` is `1u` for interior/unknown pixels, `0u` for fixed boundary
/// pixels, matching `cpu_reference`'s `bool` mask one-to-one.
pub fn run_poisson_jacobi(
    ctx: &GpuContext,
    guidance: &[[f32; 4]],
    initial: &[[f32; 4]],
    mask: &[u32],
    width: u32,
    height: u32,
    iterations: u32,
) -> Vec<[f32; 4]> {
    let device = &ctx.device;
    let total = (width * height) as usize;
    assert_eq!(guidance.len(), total);
    assert_eq!(initial.len(), total);
    assert_eq!(mask.len(), total);

    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("poisson_jacobi"),
        source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(POISSON_JACOBI_WGSL)),
    });
    let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("poisson_jacobi"),
        layout: None,
        module: &module,
        entry_point: Some("poisson_jacobi"),
        compilation_options: wgpu::PipelineCompilationOptions::default(),
        cache: None,
    });

    let buf_size = (total * std::mem::size_of::<[f32; 4]>()) as u64;
    let buf_usage =
        wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC;
    let buf_a = storage_buffer(
        device,
        "poisson buf a",
        bytemuck::cast_slice(initial),
        buf_usage,
    );
    let buf_b = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("poisson buf b"),
        size: buf_size,
        usage: buf_usage,
        mapped_at_creation: false,
    });
    let guidance_buf = storage_buffer(
        device,
        "poisson guidance",
        bytemuck::cast_slice(guidance),
        wgpu::BufferUsages::STORAGE,
    );
    let mask_buf = storage_buffer(
        device,
        "poisson mask",
        bytemuck::cast_slice(mask),
        wgpu::BufferUsages::STORAGE,
    );
    let params_buf = storage_buffer(
        device,
        "poisson params",
        bytemuck::bytes_of(&Params {
            width,
            height,
            _pad0: 0,
            _pad1: 0,
        }),
        wgpu::BufferUsages::UNIFORM,
    );

    let layout = pipeline.get_bind_group_layout(0);
    let make_bind_group = |input: &wgpu::Buffer, output: &wgpu::Buffer, label: &str| {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(label),
            layout: &layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: input.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: output.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: guidance_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: mask_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: params_buf.as_entire_binding(),
                },
            ],
        })
    };
    let bind_group_a_to_b = make_bind_group(&buf_a, &buf_b, "a->b");
    let bind_group_b_to_a = make_bind_group(&buf_b, &buf_a, "b->a");

    let (wg_x, wg_y) = workgroup_grid((total as u32).div_ceil(WORKGROUP_SIZE));

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("groom poisson encoder"),
    });
    for it in 0..iterations {
        let bind_group = if it % 2 == 0 {
            &bind_group_a_to_b
        } else {
            &bind_group_b_to_a
        };
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("poisson pass"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, bind_group, &[]);
        pass.dispatch_workgroups(wg_x, wg_y, 1);
    }

    // After an odd number of iterations the final result lives in buf_b (a->b ran last);
    // after an even number (including zero) it's back in buf_a.
    let final_buf = if iterations % 2 == 1 { &buf_b } else { &buf_a };
    let staging_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("poisson readback"),
        size: buf_size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_buffer_to_buffer(final_buf, 0, &staging_buf, 0, buf_size);
    ctx.queue.submit(Some(encoder.finish()));

    let slice = staging_buf.slice(..);
    slice.map_async(wgpu::MapMode::Read, |_| {});
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("device poll failed");
    let data = slice
        .get_mapped_range()
        .expect("readback buffer not mapped")
        .to_vec();
    let out: &[[f32; 4]] = bytemuck::cast_slice(&data);
    out.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_workload_stays_1d() {
        assert_eq!(workgroup_grid(100), (100, 1));
    }

    #[test]
    fn oversized_workload_splits_into_2d() {
        let (x, y) = workgroup_grid(MAX_WORKGROUPS_PER_DIM + 1);
        assert_eq!(x, MAX_WORKGROUPS_PER_DIM);
        assert_eq!(y, 2);
    }
}
