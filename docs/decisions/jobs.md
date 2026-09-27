## Jobs & concurrency (Pounce)

Covers the job scheduler itself: priority preemption, cooperative cancellation, `IS_EDITING`
throttling, GPU/VRAM budgeting, and the two real reference-hardware contention measurements that
shaped its design.

- **Scheduler**: `docs/adr/0054-job-scheduler-pounce.md` — **Proposed**, same-API and cross-API
  GPU contention both measured on the real reference RTX 5080; wiring into a real bake pipeline
  and into Scruff's import scan (`crates/nicti-catalog::scruff`'s own "wiring into Pounce is a
  follow-up" note) are #55's build, not this pass's.
- **Model**: `spikes/crouch/src/job.rs::ChunkedJob` (`spec()` + `step() -> Yield | Done`) —
  cooperative cancellation checked only *between* chunks, never mid-chunk, since neither
  `wgpu::Queue::submit` nor `ort::Session::run` support interrupting a call already in flight. A
  chunk is whatever unit a real bake stage submits as one dispatch/inference call (e.g. one
  SCUNet tile), not a whole-image bake. `queue::Scheduler`: a two-class priority queue, foreground
  always dequeues before background; background ordering is a pluggable key function so
  ADR-0044's own `prefetch::priority_order` (bake jobs by `|image_index − cursor|`, reprioritized
  on every cursor move) plugs in directly — copied into `spikes/crouch/src/prefetch.rs`, not
  depended-on, matching every other spike's own convention.
- **Cancellation**: plain `Arc<AtomicBool>` (`cancel::CancelToken` per-job, `cancel::EditingGate`
  process-global for `IS_EDITING`), not `tokio_util::sync::CancellationToken` — the whole design
  is built around exactly one serial GPU/`ort` worker (one shared `wgpu::Device`/one
  `ort::Session`, per ADR-0016/0019/0050's existing decisions), so there's no async runtime
  anywhere else in this design for a `tokio`-flavored token to pay for itself.
- **Throttling**: `throttle::Throttle`, a hand-rolled counting-semaphore-style concurrency limiter
  for user-set CPU-thread/disk-I/O limits on background work — Scruff's import scan is the first
  real client. `governor` (a real, maintained, MIT-licensed crate) was evaluated and rejected:
  its `RateLimiter` is shaped for cells-per-unit-time rate limiting, not "N of these run
  concurrently" (a semaphore-shaped problem) — bending it to fit would cost more than the dozen
  lines the hand-rolled version took.
- **VRAM admission**: `admission::Admission` reserves bytes against a fixed budget per in-flight
  chunk (bracketing a chunk's own runtime, not a job's whole lifetime) — a background job whose
  declared bytes exceed the remaining budget is refused outright, never queued to wait.
  Foreground is never refused on VRAM grounds: ADR-0044 already sized the live-suffix/
  present-sample kernels to a small, fixed per-frame footprint well under budget at both screen
  and full resolution, and a UI that can't render because a background bake ate the budget is a
  worse failure mode than temporarily starving that background job. This is a narrower guard than
  ADR-0044's own cache tiers (`Tier<V>`'s byte-budgeted LRU already handles *resident* cache-data
  eviction) — admission only concerns transient in-flight overcommit.
- **Telemetry**: `telemetry::HostTelemetrySource` wraps `sysinfo` (already a workspace dependency,
  `spikes/homing` set this precedent) for CPU/RAM. VRAM is a vendor-neutral DXGI query
  (`IDXGIAdapter3::QueryVideoMemoryInfo`) behind a `VramSource` trait, picked over `nvml-wrapper`:
  DXGI works on AMD/Intel too, and the v1 target is Windows-only anyway (ADR-0015), so there's no
  cross-platform cost to being Windows-specific here that NVML would avoid. `nvml-wrapper` stays a
  documented option for a future non-NVIDIA verification pass, not implemented. This telemetry
  source is explicitly shared with #70 (the standalone hardware-bottleneck indicator), per that
  ticket's own body — not a second implementation.
