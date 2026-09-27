# Crouch: job scheduler research write-up (#54, ADR-0054)

Feline name: the motionless crouch before a pounce — the scheduler's idle/ready state, matching
Pounce's own cat-behavior codename.

Read `docs/adr/0054-job-scheduler-pounce.md` first for the actual decision — this doc is the
method and raw numbers behind it.

## What this pass built

`spikes/crouch` is a headless scheduler core (priority queue, cooperative cancellation, an
`IS_EDITING` gate, VRAM admission, CPU/disk throttling, hardware telemetry) plus two real
GPU-contention harnesses and a tile-granular extension of `loaf`'s own hero-scenario simulation. It
does not include a real bake pipeline to schedule (that's #55's build, once a render engine exists
to hand it real jobs) or a wired-in Scruff/telemetry consumer.

## Method

1. **Scheduler core** (`job.rs`/`queue.rs`/`cancel.rs`/`admission.rs`/`throttle.rs`): a
   `ChunkedJob` trait (`spec()` + `step() -> Yield | Done`), a two-class `Scheduler` (foreground
   always dequeues first, background order is a pluggable key so ADR-0044's own
   `prefetch::priority_order` plugs straight in), plain-atomic cancellation
   (`CancelToken`/`EditingGate` — no async runtime anywhere in this design), a VRAM-budget
   reservation guard, and a hand-rolled concurrency-limit semaphore for CPU/disk work.
2. **Telemetry** (`telemetry.rs`): `sysinfo`-backed CPU/RAM, DXGI-backed VRAM behind a `VramSource`
   trait (Windows-only impl, unverified in this sandbox — no GPU adapter under WSL — same caveat
   `spikes/homing` already carries).
3. **Same-API contention** (`gpu_contend.rs`): a tunable-duration `busy.wgsl` compute kernel,
   dispatched as a persistent `*Kernel` (never rebuild inside a timing loop — ADR-0044's own
   gotcha). `BackgroundLoad` runs it on a second thread sharing the same `wgpu::Device`/`Queue`,
   in two modes: throttled (wait for each chunk's own completion before the next — the realistic,
   scheduler-matching mode) and unthrottled (fire-and-forget, a deliberate stress test).
4. **Cross-API contention** (`ort_contend.rs`): `TileLoad`, a trimmed copy of
   `spikes/rods::ai::TiledDenoiser`'s scaffolding (load a session, run one fixed-size tile,
   discard the output) — real SCUNet-256px inference on a background thread via `ort`'s CUDA
   execution provider, while the foreground `busy.wgsl` kernel is timed on the main thread.
5. **Tile-granular re-sim** (`sim.rs`): copies `loaf::sim`'s cursor-walk model, but splits each
   image's bake into real chunks (atomic decode, N denoise tiles, atomic mask bake) and adds a
   periodic foreground demand serviceable only at a chunk boundary.

## A real environment gap this pass had to close

`spikes/rods`'s own CUDA/cuDNN/onnxruntime-gpu/TensorRT install from #40's research pass was no
longer present on the reference machine — checked via `find` for `onnxruntime*.dll`/`*.onnx` under
the Windows user profile before assuming anything, rather than skipping the cross-API measurement
silently. Re-installed fresh, matching #40's own cited versions exactly: Python 3.13 (via
`winget install Python.Python.3.13`), a venv, `pip install onnxruntime-gpu==1.30.0
nvidia-cudnn-cu13==9.26.0.51` (CUDA 13.4 toolkit itself was still present system-wide; only cuDNN
and the `ort`-native DLLs were missing) — all via pip/winget, no NVIDIA developer login, same as
#40's own note. `SCUNet-PSNR.onnx` was re-downloaded from `deepghs/image_restoration` (MIT,
evaluate-only per `docs/licensing.md`'s existing row) rather than assumed still on disk.

## A real GPU crash this pass's own measurement caught

