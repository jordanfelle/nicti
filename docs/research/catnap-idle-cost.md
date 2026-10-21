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
3. **The idle CPU in the shipped app tracks the repaint rate, not Pounce.** Idle Nicti repaints about
   4.5 times per second. What triggers those frames is **not yet attributed**: `repaint_causes()`
   lists request sites, not triggers (see below), so the first follow-up step is to find the real
   triggers.
4. **On this runner type, idle frame rate was a stable signal; idle CPU% was not.** The frame counter
   read identically in the launches inspected (9, 13, 18, 22, 27... frames at 1 s steps), while CPU%
   on the same runs varied by 13 points. Stability across other runner types or builds is not shown.

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
4 workers, without it. The sampler is also near zero: `idle_burn` used the real Windows sources
(`default_vram_source()` = DXGI, `default_load_source()` = PDH), 16 ms / 10 s at the 500 ms tick.

The roughly 1.5 busy cores come from rendering frames, by inference only. The thread count fits (4
vCPUs gives 5 busy threads of about 3.4 s each) and the likely culprit is WARP's software-rasterizer
threads, but thread descriptions were empty so the threads could not be named, and no experiment
changed the frame rate and watched CPU move. Cost per frame, if the inference holds, is about 0.35
core-seconds (1.6 cores / 4.5 fps) on WARP. On a real GPU the same frame rate is cheap in CPU terms
but still wakes the GPU and display path; that was **not measured** (no reference-machine run).

### What requests repaints at idle (request sites, not triggers)

Run 39007820626 logged `ctx.repaint_causes()` once per second; the same four entries appeared on all
111 sampled frames:

| Call site | Delay requested |
|---|---|
| `nicti-pelt/src/app.rs:411` (telemetry `on_sample` -> `request_repaint`) | immediate, from the sampler thread every 500 ms |
| `nicti-pelt/src/cache_settings.rs:382` (`STATS_REFRESH`) | 1 s |
| `nicti-pelt/src/xmp_sync.rs:318` (`REVIEW_REFRESH`) | 2 s |
| `nicti-pelt/src/app.rs:1906` (`folder_panel::CACHE_TTL`) | 2 s |

**This table does not say what drove the 4.5 frames/s.** In egui 0.36 (measured then; not re-measured on 0.37) every `request_repaint*` call
is logged as a cause for the next pass, whether or not it is what triggers it, and the pending delay
is reset at the start of each pass (`context.rs`, `repaint_delay = Duration::MAX`). So the three timed
requests re-arm on every frame; while 500 ms telemetry frames keep arriving they probably never fire
themselves, and summing the periods (2 + 1 + 0.5 + 0.5 = 4/s) is **not** a valid model. The first
draft of this doc made that error.