- **Same-API contention (real RTX 5080, throttled — the realistic mode)**: a persistent, tunable
  `busy.wgsl` compute kernel stands in for a real bake stage (ADR-0044's own persistent-kernel
  pattern, `spikes/loaf::gpu::LiveSuffixKernel`'s reasoning). Foreground latency under a
  background chunk running on the *same* `wgpu::Device`/`Queue` tracks the chunk's own duration
  almost 1:1: nominal 1/4/16/64ms background chunks measured 0.929/3.807/15.826/67.576ms p50
  foreground latency (0.948/3.820/15.845/68.161ms p95, close to p50 at every size), against
  foreground-alone baselines of ~0.25ms. A background chunk needs to stay well under ~16ms for
  foreground to reliably clear the 16.7ms slider-drag budget under contention — a real input to
  any future same-API (wgpu-side) background chunk sizing decision. (These are the corrected
  numbers after fixing a real measurement race an adversarial review caught — see below.)
- **Same-API contention, unthrottled (a deliberate stress test, not a design recommendation)**:
  fire-and-forget background submission (no wait between chunks) built an unbounded driver queue
  backlog — `wgpu::Device::poll(Wait)` drains everything already queued, and CPU submission for
  small dispatches vastly outpaces GPU retirement, so the backlog only grows. At a ~1ms nominal
  chunk size this alone produced p50=1948ms/p95=6260ms foreground latency; at ~4ms and ~64ms
  nominal chunk sizes, **the GPU device itself crashed** — `wgpu` reported "Error in Device::poll:
  Validation Error — Caused by: Parent device is lost," a genuine Windows TDR (Timeout Detection
  and Recovery) reset, not a harness bug. This is real, load-bearing evidence for why the
  scheduler's single-chunk-in-flight discipline (decision rule #3) is a correctness requirement,
  not tidiness: an unthrottled background worker can take the entire GPU context down, foreground
  included. `gpu_contend::BackgroundLoad::start` (throttled) is the only mode a real caller should
  use; `start_unthrottled` exists specifically to reproduce this finding on demand, gated behind
  an explicit CLI flag with a printed warning.
