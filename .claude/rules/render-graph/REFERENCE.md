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
  as-is). The `RenderStage` execution trait itself (GPU buffer bindings, the fused live-suffix
  dispatch) is still open — #45's own GPU slice.
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
