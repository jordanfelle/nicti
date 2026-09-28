# ADR-0070: Hardware bottleneck telemetry indicator

- **Status:** Proposed — the classifier and PDH sources compile and pass their own (platform-
  independent) unit tests, and the Windows-only PDH FFI cross-compiles cleanly to
  `x86_64-pc-windows-gnu`, but nothing here has run yet against the real reference-machine GPU/PDH
  counters (this sandbox has neither a GPU adapter nor a live Windows PDH service under WSL — same
  caveat ADR-0054's own `telemetry.rs` doc comment already states for DXGI). See "Reference-machine
  checklist" below for what promotes this to Accepted.
- **Date:** 2026-09-28
- **Ticket:** [#70](https://github.com/jordanfelle/nicti/issues/70) Build: hardware bottleneck
  telemetry indicator

## Context

#70 asks for a live, color-coded UI indicator showing whether CPU, GPU, or disk I/O is the current
limiting factor during a batch operation, sharing its telemetry source with Pounce's own VRAM
admission control (ADR-0054). It was filed 2026-09-13, before #54/#55 landed, so its own body names
`nvml-wrapper` for "GPU/VRAM/CUDA-core/PCIe telemetry" and treats the activity panel (#55) as
future work. Both are stale by the time this ticket was picked up (2026-09-28):

- **Telemetry already exists.** `crates/nicti-pounce/src/telemetry.rs` (ADR-0054) has
  `TelemetrySampler`: `sysinfo` for CPU/RAM, DXGI for VRAM. `crates/nicti-pelt/src/activity.rs`
  (#55, landed) already shows those readings in the bottom status bar. This ticket extends that
  panel; it isn't a new spike.
- **ADR-0054 already rejected `nvml-wrapper`** for VRAM, specifically for vendor neutrality — DXGI
  works on any GPU vendor on Windows (the v1 target, ADR-0015), `nvml-wrapper` only works on
  NVIDIA. Reusing `nvml-wrapper` here for GPU-busy% would silently reintroduce the exact lock-in
  that decision avoided.
- **What's actually missing** is GPU-busy% (DXGI's `QueryVideoMemoryInfo` only reports VRAM, not
  utilization) and disk-busy% (not sampled at all yet), plus a classifier turning readings into
  "which resource is the limit," plus a way for the UI to notice load Pounce itself didn't start
  (e.g. the Develop view's continuous render) — the app currently only repaints on input or a
  Pounce job-state change.

**User decision (2026-09-28):** GPU-busy% comes from the Windows PDH ("Performance Data Helper")
`\GPU Engine(*)\Utilization Percentage` counter rather than `nvml-wrapper` — vendor-neutral like
DXGI (any WDDM driver populates it), matching ADR-0054's own reasoning rather than reopening it.

## Decision

1. **Disk-busy% also comes from PDH, not `sysinfo`.** `sysinfo` only reports cumulative bytes
   read/written, which can't answer "is the disk saturated" without knowing the device's own
   maximum throughput. PDH's `\PhysicalDisk(*)\% Idle Time` gives busy% = 100 − idle% directly, and
   PDH is already being added for the GPU counter. The reading is the busiest physical disk,
   excluding the `_Total` instance (which averages across disks and would hide one saturated drive
   behind several idle ones).

2. **GPU-busy% aggregation matches Task Manager's own GPU graph:**
   - Counter added via `PdhAddEnglishCounterW` (not the localized `PdhAddCounterW`) so the path
     works on non-English Windows installs too.
   - Instance names are parsed as `pid_<pid>_luid_0x<high>_0x<low>_phys_<phys>_eng_<eng>_engtype_<type>`
     (`telemetry::pdh::parse_engine_instance`).
   - Only instances whose LUID matches the same adapter `DxgiVramSource` already queries
     (`EnumAdapters1(0)`, via `IDXGIAdapter1::GetDesc1`) count — so GPU-busy% and VRAM always
     describe the same physical GPU, and a machine with an integrated GPU alongside the dedicated
     one doesn't get its reading diluted by the wrong adapter's idle engines.
   - Per-`(phys, eng)` values are summed across processes (multiple processes can drive the same
     engine concurrently), then the busiest engine on the target adapter wins
     (`telemetry::pdh::aggregate_engines`) — a batch saturating the 3D/Compute engine while Copy
     sits idle is still 100% GPU-bound, not diluted by averaging across engines.

3. **Sampling moved to a background thread** (`TelemetrySampler::spawn`, replacing the old
   `TelemetrySampler::new` + per-frame `sample()`). Enumerating the GPU Engine wildcard can return
   many instances and take real time, and `DxgiVramSource::query` already creates a fresh DXGI
   factory on every call — neither belongs on the UI thread inside `activity::show`, which used to
   call `sample()` directly once per frame. The sampler now samples at most once per
   `min_interval` on its own thread and calls an `on_sample` callback afterward; `PeltApp` passes
   `egui_ctx.request_repaint()`, the same pattern `Pounce`'s own `on_change` callback already uses.
   `sample()` is now a non-blocking read of the latest value (`Option<Sample>`, `None` until the
   first background sample lands, never blocking on the query itself).

4. **The classifier (`nicti_pounce::hackles`) is a pure, UI-agnostic module**, not part of
   `activity.rs`, so it's unit-testable without a reference machine:
   - `Level` (`Calm`/`Busy`/`Saturated`) per resource, thresholds at 60% and 85% (named consts,
     `BUSY_THRESHOLD`/`SATURATED_THRESHOLD` — tunable once real reference-machine numbers exist).
   - `Limit` is the busiest resource at or above `Busy`, or `Idle` if none qualify. There is no
     `Unknown` variant: CPU telemetry never fails (`sysinfo` always answers), so `classify` can
     always name a real verdict; "no sample has landed yet at all" is represented one level up, as
     `Option<Sample>`/`Option<Verdict>`, not inside the classifier.
   - **Hysteresis:** the current limit keeps the title until a challenger beats its reading by more
     than `HYSTERESIS_MARGIN` (5 points) — without this, two resources sitting within a point of
     each other would flip the headline every sample. This debounce only applies between two
     already-busy candidates; entering or leaving `Idle` is never debounced, since that transition
     is real and worth showing immediately.

5. **VRAM stays a display-only readout and doesn't feed the classifier.** Being close to the VRAM
   budget isn't the same as being GPU-limited right now — `admission.rs` already has its own
   skip-vs-queue signal for that, which can feed a future refinement if needed.

## Known limitations (documented, not solved here)

- **CPU reading is the whole-machine average** (`sysinfo::System::global_cpu_usage()`), so it won't
  surface a single-threaded stall or a batch capped by Pounce's own `Throttle` concurrency limit
  rather than genuine CPU saturation. A later improvement could compare the CPU lane's running
  count against `Pounce::cpu_limit()`.
- **Disk-busy% is the busiest disk overall**, not specifically whichever disk holds the catalog's
  own roots.
- **Unverified assumption:** that PDH's `\GPU Engine(*)` wildcard picks up a newly-started
  process's instances on the very next `PdhCollectQueryData` call, with no extra
  `PdhAddEnglishCounterW`/re-open needed. This is a reference-machine checklist item, not yet
  confirmed.
- **Found by adversarial review, unverified without real hardware** (none of these can be
  reproduced in this sandbox, so none are fixed speculatively -- see the checklist below):
  - `parse_engine_instance`'s prefix/segment matching assumes PDH's real `\GPU Engine(*)` instance
    names never carry a duplicate-disambiguation `#N` suffix landing *before* the `engtype`
    segment is fully consumed (a suffix after `engtype` is harmless, since `EngineKey` only
    depends on `phys`/`eng`), and assumes lowercase hex/prefix casing throughout. Either mismatch
    fails closed -- the instance is silently dropped, never fabricated -- but could undercount a
    real reading or make GPU-busy% permanently `None` on some real machine's actual naming
    convention.
  - `\PhysicalDisk(*)\% Idle Time` can be administratively disabled (historically via `diskperf`);
    if so, disk-busy% would correctly report `None` forever with no visible cause.
  - `PdhLoadSource::new()` (`PdhOpenQueryW`/`PdhAddEnglishCounterW`) runs on whichever thread calls
    `default_load_source()` -- today, the UI thread, before the resulting value moves once into
    `TelemetrySampler`'s background thread, where every later `query()` call runs. PDH has no
    documented apartment/thread-affinity model for query handles, but that's an absence-of-
    evidence argument, not a confirmed one -- see `telemetry/pdh.rs`'s own `unsafe impl Send`
    comment.
  - `DxgiVramSource`/`query_adapter0_luid` call `CreateDXGIFactory1` from the background thread now
    (previously the UI thread, once per frame) with no explicit `CoInitializeEx` -- widely believed
    unnecessary for DXGI factory creation, but this exact call site is newly exercised off the UI
    thread by this ticket.

## Reference-machine checklist (promotes this ADR to Accepted)

1. Cross-compile (`x86_64-pc-windows-gnu`, rustup-managed toolchain) and run via WSL interop, per
   the `gpu-gui-and-healing` REFERENCE.md's existing pattern.
2. **Disk load:** Library → Import a large folder (Scruff's `IngestJob`, CPU lane). Expect a Disk
   or CPU verdict depending on the drive.
3. **GPU load:** drive the GPU via `spikes/crouch`'s `bench-wgpu` or the Develop view's continuous
   render (no real bake job exists yet). Expect a GPU verdict, and confirm the reported % roughly
   tracks Task Manager's own GPU graph for the RTX 5080 specifically (not the 9950X's integrated
   GPU).
4. **Idle:** confirm the panel settles on `Idle` with no jobs running, and that the readout keeps
   updating with no mouse movement (proving the `on_sample`/`request_repaint` wiring, not just
   egui's own input-driven repaint).
5. Record the three runs' actual numbers here, and re-tune `BUSY_THRESHOLD`/`SATURATED_THRESHOLD`/
   `HYSTERESIS_MARGIN` against them if the defaults look wrong in practice.
6. Confirm the real `\GPU Engine(*)` instance-name shape on this machine matches
   `parse_engine_instance`'s assumptions (no PDH-internal `#N` disambiguation before `engtype`, all
   lowercase) -- if GPU-busy% comes back `None` even under real GPU load, this parsing mismatch is
   the first thing to check.
7. Confirm `\PhysicalDisk(*)\% Idle Time` actually returns instances (not disabled via `diskperf`)
   -- if disk-busy% is permanently `None` even under real disk load, check this before assuming a
   parsing bug.
8. Confirm no PDH thread-affinity issue in practice: `PdhLoadSource::new()` runs on the UI thread,
   `query()` runs on the background thread -- if PDH calls fail or silently return no data only in
   the real app (not in an isolated same-thread test), this cross-thread handle pattern is the
   suspect.

## Options considered

- **`nvml-wrapper`** (the ticket's own original suggestion) — rejected for the same vendor-lock-in
  reason ADR-0054 rejected it for VRAM; the v1 target being Windows-only doesn't change this, since
  DXGI/PDH already cover any Windows GPU vendor while NVML only covers one.
- **`sysinfo` for disk-busy%** — rejected; it exposes cumulative byte counters, not a busy/idle
  ratio, and computing one would need a per-device max-throughput baseline this project doesn't
  have. PDH's `% Idle Time` counter gives the ratio directly.
- **Polling telemetry on the UI thread** (the pre-#70 status quo) — rejected once GPU Engine
  wildcard enumeration was added; acceptable for CPU/RAM/VRAM alone, not for a query that can touch
  many instances per frame.