- **Cross-API contention (real RTX 5080) — the pass's most surprising finding**: real SCUNet-256px
  tile inference via `ort`'s CUDA execution provider (`ort_contend::TileLoad`, trimmed from
  `spikes/rods::ai::TiledDenoiser`'s scaffolding — no tiling/blending logic needed, only "keep the
  session busy") ran concurrently with the foreground `wgpu`/Vulkan kernel with **no measurable
  contention**: 53 real CUDA tile inferences (~45ms each per ADR-0040, roughly 2.4s of actual GPU
  compute) completed over a ~3s measurement window while foreground measured 0.440ms p50/0.486ms
  p95, indistinguishable from its own 0.441ms-alone baseline. A CPU execution-provider sanity check
  completed only 5 tiles in the same wall-clock window (a ~10x difference), confirming the CUDA
  EP was genuinely active rather than silently falling back to CPU (`ort` gives no cheaper way to
  check this after the fact, per `spikes/rods`'s own note). Unlike the same-API case above, this
  harness's two sides never share a `wgpu::Device`/`Queue` (the background side is a wholly
  separate `ort`/CUDA session), so it was never subject to the same-API measurement race described
  below. Vulkan and CUDA evidently get scheduled
  as independent contexts by this GPU's own hardware scheduler, unlike two `wgpu::Device` handles
  sharing one queue (which is itself a consequence of ADR-0016/0019/0050's own "one shared
  `wgpu::Device`" decision, not a property of GPUs in general). Practical consequence: chunk-size
  discipline matters for same-API (wgpu-side) background work, but the CUDA-side bake stages
  (SCUNet denoise, and by the same reasoning likely the AI mask model) don't need chunking *for
  this specific reason* — though they may still need it for cancellation responsiveness and VRAM
  admission, independent of any contention concern.
- **Adversarial-review findings, fixed before merge**: (1) `sim::ChunkedBakeCost::chunks()` never
  terminated for a zero `denoise_chunk` with a nonzero `denoise_total` — fixed by treating that
  case as one unchunked unit. (2) The same-API contention harness's `dispatch_and_wait` waited on
  `wgpu::PollType::wait_indefinitely()`, which resolves against "the most recent submission at
  poll time," not necessarily its own — under concurrent background submission this could fold
  extra background work into a measurement meant to be foreground-only, worse at higher
  percentiles (the same-API numbers above are the corrected, re-measured versions; the original
  buggy run showed p95 values 1.1-2x their own p50). Fixed by capturing each submission's own
  `SubmissionIndex` and polling on exactly that. (3) `job::JobSpec`'s own doc comment claimed VRAM
  admission was enforced before every background chunk, but `queue::Scheduler` never actually held
  or consulted an `Admission` — decision rule #5 was only ever validated by `admission.rs`'s own
  isolated unit tests. Fixed by wiring `Admission` into `Scheduler` for real, with a new test
  exercising this through `Scheduler::run_next` itself. **This fix itself had two real bugs**,
  found by a subsequent CodeRabbit review: `admit`/`release` bracket one `step()` call on one
  thread, so `remaining()` is always the full budget when checked — a job over the *total* budget
  was left retrying a check it could never pass, forever, rather than being refused (fixed by
  dropping such a job outright, checked against `Admission::budget()` rather than `remaining()`);
  and a yielded background job was pushed to the end of the vec instead of back into its sorted
  slot, silently degrading nearest-to-cursor-first into round-robin for any multi-chunk job (fixed
  by reinserting at the removed index). See `docs/adr/0054`'s own "Review findings" section for
  the full account of both review passes.
- **Tile-granular hero-scenario re-sim** (`sim.rs`, extends `spikes/loaf::sim` — copied, not
  depended-on): splits each image's bake into real chunks (atomic decode, N denoise tiles, atomic
  mask bake) and adds a periodic foreground demand serviceable only at a chunk boundary. With no
  foreground demand, reproduces ADR-0044's own numbers exactly (`first_image_ready=53.6s
  total_wall_time=2680s stale_at_arrival=50/50`), confirming the chunked model is a faithful
  extension, not a divergent reimplementation. **The real finding this adds**: chunking denoise
  into tiles bounds only *its own* worst-case preemption latency (one tile's duration) — decode
  (ADR-0037, ~1.7s midpoint) and mask bake (ADR-0048's own hypothesis, ~1.0s) aren't chunked in
  this model, so the real worst-case foreground latency this sim reports is bounded by whichever
  atomic stage is in flight, dominated by decode (`ChunkedBakeCost::worst_case_atomic_unit` with
  real ADR-0037/0040/0048 figures: 1700ms, not the ~45ms tile figure). Two caveats this sim (like
  `loaf`'s own) doesn't model, stated rather than silently assumed away: LibRaw decode is CPU-only
  work that doesn't actually contend for the GPU queue the way a chunk-modeled dispatch would, and
  a real implementation could run CPU decode fully concurrently with GPU-side foreground rendering
  instead of serializing all three stage types into one worker timeline for simplicity.
- **Environment gap closed this pass**: `spikes/rods`'s own CUDA/cuDNN/onnxruntime-gpu/TensorRT
  install from #40's research was no longer present on the reference machine (checked via `find`
  for `onnxruntime*.dll`/`*.onnx` before assuming otherwise) — re-installed fresh, matching #40's
  own cited versions exactly (Python 3.13, `onnxruntime-gpu==1.30.0`,
  `nvidia-cudnn-cu13==9.26.0.51`, CUDA 13.4 toolkit itself was still present system-wide), all via
  pip/winget, no NVIDIA developer login, same as #40's own note. `SCUNet-PSNR.onnx` was
  re-downloaded from `deepghs/image_restoration` (MIT, evaluate-only per `docs/licensing.md`'s
  existing row, not re-committed to the repo).
- **Open follow-ups**: [#205](https://github.com/jordanfelle/nicti/issues/205) (measure a smaller
  SCUNet tile size, 128px, if a same-API `wgpu` background chunk of similar duration to 256px is
  ever introduced — ADR-0040 only measured 256px); [#206](https://github.com/jordanfelle/nicti/issues/206)
  (decode/mask-bake chunking, or an explicit multi-lane concurrency model — CPU decode running
  independent of the GPU/`ort` worker — to bound their own worst-case foreground-preemption
  latency) — neither solved this pass, both genuine gaps in scope (#37 has no streaming decode
  interface yet).
