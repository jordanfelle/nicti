---
paths:
  - "spikes/glint/**"
  - "crates/nicti-groom/**"
  - "crates/nicti-stalk/**"
  - "crates/nicti-tapetum/**"
  - "crates/nicti-pelt/**"
---

# GPU, GUI, and Healing — Quick Reference

Full reasoning/history: `docs/decisions/gpu-gui-and-healing.md`.

- **GPU compute API** — `docs/adr/0016`: `wgpu` (WGSL), Vulkan backend on Windows (not Dx12 — no
  `SHADER_F16` there, Tapetum's cache tiers need f16). Measured on RTX 5080: within 2x of CUDA at
  4K/45MP. **Always dispatch as a 2D grid** (`gpu.rs::workgroup_grid`) — naive 1D overflows wgpu's
  65535-per-dimension workgroup limit. **Never a full-frame host↔device round-trip in the hot
  path** (0.8–1.5s, confirmed expensive) — baked stage output stays GPU-resident.
- **Production `GpuContext` landed in #45** (`crates/nicti-tapetum::gpu`), adapted from
  `spikes/glint`'s: one shared `wgpu::Device`/`Queue` per ADR-0016, requesting `adapter.limits()`
  (not the 256MB default) and `TIMESTAMP_QUERY`/`SHADER_F16` when the adapter supports them. Frame
  storage is `Rgba16Float` **textures** (`crates/nicti-tapetum::frame::FrameTexture`), not the
  `array<vec4<f32>>` storage **buffers** `spikes/glint`/`spikes/loaf` use — a texture format needs
  no `SHADER_F16` feature at all, so it works identically on Vulkan, Dx12, WARP and lavapipe.
- **GUI framework** — `docs/adr/0068`: **Accepted — egui** (2026-09-27, #90), on hard-gate evidence
  alone; the reference-machine benchmark pass was waived, see the ADR's Amendments. **Correction
  (2026-09-26)**: Prior-art section wrongly claimed RapidRAW uses egui/eframe — it's Tauri+React;
  doesn't change the Decision. GPUI eliminated (Windows backend has no wgpu/Vulkan path). egui wins
  (wgpu 30.0.0 match, MIT/Apache-2.0). Iced pins wgpu 27 (compat cost). Slint's GPU integration is
  cleanest but its license (`GPL-3.0-only OR LicenseRef-Slint-*`) needs its own ADR-0018 amendment
  to ship. `spikes/pelt*`/`bench/pelt/` deleted in #232; real perf validation against the actual UI
  crate tracked in #233.
- **Production UI crate: `crates/nicti-pelt`** (#241, landed) — device sharing implemented as
  `nicti_tapetum::gpu::device_descriptor_for` passed to eframe's `WgpuSetup::CreateNew` (so
  eframe's own device gets Tapetum's own limits/features, not `wgpu::Limits::default()`'s
  conservative 8192px/no-optional-features default), then `GpuContext::from_device(adapter,
  device, queue)` wraps the resulting `cc.wgpu_render_state`'s device — one real device shared
  between egui's render pass and every Tapetum compute dispatch, exactly ADR-0016's "one shared
  device" rule. Displaying a `FrameTexture` (linear ProPhoto RGB, `Rgba16Float`) needs its own
  small fragment shader (`nicti-pelt/shaders/display.wgsl`, `viewport.rs`'s `ViewportCallback`) —
  the same `color::prophoto_to_srgb_linear_matrix()`/sRGB-OETF pair
  `geometry::output_encode`'s CPU reference already uses, with the OETF skipped when the render
  target itself is an `*Srgb` format (hardware already applies it on write then — applying it
  twice double-gammas the image).
- **Healing/removal** — `docs/adr/0050`: **Accepted** (2026-09-26, #97's reference-machine pass).
  Ships both classic clone/heal (CPU Poisson-Jacobi + `wgpu` compute-shader twin) and AI removal
  (MobileSAM+LaMa via `ort`/`load-dynamic`) as two `SpotKind` variants of one `HealStage`.
  **GPU Poisson-solve validated on real RTX 5080 hardware: 0.386ms p50 (Vulkan) vs. the <16ms/
  update target** — ~40x headroom; see #97's WSL-has-no-NVIDIA-Vulkan-ICD gotcha below. **(State
  as of #50/#97 -- #51 has since measured AI removal and resolved the LaMa question by owner
  sign-off; see the "Built in #51" bullet below.)** At the time, AI removal latency/quality was
  TBD and LaMa's Places2 license status unresolved (unreachable primary source); MI-GAN was
  investigated as an alternative and found not cleaner (same exposure). Proposed stage order for #44: after lens correction, before global
  tone, in linear space. **Model loading must follow ADR-0218**: offline inference by default, no
  telemetry, no hosted API; weight fetch only as an explicit user-initiated + checksummed
  download, never a silent auto-fetch.
  - **Built in #51 (`docs/adr/0051`)**: `spikes/groom` deleted/promoted. Classic heal = real baked
    stage `nicti-tapetum::heal` (`HealStage`/`HealKernel`/`HealExec`, `shaders/heal.wgsl`,
    `coat::HealParams`); AI removal = `nicti-groom` (`remove::RemovalEngine` → `RemovalPatch`);
    model store = `nicti-stalk::models`. **RTX 5080 (noisy shared GPU, ranges): 1 heal r=24 ≈ 1.2-2.2 ms, r=100 ≈ 3-6 ms, r=300 ≈ 8-28 ms, 10 heals ≈ 7-18 ms** (end to end) — big/many heals reach the 16 ms budget.
    **AI removal on the CPU ORT build ≈ 3-4 s — misses the <2 s CUDA-EP target**; quality checked
    on synthetic scenes only. **LaMa = on-demand download only, owner sign-off 2026-09-29.**
  - **Gotcha (#51, real hardware only)**: the heal suite passed on lavapipe + Vulkan and **failed on
    the RTX 5080 under Dx12** (Jacobi chain garbage from sweep 3; CI's Windows job is WARP/Dx12, so
    it would have failed the required check). Fixes: read inputs as sampled `texture_2d` +
    `textureLoad` (NOT `texture_storage_2d<.., read>`), write-only storage outputs, reset scratch by
    copy from a zero texture per spot, and copy each Jacobi sweep back instead of swapping roles.
    Mechanism inferred, not confirmed. **Always run new GPU kernels on the reference machine under
    BOTH backends** (`NICTI_WGPU_BACKEND=vulkan|dx12` with the cross-built `.exe`); other shaders
    still using `read` storage textures (`detail_blur`, `present_sample`, ...) are unaudited.
  - **Gotcha (#51)**: **Jacobi must not start from the destination** — it needs ~side² sweeps to
    smooth a blemish and we run 50-400, so a blemish over ~half the spot survived (Heal was a
    near no-op). `boundary_mean`+`init_heal` start at source + mean ring offset. GPU-vs-CPU parity
    tests can't catch this (reference shares the algorithm): keep `heal_actually_removes_the_blemish`
    (asserts the outcome, mutation-checked). Also: bound per-render work (`MAX_SPOTS`,
    `MAX_JACOBI_PASSES`) and clamp document coordinates (`COORD_LIMIT`) — documents are untrusted.
  - **Gotcha (#51)**: a finished patch isn't a params change, so `heal::stamp_removal_state` stamps
    `"removals": {spot key → patch hash}` into the heal entry the *render* sees (never the stored
    one) — that is how a patch arriving rebakes the stage; overriding `own_hash` after
    `apply_document` would thrash the memo (it resets the hash every render).
  - **Gotcha (#51)**: `RemoveJob`/`InstallModelsJob` must return `Ok(Step::Done)` on a *removal or
    download* failure and resolve their slot with the error — a `step()` `Err` makes Pounce drop
    the job without touching the slot, leaving the UI waiting forever (same rule as `DecodeJob`).
  - **Gotcha (#51)**: egui's `drag_started` fires *after* the pointer crosses the drag threshold, so
    hit-test and anchor a drag at `ui.input(|i| i.pointer.press_origin())`, not
    `interact_pointer_pos()`.
  - **Dev/test**: models can be dropped in by hand at `$NICTI_MODELS_DIR/<id>/<file>` (exact
    pinned size) with `NICTI_ORT_DYLIB` set; the real-weight tests are `#[ignore]`d in
    `crates/nicti-groom/tests/real_models.rs` (env vars listed in its header); the heal
    benchmark is `cargo test -p nicti-tapetum --release heal::tests::throughput -- --ignored`.
  - **Gotcha (#97)**: this WSL sandbox has no NVIDIA Vulkan ICD registered at all — `wgpu` here
    only reaches the software `llvmpipe` adapter (88ms p50, not real hardware), even though
    `libcuda.so`/D3D12 interop libs under `/usr/lib/wsl/lib/` give real CUDA/D3D12 access.
    `mesa-vulkan-drivers` on this distro also has no `dzn` (D3D12-translation) ICD. Fix: cross-
    compile for `x86_64-pc-windows-gnu` (`rustup target add`, needs `x86_64-w64-mingw32-gcc` for
    the linker — **and must use the rustup-managed `cargo`/`rustc`, not a Homebrew-installed one
    that shadows it on `PATH` first and has no Windows target installed**) and run the real `.exe`
    directly on the reference machine's Windows side via WSL interop
    (`powershell.exe -Command "& '<unc-path-from-wslpath--w>' --ignored --nocapture"`) — same
    pattern `spikes/retina`/`spikes/sniff` already used for their own real-hardware passes.

## Package contents

- **`spikes/glint`** (#16/ADR-0016) — wgpu-vs-CUDA measured comparison: correctness, feature/limit
  availability, throughput, dispatch overhead, host↔device interop cost.
- **`spikes/pelt` + `spikes/pelt-egui`/`pelt-iced`/`pelt-slint`** (#68/ADR-0068, deleted in #232) —
  GUI-framework research. `pelt` was the toolkit-agnostic shared fixture/math crate; each `pelt-*`
  was one candidate's virtualized-grid + loupe + custom-wgpu-viewport spike. No `spikes/pelt-gpui`
  ever existed (ADR-0068's Hard-gate-1 early exit).
- **`crates/nicti-pelt`** (#241, landed) — the production app shell: `lib.rs` (`run`, the
  `WgpuSetup::CreateNew` device-descriptor wiring), `app.rs` (`PeltApp`, view routing), `render.rs`
  (`DevelopView` — wires a `LinearFrame` through the real Tapetum pipeline; #46: owns a real
  in-memory `nicti_pawprint::EditDocument` + `StageRegistry`, `histogram`/`apply_auto_tone`
  methods, `show_before` toggle), `develop_panel.rs` (#46: the Develop view's right-side edit
  panel — Basic/Tone Curve/HSL/Detail sections, live histogram, Auto + before/after buttons; see
  `render-graph`'s own "#46 completion" bullet), `viewport.rs` (`ViewportResources`/
  `ViewportCallback`, the `egui_wgpu::CallbackTrait` display pass) and `catalog.rs` (opens a
  `nicti-lair` `SqliteCatalog`). Supersedes `spikes/pelt-egui` as the real, non-throwaway crate
  ADR-0068 points to.
  - **#31 (loupe, landed) module breakdown**: `decode_job.rs`'s `DecodeJob` — a single-chunk
    Pounce CPU-lane job (new `JobKind::Decode`) wrapping `RawDecoder::decode_linear`, generic over
    the decoder trait (not hard-coded to `LibRawDecoder`) so tests use a fake decoder rather than
    needing a real NEF file — always resolves its `ReportSlot` even on decode failure, matching
    `BackupJob`'s own established fix for the same "a poller must never wait on a slot that never
    resolves" failure mode. `loupe.rs`'s `LoupeSession` — an ordered asset-id list + cursor, a
    `nicti_tapetum::cache::Tier<Arc<LinearFrame>>` RAM cache keyed by `asset_cache_key`
    (fingerprint, falling back to `id:mtime_unix`), submits `DecodeJob`s for the cursor +/- 1 on
    every `set_cursor` and reprioritizes Pounce's background queue by distance-from-cursor; errors
    are tracked as `(identity, message)` pairs so a stale error from a since-superseded revision
    (a re-ingest fixing/replacing the file) doesn't keep blocking the new one; `cancel_all` clears
    `inflight` immediately rather than waiting on a cancelled-while-queued job's slot, which would
    never resolve at all. `render.rs`'s `DevelopView::load_real_frame(frame: Arc<LinearFrame>,
    identity)` swaps in a real decode and calls `RenderGraph::set_own_hash(DECODE, identity)` so
    Tapetum's baked-output cache doesn't collide across different real photos at the same pixel
    extent; `has_edits()` lets a caller check for a real in-memory edit before swapping (`app.rs`'s
    Loupe view blocks the swap behind an explicit confirmation if Develop has unsaved edits open
    for a *different* photo, since `develop` is one instance shared between the Develop and Loupe
    tabs — an adversarial review caught an earlier version silently discarding them). `app.rs`'s
    "Open in Loupe" button (Library view) builds a session over a folder's assets (`id`-ordered —
    real grid/filter-driven selection is #30/#242's job); the Loupe view polls every frame, shows
    the T0 embedded JPEG preview (decoded via the `image` crate, cached per asset id) while a real
    decode is still in flight — the actual mechanism behind the ticket's "< 50ms next/prev" target,
    since the RAW decode itself never hits that number (`raw-decoder` topic's own 1-2.4s/file
    measurement) — Left/Right navigate, Space toggles Fit/100% zoom (reset on every cursor move),
    drag pans while zoomed. `viewport.rs`/`display.wgsl` gained `view_scale`/`view_offset` uniform
    fields and a real bilinear `wgpu::Sampler` (replacing the old `textureLoad`, which had no
    resampling at all) so the same shader serves both Fit (aspect-correct, `fit_scale`) and 100%
    (`one_to_one_scale`) zoom; `ViewportCallback::identity(frame)` keeps the Develop tab's old
    always-1:1-stretch mapping exactly. Known non-blocking follow-ups: #294 (unclamped 100% pan),
    #295 (a rare cache-eviction flicker back to the T0 fallback), #296 (NaN/Inf on a zero-dimension
    rect or corrupt asset in `fit_scale`/`one_to_one_scale`).
  - **#30 (virtualized library grid, landed) module breakdown**: `grid/layout.rs`'s `GridLayout`/
    `batches_for` — pure geometry (columns, visible index range, fixed 64-id thumbnail batches),
    no egui. `grid/jobs.rs` — `SnapshotJob` (one `CatalogStore::hunt_ids` scan on the CPU lane, so
    a 1M-row read never blocks the UI thread; `JobKind::Snapshot`) and `ThumbBatchJob` (8 T0
    previews per `step()`, `get_previews` batch read + JPEG decode + downsize to 256px,
    `JobKind::Thumbnail`; results accumulate in a `ThumbSlot` so cells fill before the batch ends,
    and `Drop` marks the slot done so a cancelled job never strands a poller). `grid/session.rs`'s
    `GridSession` — the id snapshot (8 MB at 1M), a byte-budgeted `Tier<TextureHandle>` (256px
    thumbnails, keyed by asset id; `refresh` drops it after ingest/sync/move since a rescan can
    replace previews), `request_visible` (queues the visible batches ± 2, cancels ones > 4 batches
    away, reprioritizes only when the window moved), `poll` (applies only the latest snapshot
    generation; uploads ≤ 32 textures/frame), `pause` (cancels batches while another view is
    showing), `set_query`/`reload`/`refresh`. `grid/view.rs` — `ScrollArea::show_rows` drawing plus
    arrow/Page/Home/End navigation, Enter/double-click to open; `app.rs`'s `show_library`
    (root + sort toolbar, collapsible import/sync/move controls), `open_from_grid` (loupe over the
    grid's own ordering; `loupe_from_grid` mirrors the loupe cursor back onto the grid) and
    `drive_grid` (live snapshot reload while an import runs, full refresh on the busy → idle edge).
    `activity.rs` folds Thumbnail/Snapshot jobs into one line. Catalog side (`nicti-lair`, schema
    V7): expression indexes `idx_asset_sort_*` + `idx_asset_root_sort_*` matching `hunt`'s ORDER BY
    term-for-term (`sort_column_sql`) so no sort needs a temp B-tree — **never add `ANALYZE`/
    `PRAGMA optimize`** without re-running `hunt_sort_uses_an_index` (with stats, a one-root
    catalog flips to scanning `root`/`volume` outer and re-sorts). Measured, 1M synthetic assets,
    release build (`cargo test -p nicti-lair --release --test scale -- --ignored --nocapture`):
    `hunt_ids` 37–270 ms (filename across all roots 1.1 s), first keyset page 0.19 ms,
    `get_previews` 64 × ~138 KB 6 ms. Not measured: real-machine grid frame time (#233).
  - **#242 (Library filter bar, landed)**: `filter_bar.rs`'s `FilterBar` — keyword (+subtree), rating
    (any / unrated / exactly N / N+ / rejected), flag (any / picked / unflagged), label (any / none /
    a name), make, model, capture-date range, filename controls (#32 added the unrated, exactly-N,
    unflagged and no-label choices; a saved rule whose rating, flag or label shape the controls
    can't express is carried verbatim via the `Custom` variants) →
    `to_filter(root_id)` → `Filter` for `GridSession::set_query`; `load_filter` is its inverse and
    round-trips a saved rule losslessly (raw date bounds, `rel_path_prefix`, and any rating shape the
    controls can't express via `RatingChoice::Custom` are carried through). Facet
    counts come from `compute_facets` (each dimension counted with its *own* filter cleared, so a
    picked model doesn't zero the others) on a worker thread (`Pending`), as do the option lists
    (`load_options`). Text fields commit on Enter/focus-loss, not per keystroke (filename is a
    `GLOB` scan). Dates are typed `YYYY-MM-DD` and compared as `YYYY-MM-DD HH:MM:SS` text — the *dashed* form
    kamadak-exif's `display_value` gives ingest (NOT raw-EXIF colons: mixing them mis-sorts at char 4); an invalid date is *no bound*, never an empty result. Save =
    `create_collection(Smart)` + `set_smart_rule`, rolled back if the second fails. `app.rs` reads
    `to_filter` even while the header is collapsed (a collapsed bar still filters) and calls
    `invalidate_options` on the import/sync/move busy → idle edge. Not verified by eye: no display
    in the dev sandbox, only unit tests over the state/query logic.
- **`crates/nicti-pelt`'s `folder_panel.rs`** (#303) — Library left panel: roots grouped by drive
  (`drive_of` derives the drive from the path — the shell still registers every root under one
  placeholder volume, so no real volume/offline tree until ADR-0071 identity lands), drag a folder
  onto a drive → `PeltApp::submit_move_to` → `MoveJob`. Also lists still-open
  `root_move` journal rows (`attention_lines`) so a `Resumed::Stuck` does not only flash once. Folder rows are drag sources only: `Carry` refuses a destination inside another registered root.
- **`crates/nicti-tapetum`'s `heal.rs`** (#51) — the baked heal stage: `HealStage` (registry entry,
  `impl_version`), `HealKernel` (pipelines built once), `HealExec` (one render's spots),
  `RemovalPatch`/`RemovalSet`/`spot_key`/`stamp_removal_state` (AI patches), `spot_geometry` (integer
  patch geometry shared with the CPU reference). `coat.rs` has `HealParams`/`Spot`/`SpotKind`/
  `MaskRecipe`; `stages::normalize_pixels` is the public CPU twin of decode's normalize pass.
- **`crates/nicti-groom`** (#51, promoted from `spikes/groom`) — AI removal + heal-source picking:
  `sam.rs` (MobileSAM, real encoder/decoder contract, per-photo embedding cache), `lama.rs`,
  `remove.rs` (`RemovalEngine`: prompt → mask → crop → inpaint → `RemovalPatch`), `space.rs`
  (camera-linear ↔ model sRGB), `geom.rs` (resize, exact EDT, dilate/feather), `source.rs`
  (`auto_source_pick`), `real.rs` (`LazyBackend`), `job.rs` (`RemoveJob`, Pounce GPU lane),
  `install.rs` (`InstallModelsJob`, Pounce CPU lane). `tests/ort_cross_module.rs` is #179/#229's
  cross-crate `ort` regression test, moved here from the deleted spike.
- **`crates/nicti-stalk`'s `models.rs`** (#51) — the on-demand model store and pinned manifest
  (`MOBILE_SAM_*`, `LAMA`, `ORT_RUNTIME`, `ModelStore`, `RemovalModels::locate`).
- **`crates/nicti-pelt`'s `heal_tool.rs`** (#51) — the Crop | Heal tool: `HealUi`, gestures
  (`handle_viewport`), panel (`show_panel`), `RemovalService` (install + backend + pending jobs).
  `DevelopView` gained `uncropped_preview`, `set_removal`/`prune_removals`, `frame_arc`/`frame_key`.
- ~~**`spikes/groom`**~~ (#50/ADR-0050) — deleted in #51; see `docs/research/groom-healing-removal.md`
  for the LaMa/MI-GAN licensing findings it produced.