The first same-API contention pass used a fire-and-forget background load (submit as fast as
possible, no wait). At a ~1ms nominal chunk size this "only" produced a severe backlog
(p50=1948ms/p95=6260ms foreground latency, vs. ~0.3-0.5ms alone) — but at ~4ms and ~64ms nominal
chunk sizes, the process crashed outright:

```
thread 'main' panicked at .../wgpu-30.0.1/src/backend/wgpu_core.rs:1924:30:
Error in Device::poll: Validation Error
Caused by:
  Parent device is lost
```

This is a genuine Windows TDR (Timeout Detection and Recovery) GPU reset, triggered because
unthrottled submission from the CPU vastly outpaces GPU retirement for small dispatches, building
an unbounded queue backlog the driver watchdog eventually treats as a hung GPU. Re-running the same
chunk sizes in throttled mode (wait for each chunk's own completion before submitting the next)
produced clean, bounded results with no crash — see ADR-0054's own Measured results table. This
converted what could have looked like a flaky/uninteresting crash into the pass's strongest
argument for the scheduler's single-chunk-in-flight discipline: it isn't just about keeping
latency low, an unthrottled background worker can genuinely take the whole GPU device down.

## Three real bugs an adversarial review caught, all fixed before merge

1. **An infinite loop in `sim::ChunkedBakeCost::chunks()`**: a zero `denoise_chunk` with a nonzero
   `denoise_total` never made progress (`remaining.min(ZERO)` is always `ZERO`), directly reachable
   from `bin/crouch.rs`'s `sim --denoise-chunk-ms 0`. Fixed by treating a zero chunk size as one
   unchunked unit covering the whole total, matching `worst_case_atomic_unit`'s own accounting for
   the same case. Caught by the review reasoning through the loop's own termination condition
   against an adversarial input, not by running it (it doesn't self-terminate).
2. **A measurement race in `gpu_contend::BusyKernel::dispatch_and_wait`**: it called
   `wgpu::PollType::wait_indefinitely()`, which `wgpu-types`' own doc comment says waits for "the
   most recent submission at the time of the poll," not necessarily the caller's own. Under
   concurrent submission from a background contention thread sharing the same `wgpu::Device`/
   `Queue`, that could be a *later* submission than this call's own, silently folding extra
   background work into a measurement meant to be foreground-only -- worse at higher percentiles,
   since a wider race window is rarer but costlier when it hits. Fixed by capturing each
   submission's own `SubmissionIndex` (`Queue::submit`'s own return value, previously discarded)
   and polling on exactly that. This materially changed the same-API contention numbers -- see
   Raw numbers below for the corrected, re-measured table.
3. **A doc/code mismatch on VRAM enforcement**: `job::JobSpec`'s own doc comment claimed VRAM was
   checked before every background chunk, but `queue::Scheduler` never held or consulted an
   `admission::Admission` anywhere -- decision rule #5 was only ever exercised by `admission.rs`'s
   own isolated unit tests, never through the scheduler itself. Fixed by wiring `Admission` into
   `Scheduler::run_next` (an over-budget background job is now skipped for the current pick, not
   dropped, and re-tried once another job's reservation releases), with a new test
   (`background_job_over_vram_budget_is_skipped_not_dropped`) exercising this through the
   scheduler directly rather than `admission.rs` in isolation.

## Raw numbers

See ADR-0054's own Measured results for the full tables; summarized here:

- **Same-API (wgpu-vs-wgpu), throttled**: foreground alone ~0.25ms; under background chunks of
  ~1/4/16/64ms nominal size, foreground measured 0.929/3.807/15.826/67.576ms p50 (0.948/3.820/
  15.845/68.161ms p95, close to p50 at every size) — tracks background chunk size roughly 1:1.
  These are the corrected numbers after fixing a real `SubmissionIndex`-polling race an
  adversarial review caught (see below) — the original, buggy run showed p95 values 1.1-2x their
  own p50, which turned out to be substantially a measurement artifact, not real driver-level tail
  variance.
- **Same-API, unthrottled (stress test)**: ~1ms chunks alone produced p50=1948ms/p95=6260ms;
  ~4ms and ~64ms chunks crashed the GPU device (Windows TDR, "Parent device is lost"). These
  numbers predate the polling fix and weren't re-run afterward (to avoid deliberately
  re-triggering a device crash) — the device-loss finding itself is unaffected either way, since
  it's an independent driver-level failure, not a consequence of which submission a *different*
  measurement waits on.
- **Cross-API (ort/CUDA-vs-wgpu)**: foreground alone ~0.44-0.55ms; under 53 real SCUNet-256px CUDA
  tile inferences over ~3s, foreground measured 0.440ms p50/0.486ms p95 — no measurable
  contention. A CPU-EP sanity check completed only 5 tiles in the same window (~10x slower),
  confirming the CUDA EP was genuinely active. This harness's two sides never share a
  `wgpu::Device`, so it was never subject to the same-API polling race above.
- **Sim**: `crouch sim` with no foreground demand reproduces ADR-0044's own hero-scenario numbers
  exactly (`first_image_ready=53.6s total_wall_time=2680s stale_at_arrival=50/50`), confirming the
  chunked model is a faithful extension. `worst_case_atomic_unit` with real ADR-0037/0040/0048
  figures plugged in is 1700ms (decode), not the ~45ms denoise-tile figure — decode and mask bake
  aren't chunked in this model, so they (not the tile-chunked denoise) set the real worst-case
  foreground-preemption bound.

## Follow-up pass (#205): SCUNet 128px tile timing

Added `bench-tile` to `src/bin/crouch.rs`: isolated per-tile SCUNet timing (`ort_contend::TileLoad`,
no wgpu contention involved), plus `ort_contend::tiles_for_frame` — a pure helper mirroring
`rods::ai::TiledDenoiser::denoise`'s own stride/edge-clamp loop, to turn a per-tile timing into an
estimated whole-frame cost at a given tile size/overlap.

**Environment check**: the venv (`C:\Users\hyper\crouch-ort-venv`) and `SCUNet-PSNR.onnx` from this
ADR's original pass were still present — no reinstall needed this time (checked via `find` for
`onnxruntime*.dll`/`*.onnx` under the Windows profile before assuming so, same as the original
pass's own discipline). Reused the existing `run-crouch-ort.bat` pattern, pointed at this
worktree's own build output (`run-crouch-205.bat`, since the original `.bat` hardcoded the old
`nicti-wt-54-pounce` worktree path).

