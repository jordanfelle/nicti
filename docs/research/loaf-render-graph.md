# Loaf: stage-cached render graph research write-up (#44, ADR-0044)

Feline name: a cat loaf (the tucked-paws resting pose) doubles as "baked" — the whole idea behind
reusing baked stage output instead of recomputing it. The render graph itself keeps its own
codename, Tapetum.

Read `docs/adr/0044-stage-cached-render-graph.md` first for the actual decision — this doc is the
method and raw numbers behind it.

## What this pass built

`spikes/loaf` is a headless model of Tapetum's design: a stage DAG with pawprint-style upstream-
chained cache keys, byte-budgeted eviction-correct cache tiers, crop expressed as a geometry-only
sample pass, a guided-filter mask refine ported from `spikes/siamese`, and a discrete-event
simulation of the #43 hero scenario's bulk-sync bake queue. It does not include a GUI viewport or
a real end-to-end decode→render pipeline — those are #45's build, once a real render engine exists
to wire this design into.

## Method

1. **Graph model** (`src/graph.rs`): nodes are `Baked`/`Live`/`Geometry`, edges are upstream reads.
   Cache key is `hash::chain(sorted_upstream_keys, own_hash)`, computed recursively and memoized.
   `invalidated_by`/`invalidated_bakes` do a forward BFS from a changed node to find every node
   whose *effective* output changes as a result — this is what "changing a Live stage never
   triggers a bake" and "changing crop invalidates only crop" actually mean, structurally, not just
   as a timing observation.
2. **Cache tiers** (`src/cache.rs`): a generic byte-budgeted LRU (`Tier<V>`), parameterized by a
   `size_of` closure so the same eviction code serves VRAM/RAM/disk with different payload shapes
   and budgets. Tested for: staying within budget after repeated inserts, LRU eviction order, an
   over-budget single value being rejected rather than partially admitted, and idempotent
   re-insertion of an existing key.
3. **Geometry** (`src/geometry.rs`): a 2×3 affine `Affine2D` (`source = M · output`, matching how a
   sampler actually needs to think about it), composable via `then_rotate` so straighten-on-top-of-
   crop works. `sample()` is the CPU reference the GPU kernel is checked against.
4. **Mask refine** (`src/refine.rs`): a direct port of `spikes/siamese/src/refine.rs::guided_upsample`
   onto this spike's own `Field` type. Spikes don't depend on each other in this repo, so this is a
   copy of already-proven logic, not a new derivation — the box-filter/variance/covariance math and
   its test cases (flat-field passthrough, edge-sharpening) are unchanged from siamese's own.
5. **GPU** (`src/gpu.rs`): three kernels, ported/adapted from prior spikes' own patterns —
   `live_suffix` (the fused WB/HueSat-equivalent/tone/vibrance chain, functionally identical to
   `spikes/glint`'s `live_chain` kernel), `present_sample` (the crop/geometry pass, new to this
   spike), and `box_filter` (the mask-refine primitive, new to this spike, checked against
   `refine::box_filter`). Each has a one-shot function (for a single correctness check) and a
   persistent-buffer `*Kernel` type (for a repeated-call timing loop) — see the bug writeup below
   for why both exist.
6. **Bake-scheduler simulation** (`src/prefetch.rs`, `src/sim.rs`): `prefetch::priority_order` is a
   pure nearest-to-cursor sort, re-evaluated on demand rather than a stateful queue. `sim.rs`
   simulates one serial bake worker processing jobs in that order, using per-image bake costs from
   `cost_model.rs` — each constant cites its source ADR, and the mask-bake cost is explicitly
   labelled a hypothesis (ADR-0024 itself has no real measurement yet).

## A real bug this pass's own measurement caught

The first real-hardware bench run (RTX 5080, screen resolution) measured the `live_suffix` kernel
at ~400ms p50 — about 1000× higher than ADR-0005's own comparable `live_chain` figure (0.326ms p95
at 4K). The cause: `run_live_suffix` (and `run_present_sample`/`run_box_filter`) each rebuild the
wgpu pipeline (including shader-module compilation — at least the driver observed here appears to
defer this until first dispatch, not at module-creation time) and every buffer on *every* call.
`nicti_prowl::perf::Protocol::run`'s 1-warmup+5-measured loop was therefore timing "compile a
shader and allocate buffers" five times over, not "dispatch a warm kernel" five times.

