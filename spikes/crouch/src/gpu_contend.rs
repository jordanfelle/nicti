//! Measures ADR-0054's central open question: GPU compute dispatches can't be preempted mid-flight
//! (neither Vulkan nor CUDA expose that), so foreground preemption latency is bounded by
//! background *chunk* size plus however the driver schedules between contexts sharing one
//! physical GPU. This module measures wgpu-vs-wgpu contention (one shared `wgpu::Device`, per
//! ADR-0016/0019/0050 -- background and foreground compete for the same hardware queue);
//! `ort_contend.rs` measures the cross-API case (background CUDA via `ort` vs. foreground
//! Vulkan via `wgpu`).
//!
//! Adapted (copied, not depended-on) from `spikes/loaf/src/gpu.rs`'s `GpuContext` enumeration and
//! persistent-kernel pattern -- see that file's own doc comment for why a persistent `*Kernel`
//! (build the pipeline/buffers once, `write_buffer` + submit on every call) is required for a
//! timing loop at all: ADR-0044 measured a ~1000x regression from rebuilding the pipeline inside
//! one.

use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytemuck::{Pod, Zeroable};

const BUSY_WGSL: &str = include_str!("../shaders/busy.wgsl");
const WORKGROUP_SIZE: u32 = 64;

pub struct GpuContext {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub backend: wgpu::Backend,
    pub adapter_name: String,
}

impl GpuContext {
    /// Enumerates every adapter wgpu can see. In this sandbox (WSL, no NVIDIA Vulkan ICD) this is
    /// lavapipe/llvmpipe software rendering only -- real contention numbers require the
    /// cross-compile-to-Windows-and-run-via-WSL-interop path ADR-0044/0016 already document.
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
            label: Some("crouch device"),
            required_limits: adapter.limits(),
            ..Default::default()
        }))
        .ok()?;
        Some(GpuContext {
            device,
            queue,
            backend: info.backend,
            adapter_name: info.name,
        })
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct BusyParams {
    iterations: u32,
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
}

fn workgroup_grid(total_workgroups: u32) -> (u32, u32) {
    const MAX_WORKGROUPS_PER_DIM: u32 = 65535;
    if total_workgroups <= MAX_WORKGROUPS_PER_DIM {
        (total_workgroups.max(1), 1)
    } else {
        let x = MAX_WORKGROUPS_PER_DIM;
        (x, total_workgroups.div_ceil(x))
    }
}

/// A persistent `busy.wgsl` pipeline + buffers, re-dispatched many times with a fresh iteration
/// count per call -- the same "never rebuild inside a timing loop" pattern `spikes/loaf`'s
/// `LiveSuffixKernel` established.
pub struct BusyKernel {
    pipeline: wgpu::ComputePipeline,
    params_buf: wgpu::Buffer,
    // Never read directly after construction -- kept alive here because `bind_group` only holds
    // a binding into it, not ownership; dropping this field would free the buffer out from under
    // the bind group.
    #[allow(dead_code)]
    data_buf: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    elements: u32,
}

impl BusyKernel {
    pub fn new(ctx: &GpuContext, elements: u32) -> Self {
        let device = &ctx.device;
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("busy"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(BUSY_WGSL)),
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("busy"),
            layout: None,
            module: &module,
            entry_point: Some("main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            cache: None,
        });

        let params_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("busy params"),
            size: std::mem::size_of::<BusyParams>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let data_size = (elements as u64) * 4;
        let data_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("busy data"),
            size: data_size,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group_layout = pipeline.get_bind_group_layout(0);
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("busy bind group"),
            layout: &bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: params_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: data_buf.as_entire_binding(),
                },
            ],
        });

        BusyKernel {
            pipeline,
            params_buf,
            data_buf,
            bind_group,
            elements,
        }
    }

    /// Submits one dispatch, fire-and-forget (no wait) -- used by the background thread, which
    /// wants to keep the queue continuously busy with `iterations`-sized chunks rather than
    /// synchronize after each one. Returns the submission index so a caller that *does* want to
    /// wait can target this exact submission (see [`BusyKernel::dispatch_and_wait`]'s own doc
    /// comment on why that matters).
    pub fn submit(&self, ctx: &GpuContext, iterations: u32) -> wgpu::SubmissionIndex {
        ctx.queue.write_buffer(
            &self.params_buf,
            0,
            bytemuck::bytes_of(&BusyParams {
                iterations,
                _pad0: 0,
                _pad1: 0,
                _pad2: 0,
            }),
        );
        let mut encoder = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("busy encoder"),
            });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("busy pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            let (wg_x, wg_y) = workgroup_grid(self.elements.div_ceil(WORKGROUP_SIZE));
            pass.dispatch_workgroups(wg_x, wg_y, 1);
        }
        ctx.queue.submit(Some(encoder.finish()))
    }

    /// Submits one dispatch and blocks (host wall-clock, not GPU-timestamp) until *this specific
    /// submission* -- and anything already queued ahead of it -- has completed. This is the
    /// number that matters for decision rule #1: a real UI thread waiting on this frame's render
    /// experiences exactly this latency, whatever else is queued ahead of it.
    ///
    /// **Must target this call's own submission index, not "whatever's most recent."**
    /// `wgpu::PollType::Wait { submission_index: None, .. }` (what `wait_indefinitely()` builds)
    /// waits for the most recent submission *at the time `poll` is called*, per its own doc
    /// comment in `wgpu-types` -- under concurrent submission from another thread (a background
    /// contention load), that submission can be a *later* one than this call's own, silently
    /// folding extra background work into what's supposed to be a foreground-only measurement.
    /// Caught in review: the first version of this method called `wait_indefinitely()` and
    /// measured contention latency that included however much extra background work snuck in
    /// during the race window between this call's own `submit` and its `poll`.
    pub fn dispatch_and_wait(&self, ctx: &GpuContext, iterations: u32) -> Duration {
        let start = Instant::now();
        let index = self.submit(ctx, iterations);
        ctx.device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(index),
                timeout: None,
            })
            .expect("device poll failed");
        start.elapsed()
    }
}

