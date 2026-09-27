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
  structural graph property, not a timing coincidence.
- **Cache tiers**: `spikes/loaf/src/cache.rs::Tier<V>`, a byte-budgeted LRU generic over a
  `size_of` closure, backs VRAM/RAM/disk with different budgets from the same eviction logic. A
  full-res (8280×5520) RGBA16F frame is ~349MB, a screen-res (3840-long-edge) frame is ~75MB — a
  resident N±2 screen-res window is under 2.5% of the reference machine's 16GB VRAM.
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
- **Open**: lens-correction placement relative to color isn't confirmed by any ADR yet (#39 has
  none) — this ADR assumed ADR-0050's proposed placement; follow-up #191. Real-photo (not
  synthetic) disk-tier compression ratio: follow-up #190, blocked on #45 producing real baked
  output to measure against.
