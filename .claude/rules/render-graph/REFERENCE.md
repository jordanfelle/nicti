---
paths:
  - "spikes/loaf/**"
  - "crates/nicti-render/**"
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
- **Cache key**: `spikes/loaf/src/hash.rs::chain` generalizes ADR-0021's flat one-upstream chain to
  a DAG; `graph.rs::RenderGraph::cache_key`/`invalidated_bakes` are the tested, structural proof
  that a live-slider change triggers zero bake dispatches.
- **Cache tiers**: `cache.rs::Tier<V>` — byte-budgeted LRU, generic over `size_of`, backs
  VRAM/RAM/disk. Full-res (8280×5520) frame ≈349MB, screen-res (3840 long edge) ≈75MB.
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

- **`spikes/loaf`** (#44/ADR-0044's stage-cached render-graph research) — a stage DAG
  (`graph.rs`) with pawprint-style upstream-chained cache keys generalized to a DAG (`hash.rs`),
  byte-budgeted VRAM/RAM/disk cache tiers (`cache.rs`), crop as a geometry-only affine sample pass
  (`geometry.rs`), a guided-filter mask refine ported from `spikes/siamese` (`refine.rs`), three
  `wgpu` kernels (`gpu.rs`: `live_suffix`, `present_sample`, `box_filter`) each with a
  persistent-buffer `*Kernel` type for repeated-call timing, nearest-to-cursor bake prioritization
  (`prefetch.rs`), and a discrete-event simulation of the #43 hero scenario's bake queue
  (`sim.rs`). `src/bin/loaf.rs` exposes `graph`/`bench`/`sim` subcommands. Real, tested (32 unit +
  3 GPU-parity tests), not path-gated — measured on the real reference RTX 5080 via the documented
  cross-compile-to-Windows path, not just lavapipe. See `docs/research/loaf-render-graph.md`.