**Commands** (via `\\wsl.localhost\...\crouch.exe`, invoked through `cmd.exe /c` since a `.bat`
needs a real Windows shell, not plain WSL-interop exec):

```
cargo build --release --target x86_64-pc-windows-gnu -p crouch --bin crouch

run-crouch-205.bat bench-tile --model-path C:\Users\hyper\crouch-ort-venv\SCUNet-PSNR.onnx \
  --ort-dylib-path C:\Users\hyper\crouch-ort-venv\Lib\site-packages\onnxruntime\capi\onnxruntime.dll \
  --ep cuda --tile-size 128 --tile-size 256 --out-dir C:\Users\hyper\crouch-ort-venv\bench-results-205

run-crouch-205.bat bench-tile --model-path C:\Users\hyper\crouch-ort-venv\SCUNet-PSNR.onnx \
  --ort-dylib-path C:\Users\hyper\crouch-ort-venv\Lib\site-packages\onnxruntime\capi\onnxruntime.dll \
  --ep cpu --tile-size 128 --warmup 1 --measured 5 --out-dir C:\Users\hyper\crouch-ort-venv\bench-results-205
```

**Raw output**:

```
crouch-tile-128px-Cuda: p50=33.299ms p95=45.119ms max=57.432ms
  tile=128px overlap=32 ep=Cuda: 2646 tiles/frame (6064x4040) -> estimated 88110ms/frame -- DOES NOT CLEAR
crouch-tile-256px-Cuda: p50=37.044ms p95=51.534ms max=63.003ms
  tile=256px overlap=32 ep=Cuda: 486 tiles/frame (6064x4040) -> estimated 18003ms/frame -- DOES NOT CLEAR
crouch-tile-128px-Cpu:  p50=186.852ms p95=198.314ms max=198.314ms
  tile=128px overlap=32 ep=Cpu: 2646 tiles/frame (6064x4040) -> estimated 494411ms/frame -- DOES NOT CLEAR
```