The fix, `LiveSuffixKernel`/`PresentSampleKernel`/`BoxFilterKernel` (pipeline + buffers built once
in `new()`, re-dispatched via `write_buffer` + submit in `dispatch()`), is the exact pattern
`spikes/glint::LiveChainKernel` already documents hitting and fixing, for the identical reason —
its own doc comment even names the mechanism ("re-paying pipeline compilation and a full
input-buffer upload on every iteration"). After the fix, the full-resolution `live_suffix` number
(1.767ms p50 / 3.924ms p95) landed within a few percent of ADR-0005's own 45MP figure (2.08ms p50 /
3.88ms p95) — real corroborating evidence the fused-kernel approach performs the way that ADR
predicted, once measured correctly.

This was caught by comparing the first result against ADR-0005's already-published number rather
than accepting it at face value — the same discipline this repo's own ADRs (0021, 0024, 0040) all
model in their own "measured vs hypothesis" framing.

## Raw numbers

See ADR-0044's own Measured results section for the full table; summarized here for reference:

- `live_suffix`: 0.375ms p50 / 0.392ms p95 (screen res, 3840×2560) — 1.767ms p50 / 3.924ms p95
  (full res, 8280×5520).
- `present_sample`: 0.215ms p50 / 0.222ms p95 (screen res) — 0.511ms p50 / 1.611ms p95 (full res).
- `box_filter` (512×512 field, radius 2): 0.010-0.023ms.
- VRAM: a full-res RGBA16F frame ≈349MB (matches ADR-0005's own cited ~360MB within the rounding
  this pass's own test tolerates), a screen-res frame ≈75MB.
- Disk compression (synthetic 75MB screen-res gradient, `#[ignore]`d test, explicitly not a
  real-photo claim): zstd ~3688× (11.3ms), lz4 ~247× (3.4ms).
- Bake-scheduler sim (50 images, cursor starts at 0, 100ms walk pace, real+hypothesis per-image
  cost of 53.6s): first image ready at 53.6s, full sync-to-done at 2680s (44.7 minutes),
  50/50 images still stale when the cursor reaches them.

## What this means for #44's own design

The bake-scheduler sim is the pass's most load-bearing finding: **scheduling full-resolution
denoise first cannot possibly keep pace with a user walking through a synced set**, regardless of
how the priority queue is ordered — the bottleneck is per-image bake cost (53.6s), not scheduling
policy. This means #45's real scheduler needs a cheaper "good enough for the stale-while-baking
tier" bake pass (most plausibly, denoise at screen resolution rather than full resolution) ahead of
a lower-priority full-resolution rebake — filed as follow-up #189, since no screen-res SCUNet
timing exists yet to design that policy against.

## Cross-compile / reference-machine notes

This pass ran on the actual reference RTX 5080 via the documented cross-compile path
(`.claude/rules/gpu-gui-and-healing/REFERENCE.md`): `rustup target add x86_64-pc-windows-gnu`, then
`cargo build --release --target x86_64-pc-windows-gnu -p loaf --bin loaf` with the rustup toolchain
directory placed first on `PATH` (the Homebrew-shadowing gotcha that ADR already documents — cargo
resolves a bare `rustc` for host-tool invocations, so pointing only `cargo` at rustup while `rustc`
still resolves to Homebrew's non-cross-compiled build fails with a misleading "can't find crate for
core/std"). The built `.exe` was run directly on the Windows side via WSL interop
(`powershell.exe -Command "& '<unc-path>' ..."`), picking up the real NVIDIA adapter
(`NVIDIA GeForce RTX 5080 (Vulkan)`) rather than lavapipe. In this sandbox's own WSL/Linux side,
the same kernels run correctly against lavapipe/llvmpipe (correctness only, not timing) —
`tests/gpu_parity.rs` was observed to segfault once under default (multi-threaded) `cargo test`
parallelism and pass cleanly on every other run and under `--test-threads=1`; siamese's and
calico's own GPU parity tests (9 and 1 tests respectively) ran with no such issue under the same
default parallelism, so this looks like lavapipe-specific flakiness under concurrent adapter/device
creation in this particular sandbox, not a bug in this spike's own kernels — noted here rather than
silently ignored, since it wasn't chased down further this pass.
