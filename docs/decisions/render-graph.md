## Render graph (Tapetum)

Covers the stage-cached render graph itself: the bake/live boundary, the cache-key scheme, cache
tiers, crop-as-geometry, mask refine reuse, and the bake-scheduling contract it hands to Pounce.

- **Render graph**: `docs/adr/0044-stage-cached-render-graph.md` — **Proposed**, kernel-level
  decision rules measured clean on the real reference RTX 5080; the full end-to-end pipeline and
  the hero scenario's own screen-capture pass are #45's build, not this pass's. Stage order:
  `decode → demosaic (AHD) → denoise (SCUNet) → lens correction → heal/remove` (baked prefix,
  linear camera RGB) → `neutral render → AI mask bake` (a separate neutral branch, decoupled from
  live sliders) → `WB → HueSatMap → exposure → tone → vibrance → mask compose/apply` (one fused
  live dispatch) → crop/rotate/zoom/pan (an affine sample pass reading only the live suffix's
  output, structurally incapable of touching a baked or live node).
- **Cache key**: generalizes ADR-0021's flat per-stage `blake3(upstream_hash ‖ own_hash)` chain to
  an arbitrary DAG (`spikes/loaf/src/hash.rs::chain`, `graph.rs::RenderGraph::cache_key`) — a
  node's key changes iff its own params changed or any upstream node's did, proven both directions
  in `graph.rs`'s tests. Changing a live-suffix stage's params triggers **zero** bake dispatches, a
  structural graph property, not a timing coincidence. **Landed in #45** as
  `crates/nicti-pawprint::chain` (the hashing) plus `crates/nicti-tapetum::graph::RenderGraph` (the
  DAG), with `set_own_hash` added as the missing "update a node in place" API — the spike's own
  cache-key test had no way to change one node without reconstructing the whole graph.

  **Also landed in #45**: the real `RenderStage` execution trait (`crates/nicti-tapetum::renderer`)
  and the graph-driven `Renderer` that dispatches against it. A `Baked` node's `BakedExec` runs
  only on a `cache::Tier` miss; every `Live`/`Geometry` node fuses into exactly one dispatch each,
  keyed by a composite hash chaining its constituent nodes' cache keys — literally the same
  `RenderGraph::cache_key` this ADR's own decision rule #1 is about, so the dispatch-count
  invariant is a direct consequence of the already-proven graph semantics rather than a second,
  independently-argued claim. Proven against counting mock stages (no real shader exists yet):
  live-only change → 0 bake dispatches; crop-only change → 0 bake and 0 live dispatches;
  unchanged re-render → 0 dispatches of any kind; undo → a cache hit; a baked-stage change →
  rebakes exactly its `invalidated_bakes` set. `gpu.rs` (the shared `GpuContext`, adapted from
  `spikes/glint`) and `frame.rs` (`FrameTexture`, always `Rgba16Float`) are the concrete GPU
  plumbing this runs against — real wgpu, exercised against the lavapipe software adapter in CI.

  **Also landed in #45**: the concrete decode/live-suffix/geometry stages themselves
  (`crates/nicti-tapetum::{color,geometry,stages}`), wired to a real `nicti_cornea::LinearFrame`.
  Decode uploads the frame's pixel data and runs a normalize pass (black/`cblack` subtraction,
  scale to ~[0,1]); demosaic/denoise/lens/heal are plain texture-copy passthroughs (LibRaw already
  demosaiced; denoise/lens/heal have no algorithm yet, #40/#39/#51); the live suffix fuses WB
  (ratio over as-shot `cam_mul`), camera→XYZ(D50)→ProPhoto (one folded 3x3 matrix, computed on the
  CPU), exposure, a simple contrast curve, and a luma-preserving vibrance boost into the single
  dispatch this ADR's decision rule already requires, staying in linear ProPhoto RGB (the working
  space) end to end; crop is a bilinear affine sample. **No `HueSatMap`/`LookTable` bindings are
  reserved** in this pass's shader, unlike this section's own stage-order sketch above — that's
  #42's DCP-profile scope, and wiring in unused texture bindings with no real content to sample
  would be exactly the half-finished scaffolding this repo's conventions ask to avoid. Every
  kernel has a GPU-vs-CPU parity test against a CPU reference, plus one end-to-end test wiring
  the whole chain through `Renderer` and checking both dispatch counts and actual output pixel
  values. These tests skip when no `wgpu` adapter is available.
