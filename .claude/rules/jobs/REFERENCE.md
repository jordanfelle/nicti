---
paths:
  - "spikes/crouch/**"
  - "crates/nicti-pounce/**"
  - "crates/nicti-lair/src/pounce_jobs.rs"
  - "crates/nicti-pelt/src/activity.rs"
  - "docs/adr/0054-job-scheduler-pounce.md"
---

# Jobs & Concurrency (Pounce) — Quick Reference

Full reasoning/history: `docs/decisions/jobs.md`.

- **Production crate (#55, landed): `crates/nicti-pounce`** — promotes `spikes/crouch`'s
  `job`/`cancel`/`queue`/`admission`/`throttle`/`telemetry` (unchanged in design) into a real
  threaded runtime, `runtime::Pounce`. Two lanes answer #206's "CPU decode vs. GPU work" question
  structurally: `Lane::Gpu` (one worker thread, real VRAM admission, ADR-0054's one-chunk-in-flight
  rule) and `Lane::Cpu` (a worker-thread pool gated by a live-adjustable `Throttle` — Scruff/
  Patrol's import/sync scan, `nicti-lair::pounce_jobs`, is the first real client). `queue::
  Scheduler::take_next`/`finish` replaces the spike's single `run_next` so a worker never holds
  the lane's lock across a job's own `step()`. `Pounce::cancel` keeps its own `JobId`-keyed
  `CancelToken` registry rather than delegating to `Scheduler::cancel` — the scheduler's own
  `cancel` only finds a job still sitting in its queue, which most of a fast-yielding job's
  lifetime isn't (a real bug this ticket's own tests caught: cancelling a job with no per-job
  delay raced the job to completion before `Scheduler::cancel` ever found it queued). No bake
  pipeline exists yet (Tapetum has no worker; Develop renders synchronously every frame), so the
  GPU lane and VRAM admission are exercised only by synthetic jobs in `nicti-pounce`'s own tests —
  the first real bake job is a follow-up tied to #31/#27.
- **UI (#55, landed): `crates/nicti-pelt/src/activity.rs`** — a bottom status bar reading
  `Pounce::snapshot()`: collapsed, running/queued counts + a `TelemetrySampler` readout (CPU/RAM/
  VRAM, VRAM shown "n/a" rather than fabricated when unavailable); expanded, one row per job with
  a progress bar/spinner and a cancel button; plus a live CPU-lane concurrency `DragValue`. The
  Library view's Import/Sync buttons (`app.rs`) register a placeholder single-volume root (no real
  volume-identity system wired into this shell yet) and submit `IngestJob`/`SyncJob`.
- **Scheduler (#54)** — `docs/adr/0054`: **Proposed**, same-API and cross-API contention both
  measured on real RTX 5080 hardware; wiring into a real bake pipeline is #55's build.
- **Model**: `job::ChunkedJob` (`spec()`/`step() -> Yield|Done`) — cooperative cancellation only
  *between* chunks, never mid-chunk (neither `wgpu::Queue::submit` nor `ort::Session::run` support
  interrupting a call in flight). Two-class `queue::Scheduler`: foreground always dequeues before
  background; background order is a pluggable key (ADR-0044's own `prefetch::priority_order`
  plugs straight in, copied not depended-on).
- **Cancellation**: plain `Arc<AtomicBool>` (`cancel::CancelToken`/`EditingGate`), not
  `tokio_util::sync::CancellationToken` — exactly one serial GPU/`ort` worker in this design, no
  async runtime anywhere else to make a `tokio`-flavored token pay for itself.
- **Throttling**: hand-rolled counting-semaphore (`throttle::Throttle`) for CPU/disk-bound
  background work (Scruff's import scan is the first real client) — `governor` evaluated and
  rejected, it's rate-per-time shaped, not concurrency-limit shaped.
- **VRAM admission** (`admission.rs`): a background job over the *total* budget (`Admission::
  budget()`) is dropped outright — it could never become admittable, since `admit`/`release`
  bracket one synchronous `step()` call, so `remaining()` is always the full budget when checked.
  A job over only the currently-*remaining* budget is skipped for the current pick and stays
  queued. Foreground is never refused on VRAM grounds. Wired into `queue::Scheduler::run_next`
  itself (an adversarial review caught this only being enforced in `admission.rs`'s own isolated
  tests; a follow-up CodeRabbit review then caught the wired-in version itself leaking an
  over-total-budget job forever — both fixed, see `docs/adr/0054`'s Review findings).
- **Telemetry** (`telemetry.rs`): `sysinfo` for CPU/RAM, DXGI (`IDXGIAdapter3::
  QueryVideoMemoryInfo`) for VRAM behind a `VramSource` trait — picked over `nvml-wrapper` for
  vendor neutrality (v1 target is Windows-only anyway). Windows-only code, unverified in this
  sandbox (no GPU adapter under WSL) — shared with #70's own bottleneck indicator.
- **Same-API contention (real RTX 5080, throttled/realistic)**: foreground latency under a
  background `wgpu` chunk tracks chunk size ~1:1 — 1/4/16/64ms nominal chunks measured
  0.93/3.8/15.8/67.6ms p50 foreground latency (p95 close to p50 at every size — corrected numbers
  after fixing a real `SubmissionIndex` polling race an adversarial review caught, see
  `docs/decisions/jobs.md`). Background chunks must stay well under ~16ms to keep foreground
  inside the 16.7ms slider-drag budget under contention.
- **Same-API contention, unthrottled (deliberate stress test)**: fire-and-forget background
  submission **crashed the GPU device** (Windows TDR, "Parent device is lost") at moderate chunk
  sizes — real evidence the scheduler's single-chunk-in-flight discipline is a correctness
  requirement, not tidiness. `gpu_contend::BackgroundLoad::start` (throttled) is the only mode a
  real caller should use; `start_unthrottled` exists only to reproduce this finding on demand.
- **Cross-API contention (real RTX 5080)**: 53 real SCUNet-256px CUDA tile inferences (`ort`) ran
  concurrently with the foreground `wgpu` kernel over ~3s with **no measurable contention**
  (0.440ms p50 vs. ~0.44ms alone) — Vulkan and CUDA get scheduled as independent contexts on this
  hardware, unlike two `wgpu::Device`s sharing one queue (this harness's two sides never share a
  device, so it was never subject to the same-API polling race above). CPU-EP sanity check (5 tiles vs. CUDA's
  53 in the same window) confirms the CUDA EP was genuinely active.
- **Sim finding (`sim.rs`, tile-granular extension of `loaf::sim`)**: chunking denoise into tiles
  bounds only *its own* worst-case preemption latency — decode (~1.7s, ADR-0037) and mask bake
  (~1.0s, ADR-0048) aren't chunked in this model, so they set the real worst-case foreground-
  latency bound (`ChunkedBakeCost::worst_case_atomic_unit`), not the ~45ms denoise tile. Not solved
  this pass — #37 has no streaming decode interface, and this sim (like `loaf`'s own) serializes
  CPU decode into the same worker timeline as GPU work for simplicity, when a real implementation
  could run them concurrently.
- **Environment note**: `spikes/rods`'s own CUDA/cuDNN/onnxruntime-gpu install from #40 wasn't
  present on the reference machine and was re-installed fresh this pass (Python 3.13 venv,
  `onnxruntime-gpu==1.30.0`, `nvidia-cudnn-cu13==9.26.0.51` — matching #40's own cited versions,
  pip/winget only, no NVIDIA login) — check before assuming a prior pass's environment still holds.
- **[#205](https://github.com/jordanfelle/nicti/issues/205), measured**: 128px SCUNet tile does
  *not* clear the same-API contention budget either, despite being the faster tile per call
  (p50=33.3ms/p95=45.1ms vs. 256px's 37.0ms/51.5ms, isolated per-tile timing via the new
  `bench-tile` subcommand — fixed per-call overhead dominates at 128px) — its p95 is still far
  past the ~16ms budget. Separately, 128px needs 5.4x more tiles/frame so its estimated
  whole-frame cost is ~4.9x *worse* (~88.1s vs. ~18.0s) — faster per call, worse in total. Moot for
  now: SCUNet's real path is cross-API CUDA, already found contention-free above; only matters for
  a hypothetical future same-API `wgpu` background chunk.
- **Open follow-up**: [#206](https://github.com/jordanfelle/nicti/issues/206) (decode/mask-bake
  chunking or explicit cross-lane concurrency, CPU decode vs. GPU work, to bound their own
  worst-case latency).

## Package contents

- **`spikes/crouch`** (#54/ADR-0054's job-scheduler research) — `job.rs`/`queue.rs`/`cancel.rs`/
  `admission.rs`/`throttle.rs` (the scheduler core), `telemetry.rs` (`sysinfo` + DXGI VRAM),
  `prefetch.rs` (copy of ADR-0044's own priority ordering), `gpu_contend.rs` (persistent
  `busy.wgsl` kernel + throttled/unthrottled `BackgroundLoad`, the wgpu-vs-wgpu contention
  harness), `ort_contend.rs` (`TileLoad`, trimmed from `spikes/rods::ai::TiledDenoiser`, the
  CUDA-vs-wgpu contention harness, plus `tiles_for_frame` — a pure helper mirroring
  `rods::ai::TiledDenoiser::denoise`'s own stride/edge-clamp loop, used by #205's whole-frame cost
  estimate), `sim.rs` (tile-granular hero-scenario re-sim). `src/bin/crouch.rs` exposes
  `bench-wgpu`/`bench-ort`/`bench-tile`/`sim` subcommands. 42 unit tests, real reference-hardware
  numbers for both contention cases (not just lavapipe correctness). See
  `docs/research/crouch-scheduler.md`.
