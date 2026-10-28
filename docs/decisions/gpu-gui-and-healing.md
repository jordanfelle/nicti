## GPU, GUI, and healing

Covers the GPU compute API choice, the GUI framework decision, and the healing/removal design.

- **GPU compute API**: `docs/adr/0016-gpu-compute-api.md` — `wgpu` (WGSL), Vulkan backend on
  Windows (not Dx12 — Dx12 doesn't expose `SHADER_F16` on wgpu 30/current driver, Vulkan does, and
  Tapetum's cache tiers need f16). Measured on the reference RTX 5080 in `spikes/glint/`: live-stage
  chain at 4K and 45MP both clear the decision rule (within 2x of an equivalent CUDA kernel, one
  wgpu backend actually faster); `max_buffer_size`/`max_storage_buffer_binding_size` clear the 45MP
  RGBA16F (~360MB) hero-frame size by 5–10x on real hardware. Found and fixed a real
  dispatch-dimensioning bug along the way: a naive 1D dispatch overflows wgpu's 65535-per-dimension
  workgroup limit at hero-scenario resolution — any future wgpu compute-stage code must dispatch as
  a 2D grid (`gpu.rs::workgroup_grid`), not assume 1D is safe. Never do a full-frame host↔device
  round-trip in the hot path (confirmed expensive, 0.8–1.5s, by the spike's own harness) — baked
  stage output must stay GPU-resident, per ADR-0021/#44. Unblocks #20, #41, #45; feeds #68 (GUI
  framework)'s wgpu-interop question.
- **GUI framework**: `docs/adr/0068-gui-framework.md` — **Accepted: egui** (2026-09-27, #90), on
  the hard-gate findings alone; the planned reference-machine measurement pass was waived rather
  than run (see the ADR's Amendments section for the reasoning). **Correction, 2026-09-26** (#69's
  prior-art research): the Prior-art section had wrongly claimed RapidRAW uses egui/eframe — it's
  actually Tauri + React (see the ADR's own Amendments section). Doesn't change this ADR's
  Decision, which rests on egui's own wgpu-30 match, license, and `CallbackTrait` maturity,
  independent of that citation. GPUI is eliminated outright: its Windows
  backend is a bespoke Direct3D11 renderer (`windows-rs`), with no `wgpu`/Vulkan path in its own
  dependency graph on that platform at all (`blade-graphics` is Linux/macOS-only) — no
  `spikes/pelt-gpui` was built. Of the other three, egui (via eframe) wins: the only
  candidate whose own `wgpu` dependency (30.0.0) matches ADR-0016's choice exactly, with a clean
  MIT/Apache-2.0 license and a mature `egui_wgpu::CallbackTrait` custom-viewport story. Iced works
  but pins `wgpu` 27, not 30 (a real version-compatibility cost). Slint's GPU-resident
  `Image::try_from(wgpu::Texture)` integration is the cleanest of the three mechanically, but its
  own license (`GPL-3.0-only OR LicenseRef-Slint-*`) only passes today under a spike-scoped
  `deny.toml` exception — shipping it needs its own ADR-0018 amendment. Neither Iced nor Slint has
  egui's/GPUI's built-in virtualized-list primitive, so both had to hand-roll grid-windowing math
  (`spikes/pelt/src/virtualize.rs`) for #68's grid gate. `spikes/pelt*`/`bench/pelt/` and their CI
  jobs are now dead weight, tracked for deletion in #232; real performance validation against
  this ADR's measured-gate targets moves to #233, against the actual UI crate via #43's
  hero-scenario tooling, once it exists.
- **Production UI crate, `crates/nicti-pelt` (#241, landed)**: the app shell ADR-0068 points to.
  Named `nicti-pelt`, not the ticket's own `nicti-ui` filing name, to match the feline naming
  convention every other crate follows — reusing "pelt" from the now-superseded research spikes
  above. Device sharing: `nicti-pelt::run` passes eframe's `WgpuSetup::CreateNew` a
  `device_descriptor` closure set to the newly-factored-out
  `nicti_tapetum::gpu::device_descriptor_for` (the same adapter-limits/optional-features
  descriptor `GpuContext::new` itself requests, rather than eframe's own conservative
  `wgpu::Limits::default()`), then `PeltApp::new` wraps `cc.wgpu_render_state`'s resulting
  adapter/device/queue with a new `GpuContext::from_device` constructor — one real device shared
  between egui's render pass and every Tapetum compute dispatch, exactly this section's own "one
  shared device" rule. Displaying a `FrameTexture` (linear ProPhoto RGB, `Rgba16Float`) into an
  8-bit surface needed its own small fragment shader (`nicti-pelt/shaders/display.wgsl`,
  `viewport.rs`'s `ViewportCallback`), applying the same
  calico's `OutputSpace::Srgb.from_working_f32()`/sRGB-OETF pair (#318: the same matrix
  `geometry::output_encode`'s CPU reference and export use) — skipping the OETF when the render target itself is an `*Srgb` format,
  since the hardware already applies it on write then (applying it twice would double-gamma the
  image).
- **Healing/removal**: `docs/adr/0050-healing-and-removal.md` — **Accepted** (2026-09-26, #97's
  reference-machine pass). Ships both classic clone/heal (CPU Poisson-Jacobi solve + a `wgpu`
  compute-shader twin, proven correct against each other in `spikes/groom/`) and AI removal
  (MobileSAM+LaMa via `ort`/`load-dynamic`, per ADR-0019 §3's already-decided pattern) as two
  `SpotKind` variants of one `HealStage`, not competing alternatives. Its model loading must
  follow `docs/adr/0218-local-only-ai.md`: offline inference/training by default, no telemetry, no
  hosted API; weight downloads are explicit, user-initiated, and checksum-verified, never a silent
  auto-fetch. No real ONNX weights exist
  yet — the AI-removal wrappers prove the loading/error-handling shape only, latency/quality still
  TBD pending #51's real checkpoints. Re-verified LaMa's Places2 training-data flag (still
  unresolved — the primary source stays unreachable, a mirror confirms Places2's own
  non-commercial/no-redistribution terms) and researched MI-GAN as an alternative, which turned
  out **not** to be cleaner (same Places2 exposure, plus its own unresolved
  weights-license-legitimacy question) — see `docs/research/groom-healing-removal.md`. Measured
  CPU timings (clone_stamp 0.12ms/op, spot_heal 0.25ms/op, auto_source_pick 0.05ms/op) and
  `HealStage` serialized sizes (120/1,511/7,531 bytes at 1/10/50 spots). **#97 measured the GPU
  Poisson-solve on the real RTX 5080/Windows box: 0.386ms p50 (Vulkan) against the <16ms/update
  target** — ~40x headroom, clearing the last open speed question this ADR had (AI-removal
  latency stays gated on #51's weights, not on hardware access). Proposes (not commits)
  heal/remove's stage-order placement for #44: after lens correction, before global tone, in
  linear space.
- **Healing/removal, the build (#51)**: `docs/adr/0051-healing-removal-build.md` — **Accepted**
  (2026-09-29). `spikes/groom` is deleted and promoted: classic clone/heal became the real baked
  `nicti.heal` stage in `nicti-tapetum::heal` (GPU passes on `Rgba16Float` textures, GPU-vs-CPU
  parity-tested; **on the RTX 5080 (noisy shared GPU) 1 heal spot ≈ 1-6 ms up to r=100, r=300 ≈
  8-28 ms, 10 heals ≈ 7-18 ms end to end** — large or numerous heals reach the 16 ms budget. The
  adversarial review caught that the first Jacobi solve started from the destination and so left a
  blemish in place; it now starts from source + mean boundary offset), and AI removal became `nicti-groom` (MobileSAM + LaMa against their **real**
  tensor contracts, read from the ONNX files) producing a `RemovalPatch` the GPU blends in. A
  finished patch reaches the render graph by being *stamped into the heal params the render sees*
  (`heal::stamp_removal_state`), so it invalidates through the normal cache-key path. Models are
  fetched only by an explicit click through `nicti-stalk::models` (pinned URL + size + SHA-256,
  atomic, bounded, cancellable, per ADR-0218). **LaMa's Places2 flag was resolved by owner
  sign-off for on-demand download only, never bundled — an accepted risk, not a legal conclusion.**
  ONNX Runtime ships as the official CPU build (DirectML wasn't a single pinnable asset), so AI
  removal measures ~3-4 s and **misses ADR-0050's <2 s target** (which assumed CUDA); quality was
  verified on synthetic scenes only, never real photos. Deferred: GPU execution provider,
  real-photo evaluation, undo/persistence (Develop-wide), LRC `RetouchInfo` import (#62).
- **Headless UI harness (#426)** -- `crates/nicti-pelt/src/swat/`: `egui_kittest` (AccessKit queries, input
  injection) with a ported LightCraft CPU rasteriser (`softpaint.rs`) as its `TestRenderer`, so view tests and
  PNG snapshots run with no wgpu adapter (verified with `VK_ICD_FILENAMES=/nonexistent
  NICTI_WGPU_BACKEND=vulkan`, where the existing GPU tests skip and `swat` still runs). Paint callbacks (the
  wgpu viewport) are not drawn, so anything needing render pixels stays a `test_gpu::shared()` test. To be
  findable a custom-painted widget needs `response.widget_info(..)` (grid cells have it). `DevelopView` is split
  into `DevelopDoc` (GPU-free) + `DevelopEngine`; UI that only edits params takes `&mut DevelopDoc`.
  **Gotcha**: kittest runs one frame inside `build`, before any context setup, and fonts/styles apply a frame
  later -- `swat::harness` therefore installs the theme on a discarded first frame (the app's panels panic on
  the missing `semibold` family otherwise).

- **Agent control channel (#434)**: `docs/adr/0434-agent-control-channel.md` (Proposed; go, phased, nothing built). A
  pelt-local command table (`id`, `params` doc, `risk`, `enabled` reason, `run`) wrapping existing `PeltApp` methods; a
  loopback JSON-lines channel run on the UI thread between frames, opt-in (`--control`), with a random 0600 token file, a
  1 MiB line cap, 4 connections and a `Destructive` risk gate; a widget-rect registry (not AccessKit) for clicks; screenshots
  via `ViewportCommand::Screenshot`; a separate stdio MCP shim. Rebindable shortcuts need a separate migration of ~40 inline
  key sites and are out of scope.

## #437: justified grid layout and GridStats (ADR-0437)

Research outcome from the LightCraft audit (#424). The assertion pattern was adopted: `GridStats { frames,
cells_built }` on `ViewState`, and a swat test that seeds 5000 photos and asserts fewer than 30 cells built per frame
at the top and the end (failing with "built 5000 of 5000" when the visible-row range is replaced by every row).
Justified rows and date headers are deferred: LightCraft's layout is O(n) over all aspects (a per-resize pass over 1M
photos, #335), Nicti's `row_of`/`indices_for_rows`/`navigate` and `show_rows` assume fixed columns,
and whether catalog `width`/`height` are orientation-corrected is unverified. If wanted: a chunked, Pounce-built
row-offset index keyed on (generation, width bucket, target height), `partition_point` for the visible rows, and a
`layout_builds` counter asserted flat across unchanged frames.

2026-10-28 (#550): decided go for justified rows AND date-group headers, and the pure core was built:
`nicti-pelt/src/grid/justified.rs`. `RowIndex` is an append-only table (first cell u32, top **f64**, height f32, header
id u32 per row, about 20 bytes per row, asserted under 4 MB at 1M photos; `f32` tops drift by thousands of points at 2M
photos, a test pins them exact) filled by a streaming builder
(`begin_group`, `push_cells`, `finish`) whose result does not depend on the chunking; queries are `partition_point`
(`row_at_y`, `rows_between`, `row_of`, `indices_for_rows`), `row_cells` positions only one row's cells, and
`navigate`/`navigate_page` pick the cell nearest the current horizontal centre in the target row. Rows are filled to the
width and stretched at most 1.5x the target height; a group's last row keeps the target; a lone wide photo shrinks to
fit; non-finite or non-positive aspects are square and the rest clamped to 0.1..10. `LayoutGate` keys a build on
(generation, width bucket of 16 px, target height, headers on) and counts them (`layout_builds`). **Catalog
`width`/`height` are not orientation-corrected**: a RAW asset's size is its embedded preview's declared size and a
JPEG asset's is the file's own size, both before EXIF rotation, so `oriented_aspect` swaps the axes for
orientations 5-8 and the catalog needs an orientation column and a backfill (ingest skips unchanged files). All of that
and the view wiring (build job, `show_viewport` loop, toggle, swat assertion, 1M measurement) is #604.

## #512: idle repaint rate

An idle, empty Nicti repainted about 4.5 times a second (#409). `NICTI_REPAINT_TRACE=1` now prints, once a second to
stderr, how many frames ran and which immediate-repaint source asked for them (`repaint_trace.rs`: telemetry, `Pending`
workers, Pounce, cull; a frame none of them asked for is `unattributed`, with its gap since the previous frame and the
`ctx.repaint_causes()` seen on it). egui multi-pass relayout was ruled out (`relayout=0`). Measured on `windows-latest`
(WARP, release build, empty catalog, 15 s settle, 25-30 one-second samples), before: 4.50 frames/s = telemetry 2.00 +
`Pending` 0.50 + unattributed 2.00. Changes: the telemetry sampler reports only a *visible* change (the readout text
lives on `Sample` so the gate cannot drift from what is drawn; a reading at or above the busy threshold repaints on any
movement because `hackles::classify` uses raw values) and at most once a second; folder-panel `Source` re-reads wake the
UI only when the value changed or an invalidate was raised (the `CACHE_TTL` timer polls an unchanged result);
the idle cache-stats refresh went 1 s -> 5 s. After the first three: 3.00 frames/s (telemetry 1.00 + one follow-up frame
1-2 ms after each wake + a 1 s timer frame); with the fourth: **2.17 frames/s** (telemetry 1.00 + 1.17 unattributed,
mostly that follow-up frame), a 52% cut from 4.50. The issue's ~1 frame/s target is not reached. **Not
attributed**: why every wake is followed 1-2 ms later by a second frame whose only `repaint_causes()` are the delayed
`request_repaint_after` sites (not multi-pass; eframe's `RequestRepaint` handling looked sane). **Not measured**: the
runner's CPU/disk readouts jitter (the app's own software rendering), so the telemetry gate suppressed little there; a
quiet real machine should save more, and idle frame cost on a real GPU (the issue's step 4) needs the reference machine.

## #513: idle frame-rate probe in CI

`scripts/idle-probe.ps1` runs in `cargo build (windows, v1 target)` after the release build: it launches `nicti.exe`
on a fresh empty catalog with `NICTI_REPAINT_TRACE=1` and computes sum(frames) / sum(`elapsed_ms`) over the trace lines
past a 15 s settle (about 10 s of window), failing above 6 frames/s (baseline 2.17; a true repaint loop saturates near
11 on 4 vCPUs). The trace line gained `elapsed_ms` for this: lines print from the UI pass, so an idle app prints less
often than once a second and a line count is not a clock. On failure it prints the tail of the trace, whose `cause:`
entries name the `repaint_causes()` call sites. It catches repaint loops only, not a worker burn (#407). The step is
advisory (`continue-on-error`, 3 min timeout) until its variance on WARP is known; the job's `ci-budgets.json` entry
went 12 -> 13 min for it.
Known limit: the trace prints from the UI pass, so a hang and a fully idle app both produce no lines and the probe
cannot tell them apart; it treats too little post-settle data (under 5 s) as a failure so a hang is never a silent
green. If idle ever drops below roughly 0.3 frames/s, revisit this (count the time since the last line as zero-frame
span, or add a timer thread to the trace) instead of loosening the gate.
