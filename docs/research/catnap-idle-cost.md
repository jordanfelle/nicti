# Catnap: catching idle-CPU and wake-loop regressions (#409)

Feline name: a cat asleep in a sunbeam costs nothing. This doc is about asserting that an idle Nicti
does the same.

Background: #407 shipped unnoticed. An idle, empty `nicti.exe` pinned about 15 cores because Pounce's
idle CPU workers woke each other forever (`WakingPermit::drop` calling `notify_all` on the no-job
path). Every test asserted behavior; none asserted cost when there is nothing to do. #408 fixed it and
added `crates/nicti-pounce/tests/idle_cpu.rs`. This pass asks how to catch the whole class (idle burn,
wake storms, repaint loops, spinning threads) earlier.

No product code changed in this pass. Everything measured below used throwaway spike commits on
`research/409-idle-cpu`, since removed.

## Headline findings

1. **A whole-app CPU% gate on a GitHub-hosted Windows runner does not work.** The idle app burns
   34-47% of a 4-core runner with the #408 fix in place, and the same 40-43% with #408 reverted.
   The probe cannot tell a healthy build from #407.
2. **A 4-core runner cannot reproduce #407 at all.** With #408 reverted, a Pounce pool sized to the
   runner (4 CPU workers, 2 permits) burned 0 ms of CPU in 10 s. The livelock needs a wide pool (#408
   measured 80 ticks/s with 16 workers). Any pool-level test must size the pool explicitly instead of
   inheriting the host's core count.
3. **The real idle cost in the shipped app is the repaint rate, not Pounce.** Idle Nicti repaints about
   4.5 times per second, from four independent timers that are out of phase. Attributed directly with
   egui's `Context::repaint_causes()` (below).
4. **Idle frame rate is a stable, deterministic signal; idle CPU% is not.** The frame counter read
   identically across runs (9, 13, 18, 22, 27... frames at 1 s steps, every run). CPU% on the same
   runs varied by 13 points.

## Method

All runs: `windows-latest` (4 vCPU, no GPU; wgpu falls back to the WARP software adapter), `cargo build
--release -p nicti`, launch `nicti.exe <fresh empty catalog>`, settle 15 s, then three 10 s sampling
windows per launch, three launches per run. CPU from `Process.TotalProcessorTime` and per-thread
`Threads[].TotalProcessorTime` deltas. Runs are on `jordanfelle/nicti` Actions, branch
`research/409-idle-cpu`.

| Run | What it added | Result |
|---|---|---|
| 39002730111 | per-thread CPU probe | GUI starts fine on the hosted runner; 37-40% of machine (first launch only, a script bug in the cleanup step ended the rest) |
| 39003639710 | per-launch catalog, thread names | 3 launches x 3 windows: 40-47%. 5-7 busy threads, each 3.0-3.9 s CPU per 10 s. Thread names came back empty |
| 39004531203 | `idle_burn` example: Pounce alone, sampler alone (process CPU via `sysinfo`) | pool only 0 ms / 10 s; pool + telemetry sampler 16 ms / 10 s; bare process 0 ms. GUI probe still 40-45% |
| 39005656890 | frame counter in `PeltApp::ui` | steady ~4.5 frames/s idle, identical across launches; GUI probe 34-45% |
| 39006742160 | **mutation**: `git revert fb6b4a1` (#408) | GUI probe 40-43% (no change from baseline); `idle_burn` pool only 0 ms / 10 s |
| 39007820626 | `repaint_causes()` logged per second (release build with `debug-assertions` on) | the same four causes on all 111 sampled frames (table below) |

Linux cross-check on the dev host (24 threads): `idle_burn` pool only 10 ms / 10 s, pool + sampler
20 ms / 10 s.

Run time on the hosted runner: 10-15 min per run, almost all of it a cold `cargo build --release`
(the spike set `save-if: false`). The probe itself is about 3 min of wall clock for three launches;
one launch is about 1 min.

## Where the idle CPU goes

Pounce is cleared (rows 3 and 5 above): at idle it costs about 0 on Windows, both with the fix and, at
4 workers, without it. The sampler is also near zero. The roughly 1.5 busy cores come from rendering
frames, and the thread count fits: 4 vCPUs gives 5 busy threads of about 3.4 s each. Inference, not
confirmed: those are WARP's software-rasterizer threads (thread descriptions were empty, so they could
not be named). Cost per frame is about 0.35 core-seconds (1.6 cores / 4.5 fps) on WARP. On a real GPU
the same frame rate is cheap in CPU terms but still wakes the GPU and the display path; that was
**not measured** here (no reference-machine run in this pass).

Why 4.5 fps: egui's `repaint_causes()` at idle, the same four on every sampled frame:

| Call site | Period | Why it is there |
|---|---|---|
| `nicti-pelt/src/app.rs:411` (telemetry `on_sample` -> `request_repaint`) | 500 ms | live CPU/GPU/VRAM readout in the always-shown activity panel |
| `nicti-pelt/src/cache_settings.rs:382` (`STATS_REFRESH`) | 1 s | cache stats; drawn inline in the Library side panel, so always live |
| `nicti-pelt/src/xmp_sync.rs:318` (`REVIEW_REFRESH`) | 2 s | sidecar review count; same panel |
| `nicti-pelt/src/app.rs:1922` (`folder_panel::CACHE_TTL`) | 2 s | folder cache re-read cadence |

Each is a separate `request_repaint_after` re-armed every frame, so the frames land out of phase:
the sum of the rates (2 + 1 + 0.5 + 0.5 = 4/s, plus jitter) is what we see, not the minimum.

Other idle wake sources audited (no spike needed):

- Pounce workers: bounded `wait_timeout(POLL_INTERVAL = 200 ms)` in `runtime.rs` (about 5 wakes/s per
  worker, deliberate belt-and-braces). Measured cost about 0.
