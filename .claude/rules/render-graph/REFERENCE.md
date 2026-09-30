---
paths:
  - "crates/nicti-tapetum/**"
  - "crates/nicti-pawprint/**"
  - "bench/knead/**"
  - "docs/adr/0044-stage-cached-render-graph.md"
---

# Render Graph (Tapetum) — Quick Reference

Full reasoning/history: `docs/decisions/render-graph.md`.

- **Render graph (#44)** — `docs/adr/0044`: **Proposed**, kernel-level decision rules measured
  clean on real RTX 5080 hardware; full pipeline landed in #45 (4/4 PRs: pawprint+graph,
  GpuContext/frames/execution trait, real decode/live-suffix/geometry, tiled full-res path + real-
  NEF goldens via `bench/knead`) — the hero-scenario screen-capture pass is a separate follow-up
  issue (filed when #45 itself closes).
- **Stage order**: baked prefix `decode → demosaic (AHD) → denoise (SCUNet) → lens correction →
  heal/remove` → (#49, amends ADR-0044) the neutral render for AI masks taps **post-lens, pre-heal**, a
  keying-only node that is not executed → one
  fused live dispatch `WB → HueSatMap → exposure → tone → vibrance → HSL → local corrections (masks)` → crop as
  an affine sample pass over the live suffix's own output only.
- **Cache key**: `nicti_pawprint::chain` generalizes ADR-0021's flat one-upstream chain to a DAG;
  `nicti_tapetum::graph::RenderGraph::cache_key`/`invalidated_bakes`/`set_own_hash` are the tested,
  structural proof that a live-slider change triggers zero bake dispatches — **landed in #45**
  (promoted from `spikes/loaf/src/graph.rs`, now deleted; `set_own_hash` is the "update a node in
  place" API the spike's own tests lacked).
- **Cache tiers**: `nicti_tapetum::cache::Tier<V>` — byte-budgeted LRU, generic over `size_of`,
  backs VRAM/RAM/disk. Full-res (8280×5520) frame ≈349MB, screen-res (3840 long edge) ≈75MB.
  **Landed in #45** (promoted from `spikes/loaf/src/cache.rs`, now deleted, with its self-
  documented `O(n)` `touch` replaced by an `O(log n)` generation-counter `BTreeMap`). The disk-tier
  codec (zstd/lz4) stayed with #190, which needs real baked output to choose against.
- **Disk compression**: zstd/lz4 both round-trip; synthetic-gradient ratios (~3688×/~247×) are
  **not** a real-photo promise — see follow-up #190.
- **Masks (#49)**: `nicti-tapetum/src/mask/` -- the `nicti.masks` Live stage (in the fused live dispatch, tail),
  the keying-only `nicti.neutral` Baked node (upstream `nicti.lens`, **pre-heal**, not in the baked chain), the
  guided-filter refine (`mask/guided.rs`, CPU reference + GPU kernels) and `Renderer::render_baked` (bakes +
  submits the chain so the mask engine can read the baked frame; the following `render` finds it cached).
  See the `masking` topic.
- **Real RTX 5080 numbers** (GPU-timestamp only): live-suffix 0.375ms p50/0.392ms p95 (screen res),
  1.767ms p50/3.924ms p95 (full res, matches ADR-0016's own comparable figure within a few
  percent). Present/sample (crop): ≤1.611ms p95 even at full res.
- **Gotcha caught this pass**: rebuilding the wgpu pipeline+buffers inside a timed loop (instead of
  a persistent `*Kernel`) measured ~1000x too slow — same class of bug `spikes/glint::
  LiveChainKernel` already documents. Always benchmark against a persistent kernel
  (`nicti_tapetum::stages::{DecodeKernel,LiveSuffixKernel,CropKernel}` in production, `bench/knead`'s
  own subcommands are the real replacement for what this bullet used to warn against re: the now-
  deleted `spikes/loaf`'s own `run_live_suffix`/`run_present_sample`/`run_box_filter` helpers),
  never rebuilding one inside a timing loop.
- **Bake-scheduler sim** (`spikes/loaf/src/sim.rs`, now deleted): one serial bake worker, nearest-
  to-cursor-first. Real costs (decode 1.7s, denoise 50.9s/ADR-0040) show full-res-first denoise
  cannot keep the hero scenario's 50-image bake queue ahead of a walking cursor
  (`stale_at_arrival = 50/50`) — needs a screen-res-first denoise pass, follow-up #189 (the
  finding stands even though the sim code itself is gone; re-derive it if #189 needs to re-run
  this class of simulation).
- **Lens correction**: confirmed in the baked prefix, before ADR-0038's color pipeline (#191) — CA
  correction is calibrated in camera-native RGB channel space, which no longer exists once
  cam→XYZ mixes channels, so it must precede that matrix. Bakeability isn't a free second
  argument, though: ADR-0061's LRC schema mapping shows LRC's own Lens Corrections panel has
  manual distortion/vignette/defringe sliders, so this rests on the same "not a live-drag slider"
  choice ADR-0050 made for heal/remove, for #39 to confirm once it designs that UX. #39's other
  scope (correction-data source, lens coverage) stays open too.
- **Full-res tiling** (#45 PR4, landed): `nicti_tapetum::tile::TilePlanner::plan` partitions an ROI
  into non-overlapping core tiles (each padded by a chain-wide halo, clamped to the source
  extent), bounded by both a max-dimension and a max-staging-bytes budget; `calibrate_core_size`
  adjusts tile size between renders toward ADR-0054's per-chunk target. `TiledRender` re-runs only
  the geometry (crop/present-sample) stage once per tile against the one already-baked+live-
  suffixed full-frame texture -- every baked stage and the fused live dispatch still run exactly
  once regardless of tile count. Proven bit-for-bit against a whole-frame render, including a
  fractional-offset (non-integer pan) transform. **No stage in this crate yet reads a genuinely
  windowed per-tile input** (`present_sample.wgsl` always samples the complete source texture), so
  the halo mechanism, while real and tested (padding/clamping math), has no currently-observable
  effect on output correctness -- kept for the first future stage (an AI/box-filter tile, matching
  the original design sketch's `BlendMode::Feather`) that does.
- **Real-NEF harness**: `bench/knead` (#45 PR4) replaces `spikes/loaf`'s own `bench` subcommand --
  a real `nicti_cornea::LibRawDecoder` decode through the full pipeline at neutral default params,
  `nicti_prowl::golden::Render`-backed golden compare/bless, and `bench-live-suffix`/
  `bench-present`/`bench-tile` perf subcommands via `nicti_prowl::perf::Protocol`. Gated on the
  ref-10k manifest verifying clean first (`NICTI_REF10K` env var, no fixtures committed to this
  repo). Verified once, ad hoc, against a real Nikon Z8 NEF: decode + full render produced a
  recognizable, correctly color-matrixed photo (not the uniform green cast the cam_xyz direction
  bug below produced) across multiple row-strips.
- **cam_xyz direction bug (caught in #45 PR4's real-hardware pass)**: `LibRaw`'s own `cam_xyz`
  field is **XYZ -> camera** (confirmed against `cam_xyz_coeff` in the vendored LibRaw source, not
  just its header comment), the opposite of what "camera to XYZ" reads as. Using it un-inverted
  produced a uniform, badly-wrong green cast on a real photo while every existing test (synthetic
  fixtures only) stayed green -- `camera_to_working_space_matrix` now inverts it
  (`color::mat3_invert`) before composing. A real decode is what caught this; no synthetic fixture
  exercised the direction.
- **Decode row-strip splitting** (#45 PR4): a full-res 8280x5520 frame's packed decode pixel
  buffer (~261.5MB) exceeds a software (lavapipe) adapter's real
  `max_storage_buffer_binding_size` (128MiB exactly, measured in this sandbox) well before real
  hardware's own limit -- `stages::rows_per_strip` splits the upload into per-strip buffers/
  dispatches, each writing to the correct absolute row range of the one shared output texture.
- **Open follow-ups**: #189 (screen-res-first denoise scheduling), #190 (real-photo compression
  ratio, blocked on #45).
- **#46 slice 1/5 (typed params + Basic + WB, landed)**: `coat.rs` adds typed, `#[serde(default)]`
  params structs per live stage (`WbParams`/`ExposureParams`/`ToneParams`/`VibranceParams`),
  parsed from a `StageEntry`'s untyped JSON via `coat::parse`/`coat::default_value` -- until this,
  `BasicStage::default_params`/`cache_contribution` existed but were never wired to a graph node's
  `own_hash`, so a slider change never actually invalidated anything (every test built its own
  `own_hash` by hand, e.g. `blake3::hash(id.as_bytes())`). `graph::RenderGraph::apply_document`
  closes that gap: given an `EditDocument` + `StageRegistry`, it sets every graph node's `own_hash`
  from `RenderStage::cache_contribution`, falling back to a stage's own `default_params()` when the
  document has no entry. `LiveSuffixKernel::set_params` now takes one `stages::LiveParams` struct
  (matrix + `ExposureParams`/`ToneParams`/`VibranceParams`) instead of four positional floats --
  `bench/knead` updated to match. `color::apply_tone` grew from a single `contrast` float to the
  full Basic panel (contrast/highlights/shadows/whites/blacks, `ToneParams`) -- whites/blacks are a
  linear endpoint remap in perceptual space (positive whites moves the white point *down*,
  brightening highlights; negative blacks moves the black point *up*, crushing shadows -- matching
  LRC's own slider direction, not a same-sign offset), highlights/shadows are a luminance-weighted
  additive shift via `smoothstep` masks -- a deliberately *global* v1 approximation of LRC's own
  locally-adaptive highlights/shadows (a local-adaptive follow-up + a real LRC-export comparison
  are #46's own later slices, once #202's reference-machine run exists). WB temp/tint
  (`color::wb_gains_for_temp_tint`) estimates gains from this frame's single `cam_xyz` matrix (a
  Planckian-locus xy approximation, computed in `f64` to avoid clippy's `excessive_precision` on
  the published coefficients, cast to `f32` in the result) rather than #42/ADR-0038's dual-
  illuminant DNG solve -- see the `color` topic's own REFERENCE.md for that scope split. `tint`
  applies as a green-gain multiplier in the as-shot path (no chromaticity to shift without an
  explicit temp) but as a perpendicular xy shift in the temp-override path -- two different
  approximations, both documented, not accidentally inconsistent.
- **#46 completion (remaining slices, all in one PR)**: adds four more live stages --
  `TONE_CURVE`/`HSL`/`SHARPEN`/`NOISE_REDUCTION` -- plus a live histogram, before/after compare,
  and a provisional Auto-tone button.
  - **Tone Curve + HSL** fuse into the same per-pixel `live_suffix.wgsl` dispatch every other live
    stage already shares. `color::build_tone_curve_lut` builds a 256-entry monotone-cubic (PCHIP)
    LUT from the 4 region sliders (fixed split points at x=0.25/0.75, per `ToneCurveParams`'s own
    doc comment), uploaded as `array<vec4<f32>, 64>` in `LiveUniforms` (WGSL's uniform-address
    -space array rules force the vec4 packing -- same reason `LiveUniforms` was already vec4-only).
    `color::apply_hsl` is HSV-based, not canonical HSL (`l=(max+min)/2` breaks for this crate's
    unbounded-above-1.0 linear working-space values) -- 8 bands, each a raised-cosine (Hann) hue
    -weight window (`hsl_band_weight`, +/-45 degrees around a 45-degree-spaced center), hue/sat
    adjusted in HSV space, luminance shifted separately in `apply_tone`'s own cube-root perceptual
    space.
  - **Sharpening/Noise Reduction are a second, separate multi-pass path** inside the *same*
    `LiveExec::encode` call (`stages.rs::LiveSuffixKernel::encode`) -- unlike every other live
    stage, a blur needs neighboring pixels, so it can't fuse into the per-pixel shader.
    `DecodeExec` already sets the "one `encode` call, several internal compute passes" precedent
    for a baked node; ADR-0044's "one fused live dispatch" invariant is about the render graph's
    own dispatch *count*, not about internal pass count. Fast path: when both `SharpenParams`/
    `NoiseReductionParams` are at their default (`is_noop()`), `encode` runs the original single
    per-pixel dispatch straight to `output` -- today's exact pre-#46 cost, paid only when a Detail
    -panel edit is actually active. Otherwise: per-pixel -> `stage_a`, then `detail_blur.wgsl` (one
    shared separable-Gaussian pipeline, `direction` uniform picks horizontal/vertical) runs twice
    per sigma (NR's fixed `NR_BASE_SIGMA` and Sharpen's own `radius_px`, both scaled by a new
    `LiveParams::pixel_scale` -- render extent / source extent, so a screen-res preview and a
    full-res export sharpen the same image *content*), then `detail_combine.wgsl` implements
    `detail::apply_detail_rgb`'s luma/chroma split (NR-luminance and Sharpen both operate on luma
    only via `detail::apply_detail`'s single-channel formula, avoiding color fringing;
    NR-color is a separate ungated chroma-only lerp toward its own blurred chroma).
    **Gotcha, real**: each of the 4 blur dispatches (H/V x NR/Sharpen sigma) needs its own
    dedicated uniform buffer, never a shared one written+rewritten between dispatches --
    `gpu.queue.write_buffer` calls all land before the *whole render's* one `queue.submit`
    (not per-dispatch), so two dispatches recorded in the same encoder sharing one buffer would
    both end up reading only the *last* write at execution time, not a snapshot each. Kernel
    intermediate textures (`stage_a` and each blur's H/V output) are freshly allocated per
    `encode` call (`FrameTexture::new`, no pooling) -- this only runs on an already-live
    -recomputed frame, not a hot bake-tier path, so the allocation cost is fine.
  - **Histogram**: `histogram.rs`'s `Histogram`/`from_display_pixels` bins a display-encoded
    (`geometry::output_encode`) RGBA buffer into 256 R/G/B/luma buckets, with nearest-rank
    percentile/mean/fraction-below/fraction-above matching `spikes/pupil::histogram`'s own
    semantics. `nicti-pelt`'s `DevelopView::histogram` computes it via a plain CPU `read_frame`
    readback -- ADR-0016's "never a full-frame host<->device round-trip in the hot path" concern
    is about a real full-res photo; this view still renders a small synthetic frame (#31 hasn't
    landed a real NEF yet), so the readback cost here is trivial. A throttled/GPU histogram is a
    follow-up once #31 replaces the synthetic frame with a real one.
  - **Auto tone (provisional)**: `perk.rs::estimate` ports `spikes/pupil::heuristic`'s candidate A
    (percentile heuristic) onto `histogram::Histogram`, outputting `ExposureParams`/`ToneParams`
    directly (normalized to `coat.rs`'s -1.0..=1.0 convention). Explicitly provisional pending
    #202's reference-machine run picking a final candidate (ADR-0099); swapping to candidate B
    only changes `perk::estimate`'s own body. `spikes/pupil` is untouched -- still #202's own
    measurement tool, not depended on here (spikes stay self-contained).
  - **Before/after**: `DevelopView::show_before` renders with an empty `EditDocument`, reusing
    `apply_document`'s existing "no entry -> stage's own default" fallback for the whole document
    at once, rather than a second code path.
  - **UI**: `nicti-pelt`'s `develop_panel.rs` is new -- Basic/Tone Curve/HSL/Detail sections, a
    painted histogram, Auto and before/after buttons, double-click-to-reset per slider. `render.rs`'s
    `DevelopView` now owns a real in-memory `nicti_pawprint::EditDocument` (catalog persistence is
    #31's scope, once a real asset exists to persist against) and a `StageRegistry` covering every
    stage id `build_graph` adds.

- **Crop/straighten/auto-level (#47)** — `docs/adr/0047`: **Accepted**. Rotation composes into the
  existing affine crop transform (`geometry::Affine2D::crop_and_rotate`/`affine_for_crop`), no new
  stage, no shader change (`present_sample.wgsl` already read a full 2x3 affine). Both the manual
  Ctrl-drag-a-reference-line gesture (`nicti-pelt::render::DevelopView::straighten_from_drag`) and
  the Canny/Hough auto-level button (`nicti_tapetum::autolevel::detect_level_angle`, via
  `imageproc` -- chosen over `opencv-rust`, both primitives confirmed present in its actual source)
  write into the same `coat::CropParams::rotation_degrees` field via `straighten_delta_degrees`'s
  shared sign-convention math (clockwise-positive, y-down; locked in by a GPU/CPU parity test with
  a real 12-degree rotation, not just the pre-#47 translation-only case). `CropParams` (x/y/width/
  height/rotation, `Default` = full-frame no-op) is a real cached `StageEntry`, wired through
  `stages::crop_stage`'s `default_params`. The interactive Develop preview's own canvas doesn't
  resize to the crop rect (deliberate, matches real editor UX); the already-landed (#45 PR4)
  `tile::TiledRender`/`MemorySink` full-res export path already supports an arbitrary output
  extent, so the crop rect *is* honored end-to-end at export time. Two follow-ups filed: #272
  (live-preview canvas resize) and #273 (real-photo Canny/Hough threshold tuning,
  `needs-physical-testing`). Failure/low-confidence behavior of auto-level: ADR-0101 (see the
  `develop` topic) — a low-confidence angle is skipped with a hint, not applied.

## Package contents

- **`crates/nicti-tapetum`** (#45, landed) — `coat.rs` (#46: typed `WbParams`/`ExposureParams`/
  `ToneParams`/`ToneCurveParams`/`VibranceParams`/`HslParams`/`SharpenParams`/
  `NoiseReductionParams`, `#[serde(default)]` so a missing/unrecognized field always parses
  to something sane — see this file's own "#46 slice 1/5"/"#46 completion" bullets above),
  `detail.rs` (#46: `gaussian_kernel`/`gaussian_blur`/`apply_detail`/`apply_detail_rgb` — the CPU
  reference the GPU Sharpen/NR multi-pass proves itself against), `histogram.rs` (#46: the live
  -histogram bin counter), `perk.rs` (#46: the provisional Auto-tone port of `pupil::heuristic`),
  `graph.rs` (the stage DAG,
  `RenderGraph`/`StageNode`/`StageKind`, promoted from `spikes/loaf/src/graph.rs`, now deleted, with
  a persistent memoized cache key and the `set_own_hash` update API the spike lacked, plus #46's
  `apply_document` — the "wire an `EditDocument`'s params into the graph's own_hash" step that
  didn't exist before), `cache.rs` (`Tier<V>`, promoted
  from `spikes/loaf/src/cache.rs` with an `O(log n)` touch), `prefetch.rs` (`priority_order`,
  promoted as-is), `gpu.rs` (the shared `GpuContext`, one wgpu device/queue per ADR-0016, adapted
  from `spikes/glint/src/gpu.rs`), `frame.rs` (`FrameTexture`, always `Rgba16Float` — a core format
  needing no `SHADER_F16` feature, unlike a raw f16 storage buffer — plus a `FramePool` free-list,
  and the shared production `read_frame` CPU readback every caller that needs a `FrameTexture`'s
  pixels reuses), `tile.rs` (`TilePlanner`/`TiledRender`/`MemorySink`, #45 PR4's output-side tiling
  — see this file's own "Full-res tiling" bullet below), and `renderer.rs` (the real `RenderStage`
  execution trait extension — `kind`/`default_params`/
  `cache_contribution`/`impl_version` — and the graph-driven `Renderer`: a `Baked` node dispatches
  its `BakedExec` only on a `cache::Tier` miss; every `Live`/`Geometry` node fuses into exactly one
  `LiveExec`/`GeometryExec` dispatch each, keyed by a composite hash of its constituent nodes'
  cache keys plus its own upstream stage's key (the baked chain's for live, the live composite's
  for geometry — otherwise an empty `live_nodes`/`geometry_nodes` slice, a real reachable state
  before a `Live`-kind stage exists at all, would hash the same regardless of what upstream
  produced), sorted before hashing so it doesn't depend on the caller's slice order the way
  `graph::RenderGraph::cache_key` already doesn't for upstream ids). Proven against counting mock
  stages: a live-only change costs 0 bake dispatches, a crop-only change costs 0 bake *and* 0 live
  dispatches, an identical re-render costs 0 of anything, undoing back to a prior baked state is a
  cache hit (true as long as `cache::Tier`'s byte budget hasn't evicted that entry meanwhile — a
  performance guarantee, not a correctness one: a wrongly-evicted entry just costs a redundant
  re-dispatch, never a wrong image, since a hit is only ever reused when `cache_key` already
  proves the state matches), and a baked-stage change rebakes exactly its downstream set — each
  test derives its expected count from `RenderGraph::invalidated_bakes` itself, not a hand-picked
  number. `gpu::GpuContext::new` with an explicit `GpuPreference::Backend` errors if no adapter
  matches that exact backend, rather than silently falling back to a different one (only `Auto`
  falls back).
  - **`color.rs`/`geometry.rs`/`stages.rs`** (#45, landed): the concrete decode/live-suffix/
    geometry pipeline wired to a real `nicti_cornea::LinearFrame`. `stages.rs` defines every stage
    id (`nicti.decode`/`nicti.demosaic`/`nicti.denoise`/`nicti.lens`/`nicti.heal`/`nicti.wb`/
    `nicti.working_space`/`nicti.exposure`/`nicti.tone`/`nicti.vibrance`/`nicti.crop`) and each
    stage's `*Kernel` (built once, `set_params`/`set_transform` writes a uniform buffer per
    render rather than rebuilding the pipeline): `DecodeKernel` uploads `LinearFrame.pixels`
    (widened to u32 per channel -- WGSL has no u16 storage-buffer element type) and runs
    `normalize.wgsl` (black/`cblack` subtraction, scale to ~[0,1]); demosaic/denoise/lens/heal are
    `PassthroughExec` (a plain texture copy -- LibRaw already demosaiced, denoise/lens/heal have
    no algorithm yet, #40/#39/#51); `LiveSuffixKernel` runs `live_suffix.wgsl`, fusing WB (as-shot
    `cam_mul`, or #46's manual temp/tint override) + camera→XYZ(D50)→ProPhoto
    (`color::camera_to_working_space_matrix`, folded into one 3x3 on the CPU) + exposure + #46's
    full Basic-panel tone (contrast/highlights/shadows/whites/blacks, `color::apply_tone`) + a
    luma-preserving vibrance boost, staying in linear ProPhoto RGB (the working space) end to end;
    `CropKernel` runs `present_sample.wgsl`, a bilinear affine sample. **Deliberate simplification
    vs. the original design sketch**: no `HueSatMap`/`LookTable` bindings are reserved (#42's
    DCP-profile scope) -- unused texture bindings with no real content would be exactly the
    half-finished scaffolding this repo's conventions ask to avoid; `color.rs`'s own doc comment
    covers the tradeoff. Every kernel has a GPU-vs-CPU parity test against a CPU reference, plus
    one end-to-end test wiring the whole chain through `Renderer` and checking both dispatch
    counts and actual output pixels. These tests skip when no `wgpu` adapter is available.
    `DecodeExec` splits a full-res upload into row-strips (`stages::rows_per_strip`) and
    `color::camera_to_working_space_matrix` inverts `cam_xyz` before use -- see this file's own
    "Decode row-strip splitting"/"cam_xyz direction bug" bullets above for why.
- **`crates/nicti-pawprint`** (#21/#44/#45, landed) — `EditDocument`/`StageEntry`/`history`
  (promoted from `spikes/pawprint`) plus `canonical::{hash_value, chain}` (merging pawprint's
  original one-upstream `hash_stage`/`cache_key` with `spikes/loaf/src/hash.rs`'s DAG-generalized
  `chain` — `nicti_tapetum::graph` is the DAG consumer of this hashing scheme).
- **`bench/knead`** (#45 PR4, workspace member, not production, same caveat as `bench/whisker`) —
  real-NEF golden/perf harness; see this file's own "Real-NEF harness" bullet above for what it
  does. Depends on `nicti-cornea` with the `libraw` feature always on (a real decode is its whole
  point), so it's path-gated into the `decode` CI job alongside `nicti-cornea`/`retina`, not the
  always-on jobs.
- `spikes/loaf` (#44/ADR-0044's stage-cached render-graph research) **is now deleted** (#45 PR4) --
  its `graph.rs`/`hash.rs`/`cache.rs`/`prefetch.rs` were already promoted into `crates/nicti-tapetum`
  /`crates/nicti-pawprint` above; its `geometry.rs`/`gpu.rs` (the three `wgpu` kernels this file's
  own "Real RTX 5080 numbers" bullet cites) were superseded by the real `stages.rs` kernels once
  those existed; `refine.rs` was always a copy of `spikes/siamese`'s own (which still exists
  independently); `sim.rs`'s finding is captured in this file's own "Bake-scheduler sim" bullet
  above. See `docs/research/loaf-render-graph.md` for the original research writeup (kept, not
  deleted, as the historical record).
- **Live suffix now also binds a DCP camera profile (#42)**: `live_suffix.wgsl` bindings 3/4 are the
  HueSatMap/LookTable 3D textures and 5 the sampler, **always bound** (1x1x1 dummies when absent —
  the auto-derived bind-group layout includes any binding the shader references), with
  `LiveUniforms.profile0/profile1` flags + baseline-exposure multiplier;
  `LiveParams.camera_profile: Option<Arc<ProfileSolution>>` carries the tables (the caller puts the
  solution's `camera_to_working` into `working_space_matrix`); `set_params` re-uploads a table only
  when its fingerprint changed. The choice enters the cache key via `nicti.working_space`'s
  `CameraProfileParams.content_hash`. See the `color` topic.

