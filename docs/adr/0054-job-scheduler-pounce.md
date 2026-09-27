# ADR-0054: Job scheduler design (Pounce)

- **Status:** Proposed — decision rules #1-#2 measured clean on the real reference RTX 5080
  (Windows-native, wgpu-vs-wgpu and ort/CUDA-vs-wgpu contention both real, not simulated); rule #3
  is both structurally unit-tested and confirmed on real hardware (the unthrottled stress test);
  rule #4 (`IS_EDITING`/priority correctness) is structural and unit-tested only, not itself a
  hardware measurement. The full scheduler's wiring into a real bake pipeline and into Scruff's
  import scan are #55's build, not measured here.
- **Date:** 2026-09-27
- **Ticket:** [#54](https://github.com/jordanfelle/nicti/issues/54) Research: scheduler design
  (Pounce)

## Context

#54 is unblocked: its only listed blocker, #44 (ADR-0044, Tapetum's stage-cached render graph),
merged and already hands Pounce a concrete contract to satisfy — "bake jobs prioritized by
`|image_index − cursor|` (`spikes/loaf/src/prefetch.rs::priority_order`), reprioritized
immediately whenever the cursor moves, one serial bake worker (one shared `wgpu::Device`/one
`ort::Session`, per ADR-0016/0019/0050's existing 'one shared GPU device' decisions), a documented
stale-while-baking fallback while a bake is in flight" (ADR-0044's own "Bake scheduling contract
(for Pounce, #54)" section). #55 (build) and #70 (the standalone hardware-bottleneck telemetry
indicator, sharing this same telemetry source) both wait on this ticket.
`crates/nicti-catalog::scruff` (the Scruff import pipeline, #22) also notes "wiring into Pounce is
a follow-up" — its own CPU/disk-bound scan work is a second real client of the throttling half of
this design, not just GPU bake jobs.

The central open question this pass exists to answer: **neither Vulkan (`wgpu`) nor CUDA (`ort`)
expose mid-dispatch preemption**, so whatever "foreground UI always preempts background batch
jobs" (ADR-0015 §8's own framing) means in practice, it can only mean *cooperative* cancellation at
a chunk boundary — the real question is how big a chunk can be before it blows the develop panel's
own slider→ready budget (≤16.7ms, `docs/benchmarks.md:17`), and whether that answer differs
depending on whether the background work shares wgpu's own hardware queue (a same-API contest) or
runs through a completely separate driver context (`ort`'s CUDA execution provider, a cross-API
contest). Neither Vulkan's nor CUDA's own documentation commits to an answer for either case; both
had to be measured.

**User decision (2026-09-27):** measure both wgpu-vs-wgpu and ort/CUDA-vs-wgpu contention on the
real RTX 5080 reference machine (cross-compile `x86_64-pc-windows-gnu`, run via WSL interop, the
same path ADR-0044/0040 already used), rather than leaving either as an unmeasured hypothesis.
`spikes/rods`'s own CUDA/cuDNN/onnxruntime-gpu/TensorRT install from #40's research pass was no
longer present on the reference machine when this pass started — **re-installed fresh this pass**
(Python 3.13 + a venv, `onnxruntime-gpu==1.30.0`, `nvidia-cudnn-cu13==9.26.0.51`, matching #40's
own cited versions exactly, all via pip/winget, no NVIDIA login needed, same as #40's own note) —
and the `SCUNet-PSNR.onnx` checkpoint (`deepghs/image_restoration`, MIT, evaluate-only per
`docs/licensing.md`'s existing row — not re-committed to the repo) was re-downloaded fresh from
Hugging Face.

## Decision rules (stated before measuring)

1. **Foreground preemption under same-API contention.** With a background `wgpu` chunk running,
   foreground `wgpu` dispatch latency is reported as a curve over background chunk size
   (targets: ~1/4/16/64ms), against the ≤16.7ms slider-drag budget.
2. **Foreground preemption under cross-API contention.** Same measurement, with the background
   load running real SCUNet-256px tile inference via `ort`'s CUDA execution provider instead of a
   second `wgpu` dispatch.
3. **Cancellation/backpressure discipline.** The scheduler's worker holds at most one background
   chunk in flight at a time (submit, wait for that chunk's own completion, then check
   cancellation/`IS_EDITING` before submitting the next) — not fire-and-forget. Verified both
   structurally (unit tests) and by contrast with a deliberately pathological unthrottled stress
   test (see Measured results).
4. **`IS_EDITING` / priority correctness.** Foreground always dequeues before background; a
   cursor move reprioritizes pending background jobs without touching an already-in-flight chunk;
   zero new background chunks start while `IS_EDITING` is set. All structural (unit-tested), not
   requiring reference hardware.
5. **VRAM admission.** A background job whose declared bytes exceed the remaining budget is
   refused outright (not queued to wait); foreground is never refused on VRAM grounds. Structural
   (unit-tested) — no real VRAM telemetry source was queryable in this sandbox (no GPU adapter
   under WSL); the DXGI-based query is implemented but unverified real-hardware code, same caveat
   `spikes/homing` already carries for its own Windows-only code.
6. **Report, not gate: tile-granular hero-scenario re-sim.** Re-runs ADR-0044's own bake-queue
   simulation with each image's bake split into real chunks (one atomic decode, N denoise tiles,
   one atomic mask bake) plus a periodic foreground demand, to show what preemption actually costs
   the bulk-sync scenario — and what it *doesn't* fix.

## Decision

### Scheduler shape: `spikes/crouch`

Two-class priority queue (`queue::Scheduler`) — foreground always dequeues before background,
background order delegated to a pluggable key function so ADR-0044's own
`prefetch::priority_order` (copied, not depended-on, matching every other spike's "spikes don't
depend on each other" convention) plugs straight in. Jobs implement `job::ChunkedJob` (`spec()` +
`step() -> Yield | Done`) — cooperative cancellation is checked only *between* chunks
(`cancel::CancelToken`), never mid-chunk, since neither `wgpu::Queue::submit` nor
`ort::Session::run` support interrupting a call already in flight. A process-global
`cancel::EditingGate` ("`IS_EDITING`") withholds new background chunks while a slider is being
dragged — an already-admitted chunk still runs to completion. `admission::Admission` is a
byte-budget reservation guard (not itself a cache — ADR-0044's own `Tier<V>` already owns resident
cache-data eviction): a background job whose declared VRAM exceeds the remaining budget is
refused, foreground never is. `throttle::Throttle` is a hand-rolled counting-semaphore-style
concurrency limiter for CPU/disk-bound background work (Scruff's import scan is the first real
client) — `governor` was evaluated and rejected: its `RateLimiter` is shaped for
cells-per-unit-time rate limiting, not "N of these run concurrently," and reaching for it would
mean bending a real dependency to a shape it isn't for, for a limiter that's a dozen lines
hand-rolled. `telemetry::HostTelemetrySource` wraps `sysinfo` (already a workspace dependency,
`spikes/homing` set the precedent) for CPU/RAM; VRAM telemetry is DXGI
(`IDXGIAdapter3::QueryVideoMemoryInfo`) behind a `VramSource` trait, picked over `nvml-wrapper`
because it's vendor-neutral (works on AMD/Intel too) and the v1 target is Windows-only anyway
(ADR-0015) — `nvml-wrapper` stays a documented option for a future non-NVIDIA verification pass,
not implemented. `#70` (the standalone bottleneck indicator) shares this exact telemetry source
rather than building its own, per that ticket's own body.

**Why plain atomics + dedicated threads, not `tokio_util::sync::CancellationToken`**: the whole
design is built around exactly one serial GPU/`ort` worker (one shared `wgpu::Device`/one
`ort::Session`, per ADR-0016/0019/0050's existing decisions) — there's no async runtime anywhere
else in this design for a `tokio`-flavored cancellation token to pay for itself. A plain
`Arc<AtomicBool>` (`CancelToken`/`EditingGate`) does the same job with none of the dependency.

### Real measurement: same-API (wgpu-vs-wgpu) contention

**A methodology note on every "p95" figure in this ADR**: `nicti_prowl::perf::Protocol`'s default
is 1 warmup + 5 measured runs, and its `percentile()` uses nearest-rank —
`ceil(0.95 × 5) = 5`, i.e. the 5th of 5 sorted samples. Every "p95" reported here (and in ADR-0044,
which uses the same protocol) is therefore literally the single worst of 5 runs, not a smoothed
tail-percentile estimate over a larger sample. Read "p50/p95" in this ADR's own tables as
"median/worst of 5," not as a claim about the underlying distribution's real 95th percentile —
n=5 is thin for that, including for this ADR's own negative claim ("no measurable [cross-API]
contention") below.

**The realistic measurement uses a throttled background load** — one chunk submitted, waited on
for its own completion, then the next — exactly matching the scheduler's own single-chunk-in-flight
discipline (decision rule #3). A synthetic `busy.wgsl` compute kernel (tunable iteration count)
stands in for a real bake stage, same reasoning ADR-0044 used a real `LiveSuffixKernel` for its own
foreground measurement but this pass needs an independently *tunable* cost for the background side.
Real RTX 5080 numbers (`elements=1,048,576`, foreground kernel calibrated to ~0.3-0.5ms alone,
matching ADR-0044's own 0.375ms live-suffix figure):

| Background chunk size (nominal) | Foreground alone | Foreground under contention (p50 / p95) |
|---|---|---|
| ~1ms | 0.25ms | 0.929ms / 0.948ms |
| ~4ms | 0.23ms | 3.807ms / 3.820ms |
| ~16ms | 0.26ms | 15.826ms / 15.845ms |
| ~64ms | 0.26ms | 67.576ms / 68.161ms |

**Decision rule #1's finding**: foreground latency under contention tracks background chunk size
almost 1:1, and — once a real measurement bug was fixed (see below) — p50 and p95 land close
together at every chunk size, not a wide spread. A background chunk must stay well under ~16ms for
foreground to reliably clear the 16.7ms budget under contention — a real, load-bearing input to
choosing SCUNet's tile size (ADR-0040 only measured 256px at 44.8ms; this ADR's own finding is that
a 256px tile chunk would risk blowing the frame budget if a foreground dispatch lands during it,
motivating a smaller tile size or explicit scheduling around `IS_EDITING`, not evaluated further
here — see Consequences).

**A real measurement bug an adversarial review caught, and its effect on these numbers**: the
first version of `dispatch_and_wait` called `wgpu::PollType::wait_indefinitely()`, which — per
`wgpu-types`' own doc comment — waits for "the most recent submission at the time of the poll," not
a specific one. Under concurrent submission from the background contention thread sharing the same
`wgpu::Device`/`Queue`, a background chunk could land in the race window between foreground's own
`submit()` and its `poll()`, silently folding extra background work into what was supposed to be a
foreground-only measurement — worse at higher percentiles, since a wider race window is rarer but
costlier when it hits. The original (buggy) run of this same table showed p95 figures 1.1-2x their
own p50 (e.g. the ~64ms case: 70.757ms p50 / 145.374ms p95); after capturing each submission's own
`SubmissionIndex` and polling on exactly that (`gpu_contend::BusyKernel::submit` now returns it,
`dispatch_and_wait` waits on `PollType::Wait { submission_index: Some(index), .. }`), p50 and p95
converged to within a few percent of each other at every chunk size, confirming the earlier spread
was substantially a measurement artifact, not real driver-level tail variance. The table above is
the corrected, re-measured version.

**A second, deliberately pathological measurement — fire-and-forget, no backpressure at all —
found a real GPU device crash.** Submitting background chunks as fast as the CPU can queue them
(no wait between submissions) built an unbounded driver queue backlog (`wgpu::Device::poll(Wait)`
drains everything already queued, so foreground's own poll had to wait out the entire backlog,
which only grows since CPU submission is far faster than GPU retirement for small dispatches) —
measured p50=1948ms/p95=6260ms foreground latency at the ~1ms chunk size alone, and **at the ~4ms
and ~64ms chunk sizes, the GPU device itself crashed**: `wgpu` reported `"Error in Device::poll:
Validation Error — Caused by: Parent device is lost"`, a genuine Windows TDR (Timeout Detection and
Recovery) reset, not a bug in this harness. **This is real, load-bearing evidence for decision
rule #3, not a corner case to dismiss**: an unthrottled background worker doesn't just slow the
foreground down, it can take the entire GPU context down, foreground included. This is exactly why
the scheduler's single-chunk-in-flight discipline (`queue::Scheduler::run_next` only ever runs one
chunk per call, `gpu_contend::BackgroundLoad::start`'s throttled default) is a correctness
requirement, not tidiness — `BackgroundLoad::start_unthrottled` exists specifically to reproduce
this finding on demand, gated behind an explicit CLI flag with a printed warning, never the default.
(These unthrottled numbers predate the `SubmissionIndex` polling fix above; not re-run afterward to
avoid deliberately re-triggering a GPU device crash. The fix doesn't change this finding either
way — device loss is a real driver-level TDR reset, independent of which submission a *different*,
throttled measurement waits on.)

### Real measurement: cross-API (ort/CUDA-vs-wgpu) contention

Background load: real SCUNet-PSNR 256px tile inference via `ort`'s CUDA execution provider
(`ort_contend::TileLoad`, a trimmed copy of `spikes/rods::ai::TiledDenoiser`'s scaffolding — no
tiling/blending logic needed, only "keep the session busy"). Foreground: the same `busy.wgsl`
kernel as above, calibrated to ~0.4-0.5ms alone.

| Background load | Foreground alone | Foreground under contention (p50 / p95) | Background chunks completed |
|---|---|---|---|
| CUDA EP, SCUNet 256px | 0.441ms | 0.440ms / 0.486ms | 53 (over ~3s) |
| CPU EP, SCUNet 256px (sanity check) | 0.553ms | 0.437ms / 0.469ms | 5 (over ~3s) |

(Cross-API's foreground/background threads never share a `wgpu::Device`/`Queue` — the background
side is an entirely separate `ort`/CUDA session — so the `SubmissionIndex` race described above
never applied here in the first place; this table's numbers are unaffected by that fix.)

**Decision rule #2's finding, and the pass's most surprising result: cross-API contention is
effectively zero.** 53 real CUDA tile inferences (SCUNet-256px, ~45ms each per ADR-0040 — roughly
2.4s of actual GPU-side CUDA compute) ran concurrently with the foreground Vulkan kernel across the
measurement window, and foreground latency didn't move outside its own alone-measurement noise
band. This is the opposite of the same-API case above: Vulkan and CUDA evidently get scheduled by
the GPU's own hardware scheduler as independent contexts on this RTX 5080/driver combination, not
serialized behind one submission queue the way two `wgpu::Device` handles on the same adapter
would be (worth noting: `wgpu`'s own single-shared-`Device` design, per ADR-0016/0019/0050, is what
makes the same-API case above share one queue in the first place — this isn't a property of GPUs
in general, it's a consequence of that earlier decision). **Sanity check**: the CPU execution
provider completed only 5 tiles in the same wall-clock window the CUDA EP completed 53 in — a
~10x difference confirming the CUDA EP was genuinely active, not silently falling back to CPU
(`ort` gives no cheaper way to check this after the fact, per `spikes/rods`'s own note).

**Practical consequence**: if Tapetum's AI mask bake and SCUNet denoise stages run through `ort`'s
CUDA EP while the live render stays on `wgpu`/Vulkan (as ADR-0044 already assumes), cross-API
contention is not the constraint on foreground responsiveness — the same-API case above is. This
simplifies Pounce's own scheduling story: chunk-size discipline matters for wgpu-vs-wgpu
contention (crop/present-sample dispatches interleaved with any other wgpu work Pounce might one
day run in the background), but the CUDA-side bake stages don't need to be chunked *for this
reason* at all — though they may still need chunking for cancellation responsiveness (decision
rule #3/#4) and for VRAM admission (decision rule #5), independent of any contention concern.

### Tile-granular hero-scenario re-sim (`sim.rs`)

Extends `spikes/loaf/src/sim.rs`'s own hero-scenario bake-queue simulation (copied, not
depended-on) with per-image chunking: one atomic decode, N denoise tiles (real ADR-0040 total
split into a configurable tile duration), one atomic mask bake — plus a periodic foreground demand
that can only be serviced at a chunk boundary. Re-running with no foreground demand (`crouch sim`)
reproduces ADR-0044's own numbers exactly: `first_image_ready=53.6s`, `total_wall_time=2680s`,
`stale_at_arrival=50/50` — confirming this sim is a faithful extension, not a divergent
reimplementation.

**The real finding this adds over `loaf`'s own sim**: chunking denoise into tiles bounds *its own*
worst-case preemption latency by one tile's duration, but decode (ADR-0037: single-file LibRaw
decode, ~1.7s midpoint) and mask bake (ADR-0048's own hypothesis, ~1.0s) are **not** chunked in
this model — so the real worst-case foreground latency this sim reports is bounded by whichever
atomic stage is in flight when a foreground request becomes due, and decode dwarfs everything else
(`sim::ChunkedBakeCost::worst_case_atomic_unit` with real ADR-0037/0040/0048 figures plugged in:
1700ms, not 45ms). This is a real structural finding, not fabricated: as currently scoped, neither
#37 (decode) nor #48 (mask bake) built a streaming/tileable interface, so Pounce's own cooperative-
cancellation contract can only bound foreground latency to *one tile's* worth of delay during
denoise, and to up to *1.7 seconds* during decode. Two mitigating caveats worth stating plainly,
neither of which this sim (or `loaf`'s own) models: (1) LibRaw decode is CPU-only work — it doesn't
actually contend for the GPU queue the way a chunk-modeled dispatch would, so a real scheduler
could run it fully concurrently with GPU-side foreground rendering rather than serializing it into
the same worker timeline this sim assumes for simplicity (matching ADR-0044's own sim's existing
simplification); (2) decode/mask-bake only block foreground *if* Pounce's one-serial-worker model
is read as "one worker for everything," when in practice CPU decode, GPU bake, and GPU live-render
could be three separate lanes with their own concurrency limits (`throttle::Throttle` already
supports this for CPU/disk work). Filed as a follow-up rather than resolved here (see
Consequences).

## Options considered

| Option | Chosen? | Why |
|---|---|---|
| `tokio_util::sync::CancellationToken` + async runtime | No | The design has exactly one serial GPU/`ort` worker thread; nothing else needs an async runtime, so a plain `Arc<AtomicBool>` does the same job for free |
| `governor` for CPU/disk throttling | No | Its `RateLimiter` is cells-per-unit-time shaped, not a concurrency-limit (semaphore) shape — bending it would cost more than the dozen-line hand-rolled `Throttle` it would replace |
| `nvml-wrapper` for VRAM telemetry | No (documented fallback) | Vendor-locked to NVIDIA; DXGI is vendor-neutral and the v1 target is Windows-only anyway, so there's no cross-platform cost to being Windows-specific here that NVML would avoid |
| Fire-and-forget background submission (no backpressure) | No | Confirmed on real hardware to crash the GPU device (Windows TDR) at moderate chunk sizes — kept only as an explicitly-flagged stress test, never the scheduler's real behavior |
| Chunk decode/mask-bake to bound their own worst-case latency too | Not built this pass | Real engineering work (#37/#48 don't expose a streaming interface); flagged as a follow-up rather than solved speculatively here |

## Review findings, fixed before merge

### Hostile adversarial review (before pushing)

A hostile review of this pass's own diff (per this repo's standing review convention) found three
real issues, all fixed and covered by a regression test before this ADR's numbers were finalized:

1. **A real infinite loop**: `sim::ChunkedBakeCost::chunks()` never terminated for a zero
   `denoise_chunk` with a nonzero `denoise_total` (`remaining.min(ZERO)` never shrinks
   `remaining`) — directly reachable from `bin/crouch.rs`'s `sim --denoise-chunk-ms 0`. Fixed by
   treating a zero chunk size as one unchunked unit covering the whole total, matching
   `worst_case_atomic_unit`'s own accounting.
2. **A real measurement race in the same-API contention harness**: `dispatch_and_wait` waited on
   `wgpu::PollType::wait_indefinitely()` (the *most recent* submission at poll time, not
   necessarily this call's own), so a concurrent background submission could land in the race
   window and inflate the measured foreground latency — worse at higher percentiles. Fixed by
   capturing each submission's own `SubmissionIndex` and polling on exactly that. **This changed
   the same-API contention table's numbers materially** — see that section's own methodology note
   for the corrected, re-measured figures (the earlier p95 values were 1.1-2x their own p50; the
   corrected ones land within a few percent). Cross-API (ort/CUDA) numbers were never affected —
   that harness's two sides never share a `wgpu::Device`/`Queue`.
3. **A doc/code mismatch**: `job::JobSpec`'s own doc comment claimed VRAM was "checked by
   `admission::Admission` before a background job's next chunk is allowed to start," but
   `queue::Scheduler` never actually held or consulted an `Admission` anywhere — decision rule #5
   was validated only in `admission.rs`'s own isolated unit tests, never through the scheduler
   itself. Fixed by wiring `Admission` into `Scheduler`, with a new test exercising this through
   `Scheduler::run_next` directly. (This fix itself had a real bug — see CodeRabbit finding #2
   below, found on the very same code this fix introduced.)

### CodeRabbit review (on the open PR)

CodeRabbit's own pass over the pushed diff found four more real issues (three in code the
adversarial review above had itself just touched, one docs-only), all fixed:

1. **Docs-only, Minor**: the ADR's own Status line originally claimed rules #1-#4 were all
   "measured clean on the real reference RTX 5080," but rule #4 (`IS_EDITING`/priority
   correctness) is explicitly structural/unit-tested only, per that rule's own description —
   never itself a hardware measurement. Fixed by separating which rules are hardware-measured
   from which are structural-only in the Status line above.
2. **A real VRAM-admission leak, Minor-rated but load-bearing**: the adversarial-review fix above
   wired `Admission` into `Scheduler::run_next`, but `admit`/`release` bracket exactly one
   `step()` call on one thread — so `reserved_bytes` is always `0`, and `remaining()` always
   equals the *full* budget, at the moment `run_next` checks it. A background job whose own
   `vram_bytes` exceeds the *total* budget therefore fails that check on every single call,
   forever — `background_len()` never reaches zero, contradicting both `admission.rs`'s and this
   ADR's own "refused, not queued to wait" framing (the wired-in version had actually made it
   "queued to wait forever," the opposite of the decision rule). Fixed by dropping such a job
   outright (checked against `Admission::budget()`, the *total*, not `remaining()`, the
   *instantaneous* value) rather than leaving it to retry a check it can never pass; a job over
   only the currently-remaining budget (but under the total) is still skipped-and-requeued, since
   that case can legitimately become admittable later.
3. **A real priority-order regression, Major**: the same fix's `run_next` pushed a yielded
   background job to the *end* of the vec instead of back into the sorted slot
   `reprioritize_background` had put it in — silently degrading nearest-to-cursor-first
   (ADR-0044's own scheduling contract) into round-robin for any job needing more than one chunk
   to finish (an N-image pending set would finish the nearest image roughly N times later than
   necessary). Fixed by reinserting a yielded entry at its removed index instead of appending.
4. **Another real infinite loop, Major**: `sim::simulate_hero_bake_chunked`'s own foreground-due
   loop never terminates when `foreground_cost >= foreground_interval` (the due-time gap never
   shrinks, and `foreground_latencies` grows without bound) — directly reachable from
   `bin/crouch.rs`'s `sim --foreground-interval-ms 5 --foreground-cost-ms 5`, the same class of
   bug as adversarial-review finding #1 above, in a different loop. Fixed with an assertion
   guarding the precondition, plus a CLI-level check in `run_sim` that returns a clean error
   instead of a panic.

## Consequences

- **Unblocks #55** (build: the real scheduler + activity panel) — `spikes/crouch`'s
  `job`/`queue`/`cancel`/`admission`/`throttle`/`telemetry` modules are its starting point.
- **Shares its telemetry source with #70** (standalone bottleneck indicator) — `telemetry.rs`'s
  `HostTelemetrySource`/`VramSource`, not a second implementation.
- **Real follow-up: measure a smaller SCUNet tile size (128px) for CUDA timing.** ADR-0040 only
  measured 256px (44.8ms); this ADR's own same-API contention finding means a smaller tile is the
  likely fix if 256px turns out to matter for foreground responsiveness in a real pipeline (it
  doesn't for cross-API CUDA contention specifically, per this ADR's own finding — it would only
  matter if a background *wgpu* chunk of similar duration existed, e.g. mask-refine's `box_filter`
  passes at larger radii/resolutions than ADR-0044 measured).
  Follow-up: [#205](https://github.com/jordanfelle/nicti/issues/205).
- **Real follow-up: decode/mask-bake chunking, or explicit cross-lane concurrency.** This ADR's
  own sim shows foreground latency is currently bounded by whichever atomic (non-chunked) stage is
  running, dominated by decode's ~1.7s. Not solved here — needs either a streaming decode interface
  (#37's own scope) or confirmation that a real implementation runs CPU decode concurrently with
  GPU work rather than serializing it into one timeline, which this pass's sim (like `loaf`'s own)
  didn't model. Follow-up: [#206](https://github.com/jordanfelle/nicti/issues/206).
- **`ort` CUDA-GPU environment (Python 3.13 venv, onnxruntime-gpu 1.30.0, nvidia-cudnn-cu13
  9.26.0.51) re-installed on the reference machine this pass** — same versions #40's own research
  used, via pip/winget, no NVIDIA login. `SCUNet-PSNR.onnx` re-downloaded from
  `deepghs/image_restoration` (MIT, evaluate-only, matches `docs/licensing.md`'s existing row — not
  re-committed to the repo).
- **New topic `jobs`** added to `CLAUDE.md`'s topic list and `.claude/rules/`/`docs/decisions/`.
- **`windows` crate (0.62, MIT) added** for the DXGI VRAM query — passes `cargo deny check
  licenses` cleanly (MIT already allowlisted), no `docs/licensing.md` update needed (that file
  tracks native libraries/ML models/data files, not every permissively-licensed Rust crate).

## Spike: `spikes/crouch`

Name: the motionless crouch before a pounce — the scheduler's idle/ready state, matching the
Pounce codename's own cat-behavior naming (`CLAUDE.md`'s feline-naming convention). 37 unit tests
(structural: priority ordering, cancellation, `IS_EDITING`, VRAM admission, throttling, sim chunk
math), all passing in this sandbox (lavapipe/software GPU where GPU-dependent, real everywhere
else). Modules: `job.rs` (`ChunkedJob`/`JobSpec`/`Step`), `queue.rs` (`Scheduler`, the two-class
priority queue), `cancel.rs` (`CancelToken`/`EditingGate`), `admission.rs` (VRAM budget
reservation), `throttle.rs` (hand-rolled concurrency-limit semaphore), `telemetry.rs`
(`HostTelemetrySource`, `VramSource` + its Windows-only DXGI impl), `prefetch.rs` (copy of
ADR-0044's own `priority_order`), `gpu_contend.rs` (persistent `busy.wgsl` kernel + throttled/
unthrottled `BackgroundLoad`, the wgpu-vs-wgpu contention harness), `ort_contend.rs` (`TileLoad`,
trimmed from `spikes/rods::ai::TiledDenoiser`, the CUDA-vs-wgpu contention harness), `sim.rs`
(tile-granular hero-scenario re-sim). `src/bin/crouch.rs` exposes `bench-wgpu`/`bench-ort`/`sim`
subcommands, writing `nicti-prowl`-format reports to `bench-results/` (reusing
`nicti_prowl::perf::Protocol`, the same 1-warmup+5-measured protocol every other spike's own
reference-machine pass uses).