Known immediate triggers that do exist: the telemetry callback (about 2/s), and every
`Pending::spawn` worker finishing (`pending.rs:19`, `ctx.request_repaint()`), which includes the
folder panel cache (one worker per `CACHE_TTL` expiry per source, `folder_panel.rs:205`) and the
filter bar's option and facet reads. Together these plausibly account for about 3/s; the remaining
~1.5/s is unexplained. Attributing it needs a trigger-side measurement (for example logging, per
frame, which wake source fired), not `repaint_causes()` alone.

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
| Whole-app idle **frame-rate** gate on hosted Windows | **Adopt, advisory first** | Reproducible on this runner type (~4.5/s in the launches inspected). Needs a tiny env-gated frame log in the product. Catches the repaint half of the class only: **it would not have caught #407**, which was a worker burn. Sensitivity is limited: at 0.35 core-s/frame on 4 vCPUs a true repaint loop saturates around 11 fps (about 2.5x baseline, not an order of magnitude), and one added 1 Hz timer is +22%. The budget needs headroom and must be re-derived after the repaint-rate fix. |
| Pool-level idle CPU test, **pool size explicit**, cross-platform | **Adopt, with a Windows positive control first** | `idle_cpu.rs` is Linux-only and reads `/proc`. `sysinfo`'s `accumulated_cpu_time()` works on the Windows runner (`idle_burn`), so the measurement can be ported. It must construct 16 workers / 8 permits (as #408 did). **Unverified:** that #407 reproduces on Windows at all, at any pool size. This pass's only Windows mutation run used 4 workers / 2 permits and saw nothing, and the mechanism (`WakingPermit::drop` notifying when workers outnumber permits) predicts it should have, so the null is unexplained (a Windows condvar difference, or the revert not taking effect in that build, are both possible). #408's own mutation proof (80 ticks vs <20) is Linux-only. The follow-up must reproduce #407 on Windows at 16/8 before relying on the Windows port. |
| Wake/iteration counter on `worker_loop`, asserted in a unit test | **Adopt (unproven)** | No wall-clock threshold: count `worker_loop` iterations that found no job over a fixed idle window and assert a bound derived from `POLL_INTERVAL` x workers. It must also build an explicit 16-worker / 8-permit pool, since a host-sized pool did not storm in the Windows run. Not prototyped; whether the counter separates a storm from the baseline at that size is untested. |
| `Context::repaint_causes()` as an investigation tool and as a test hook | **Adopt (tool); evaluate (test)** | It named all four idle causes in one run, with file:line. Works in a release build with `debug-assertions` on. Whether the headless `swat`/`egui_kittest` harness can drive simulated idle time and read causes is **not verified** here. |
| `loom` / `shuttle` on the Pounce condvar protocol | **Not pursued now (weakly reasoned)** | #407 was a cost bug, not a safety one: nothing deadlocked or raced, and the 200 ms `wait_timeout` means the pool can never hang. Model checkers are built for deadlocks and races. This is a judgment, not a result: not prototyped, and it has not been shown that a cost or liveness property cannot be modelled (the claim that neither tool models real time is from memory, unchecked). |
| Review checklist / grep rule: `notify*` inside a `Drop` reachable on the no-work path | **Adopt (cheap)** | One CONTRIBUTING line. Would have caught #407 at review. Low value alone, nearly free. |
| Per-thread CPU sampling (PowerShell) | **Keep as the investigation recipe** | What found #407. Recipe below. Thread names were not recoverable via `GetThreadDescription` on the runner, so name threads by their `std::thread::Builder` names only where a debugger or ETW is available. |
| WPR/WPA | **Defer** | Heavy; use when per-thread CPU and `repaint_causes()` do not explain it. |
| Tracy / `tracing` spans on frame and worker loops | **Defer** | Adds a dependency and a viewer for a question the counters already answer. Revisit if a regression needs timing, not counts. |

## Recommendation

1. **Per-PR, free, deterministic (do first):** the worker wake counter and the cross-platform,
   explicitly-sized idle CPU test, both in `nicti-pounce`. Add the `notify`-in-`Drop` checklist line.
2. **Attribute, then reduce, the idle repaint rate (product work), then gate it:** first find what
   actually triggers the ~4.5 frames/s (telemetry, `Pending` completions, other), then remove or
   coalesce those wakes, for example repainting on telemetry only when a displayed value changed.
   Then add the idle frame-rate probe, advisory at first (`continue-on-error`, hard timeout) for a few
   weeks to learn its variance. It would live in the **existing** `cargo build (windows, v1 target)`
   job, which already builds `nicti.exe` release: roughly +1 min (settle + one 10 s window). Cost and
   risk to weigh: that job is a **required check** and has a 12 min entry in `.github/ci-budgets.json`
   (raise it in the same PR); a GUI launch on WARP can hang or fail to create a window and would then
   block merges, so it needs its own timeout and must not be promoted to blocking until proven stable.
   A nightly job is not needed if the frame count proves stable.
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
- Thread identities of the busy GUI threads are inferred, not observed, and CPU was never tied to
  frame rate by experiment (per-thread sums also do not reconcile exactly with the process total).
- The 4.5 frames/s is not attributed to triggers (see above).
- #407 was never reproduced on Windows at any pool size; there is no Windows positive control.
- The mutation proof for the pool-level test is #408's own (80 ticks/s at 16 workers); this pass only
  showed that 4 workers do not reproduce it.
- Not prototyped: the wake counter, the cross-platform `idle_cpu` port (including a Windows 16/8
  positive control), loom/shuttle, and kittest access to `repaint_causes()`.

## Follow-ups

Filed as separate issues, each `part of #409`:

- #511: cross-platform, explicitly-sized Pounce idle CPU test, worker wake counter, review checklist line.
- #512: cut the idle repaint rate (about 4.5 frames/s, triggers unattributed).
- #513: CI idle frame-rate probe in the Windows build job (blocked by #512).
