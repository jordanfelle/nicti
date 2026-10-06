# Nicti

Rust RAW photo editor + DAM, aiming to replace Adobe Lightroom Classic. Public repo:
`github.com/jordanfelle/nicti`. This file is Claude Code-specific guidance; **`CONTRIBUTING.md`
is the canonical human contributor guide** — read that first if you're new here.

## Architecture decisions

ADRs live in `docs/adr/` (see `docs/adr/README.md` for the numbering convention, index, and
template) -- numbered by the GitHub issue that prompted it, not sequentially. Per-ADR decisions,
measured results, and gotchas live in
`.claude/rules/<topic>/REFERENCE.md` + `docs/decisions/<topic>.md`, not inline here, to keep this
file under the line-count gate. Each topic has:

- `.claude/rules/<topic>/REFERENCE.md` — terse key:value/bullet compression, the actionable fact +
  a pointer, scoped with `paths:` frontmatter so Claude Code only auto-loads it when touching a
  matching file (unscoped `.claude/rules/**` files load unconditionally every session, which is why
  this repo scopes them).
- `docs/decisions/<topic>.md` — the full original prose, verbatim, with every issue ref and piece
  of reasoning. Not auto-loaded by Claude Code, but a normal repo doc any contributor can read.

Topics: `language-and-architecture` (0015/0021/0019/0218 v1 target, 0214 v2-only), `licensing` (0018/0066, 0069),
`gpu-gui-and-healing` (0016/0068/0050/0051), `catalog-engine` (0067/0102/0106/0103/0107, 0113/0115/0116, 0025, 0026),
`preview-tiers` (0029, 0143, 0072, 0145), `raw-decoder` (0037), `volume-identity` (0071, 0024), `color` (0038, 0042),
`lrc-migration` (0061, 0062, 0156, 0158, 0380), `masking` (0048, 0049, 0353), `culling` (0032, 0033, 0034, 0035, 0108), `denoise` (0040),
`xmp-interop` (0059), `render-graph` (0044, 0047, 0380), `develop` (0099, 0053, 0101, 0052), `jobs` (0054), `export` (0056, 0057),
`release` (0249). A new ADR adds a
bullet to both files of its topic (or a new topic) and to this list — not inline here.

## Performance targets and benchmarking

