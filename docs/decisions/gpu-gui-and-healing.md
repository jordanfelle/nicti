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

2026-11-03 (#656): the view is wired. `CatalogStore::layout_items(ids)` returns width/height/orientation and the capture
day (first ten characters of `captured_at`, `:` normalised to `-`) for exactly the ids given, in the order given (a missing
id gets a blank, square item; a corrupt value is "unknown", never an error), 900 ids per `IN` statement. The first design
ran its own `hunt`-shaped scan; the adversarial review showed that a second query races the session's snapshot (a new
sort/filter built the layout for the *requested* list while the session still held the old ids, so a build was rejected and
repeated, and an import never settled), so the job now runs over the session's own id list and cannot disagree with it.
`LayoutJob` (`grid/layout_build.rs`, Pounce CPU lane, `JobKind::Snapshot` so it sorts ahead of thumbnail batches) looks up
4096 ids per step and streams them into the `RowIndex` (cancel lands within a chunk; the build holds no more than the
aspects and the index); days become groups ("No date" for undated photos; headers only while the *applied* sort, the one the
ids are in, is capture time). The layout is keyed on `GridSession::generation()` (bumped when the id list changes or on
`refresh`) plus width bucket, row height and headers, and is drawn only while its generation is current: a width, height
or header change keeps the previous layout on screen until the new one lands, but **a list change falls back to the
square grid with a spinner until the new layout lands** (a stale layout would index the wrong photos). Each body has its
own scroll area, so that fallback does not lose the justified position (test-pinned). A failed build shows the error with
a Retry button and retries by itself when the list changes. `view.rs` has two bodies, `show_square` and `show_justified`
(`ScrollArea::show_viewport` over `rows_between`, f64 row tops, header text per group, navigation through
`RowIndex::navigate`/`navigate_page`, reveal by row span), behind a Square | Justified switch and a Date headers checkbox
above the grid (session-only, **square stays the default**; the snapshot `library_grid` gained that toolbar row). Rows
are laid out at the width bucket (floored to 16 px), so the right edge can fall up to 15 px short of the pane.
`GridStats` gained `layout_builds` and `justified_frames`; swat tests assert one height per row, varied widths, <60
cells/frame at 5000 photos, **no layout build across unchanged frames** (mutation-proved: invalidating the gate every
frame fails it), End/arrow-down on 1500 photos, a list change rebuilding for the new generation, the justified scroll
offset surviving the square fallback (mutation-proved: sharing the scroll id fails it), and switching back to square.
Measured here (release, Linux CPU, **not** the reference machine, no real ref-10k): `layout_items` over every id in
4096-id chunks 0.45-0.70 s at 300k photos on a file-backed catalog (about 1.5x `hunt_ids`' 0.32-0.64 s, so ~1.5-2 s at 1M
by linear extrapolation, in the background); `RowIndex` build over 1M cells in 40-photo groups 3.9 ms (150k rows, ~3 MB;
8 ms at 2M), a viewport query ~40 ns. f32 scroll offsets lose exactness past ~1.6x10^7 points (about 100k rows), the
same limit `show_rows` has; rows are positioned from f64 tops. Known limits: during a long import the list changes every
reload, so the justified view shows the square fallback while each rebuild runs (a layout older than the list is never
shown); a catalog mixing EXIF-style and ISO-style `captured_at` text can split one day into two headers (the sort is on
the raw text). Not verified: the look on a real display, scrolling feel at 1M, and the reference-machine numbers
`docs/benchmarks.md` asks for. `cargo test -p nicti-lair --release -- --ignored --nocapture layout_items_cost` repeats the
lookup measurement (`NICTI_SCALE_N`).

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