- `request_repaint_after` sites gated on busy state, so zero at idle: `app.rs` grid live reload,
  export progress, update check, prebake, eyeshine, facet refresh, develop hint, loupe T0.
- Background threads parked on a blocking `recv()` (cull writer, `Pending`, folder dialog): zero idle
  cost. This is the pattern to copy.

## Options evaluated

| Option | Verdict | Why |
|---|---|---|
| Whole-app idle CPU% gate on hosted Windows | **Reject** | Baseline is 34-47% on WARP and varies 13 points; #407 reverted was indistinguishable. Would need a threshold above the noise that a real regression can only cross on a much wider machine. |
| Whole-app idle **frame-rate** gate on hosted Windows | **Adopt** | Deterministic (~4.5/s every launch). A repaint loop shows up as an order-of-magnitude jump regardless of runner CPU. Needs a tiny env-gated frame log in the product. Catches the repaint half of the class (not worker burn). |
| Pool-level idle CPU test, **pool size explicit**, cross-platform | **Adopt** | `idle_cpu.rs` is Linux-only and reads `/proc`. `sysinfo`'s `accumulated_cpu_time()` worked on the Windows runner (`idle_burn`), so the same test can run in `cargo test (windows)`. It must construct 16 workers/8 permits (as #408 did), because the runner's own core count cannot trigger the bug. Mutation proof was done in #408 (80 ticks vs <20). |
| Wake/iteration counter on `worker_loop`, asserted in a unit test | **Adopt** | No wall clock, no flake: count `worker_loop` iterations that found no job over a fixed idle window and assert a bound derived from `POLL_INTERVAL` x workers. Catches a wake storm on any machine, including a 2-core dev laptop. |
| `Context::repaint_causes()` as an investigation tool and as a test hook | **Adopt (tool); evaluate (test)** | It named all four idle causes in one run, with file:line. Works in a release build with `debug-assertions` on. Whether the headless `swat`/`egui_kittest` harness can drive simulated idle time and read causes is **not verified** here. |
| `loom` / `shuttle` on the Pounce condvar protocol | **Do not pursue now** | #407 was a liveness/cost bug, not a safety one: nothing deadlocked or raced, and the 200 ms `wait_timeout` means the pool can never hang. Model checkers target deadlocks and races. Also unverified (not prototyped): neither models real time, so a `wait_timeout` protocol is awkward to express. |
| Review checklist / grep rule: `notify*` inside a `Drop` reachable on the no-work path | **Adopt (cheap)** | One CONTRIBUTING line. Would have caught #407 at review. Low value alone, nearly free. |
| Per-thread CPU sampling (PowerShell) | **Keep as the investigation recipe** | What found #407. Recipe below. Thread names were not recoverable via `GetThreadDescription` on the runner, so name threads by their `std::thread::Builder` names only where a debugger or ETW is available. |
| WPR/WPA | **Defer** | Heavy; use when per-thread CPU and `repaint_causes()` do not explain it. |
| Tracy / `tracing` spans on frame and worker loops | **Defer** | Adds a dependency and a viewer for a question the counters already answer. Revisit if a regression needs timing, not counts. |

## Recommendation

1. **Per-PR, free, deterministic (do first):** the worker wake counter and the cross-platform,
   explicitly-sized idle CPU test, both in `nicti-pounce`. Add the `notify`-in-`Drop` checklist line.
2. **Reduce the idle repaint rate (product fix), then gate it:** coalesce the four timers onto one
   aligned UI heartbeat and repaint on telemetry only when a displayed value changed. Target about 1
   frame/s idle. Then add the idle frame-rate probe to the **existing** `cargo build (windows, v1
   target)` job: it already builds `nicti.exe` release, so the probe adds roughly 1 min (settle +
   one 10 s window), against a ~10 min job. That is a per-PR cost with no new runner. A nightly or
   scheduled job is not needed: the signal is deterministic.
3. **Measure on the reference machine:** idle frame cost on a real GPU, before and after item 2. This
   pass could only measure WARP.

Do not add an idle-CPU% gate for the whole app.

## Investigation recipe (what worked)

```powershell
# Per-thread CPU over a window: which threads are burning?
$p.Refresh(); $b = @{}; foreach ($t in $p.Threads) { $b[$t.Id] = $t.TotalProcessorTime.TotalMilliseconds }
Start-Sleep 10; $p.Refresh()
$p.Threads | % { [pscustomobject]@{ Id = $_.Id; Ms = $_.TotalProcessorTime.TotalMilliseconds - $b[$_.Id] } } |
  Sort-Object Ms -Descending | Select-Object -First 10
```

And for repaint loops, log `ctx.repaint_causes()` once per second from `App::ui` in a build with
`debug-assertions` on (`CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS=true`). It prints the `file:line` of
every `request_repaint*` call that drove the frame.

## Limits of this pass

- One runner type (`windows-latest`, 4 vCPU, WARP). No real-GPU or many-core number except the Linux
  dev-host cross-check.
- Thread identities of the busy GUI threads are inferred, not observed.
- The mutation proof for the pool-level test is #408's own (80 ticks/s at 16 workers); this pass only
  showed that 4 workers do not reproduce it.
- Not prototyped: the wake counter, the cross-platform `idle_cpu` port, loom/shuttle, and kittest
  access to `repaint_causes()`.

## Follow-ups

Filed as separate issues, each `part of #409`:

- #511: cross-platform, explicitly-sized Pounce idle CPU test, worker wake counter, review checklist line.
- #512: cut the idle repaint rate (four out-of-phase timers, about 4.5 frames/s).
- #513: CI idle frame-rate probe in the Windows build job (blocked by #512).