- **Performance targets + benchmark methodology**: `docs/benchmarks.md` — p95 targets per feature
  area, warm/cold measurement rules, and the `ref-10k` frozen reference dataset (manifest at
  `docs/ref-10k-manifest.csv`). Finalized 2026-09-23 (#14). Every render-engine/perf-sensitive
  ticket (#43, #17, #40, etc.) measures against this.
- **Hero-scenario benchmark (#43)**: spec at `docs/benchmarks/hero-scenario.md`, including
  interaction D's mixed-operation-sequence cross-regression check (#100) — see that doc's
  "D. Mixed sequence" section and `bench/whisker/README.md`'s "Interaction D" section for how the
  analyzer attributes flashes via a capture's `events.csv` sidecar. Tooling under
  `bench/`: `bench/select_hero_set.py` (deterministic 50-file working-set selection),
  `bench/lrc/` (AutoHotkey v2 driver + catalog setup for LRC — `hero.ahk` drives one timed pass,
  `navigate.ahk` positions the selection before a capture starts), `bench/run-hero.ps1` (one
  capture + orchestration), `bench/run-hero-series.ps1` (a full warm-up + 5-measured-run series,
  including crop/zoom's 5-image spread — this is what you actually invoke), `bench/whisker/`
  (Rust frame-diff analyzer, workspace member — not production code, see its own `Cargo.toml`
  description; its `analyze` subcommand pools a full results tree into per-config/interaction
  p50/p95/max). Requires PowerShell 7+, see `bench/lrc/README.md`.
- **Reusable harness (#17): `crates/nicti-prowl`** — a real production crate (not `bench/`-style
  tooling), since its manifest-verify/perf-protocol pieces are meant to be depended on by future
  research tickets' own exit criteria, not just the hero scenario. `manifest.rs`
  (`Manifest::load`/`verify`, checks a ref-10k copy's SHA-256 against `docs/ref-10k-manifest.csv`
  — `bench/run-hero.ps1` calls this via the `prowl` binary instead of its own inline
  `Get-FileHash` loop), `refset.rs` (`select` — general-purpose bucket-based picking, deliberately
  *not* a Rust port of `bench/select_hero_set.py`'s stratified sampler; `hero_set` reads the
  already-frozen `docs/benchmarks/hero-set.txt` instead), `perf.rs` (`Protocol` — the 1-warmup +
  5-measured-run/p50-p95-max protocol from `docs/benchmarks.md`, `run_verified` refuses to run
  against an unverified ref-10k copy), `golden.rs` (`GoldenStore`/`Render` trait — golden-image
  compare/store, hand-rolled single-scale SSIM rather than `dssim-core`, whose own published
  license string doesn't cleanly match an allowed SPDX id in `deny.toml`; tested only against
  synthetic images since no real NEF→render path exists yet, see the follow-up issue filed
  alongside #17 for wiring in real goldens once #41 lands). `prowl` (the bin target) exposes
  `verify`/`select` for PowerShell/CI callers.

## Naming convention: feline references

Name new crates, modules, internal tools, and subsystems with a feline-anatomy/behavior angle
rather than a purely descriptive name — the project itself is named after the nictitating
membrane, a cat's third eyelid that sweeps across the eye to clear debris without losing vision,
mirroring an editor that's non-destructive: it clears and reprocesses without ever losing the
original image data. That theme continues throughout. Examples already assigned
for planned subsystems: `Tapetum` (stage-cached render graph — the tapetum lucidum bounces light
back through the retina for reuse, mapping to reusing baked stage output), `Claw` (on-demand
module/plugin registry — claws stay sheathed until needed), `Pounce` (job scheduler with priority
preemption), `Sniff` (embedded-JPEG fast preview path for culling), `Scruff` (import/ingest
pipeline — the way a mother cat carries a kitten by the scruff of its neck is how a file gets
moved into the catalog).

## Package map

`crates/*` (a Cargo workspace member glob, landed in #20) holds the real production crate layout
from ADR-0019 §8. `spikes/*` holds throwaway research spikes not yet promoted — don't build on top
of one; each is deleted once its own ticket promotes it (as #20 already did for
`spikes/sheath`/`spikes/dewclaw`). Full per-spike/per-crate module breakdown moved out to each
topic's own `.claude/rules/<topic>/REFERENCE.md` "Package contents" section (#173) — this stays a
terse index: crate/spike → purpose → owning topic.

- **`crates/nicti-claw`**, **`crates/dewclaw`** — Claw registry + its dylib test fixture →
  [`language-and-architecture`](.claude/rules/language-and-architecture/REFERENCE.md)
- **`crates/nicti-prowl`** — benchmark + golden-image harness (#17); see the Performance targets
  and benchmarking section above
- **`crates/nicti-haw`** (#229; #345 adds `session_builder`/`ExecutionProvider`: GPU-provider selection with a logged CPU fallback, `NICTI_ORT_EP`) — shared, process-wide `ort`/`load-dynamic` environment init,
  replacing the six duplicated crate-local copies in `groom` (now `nicti-groom`)/`siamese`/`crouch`/`rods`/
  `litter`/`rosette`; see its own doc comment for the cross-crate path-mismatch rationale.
- **`crates/nicti-pounce`** (#55, landed) — Pounce: the production job scheduler, promoted from
  `spikes/crouch`'s research (#54/ADR-0054). `job.rs`/`cancel.rs`/`queue.rs`/`admission.rs`/
  `throttle.rs` are the scheduler core (a two-class priority queue, cooperative cancellation, the
  `IS_EDITING` gate, VRAM admission, CPU/disk throttling), unchanged in design from the spike but
  reworked for a real threaded runtime: `queue::Scheduler::take_next`/`finish` (split from the
  spike's single `run_next`) so a worker thread never holds the lane's lock across a job's own
  `step()`. `runtime.rs`'s `Pounce` is the new part the spike didn't build: two lanes (`Lane::Gpu`,
  exactly one worker thread and real VRAM admission; `Lane::Cpu`, a pool of worker threads gated by
  a live-adjustable `Throttle`), a `JobId`-keyed status map for an activity panel to poll
  (`snapshot`), and an independent `CancelToken` registry (`Pounce::cancel` can't just delegate to
  `Scheduler::cancel`, which only finds a job still sitting in its queue — most of a fast-yielding
  job's lifetime is spent checked out by a worker thread instead). `Pounce::submitter()` (#57) is a
  weak handle so a job can chain a follow-up. `telemetry/`/`hackles.rs` are
  #70's build (bottleneck indicator) — see [`jobs`](.claude/rules/jobs/REFERENCE.md).
- **`crates/nicti-calico`** (#42, landed) — color management: output spaces, runtime ICC profiles,
  the display/proof transform, Windows monitor-profile lookup; see [`color`](.claude/rules/color/REFERENCE.md).
  Also the DCP camera-profile machinery (`dcp.rs`/`cct.rs`/`huesatmap.rs`/`profile.rs`, promoted from
  `spikes/calico`) and the `ColorProfile` extension point. `nicti-pelt`'s `camera_profiles.rs`
  discovers/loads the user's Adobe `.dcp` files and Look `.xmp` profiles for the Develop panel.
  Profile tone curve (#321) → `nicti-calico/src/tonecurve.rs` (`ToneCurveLut`, `ProfileSolution.tone_lut`)
  + `profile_tone` in `nicti-tapetum/shaders/live_suffix.wgsl`; Look `.xmp` decode →
  `nicti-calico/src/xmp_profile.rs`, selected via `DevelopView::select_look`, stored in
  `CameraProfileParams.look`
- **`crates/nicti-iris`/`nicti-stalk`**
  — extension-point crates (supertrait + `Registry` alias only, no execution methods yet):
  `LensCorrection` (#39), `ModelProvider` (#48-#53/#33-#36). `nicti-stalk` also has `models.rs` (#51):
  the on-demand, checksummed AI-model store + pinned manifest (ADR-0218), and (#49) the backend-
  agnostic `SegmentationProvider`/`Segmenter`/`SegmentationRegistry` layer that makes AI mask models
  pluggable, plus the pinned BiRefNet artifact and (#345) the optional NVIDIA GPU pack (`models::gpu_pack_artifacts`, multi-file `Payload::ZipMembers`)
- **`crates/nicti-preen`** (#57, landed) — the export engine: `Exporter` (JPEG/PNG/TIFF) plus settings/
  presets, filename tokens, batch planning, collision-safe writes, linear-light resize, orientation,
  output color, watermark and EXIF/XMP/ICC metadata; `export_frame` is the entry point. GPU-free and
  catalog-free (the render + Pounce wiring is `nicti-pelt`'s `export/`) →
  [`export`](.claude/rules/export/REFERENCE.md)
- **`crates/nicti-groom`** (#51, promoted from `spikes/groom`) — AI object removal (MobileSAM + LaMa
  → `RemovalPatch`), the clone/heal auto-source picker, and the `RemoveJob`/`InstallModelsJob`
  Pounce jobs; the GPU clone/heal itself is `nicti-tapetum`'s `heal.rs`, the UI is `nicti-pelt`'s
  `heal_tool.rs` → [`gpu-gui-and-healing`](.claude/rules/gpu-gui-and-healing/REFERENCE.md)
- **`crates/nicti-siamese`** (#49, promoted from `spikes/siamese`) — AI masks: the BiRefNet and sky
  (heuristic) `SegmentationProvider`s, the provider registry + target -> default-model table
  (`providers.rs`; choosing a model is data), the neutral model image (`neutral.rs`), `RegistryBackend`
  (lazy verified load, retried) and the Pounce GPU-lane `MaskBakeJob` → [`masking`](.claude/rules/masking/REFERENCE.md)
- **`crates/nicti-tapetum`** — Tapetum's (#44/#45) stage-cached render graph, the real
  `RenderStage` execution trait, and the concrete decode/live-suffix/geometry pipeline: a DAG of
  stage nodes with a blake3 cache key chained from upstream (`graph.rs`), byte-budgeted
  VRAM/RAM/disk cache tiers (`cache.rs`), nearest-to-cursor bake prioritization (`prefetch.rs`),
  the shared wgpu `GpuContext` (`gpu.rs`) and GPU-resident `Rgba16Float` frame textures
  (`frame.rs`), `renderer.rs`'s graph-driven `Renderer` (proves ADR-0044's dispatch-count
  invariants against mock stages), and the real stages themselves (`stages.rs`): decode (uploads
  a `nicti_cornea::LinearFrame`, runs `normalize.wgsl`), passthrough slots for demosaic/denoise/
  lens (their own algorithms are #40/#39; heal is real, #51, `heal.rs`), the fused live suffix (WB + camera→working
  -space color + exposure + tone + vibrance, `color.rs`, `live_suffix.wgsl`), and crop
  (`geometry.rs`, `present_sample.wgsl`) — every kernel has a GPU-vs-CPU parity test against a
  CPU reference, plus one end-to-end test wiring the whole chain through `Renderer`. These tests
  skip when no `wgpu` adapter is available. Decode splits a full-res upload into row-strips
  (`stages::rows_per_strip`) to stay under a real adapter's `max_storage_buffer_binding_size`, and
  `tile.rs` (`TilePlanner`/`TiledRender`) tiles the output-side geometry pass for a full-res render
  (#45 PR4). Promoted from `spikes/loaf` (now deleted) / `spikes/glint`, and renamed from
  `nicti-render` to `nicti-tapetum` once the whole #45 stack merged, matching the naming-convention
  section above. `spine.rs` (#57) is the shared graph/registry/`resolve_inputs` Develop, export and
  `bench/knead` all use (#49 adds its keying-only `nicti.neutral` node and the `nicti.masks` live stage). **#380**: global Texture/Clarity/Dehaze/Saturation = `nicti.presence` (`coat::PresenceParams`, summed with local deltas in `live_suffix.wgsl`; bases via `MaskEngine::prepare`'s `presence` input) and post-crop vignette/grain = `nicti.effects` (`coat::EffectsParams`, `effects.rs` CPU reference, `present_sample.wgsl`, `RenderInputs::bind_effects`); LRC mapping in `nicti-stray`'s `develop/basic.rs` + `develop/effects.rs`; UI in `nicti-pelt`'s `develop_panel.rs` (Basic + Effects).
  **`mask/`** (#49): the `nicti.masks` stage -- `params.rs` (data model), `raster.rs`/`compose.rs`
  (CPU references, fold, AI bake key), `kernels.rs`/`guided.rs`/`bases.rs` (GPU kernels), `local.rs` (per-mask
  adjustments' uniforms + CPU twin), `engine.rs` (`MaskEngine`, the caches) → [`masking`](.claude/rules/masking/REFERENCE.md).
  See [`render-graph`](.claude/rules/render-graph/REFERENCE.md).
- **`crates/nicti-pawprint`** — the non-destructive `EditDocument`/`StageEntry` (ADR-0021), its
  canonical-JSON + blake3 stage hashing (`canonical.rs`, feeding `nicti-tapetum::graph`'s cache
  key), and append-only edit history with slider-drag compaction (`history.rs`) — promoted from
  `spikes/pawprint` (#21) and `spikes/loaf`'s DAG-generalized `hash::chain` (#44/#45). See
  [`language-and-architecture`](.claude/rules/language-and-architecture/REFERENCE.md)
- **`crates/nicti-cornea`** — `RawDecoder` extension point plus its real implementation (#41,
  landed): `LibRawDecoder`/`decode_linear` (promoted from `spikes/retina`'s `libraw_ffi.rs` —
  the FFI wrapper, `shim.cpp`/`shim.h`, `build.rs`, and the vendored `LibRaw` git submodule all
  moved here) returns a `LinearFrame` (demosaiced-but-uncorrected linear camera RGB + the metadata
  needed to color-correct it) — the input a `ColorProfile` implementation (#38/#42) needs to reach
  a working-space image. `spikes/retina` still exists for #40's own comparison tooling
  (`compare`/`sweep`/`scan`/`watch`/`dump-classic`/`dump-cfa`), now depending on this crate for its
  decode step instead of vendoring its own copy. #40's demosaic/NR algorithm choice is
  `spikes/rods`'s scope, not this crate's — `decode_linear` always uses LibRaw's own demosaic as a
  placeholder. **The LibRaw FFI/build.rs/submodule is entirely behind a non-default `libraw`
  Cargo feature** (`LibRawDecoder`, `LibRawHandle`, and `build.rs`'s C++ compile all `#[cfg]`-gated
  on it) — `nicti-lair` depends on this crate for `embedded` alone with the feature off, so it
  never needs `vendor/LibRaw` checked out or a C++ compiler; `spikes/retina` and the path-gated
  `decode-linux`/`decode-windows` CI job both enable it explicitly. A CI-only `--exclude
  nicti-cornea` on the always-on clippy/test jobs is belt-and-suspenders on top of this, not the
  actual mechanism — the feature gate is what actually keeps the C++ build off every PR that
  doesn't touch it (a workspace `--exclude` alone doesn't stop a still-unexcluded dependent like
  `nicti-lair` from pulling the dependency's build script in anyway). Also `embedded` (#22): the
  TIFF/EXIF/Nikon-MakerNote IFD walker promoted from
  `spikes/sniff` (`Walker`/`FileSource`/`SliceSource`), used at import time to extract a NEF/DNG's
  embedded T0 grid preview without a RAW decode. Trimmed vs. `sniff`'s own copy: no cold/warm
  `FILE_FLAG_NO_BUFFERING` benchmarking distinction, which stays `sniff`-only
- **`crates/nicti-lair`** — `CatalogStore` extension point plus its real implementation (#22,
  landed): `schema.rs` (SQLite migrations — `volume`/`root`/`asset`, ADR-0071; `preview`, ADR-0029;
  `edit_variant`/`edit_history`, ADR-0021; trigger-maintained `facet_counts`, ADR-0103; `asset.
  missing_since`, ADR-0024), `sqlite.rs` (`SqliteCatalog`), `scruff.rs` (the Scruff import/ingest
  pipeline: scan → stat → partial-BLAKE3 fingerprint → EXIF → T0 preview extraction → upsert, one
  bad file recorded in `IngestReport::failed` rather than aborting the run), and `patrol.rs`
  (Patrol, #24/ADR-0024, landed): the manual "Synchronize Folder"-style sync layered on top of
  Scruff — after Scruff's disk-side pass, walks the catalog side, flagging (or, if opted into,
  removing) any asset whose file has disappeared, and reporting any folder whose every asset is
  now missing; an unresolvable root touches nothing, deferring to ADR-0071's offline-volume path.
  Both are also steppable one file/asset at a time (`scruff::Ingest`/`patrol::Sync`, #55) and wired
  into Pounce (`pounce_jobs.rs`'s `IngestJob`/`SyncJob`, `Lane::Cpu`/`Priority::Background`) — see
  `catalog-engine`/`volume-identity`/`preview-tiers` topics for the design this promotes, and
  `jobs` for the Pounce wiring. **#23** extends this crate further with `hunt.rs`/`clowder.rs` —
  see the `catalog-engine` topic's own "Package contents" section (also covers **#25**'s
  `ninelives.rs`/`BackupJob`), not duplicated here. **#26** adds `carry.rs`
  (`Carry`/`resume_open_moves`, verified folder move) + `pounce_jobs::MoveJob`, wired into
  `nicti-pelt`'s Library view (**#307**: startup recovery is `ResumeMoves`/`ResumeMovesJob`, off the UI thread). **#72** adds `tier.rs`/`thumb_sidecar.rs` (archive-drive thumbnail sidecars; `Carry`'s `export_sidecars`/`Settle`, `nicti-pelt`'s `archive_drives.rs`) — see `preview-tiers`. **#304** adds `verify.rs` (`Verify`, re-hash a root against `asset.content_hash`) + `pounce_jobs::VerifyJob`. **#27** adds `larder.rs` (`Larder`, T2 preview cache;
  **#301** wires it into the loupe via `nicti-pelt`'s `t2.rs`, **#302** adds `cache_settings.rs`) — see `preview-tiers`. **#32** adds `shred.rs` (`Shred`/`resume_open_deletes`,
  delete-to-Recycle-Bin with a journal) + `pounce_jobs::DeleteJob` — see `culling`. **#57** adds
  `CatalogStore::get_master_edit`/`put_master_edit` (the Develop edit document) — see `catalog-engine`. **#52** adds the batch
  `get_master_edits`/`put_master_edits` (one transaction) — see `develop`.
- **`crates/nicti-pelt`** (#241, landed) — the production app shell ADR-0068 points to: one
  eframe/egui window sharing its wgpu device with `crates/nicti-tapetum`'s `GpuContext`
  (ADR-0016), a Tapetum-rendered frame painted via `egui_wgpu::CallbackTrait` (`viewport.rs`,
  `shaders/display.wgsl`), placeholder library/loupe/develop view routing (`app.rs`), and a real
  `nicti-lair` `SqliteCatalog` (`catalog.rs`). Named "pelt" (not the issue's own `nicti-ui`) to
  match the feline naming convention below, reusing the name from the now-deleted `spikes/pelt-*`
  research spikes (#232). Is now the real `nicti` binary's entry point (`src/main.rs` is a thin
  `nicti_pelt::run()` shim; its `windows_subsystem` attr hides the console in release, #298). **#31 (loupe, landed)**: real NEF loading, directional prefetch, and
  Fit/100% zoom — see [`gpu-gui-and-healing`](.claude/rules/gpu-gui-and-healing/REFERENCE.md)'s own
  "Package contents" section for `render.rs`/`decode_job.rs`/`loupe.rs`/`viewport.rs`/`app.rs`'s
  actual module breakdown, also covering `activity.rs` (#55, #70), `grid/` (#30) and
  [`jobs`](.claude/rules/jobs/REFERENCE.md) for Pounce. **#32 (culling, landed)**: `cull/` (marking keys, undo,
  survey/compare, delete prompt) and `grid/selection.rs` — see [`culling`](.claude/rules/culling/REFERENCE.md).
  **#49 (masks, landed)**: `mask_panel.rs` (the Masks tool: panel, gestures, overlay), `mask_edit.rs` (its
  egui-free editing ops + CPU overlay preview), `mask_tool.rs` (`MaskBakeService`: AI bakes + model
  download) and `render.rs`'s `DevelopView` mask API — see [`masking`](.claude/rules/masking/REFERENCE.md).
  **#353 (baked-alpha disk tier + pre-bake, landed)**: `stash.rs` (`AlphaFetchJob`/`AlphaStoreJob`, Larder keyed entries via `nicti-lair`'s `larder.rs` `put_keyed`/`get_keyed`; codec `AiAlpha::encode`/`decode` in `nicti-tapetum`'s `mask/engine.rs`) and `prebake.rs` (`PrebakeService`, hooked from `run_knead_command`/`poll_prebake` in `app.rs`) — see `masking`.
  **#57 (export, landed)**: `export/` (`jobs.rs` the run, `dialog.rs`, `presets.rs`, `sink.rs`) and
  Develop autosave to the catalog — see [`export`](.claude/rules/export/REFERENCE.md).
  **#52 (presets/copy/paste/sync, landed)**: `knead/` — see [`develop`](.claude/rules/develop/REFERENCE.md).
  **#145 (rendered previews, landed)**: `eyeshine.rs` (render job + display rule), `preview_settings.rs`, `export/render_core.rs` — see [`preview-tiers`](.claude/rules/preview-tiers/REFERENCE.md).
  **#354 (export masks, landed)**: `export/render_core.rs` `render_live_frame` + `nicti-tapetum`'s `MaskEngine::for_export`/`Renderer::render_live_from`; baked AI alphas via `ExportEnv.larder` — see `masking`.
  **#425 (LightCraft look, landed)**: `fur/` -- app-wide theme + Inter fonts (`tokens.rs`, applied in `PeltApp::new`), the Lightroom-style `SliderSpec`/`slider` row (`slider.rs`; Develop panel only so far, mask/heal/export still stock `egui::Slider`), section/segmented/icon-button chrome (`widgets.rs`) and code-drawn `Icon`s (`icons.rs`); ported from LightCraft, provenance in `docs/licensing.md`.
  **#319 (managed previews, landed)**: `preview_color.rs` + `nicti_calico::source_transform` convert JPEG thumbnails/T0/T2 to the monitor profile — see [`color`](.claude/rules/color/REFERENCE.md).
- **`spikes/pawprint`** (#21/ADR-0021) → [`language-and-architecture`](.claude/rules/language-and-architecture/REFERENCE.md)
- **`spikes/glint`** (#16/ADR-0016) → [`gpu-gui-and-healing`](.claude/rules/gpu-gui-and-healing/REFERENCE.md)
- **`spikes/sniff`** (#28/#29/ADR-0029) → [`preview-tiers`](.claude/rules/preview-tiers/REFERENCE.md)
- **`spikes/retina`** (#37/ADR-0037; own decode step promoted to `crates/nicti-cornea` in #41,
  see that bullet above) → [`raw-decoder`](.claude/rules/raw-decoder/REFERENCE.md)
- **`spikes/homing`** (#71/ADR-0071, Windows-only, unverified in this sandbox) →
  [`volume-identity`](.claude/rules/volume-identity/REFERENCE.md)
- **`spikes/calico`** (#38/ADR-0038) → [`color`](.claude/rules/color/REFERENCE.md)
- **`spikes/shed`** (#61/ADR-0061) → [`lrc-migration`](.claude/rules/lrc-migration/REFERENCE.md)
- **`spikes/litter`** (#33/ADR-0033) → [`culling`](.claude/rules/culling/REFERENCE.md)
- **`spikes/squint`** (#34/ADR-0034) → [`culling`](.claude/rules/culling/REFERENCE.md)
- **`spikes/rosette`** (#35/ADR-0035) → [`culling`](.claude/rules/culling/REFERENCE.md)
- **`spikes/rods`** (#40/ADR-0040) → [`denoise`](.claude/rules/denoise/REFERENCE.md)
- **`crates/nicti-scent`** (#60, promoted from `spikes/scent`) — XMP interop with LRC: field mapping,
  packet patcher, sidecar/embedded I/O, conflict rule. Catalog glue is `nicti-lair`'s
  `scent_sync.rs` (hooked into `scruff.rs`), write-back + review panel is `nicti-pelt`'s
  `xmp_sync.rs` → [`xmp-interop`](.claude/rules/xmp-interop/REFERENCE.md)
- **`crates/nicti-stray`** (#62, ADR-0062) — Lightroom Classic catalog import: `LrcImportJob` (ingest →
  match → keywords/collections → markers/variants/develop translation → provenance); UI in
  `nicti-pelt`'s `lrc_import.rs` → [`lrc-migration`](.claude/rules/lrc-migration/REFERENCE.md)
- **`spikes/pupil`** (#99/ADR-0099) → [`develop`](.claude/rules/develop/REFERENCE.md)
- **`spikes/purr`** (#53/ADR-0053) → [`develop`](.claude/rules/develop/REFERENCE.md)
- **`spikes/crouch`** (#54/ADR-0054) → [`jobs`](.claude/rules/jobs/REFERENCE.md)
- **`spikes/prey`** (#56/ADR-0056) → [`export`](.claude/rules/export/REFERENCE.md)
- **`bench/whisker`** (workspace member) — benchmark tooling for #43, not a production crate; same
  "don't build on top of it" caveat as a spike
- **`bench/knead`** (#45 PR4, workspace member) — real-NEF golden/perf harness for Tapetum, not a
  production crate; see [`render-graph`](.claude/rules/render-graph/REFERENCE.md). `spikes/loaf`
  (#44/ADR-0044), which this replaces the `bench` subcommand of, is deleted — its
  `graph.rs`/`hash.rs`/`cache.rs`/`prefetch.rs` were already promoted into `crates/nicti-tapetum`/
  `crates/nicti-pawprint`, see that same REFERENCE.md.
- **`packaging/windows/nicti.nsi`** + **`.github/workflows/release.yml`** (#249/ADR-0249) — the
  per-user NSIS installer and its tag-triggered build/sign/publish pipeline; not a crate. See
  [`release`](.claude/rules/release/REFERENCE.md).

The root placeholder binary crate (`src/main.rs`) still exists only so CI/lint tooling has
something real to run against; it is not the shipping v1 target's home yet.

## Development workflow

**Always pull main before starting any work:**
```bash
git checkout main && git pull origin main
```

**Use worktrees for feature branches** — never work directly on the main checkout:
```bash
git worktree add ../nicti-wt-myfeature -b feat/myfeature
```

Compile-feedback loop: `cargo check`, not `cargo build` — skips codegen/linking. Full
`cargo build`/`cargo test` only when the binary or test execution is actually needed. (This
section's efficiency rules are agent-specific; a human contributor doesn't need them — see
CONTRIBUTING.md instead.)

## Issue lifecycle — assign + label the moment work starts

The instant a worktree/branch is created for a GitHub issue — before the first edit, not after —
run, for that issue number `N`:

```bash
gh issue edit N --repo jordanfelle/nicti --add-assignee jordanfelle --add-label in-progress
```

(`in-progress` is a real label in this repo, not a placeholder — create it with `gh label create`
if it's ever missing.) When the PR merges, `gh issue close N` and drop the `in-progress` label in
the same turn as the merge — don't leave it dangling on a closed issue.

This is this repo's equivalent of the Shutterpaws/Scrumboy board-sync rule (see the launch-root
`~/git/CLAUDE.md`'s ticket-lifecycle rule) — same reasoning, adapted to plain GitHub Issues
instead of a Scrumboy board: an issue sitting unassigned and unlabeled while a branch is actively
open on it is invisible to anyone (including a future session) checking what's already spoken
for. Missed once on #38 (2026-09-26) — the worktree and PR were created without this step.

## PR conventions

- Plain GitHub Issues/PRs — a bare `#N` in a commit/PR/ADR/issue body means a GitHub issue/PR in
  this repo. See CONTRIBUTING.md's Issue conventions section for the `**Part of:**`/
  `**Blocked by:**` link format.
- **PR titles**: a plain summary sentence.
- **This is a public repo — never include a `Claude-Session:` trailer or a "Generated with Claude
  Code" footer** in commit messages or PR descriptions here. `Co-Authored-By:` is fine to keep;
  strip the session-link trailer and the generated-with footer entirely. A session link on a
  public repo exposes the conversation transcript to anyone who reads the commit/PR.

## Adversarial review before opening any PR

Anything substantial goes through an adversarial review loop before it's called done — review →
verify each finding → fix → re-review if the fixes were non-trivial. Small, low-risk changes may
skip it (a copy tweak, a comment, a version bump, a one-line config edit).

**Run this BEFORE opening the PR, not after.** Spawn a fresh agent (no explicit `model` override)
pointed at the branch's diff, prompted hostilely: assume the author was overconfident, name
concrete areas to attack, require a CONFIRMED/SPECULATIVE split with a failing scenario per
finding. Verify every finding yourself before acting on it, and when a finding names one instance
of a pattern, grep for its siblings instead of fixing only the one named.

**Post the outcome as a PR comment before merge**, not just the local pass/fix cycle: state what
ran, the CONFIRMED/SPECULATIVE split (or "no findings"), and how any real finding was resolved.

## Testing

See CONTRIBUTING.md's "Building, testing, linting" section for the exact commands (they mirror
CI's exclude flag for `retina`) and why `--workspace`/`--all` are required — the
root `Cargo.toml` is both the workspace root and a real package (`nicti`), not a virtual manifest,
so a bare `cargo test`/`cargo clippy` (no `-p`/`--workspace`) or a bare `cargo fmt --check` (no
`--all`) silently only checks the root crate and skips `spikes/*`/`bench/whisker` entirely — this
exact gap produced a false-negative "clean" local result once on a real PR whose CI then failed
`cargo fmt` on six files in the (now-deleted) `spikes/den` (fixed alongside #19/ADR-0019).

## CI

GitHub Actions, GitHub-hosted runners only (no self-hosted infra); Windows is the required
(blocking) platform (#17), not Linux. Full gotchas (required-check-gate history #166, the
now-deleted GUI-framework spikes' path-gating #127/#232, the removed CodeQL workflow #281, the CI-duration
watcher #160) moved to [`ci`](.claude/rules/ci/REFERENCE.md) (#34's own PR, to keep this file under
its line-count gate) — read that before touching `.github/**` or a CI-adjacent spike.