/// Keeps a background `BusyKernel` continuously submitting `iterations`-sized chunks until
/// dropped/stopped -- simulates Pounce's one serial bake worker under a steady background load,
/// without itself being the scheduler (that's `queue.rs`; this is purely a load generator for the
/// contention measurement).
///
/// **Two modes, deliberately not merged into one**:
/// - [`BackgroundLoad::start`] (throttled -- the realistic one) waits for each chunk to actually
///   complete before submitting the next, exactly matching `job.rs::ChunkedJob`'s real contract
///   (the scheduler's worker never has more than one chunk in flight -- there's no reason a real
///   Pounce implementation ever would). This is the measurement that matters for ADR-0054's
///   decision rules.
/// - [`BackgroundLoad::start_unthrottled`] fire-and-forget submits as fast as the CPU can queue
///   them, with no backpressure at all -- a real, load-bearing finding from running this on the
///   reference RTX 5080: unthrottled submission built an unbounded driver queue backlog and, at
///   large enough chunk sizes, **crashed the GPU device entirely** (`wgpu` reported "Parent
///   device is lost" -- a genuine Windows TDR reset, not a bug in this harness). This isn't a
///   measurement to trust as a latency number; it's evidence for *why* the scheduler's
///   single-chunk-in-flight discipline is a correctness requirement, not just tidiness -- an
///   un-throttled background worker can take the whole GPU context down, foreground included.
pub struct BackgroundLoad {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<u64>>,
}

impl BackgroundLoad {
    /// Throttled: waits for each chunk's own completion before submitting the next, so at most
    /// one background chunk is ever in flight -- see this struct's own doc comment for why this,
    /// not [`BackgroundLoad::start_unthrottled`], is the mode every real measurement should use.
    pub fn start(ctx: Arc<GpuContext>, kernel: Arc<BusyKernel>, iterations: u32) -> Self {
        Self::spawn(ctx, kernel, iterations, true)
    }

    /// Fire-and-forget, no backpressure -- a deliberate stress test, not a realistic scheduler
    /// simulation. See this struct's own doc comment: this mode is what surfaced a real GPU
    /// device-loss (TDR) on the reference RTX 5080.
    pub fn start_unthrottled(
        ctx: Arc<GpuContext>,
        kernel: Arc<BusyKernel>,
        iterations: u32,
    ) -> Self {
        Self::spawn(ctx, kernel, iterations, false)
    }

    fn spawn(
        ctx: Arc<GpuContext>,
        kernel: Arc<BusyKernel>,
        iterations: u32,
        throttled: bool,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let handle = std::thread::spawn(move || {
            let mut chunks = 0u64;
            while !thread_stop.load(Ordering::Relaxed) {
                if throttled {
                    kernel.dispatch_and_wait(&ctx, iterations);
                } else {
                    kernel.submit(&ctx, iterations);
                }
                chunks += 1;
            }
            if !throttled {
                // Drain anything still in flight so a stopped load doesn't leave dangling GPU
                // work for the next bench to accidentally contend with.
                ctx.device
                    .poll(wgpu::PollType::wait_indefinitely())
                    .expect("device poll failed");
            }
            chunks
        });
        BackgroundLoad {
            stop,
            handle: Some(handle),
        }
    }

    /// Stops the background thread and returns how many chunks it submitted.
    pub fn stop(mut self) -> u64 {
        self.stop.store(true, Ordering::Relaxed);
        self.handle
            .take()
            .unwrap()
            .join()
            .expect("background load thread panicked")
    }
}

impl Drop for BackgroundLoad {
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

    fn first_ctx() -> Option<GpuContext> {
        GpuContext::enumerate().into_iter().next()
    }

    #[test]
    fn busy_kernel_dispatch_completes() {
        let Some(ctx) = first_ctx() else {
            eprintln!("no wgpu adapter available in this sandbox, skipping");
            return;
        };
        let kernel = BusyKernel::new(&ctx, 1024);
        let elapsed = kernel.dispatch_and_wait(&ctx, 10);
        assert!(elapsed < Duration::from_secs(5), "sanity bound only");
    }

    #[test]
    fn background_load_reports_nonzero_chunks() {
        let Some(ctx) = first_ctx() else {
            eprintln!("no wgpu adapter available in this sandbox, skipping");
            return;
        };
        let ctx = Arc::new(ctx);
        let kernel = Arc::new(BusyKernel::new(&ctx, 1024));
        let load = BackgroundLoad::start(ctx, kernel, 10);
        std::thread::sleep(Duration::from_millis(50));
        let chunks = load.stop();
        assert!(chunks > 0);
    }
}