**2026-10-28 (#584): the follow-up frame is egui's, and it is fixed.** `egui::Context::request_repaint()` (zero delay)
sets `outstanding = 1`, and `begin_pass_repaint_logic` then runs another pass: "Each request results in two repaints,
just to give some things time to settle" (`egui-0.37 context.rs`, `request_repaint_after`). eframe 0.37 only forwards the
callback (`run.rs`, `RepaintAt`), so it was never eframe or winit. `repaint_trace::request` (every instrumented wake:
telemetry, `Pending`, Pounce, cull, all posted from worker threads) called `request_repaint()`, so each wake cost its
own frame plus a settle frame 1-2 ms later that no source had asked for (hence `unattributed`, and why its
`repaint_causes()` were only the previous pass's delayed requests). A non-zero delay is scheduled once ("for non-zero
delays, we only repaint once"), so the wake is now `request_repaint_after(WAKE_DELAY)` with `WAKE_DELAY` = 1 ms. A test
drives egui's own repaint callback as eframe's scheduler: the control (zero delay) costs 2 passes, every wake source
now costs 1, and reverting the call fails it. Expected idle: telemetry 1.00 + the 1 s timer frame, about 1.0-1.2
frames/s on the runner; the `scripts/idle-probe.ps1` figure on the PR's `build-windows` job is the measurement. Still
open (the ticket's item 2): the ~1 frame/s target on a quiet machine and idle frame cost on a real GPU.

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

- **Display orientation (#341), Loupe done, Develop tools open** — `LinearFrame.orientation` (the RAW container's IFD0 Orientation, EXIF 1-8, `0` = none; read in `decode_linear` by `embedded::Walker::ifd0_orientation`, the same reader T2 uses). The pixels, the edit document (crop, straighten, heal spots, masks) and every render stage stay in **sensor** orientation; only the display turns, the same split export already uses (`preen::orient` rotates the finished, resized buffer). `nicti-pelt/src/view_orient.rs` (`ViewOrient`) holds the mapping, pinned pixel-for-pixel to `preen::orient::apply` by a test over all eight orientations, so screen and exported file cannot disagree. `display.wgsl` has an `orient` uniform (index 0-7, reusing a padding word; struct size unchanged) and `orient_to_sensor`; `view_scale`/`view_offset` are in *displayed* space, and orientation 0 takes the exact pre-#341 expression (bit-identical for unrotated photos). A GPU readback test draws a four-quadrant frame through the real pipeline for all eight and checks it against `ViewOrient`; breaking one shader case fails it. The Loupe (`paint_loupe_viewport`) fits, zooms and pans on `orient.display_size(rendered extent)`, so a portrait NEF fits a landscape pane by its turned size. **Not done:** the Develop tab still paints sensor orientation (`ViewportCallback::identity`), because its crop, heal and mask overlays map screen <-> image through `screen_to_image`/`image_to_screen` and also scale radii and handles by `rect.width() / source.0` style ratios that swap axes under a 90-degree turn (about 26 call sites in `develop_panel.rs`, `heal_tool.rs`, `mask_panel.rs`); `ViewOrient::screen_to_image`/`image_to_screen` are the drop-in replacements, written and tested but not yet wired. Also open: resize-cursor icons on the crop handles, and the grid thumbnails (T0/T2, already turned, #309).

- **Sidebar folder browsing (#472)** — `nicti-pelt/src/folder_browse.rs` (`list_subfolders`, `Browse`) + `folder_panel::show_browse`. Each non-`/` drive in the Library sidebar has a "Browse folders" disclosure; opening a drive or a subfolder reads its immediate subfolders on a worker (`Pending`, never the UI thread: a dead share can hang `read_dir`), shows a spinner meanwhile, and lists real directories only (symlinks and junctions are skipped so a link cannot make the tree infinite; `.hidden`, `$RECYCLE.BIN` and `System Volume Information` are left out; sorted case-insensitively; capped at 500 per folder with a note). Closing a node drops its listing, so reopening re-reads what is on disk; a read error is shown in place. A registered root is shown as "name (imported)" and not offered for import again; every other browsed folder has an "Import this folder" button, which sets the Folder field and runs the same `submit_root_job(Import)` path as typing it. The disclosure is the header's own hit-rect, so browsing never selects a folder, starts a drag or imports anything. Selecting a root opens its location in the tree once (`reveal_once`, keyed by root id, so the user's own collapsing is not undone each frame). Review fixes: a hung read shows a static "Reading…" (a spinner would repaint every frame forever, the #512 regression) and a node closed mid-read keeps that one read so reopening a hung share cannot leak a thread per click; the body only reads while the node is open (it also runs during the close animation, which re-read and cached a stale listing); entries carry their real `OsString` name next to the lossy display name (a non-UTF-8 folder browsed and imported a path that did not exist); a folder inside or around an imported root is not offered for import (`nesting_conflict`, also enforced in `app.rs`; the typed Import field still has no overlap check, pre-existing); "already imported" compares a normalised key (separators, trailing separator, `\\?\` prefix, case on Windows). Windows directory junctions are reparse points that std reports as symlinks, so they are skipped (no cycle risk, but a junctioned photo folder cannot be browsed; unverified on Windows). Not done: drag-and-drop of a browsed (unimported) folder, a "Sync" button on browsed folders that are already roots, watching the disk for changes while a node stays open, and removable-drive hot-plug refresh of an open node.

## #634: Develop applies the EXIF orientation

The Develop tab painted sensor orientation (`ViewportCallback::identity`), so a portrait shot looked sideways there while the Loupe was already right (#633). It now paints with `develop.view_orient()` and fits the *turned* size (`ViewOrient::display_size`), and every overlay maps through the same `ViewOrient`: the crop/straighten gestures (`develop_panel`), heal spots and their handle tolerance (`heal_tool`), mask shapes, brush and radial handles (`mask_panel`'s `to_norm`/`to_screen`, shared by the Pick Color and targeted-adjustment tools). The old free `screen_to_image`/`image_to_screen` are gone; `ViewOrient::normal()` is the exact identity, so an unturned photo behaves as before.

Where the old code scaled by `rect.width() / source.0`-style ratios it swapped axes under a quarter turn, so those now divide by the *displayed* size (`ViewOrient::display_size`; a circle stays a circle when turned, so a spot or brush is an ellipse with half-axes `r * rect / displayed size`, written in the old operation order so an unturned photo is bit-identical): the heal spot ellipse and handle tolerance, the mask brush circle, and the grain's canvas-minification width (`frame_width_on_screen`: the rect's height when a quarter turn runs the frame's width down the screen). The mask overlay's red tint is a texture built in frame (sensor) space, so it is painted with `ViewOrient::paint_texture` (a mesh with per-corner UVs from `uv_corners`; `Painter::image` can only take an axis-aligned UV rectangle, which cannot express a quarter turn). The crop gestures themselves work in sensor-image space after mapping the pointer, so they needed only the mapping change; the handles are hit-tested at the screen position of their mapped corners. There are no crop-handle resize cursors in the code today, so none needed fixing.

Reader parity: the screen reads the orientation with `Walker::ifd0_orientation` (SHORT only) while export uses kamadak (`get_uint`: SHORT or LONG). The walker now accepts a LONG too, and `view_orient::tests::the_screens_orientation_reader_agrees_with_exports_on_every_value` pins the two readers together over both byte orders, both types and values 0-9. ISO-BMFF (HEIF) containers are read only by export's reader; Nicti does not ingest them.

Tests: a click on a turned photo lands on the sensor pixel it shows for all eight orientations (real egui harness through `heal_tool::handle_viewport`), corner handles are hit where drawn, mask coordinates round-trip, and the ellipse/scale maths. Known and unchanged: Upright guides, the Level/Vertical buttons and `straighten_from_drag` classify and solve in sensor space, so on a portrait photo (EXIF 6 or 8) a guide drawn along a building edge on screen is classified Horizontal; that is consistent with the sensor-space solver but not with what the user sees (not changed here). `ROTATE_HANDLE_OFFSET_PX` is 28 *source* pixels, so on a large photo the rotate handle sits close to the rect edge (pre-existing). **Not verified:** no real portrait NEF or JPEG has been looked at in the Develop tab (no display here); the crop-gesture drag was tested through its mapping and hit-testing, not by a scripted drag on a turned photo.

- **Library is grid-first (#757)** — found in the headless UI audit: the Library view stacked filters, a 12-button action row and the whole import/sync/move/backup/preview-cache/XMP/Lightroom block above the grid, leaving about one row of thumbnails at 1000 px. Now Filters start collapsed ("Filters (active)" still shows when one is set) and the tools drawer is open only on an empty catalog (Import is the call to action) and folds once when photos first appear (`tools_drawer_plan`, then the user's choice stands); nothing is drawn until the grid has loaded. The window opens 1440x900 (min 900x600). Not done: remembering window geometry, a purpose-built empty state, moving cache/XMP settings into a Settings window.

- **Loupe info and navigation (#760)** — found in the headless UI audit: the Loupe showed only the image and `n / N`. It now shows Prev/Next buttons, an info line (`loupe::info_line`: filename, capture time, camera with the make not repeated, lens, ISO, pixel size, whichever the catalog has; shutter and aperture are not in the catalog so are not shown) cached per asset in `PeltApp::loupe_info`, and a dim key hint (arrows, Space, 0-5, P, X, U, 6-9, matching `cull::keys::action_for`). Stars, pick/reject and label were already in the header. Not done: a filmstrip, a hover/EXIF panel with shutter and aperture, zoom controls beyond Space.

- **Folder panel locations (#761)** — found in the headless UI audit: an imported folder under `/tmp/...` showed as a volume `/` holding only its leaf name. The `/` node is the catch-all for any path outside `/mnt`, `/media/<user>` and `/Volumes`, so it is now headed "/ (other locations)", has no "Archive drive" checkbox (a location of `/` would match every path), and each root row shows its path below the drive (`photos/2026`; a long one keeps `…/Day1/Selects`) with the full path still on hover. Pure helpers `drive_label` and `root_label` are unit-tested. Not done: friendly volume names (labels/UUIDs) from the OS.

- **UI-thread stalls on open (#768)** — the operator reported nicti locking up on double-clicking an imported photo. Measured in the headless audit (24 MP JPEG, debug build, software Vulkan): the Pounce decode keeps the UI live, but when it lands `paint_loupe_viewport` runs the whole first full-resolution render on the UI thread (1.6-2.5 s; `load_real_frame` is 0 ms). Shipped here: the Loupe paints a "Rendering the full-resolution photo..." frame over the preview before that render starts (`loupe_first_render_needs_notice`, once per photo, reset per session); `NICTI_SLOW_TRACE=1` prints `slow-trace: <section> <ms> ms` for any of loupe.poll / current_frame / load_real_frame / paint_loupe_viewport / develop.render taking 250 ms or more (`slow_trace.rs`); and `gpu::read_mapped` waits at most `READBACK_TIMEOUT` (60 s) and fails with `ReadbackError::Device` instead of `wait_indefinitely`. Not done (#778): moving the render off the UI thread. The Develop tab has no pre-render notice yet.

## #775: folder panel volume names

Folder panel volume names (#775): a drive heads as `<OS volume label> (<mount path>)` (`volume_names.rs`: `/dev/disk/by-label` joined to `/proc/self/mounts` on Linux, the volume label via `nicti_lair::volume::windows_impl` on Windows -- Windows half unverified; read on a worker with the drive probe, `/` never labelled); row and heading truncation count display columns (`unicode-width`), so CJK/emoji labels are cut where they overflow. The Archive-drive checkbox on `/` is already pinned by `the_catch_all_offers_archive_only_to_switch_an_old_location_off`; a full-panel pixel snapshot was not added (needs a GPU device).

## #776: Library toolbar

Library toolbar (#776): the action row is grouped select | delete/survey/compare | export | a `Settings` menu (Copy/Paste/Sync/Straighten/Apply preset), wrapping between groups; every disabled button has a hover reason from the pure `library_toolbar::why_disabled` (same conditions as before, unit-tested). The Keywords field is `available_width - 110` clamped 240..420 so the hint is readable and the Tag button stays on its line. Control-channel names (`button:delete`, `button:export`, ...) are unchanged; the settings actions were never registered. Not hidden when nothing is selected (a stable layout and the hover reasons were preferred).

## #778: first render off the UI thread

**First render off the UI thread (#778)** — opening a photo no longer renders inline: `render_job::FirstRenderJob` (Pounce GPU lane, Foreground class) takes the whole `DevelopView` (document + engine travel together, a render mutates both), runs `render()` and hands the view back through a slot (also after a panic, and when dropped unrun), so the UI thread's own `render()` is only the live pass over a warm baked cache (`last_render_stats().bake_dispatches == 0`, test-pinned). `PeltApp.develop` is `None` while the job runs (the Loupe keeps showing the T2/T0 preview under the Rendering notice; the Develop tab has no view for that time); `poll_render_job` runs every frame whatever view is showing. Slider drags still render inline on the existing live path. Verified in the running app under the headless audit tool (`docs/ui-audit.md`): a Library click during the render took effect immediately. A batch edit (paste/preset/undo) that lands while the view is out is re-read from the catalog when it returns (`RefreshPlan::Defer`), `develop.get/set` say Develop is still rendering instead of 'no photo loaded', and the Develop tab shows a Rendering notice. Not done (follow-up): cancelling the job when the user moves to another photo (its result is kept and the next photo loads after it), Undo/Redo keys during the render are dropped, and a job cancelled from the Activity panel hands the view back unrendered so the next paint renders inline as before.
