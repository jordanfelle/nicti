---
paths:
  - "spikes/loaf/**"
  - "crates/nicti-render/**"
  - "crates/nicti-pawprint/**"
  - "docs/adr/0044-stage-cached-render-graph.md"
---

# Render Graph (Tapetum) — Quick Reference

Full reasoning/history: `docs/decisions/render-graph.md`.

- **Render graph (#44)** — `docs/adr/0044`: **Proposed**, kernel-level decision rules measured
  clean on real RTX 5080 hardware; full pipeline + hero-scenario screen-capture pass are #45's.
- **Stage order**: baked prefix `decode → demosaic (AHD) → denoise (SCUNet) → lens correction →
  heal/remove` → neutral branch `neutral_render → mask_bake` (decoupled from live sliders) → one
  fused live dispatch `WB → HueSatMap → exposure → tone → vibrance → mask compose/apply` → crop as
  an affine sample pass over the live suffix's own output only.
- **Cache key**: `nicti_pawprint::chain` generalizes ADR-0021's flat one-upstream chain to a DAG;
  `nicti_render::graph::RenderGraph::cache_key`/`invalidated_bakes`/`set_own_hash` are the tested,
  structural proof that a live-slider change triggers zero bake dispatches — **landed in #45**
  (promoted from `spikes/loaf/src/graph.rs`; `set_own_hash` is the "update a node in place" API
  the spike's own tests lacked).
- **Cache tiers**: `nicti_render::cache::Tier<V>` — byte-budgeted LRU, generic over `size_of`,
  backs VRAM/RAM/disk. Full-res (8280×5520) frame ≈349MB, screen-res (3840 long edge) ≈75MB.
  **Landed in #45** (promoted from `spikes/loaf/src/cache.rs`, with its self-documented `O(n)`
  `touch` replaced by an `O(log n)` generation-counter `BTreeMap`). The disk-tier codec (zstd/lz4)
  stayed with #190, which needs real baked output to choose against.
- **Disk compression**: zstd/lz4 both round-trip; synthetic-gradient ratios (~3688×/~247×) are
  **not** a real-photo promise — see follow-up #190.
- **Mask refine**: `refine.rs::guided_upsample` — ported from `spikes/siamese/src/refine.rs`
  (spikes don't depend on each other, this is a copy). `box_filter` also has a GPU twin.
- **Real RTX 5080 numbers** (GPU-timestamp only): live-suffix 0.375ms p50/0.392ms p95 (screen res),
  1.767ms p50/3.924ms p95 (full res, matches ADR-0016's own comparable figure within a few
  percent). Present/sample (crop): ≤1.611ms p95 even at full res.
- **Gotcha caught this pass**: rebuilding the wgpu pipeline+buffers inside a timed loop (instead of
  a persistent `*Kernel`, see `gpu::LiveSuffixKernel`) measured ~1000x too slow — same class of bug
  `spikes/glint::LiveChainKernel` already documents. Always benchmark against a persistent kernel,
  never `run_live_suffix`/`run_present_sample`/`run_box_filter` directly, inside a timing loop.
- **Bake-scheduler sim** (`sim.rs`): one serial bake worker, nearest-to-cursor-first. Real costs
  (decode 1.7s, denoise 50.9s/ADR-0040) show full-res-first denoise cannot keep the hero
  scenario's 50-image bake queue ahead of a walking cursor (`stale_at_arrival = 50/50`) — needs a
  screen-res-first denoise pass, follow-up #189.
- **Open follow-ups**: #189 (screen-res-first denoise scheduling), #190 (real-photo compression
  ratio, blocked on #45), #191 (confirm lens-correction placement, #39's own scope).

## Package contents

- **`crates/nicti-render`** (#45, landed) — `graph.rs` (the stage DAG, `RenderGraph`/`StageNode`/
  `StageKind`, promoted from `spikes/loaf/src/graph.rs` with a persistent memoized cache key and
  the `set_own_hash` update API the spike lacked), `cache.rs` (`Tier<V>`, promoted from
  `spikes/loaf/src/cache.rs` with an `O(log n)` touch), `prefetch.rs` (`priority_order`, promoted
  as-is), `gpu.rs` (the shared `GpuContext`, one wgpu device/queue per ADR-0016, adapted from
  `spikes/glint/src/gpu.rs`), `frame.rs` (`FrameTexture`, always `Rgba16Float` — a core format
  needing no `SHADER_F16` feature, unlike a raw f16 storage buffer — plus a `FramePool` free-list),
  and `renderer.rs` (the real `RenderStage` execution trait extension — `kind`/`default_params`/
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
- **`crates/nicti-pawprint`** (#21/#44/#45, landed) — `EditDocument`/`StageEntry`/`history`
  (promoted from `spikes/pawprint`) plus `canonical::{hash_value, chain}` (merging pawprint's
  original one-upstream `hash_stage`/`cache_key` with `spikes/loaf/src/hash.rs`'s DAG-generalized
  `chain` — `nicti_render::graph` is the DAG consumer of this hashing scheme).
- **`spikes/loaf`** (#44/ADR-0044's stage-cached render-graph research; slated for deletion once
  #45's remaining GPU slices land) — crop as a geometry-only affine sample pass (`geometry.rs`), a
  guided-filter mask refine ported from `spikes/siamese` (`refine.rs`), three `wgpu` kernels
  (`gpu.rs`: `live_suffix`, `present_sample`, `box_filter`) each with a persistent-buffer `*Kernel`
  type for repeated-call timing, and a discrete-event simulation of the #43 hero scenario's bake
  queue (`sim.rs`). `graph.rs`/`hash.rs`/`cache.rs`/`prefetch.rs` are now duplicated in
  `crates/nicti-render`/`crates/nicti-pawprint` above (promoted, not moved, since the spike's
  GPU/geometry/sim modules aren't promoted yet) — this copy is retained only until the whole spike
  is deleted alongside #45's later PRs. `src/bin/loaf.rs` exposes `graph`/`bench`/`sim`
  subcommands. Real, tested (32 unit + 3 GPU-parity tests), not path-gated — measured on the real
  reference RTX 5080 via the documented cross-compile-to-Windows path, not just lavapipe. See
  `docs/research/loaf-render-graph.md`.