- **Cache tiers**: `spikes/loaf/src/cache.rs::Tier<V>`, a byte-budgeted LRU generic over a
  `size_of` closure, backs VRAM/RAM/disk with different budgets from the same eviction logic. A
  full-res (8280×5520) RGBA16F frame is ~349MB, a screen-res (3840-long-edge) frame is ~75MB — a
  resident N±2 screen-res window is under 2.5% of the reference machine's 16GB VRAM. **Landed in
  #45** as `crates/nicti-tapetum::cache::Tier<V>`, with the spike's own self-documented `O(n)`
  linear-scan `touch` replaced by an `O(log n)` generation-counter `BTreeMap` (the eviction/budget
  semantics are unchanged; the disk-tier codec itself stayed with #190).
- **Disk-tier compression**: zstd and lz4 both round-trip losslessly; a synthetic screen-res
  gradient compressed ~3688×/~247× respectively — explicitly not a real-photo promise (see
  follow-up #190), included only for real round-trip/timing evidence on a realistically-sized
  payload.
- **Mask refine**: `spikes/loaf/src/refine.rs::guided_upsample`, a direct port of
  `spikes/siamese/src/refine.rs` (#48/ADR-0048) onto this spike's own `Field` type — spikes don't
  depend on each other, so this is a copy. The shared `box_filter` primitive also got a GPU twin
  (`gpu.rs::run_box_filter`), parity-tested against the CPU reference.
- **Real reference-machine kernel numbers** (RTX 5080, GPU-timestamp dispatch time): fused
  live-suffix kernel 0.375ms p50 / 0.392ms p95 at screen resolution, 1.767ms p50 / 3.924ms p95 at
  full resolution — the full-res figure lands within a few percent of ADR-0016's own comparable
  45MP `live_chain` measurement, real corroborating evidence. Present/sample (crop) kernel:
  0.215-0.511ms p50, never above 1.611ms p95 even at full resolution, against a 16.7ms budget.
- **A real timing bug this pass caught**: the first bench run rebuilt the wgpu pipeline (including
  shader-module compilation) and every buffer inside the timed loop, measuring ~400ms p50 for the
  live-suffix kernel — ~1000× ADR-0016's own figure. Fixed via persistent-buffer `*Kernel` types
  (`LiveSuffixKernel`/`PresentSampleKernel`/`BoxFilterKernel`), the same pattern
  `spikes/glint::LiveChainKernel` already documents for the identical reason. Caught by comparing
  against ADR-0016's own published number rather than trusting the first result at face value.
- **Bake-scheduler simulation** (`spikes/loaf/src/sim.rs`): nearest-to-cursor-first priority, one
  serial bake worker (per ADR-0019/0016/0050's existing one-shared-device decision). Using real
  costs (decode 1.7s, denoise 50.9s per ADR-0040's full-res SCUNet measurement) plus mask bake's
  labelled hypothesis (1.0s, ADR-0048): the hero scenario's 50-image sync takes 2680s (44.7min) to
  fully bake, and **every single image the cursor reaches while walking is still stale** at a 100ms
  walk pace. This is a real finding, not a null result — full-res-first denoise cannot keep the
  bake queue ahead of a walking cursor; a screen-resolution-first denoise pass (cheaper, not yet
  measured) is needed, filed as follow-up #189.
- **Scheduling contract for Pounce (#54)**: bake jobs prioritized by `|image_index − cursor|`
  (`prefetch::priority_order`, a pure function re-evaluated on every cursor move, not a stateful
  queue Tapetum itself owns), plus a documented stale-while-baking fallback (ADR-0029/#145) while a
  bake is in flight.
- **Resolved: lens-correction placement (#191)**. Confirmed lens correction belongs in the baked
  prefix, before all of ADR-0038's color pipeline — on the channel-space argument, the only truly
  independent constraint found: CA-correction data is calibrated in camera-native R/G/B channel
  space (`nicti-cornea::LinearFrame`'s own space), and that channel identity is gone once
  `cct.rs::solve_camera_to_xyz` linearly mixes channels on the way to XYZ, so CA correction must
  precede the camera→XYZ matrix, not merely precede tone. Geometric distortion correction alone
  would tolerate running anywhere before the tone curve; it's assumed (not independently verified
  this pass) to be bundled with CA into one resampling pass, which is what would pin it too.
  **Caveat surfaced, not resolved**: ADR-0061's LRC catalog-schema mapping shows LRC's own Lens
  Corrections panel has user-adjustable manual distortion/vignette/defringe sliders, not just a
  fixed profile lookup — so lens correction's bakeability rests on the same "not a live-drag
  slider" architectural choice ADR-0050 already made for heal/remove's per-spot params, not on the
  params being lens/body/aperture-fixed as first assumed. #39 still needs to confirm this holds
  once it designs the manual-slider UX; #39's other scope (correction-data source, lens coverage)
  stays open too.
- **Open**: real-photo (not synthetic) disk-tier compression ratio: follow-up #190, blocked on #45
  producing real baked output to measure against.

## Addendum (2026-09-27, #45 PR4): real-hardware findings from the first real NEF this pipeline
ever decoded

This sandbox gained real `ref-10k` NEF access partway through #45's build (previously every test
above ran against small synthetic `LinearFrame` fixtures only). Two real findings came directly
out of that, neither of which any synthetic-fixture test could have caught:

- **Decode needs row-strip splitting.** A real Nikon Z8 frame (8280×5520) packs to a ~261.5MB
  buffer for `normalize.wgsl`'s decode dispatch. This sandbox's lavapipe (software) adapter's real
  `max_storage_buffer_binding_size` measured exactly 128MiB — a small synthetic fixture (2×2, 3×2)
  never approached that limit, so the ceiling was invisible until a real full-res file was
  decoded. `DecodeExec::encode` now loops over row-strips sized by `stages::rows_per_strip(width,
  height, max_storage_buffer_binding_size)`, each strip a separate upload buffer + dispatch,
  writing to the correct absolute row offset of the one shared full-frame output texture.
- **`cam_xyz`'s direction was backwards.** `color::cam_xyz_to_mat3` treated LibRaw's `cam_xyz`
  field as camera→XYZ; `camera_to_working_space_matrix` composed it directly with no inversion.
  Every existing test used a synthetic `cam_xyz` matrix chosen for numerical convenience, so this
  never surfaced. Run against the real file, the rendered image had a uniform, unmistakably wrong
  green color cast. Checking LibRaw's own `cam_xyz_coeff` (`utils_dcraw.cpp`, not just the header
  comment) confirmed `cam_xyz` is actually **XYZ→camera**: `cam_rgb[i][j] = cam_xyz[i][k] *
  xyz_rgb[k][j]`, composing with an XYZ input, never a camera one. Fixed by inverting
  (`color::mat3_invert`, Cramer's rule) before use. Re-rendering the same file afterward produced a
  recognizable photo with correct rough hue relationships (reddish brick, white/grey fur, black
  clothing, green foliage) instead of the uniform green cast.

Neither finding changes this ADR's Decision section — both are implementation bugs in code that
already claimed to follow it, not a design reconsideration. Included here because "real hardware
access surfaced a bug no synthetic fixture could" is exactly the kind of finding this document
exists to record, matching this file's own established practice of noting "a real timing bug this
pass caught" above.

## #47: Crop, straighten, and auto-level

See `docs/adr/0047-crop-straighten-autolevel.md` for the full record -- summarized here since this
file is this topic's "full reasoning/history" home per `CLAUDE.md`'s own convention. Straighten
(manual Ctrl-drag-a-reference-line gesture, and an automatic Canny/Hough auto-level button) composes
a rotation into the existing affine crop transform (`geometry::Affine2D::crop_and_rotate`,
`affine_for_crop`) rather than adding a new pipeline stage -- `present_sample.wgsl` needed no
change at all, since it already read a full 2x3 affine. `imageproc` (Canny + Hough, both
primitives confirmed present by reading its actual source before committing to it) was chosen over
`opencv-rust`, matching the ticket's own stated preference. The crop rectangle is a real, typed,
cached `CropParams` `StageEntry` (`coat.rs`), with an interactive overlay (resize handles, a
freeform-rotate handle, pan, and the Ctrl-drag gesture) in `nicti-pelt`; the live preview's own
canvas doesn't resize to the crop rect (deliberate, matches real editor UX -- see the ADR's own
"Crop rectangle scope" section), while the already-existing `tile::TiledRender`/`MemorySink`
full-res export path (landed in #45 PR4) already supports an arbitrary decoupled output extent, so
crop *does* actually resize the framing at export time.

- **Auto-level degradation (#101, cross-ref)**: `docs/adr/0101-auto-op-graceful-degradation.md` (full text in `develop.md`) defines what `detect_level_angle` returning no or weak evidence must do: `NoResult` shows a hint, `LowConfidence` is skipped with a distinct hint, neither creates a history step. Implementation: #311.

## #57: the shared spine, `render_live`, and a photo-identity bug

Export needed the same graph/registry/params resolution Develop has, so they moved into
`nicti_tapetum::spine` (`build_graph`, `build_registry`, `resolve_inputs`) and `nicti-pelt`'s
`render.rs` and `bench/knead` now use it. `Renderer::render_live` was split out of `render` so a
tiled full-resolution export doesn't allocate a full-size geometry target it never reads.

Building it exposed that `RenderGraph::apply_document` recomputes every node's `own_hash` from the
document — including DECODE's, whose only "params" are the stage default. `DevelopView::
load_real_frame` had set DECODE's hash to the photo's identity with `set_own_hash`, and the next
`render()`'s `apply_document` silently reset it, so two photos of the same pixel size shared baked
and live cache keys and could be served each other's pixels. Fix: `spine::stamp_source_identity`
puts the identity into the DECODE entry of the render-time copy of the document (the stored one is
never stamped), which `apply_document` then reapplies identically every frame (no per-frame
invalidation). Regression tests: `render.rs::two_photos_of_the_same_size_never_share_cached_pixels`
and `export/jobs.rs::exports_every_photo_with_planned_names_and_distinct_pixels`, both confirmed to
fail when the stamp is removed.

## #380: global Presence and post-crop Effects

Full decision: `docs/adr/0380-global-presence-and-effects.md`. Two new stages. `nicti.presence`
(global Texture/Clarity/Dehaze/Saturation) is a Live stage summed with the per-mask deltas in
`live_suffix.wgsl`, reusing the mask kernels; with no mask `MaskEngine::prepare` still builds the
clarity/texture/dehaze bases (a zero-correction `MaskFrame` over a 1x1 atlas) and every renderer --
Develop, export, rendered previews -- bakes first whenever masks are active or
`PresenceParams::needs_bases`. `nicti.effects` (post-crop vignette + film grain) is a second Geometry
node (`spine::GEOMETRY_IDS`), evaluated inside `present_sample.wgsl` in crop-normalized coordinates
(`effects::crop_norm`: inverse crop transform over the crop size, bound once per photo with the
untiled, unscaled transform by `RenderInputs::bind_effects`), so a preview, a full-size export and
every export tile show the same pattern and an Effects edit re-runs only the geometry pass (pinned by
`render.rs::an_effects_edit_only_reruns_the_geometry_pass`). The formulas are this repo's
approximation of LRC's Effects panel (no LRC to compare against); the PCG hash is pinned by reference
vectors so the WGSL copy cannot drift silently. `present_sample` is still unaudited on Dx12 (#355).
