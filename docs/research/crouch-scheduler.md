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

## Raw numbers

See ADR-0054's own Measured results for the full tables; summarized here:

- **Same-API (wgpu-vs-wgpu), throttled**: foreground alone ~0.3-0.5ms; under background chunks of
  ~1/4/16/64ms nominal size, foreground measured 0.968/7.745/16.427/70.757ms p50 (1.386/8.087/
  17.170/145.374ms p95) — tracks background chunk size roughly 1:1, worse at the tail.
- **Same-API, unthrottled (stress test)**: ~1ms chunks alone produced p50=1948ms/p95=6260ms;
  ~4ms and ~64ms chunks crashed the GPU device (Windows TDR, "Parent device is lost").
- **Cross-API (ort/CUDA-vs-wgpu)**: foreground alone ~0.48-0.55ms; under 37 real SCUNet-256px CUDA
  tile inferences over ~3s, foreground measured 0.481ms p50/0.616ms p95 — no measurable
  contention. A CPU-EP sanity check completed only 5 tiles in the same window (~7x+ slower),
  confirming the CUDA EP was genuinely active.
- **Sim**: `crouch sim` with no foreground demand reproduces ADR-0044's own hero-scenario numbers
  exactly (`first_image_ready=53.6s total_wall_time=2680s stale_at_arrival=50/50`), confirming the
  chunked model is a faithful extension. `worst_case_atomic_unit` with real ADR-0037/0040/0048
  figures plugged in is 1700ms (decode), not the ~45ms denoise-tile figure — decode and mask bake
  aren't chunked in this model, so they (not the tile-chunked denoise) set the real worst-case
  foreground-preemption bound.

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
(31 tests, all passing) ran against lavapipe/llvmpipe software rendering in this sandbox's own
Linux side — correctness only, not timing, same as every other spike's own sandbox/reference-
machine split.
