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
  heal/remove` → neutral branch `neutral_render → mask_bake` (decoupled from live sliders) → one
  fused live dispatch `WB → HueSatMap → exposure → tone → vibrance → mask compose/apply` → crop as
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
- **Mask refine**: `refine.rs::guided_upsample` — ported from `spikes/siamese/src/refine.rs`
  (spikes don't depend on each other, this is a copy). `box_filter` also has a GPU twin.
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

## Package contents

- **`crates/nicti-tapetum`** (#45, landed) — `graph.rs` (the stage DAG, `RenderGraph`/`StageNode`/
  `StageKind`, promoted from `spikes/loaf/src/graph.rs`, now deleted, with a persistent memoized
  cache key and the `set_own_hash` update API the spike lacked), `cache.rs` (`Tier<V>`, promoted
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
    no algorithm yet, #40/#39/#51); `LiveSuffixKernel` runs `live_suffix.wgsl`, fusing WB (ratio
    over as-shot `cam_mul`) + camera→XYZ(D50)→ProPhoto (`color::camera_to_working_space_matrix`,
    folded into one 3x3 on the CPU) + exposure + a simple cube-root-space contrast curve + a
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