Hardware: NVIDIA GeForce RTX 5080, driver 616.56 (recorded by hand — `HardwareIdentity` doesn't
capture GPU/driver, same caveat this ADR's own tables already note).

**Reading these numbers**: 128px is genuinely faster per call than 256px on both p50 and p95
(33.3ms vs. 37.0ms, 45.1ms vs. 51.5ms) rather than roughly a quarter of 256px's cost, because
per-call overhead (kernel launch, H2D/D2H, ONNX Runtime session dispatch) is largely fixed
regardless of tile size, and dominates at this scale — but that per-tile win still isn't enough to
clear the ~16ms same-API budget (128px's p95, 45.1ms, is still far past it). Separately, since
128px also needs 5.4x more tiles to cover the same 6064×4040 frame at a fixed 32px overlap (2646
vs. 486), its estimated whole-frame cost is ~4.9x *worse* (~88.1s vs. ~18.0s) — two different
measurements pointing opposite ways: faster per call, but worse in total because it needs so many
more calls. The 128px CPU-EP sanity run
confirms CUDA was genuinely active (33.3ms vs. 186.9ms, ~5.6x — comfortably past
`suspiciously_close_to_cpu_speed`'s ~2x fallback-detection floor), but that ~5.6x speedup is itself
far below ADR-0040's own ~36x at 256px — the same fixed-overhead effect eating a
proportionally larger share of an already-small GPU number. See ADR-0054's own "Follow-up
measurement (#205)" section for the decision-rule verdict and what this means for Pounce's design.

## What this means for #54's own design

The two contention measurements point in different directions for the same design question ("does
Pounce need to chunk its background AI work to protect the live render?"): **yes for same-API
work** (any future wgpu-side background dispatch, e.g. a larger mask-refine `box_filter` pass) —
chunks must stay well under 16.7ms to be safe under contention — but **effectively no for the
CUDA-side bake stages specifically** (SCUNet denoise, and by the same reasoning likely the AI mask
model too, once #48 measures it), since cross-API contention on this hardware is negligible.
Chunking those still matters for cancellation responsiveness and VRAM admission (decision rules #3
and #5), just not for the same-API-contention reason ADR-0044's own scheduling contract initially
worried about.

The tile-granular sim's own finding — that decode and mask bake, not the tile-chunked denoise, set
the real worst-case foreground-preemption bound — is a genuine gap in scope, not solved this pass:
#37 (decode) has no streaming interface, and this sim (like `loaf`'s own) models one serial worker
timeline across all three stage types for simplicity, when a real implementation could run
CPU-only decode fully concurrently with GPU work instead. Filed as a follow-up rather than
resolved speculatively here.

## Cross-compile / reference-machine notes

Same documented path as every prior reference-machine pass
(`.claude/rules/gpu-gui-and-healing/REFERENCE.md`): rustup's toolchain directory placed first on
`PATH` for both `cargo` and `rustc` (the Homebrew-shadowing gotcha `denoise`'s own REFERENCE.md
already documents), `cargo build --release --target x86_64-pc-windows-gnu -p crouch --bin crouch`,
run directly via WSL interop (the built `.exe` invoked by its own path from WSL — no
`powershell.exe`/UNC wrapper needed for the plain benches; the `bench-ort` runs needed a small
`.bat` wrapper to prepend the pip-installed CUDA/cuDNN DLL directories to `PATH` before invoking
the same `.exe`, since those DLLs live under the Windows user's pip venv, not a system directory).
Picked up the real `NVIDIA GeForce RTX 5080 (Vulkan)` adapter, not lavapipe. `cargo test -p crouch`
(37 tests, all passing) ran against lavapipe/llvmpipe software rendering in this sandbox's own
Linux side — correctness only, not timing, same as every other spike's own sandbox/reference-
machine split.
