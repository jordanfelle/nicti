# ADR-0044: Stage-cached render graph (Tapetum)

- **Status:** Proposed — kernel-level decision rules (#2-#5) measured clean on the real reference
  RTX 5080; the full end-to-end pipeline and the hero scenario's own screen-capture pass are #45's
  build, not measured here; two follow-ups remain open ([#189](https://github.com/jordanfelle/nicti/issues/189),
  [#190](https://github.com/jordanfelle/nicti/issues/190)) — see Consequences.
  [#191](https://github.com/jordanfelle/nicti/issues/191) (lens-correction placement) is resolved,
  see "Lens-correction placement, confirmed" under Decision.
- **Date:** 2026-09-27
- **Ticket:** [#44](https://github.com/jordanfelle/nicti/issues/44) Research: stage-cached render
  graph design (Tapetum)

## Context

#44 is the largest remaining gate in the backlog: #45, #46, #49, #51, #52, #54/#55, #60, #99,
#100, #101, #144, #145 all wait on it. Its own blockers (#40, #43, #48) are closed. Eight ADRs
left decisions open for Tapetum to make, rather than deciding them itself:

- **ADR-0021**: render order and the per-stage cache-key chain (`upstream_hashes ‖ own_hash`) are
  explicitly Tapetum's to own, not the edit document's.
- **ADR-0016**: one shared `wgpu::Device`, f16 cache tiers, never a full-frame host↔device
  round-trip in the hot path, mandatory 2D dispatch grid.
- **ADR-0050**: proposes heal/remove after lens correction, before global tone, in linear space —
  "#44 has the final say."
- **ADR-0029/0143**: T2/T3 tiers go stale-while-baking once an image has edits, until Tapetum
  renders a real screen tier (#145).
- **ADR-0038**: the render graph's overall stage order is #44's decision; ADR-0038 only fixes the
  order *within* color.
- **ADR-0048**: proposes AI masks bake after color/tone, before masked local adjustments; masks at
  preview resolution refined via guided filter only when zoomed/exporting; the model's own bake key
  is decoupled from live tone sliders.
- **ADR-0040**: proposes classic AHD demosaic (FBDD 0) → SCUNet AI denoise as the demosaic+NR
  segment, and reports SCUNet's real cost (50.9s/full-res image) as an input to Tapetum's own
  scheduling story, not something #40 itself schedules.

Nothing in the repo executes a render stage or caches its output yet — `crates/nicti-tapetum`
(`RenderStage: Module`) has no execution method, deliberately left to #16/#44/#45.

**User decisions for this pass** (2026-09-27): measure on the real RTX 5080 reference machine
(cross-compile to `x86_64-pc-windows-gnu`, run via WSL interop — same pattern #97/#149/#163 used),
and scope this pass to a headless graph plus a scheduler simulation, not a full GUI app driving a
real screen-capture hero run — that's #45's job, once a real viewport exists.

## Decision rule (stated before measuring)

1. **Invalidation correctness.** Changing stage S's params must rebake exactly S and the stages
   downstream of it, and must not touch any stage upstream of S. Changing a `Live` stage's params
   must trigger **zero** bake dispatches.
2. **Live update**, warm, RTX 5080: the fused live-suffix kernel's own GPU dispatch time should stay
   within ADR-0016's quarter-frame precedent (≤4ms) at both screen and full resolution, and the
   develop-panel's own slider→ready budget (≤16.7ms, `docs/benchmarks.md:17`).
3. **Crop drag:** zero upstream (Baked/Live) dispatches — a structural graph property, not just a
   timing one — and the sample-pass kernel's own dispatch stays within the same 16.7ms budget.
4. **VRAM:** the chosen resident window (N screen-res frames ± k neighbors) fits a stated budget
   against ADR-0016's real 16GB reference figure.
5. **Disk tier:** compressed half-float planes round-trip losslessly; zstd/lz4 are both measured,
   not picked blind.
6. **Bake-scheduler sim:** report, not gate — the hero sequence's time-to-cursor-ready and
   sync-to-done, using each bake stage's own already-measured (or explicitly hypothesis-labelled)
   cost. There's no separate v1 target for this number yet (`docs/benchmarks/hero-scenario.md`'s own
   secondary metric is still pending its own real measurement); this pass exists to show what the
   proposed scheduling policy (nearest-to-cursor-first, one serial bake worker) actually achieves
   given real per-stage costs, which is itself a finding.

## Decision

### Stage order and the bake/live boundary

Combining ADR-0021/0050/0037/0038/0048/0040's individual constraints into one graph:

- **Baked prefix**, linear camera RGB, one RGBA16F texture per image per resolution tier:
  `decode → demosaic (AHD) → denoise (SCUNet) → lens correction → heal/remove`.
- **Neutral branch** (ADR-0048's own key decision): `bake boundary + default tone → AI mask bake`,
  at preview resolution. The mask's `ai_bake_key` never depends on a live slider — this is why a
  tone-slider drag triggers zero mask rebakes, proven structurally in `graph.rs`'s tests.
- **Live suffix**, one fused dispatch: `WB (ratio over as-shot) → cam→XYZ→ProPhoto → HueSatMap →
  exposure → LookTable → working space → tone → vibrance → mask compose + masked_adjust`. Only the
  CPU-side DCP prep (blending illuminant maps into ADR-0038's 3D HueSatMap texture) is baked,
  keyed by profile+CCT — everything else here recomputes every frame.
- **Crop/rotate/zoom/pan**: an affine sample pass reading only the live suffix's output — never a
  Baked or Live node input (`geometry.rs`, `graph.rs`'s `changing_crop_invalidates_only_crop_itself`
  test).

### Lens-correction placement, confirmed (#191)

This ADR originally placed lens correction in the baked prefix, before *any* of ADR-0038's color
pipeline, on ADR-0050's own proposed authority — flagged as an unconfirmed assumption (#39 had no
ADR of its own settling it). This pass confirms the placement, for two independent reasons that
both point the same way rather than one single hard constraint:

1. **Chromatic-aberration correction is channel-space-bound.** Lensfun's (and embedded-NEF) CA
   calibration data describes a per-channel spatial misalignment of the camera's own R/G/B
   channels — it's measured and expressed in that channel space, not in XYZ or a working-space
   profile-connection space. `nicti-cornea::LinearFrame` (#41) is exactly that space:
   demosaiced-but-uncorrected, WB/color-matrix/gamma-free linear camera RGB. ADR-0038's
   `cct.rs::solve_camera_to_xyz` linearly mixes R/G/B per pixel on the way to XYZ(D50); once that
   mix has happened, the calibration data's per-channel shift no longer corresponds to anything —
   there's no "R channel" left to shift independently. **CA correction must run before the
   camera→XYZ matrix**, not merely "somewhere before tone."
2. **Geometric resampling belongs in linear light.** Distortion-warp correction is an
   interpolation over neighboring pixels; interpolating through a nonlinear tone curve produces
   edge haloing and local brightness shifts that don't occur when the same warp is applied to
   linear-referred data — the same reasoning ADR-0050 already used to place heal/remove's Poisson
   solve before tone. Everything in ADR-0038's pipeline up through `LookTable` operates on
   linear-referred per-pixel RGB values (WB is a per-channel scale, cam→XYZ/ProPhoto/HueSatMap/
   LookTable are all matrix or LUT transforms of linear values) — with one caveat ADR-0038 itself
   flags: `ProfileHueSatMapEncoding`/`ProfileLookTableEncoding` can run a profile-defined nonlinear
   curve over the LUT's own *value* lookup axis when a profile sets that flag (true for real Adobe
   profiles, including Adobe Vivid). That curve only reshapes how the 3D LUT is indexed, not the
   RGB values entering/leaving the stage, so it doesn't change this argument's conclusion — but the
   pipeline isn't as uniformly "just linear" as a first pass over it suggests. Only the tone-curve
   step is genuinely nonlinear *on the pixel values themselves*. So distortion correction alone
   would tolerate running anywhere before tone — it's reason 1 above (CA's channel-space
   requirement) that actually pins it to the *front* of the color pipeline, not just somewhere
   ahead of the tone curve.

Distortion and CA correction are conventionally calibrated and applied together as one resampling
pass in lens-correction tooling (lensfun and Adobe's own embedded-NEF correction both bundle
distortion + CA + vignette into one profile) — reason 1's channel-space constraint on CA would
then pin distortion too, once #39 decides whether Nicti's own implementation keeps them bundled.
This is an assumption about #39's likely implementation shape, not something independently
verified against lensfun's actual API in this pass.

**A caveat this pass surfaced, not a third confirming argument**: ADR-0061's LRC catalog-schema
mapping (`docs/research/shed-lrcat-schema.md`, `docs/adr/0061`) documents that LRC's own Lens
Corrections panel — the feature #39 is scoped to replicate — includes user-adjustable manual
distortion, manual vignette, and defringe (CA) amount sliders, not just a fixed profile lookup.
Lens correction's params are therefore not fully determined by the lens/body/focal-length/aperture
tuple alone, the way this pass first assumed. That doesn't itself argue for a *different*
placement — ADR-0050's `HealStage` already shows that a stage with per-edit, user-drawn parameters
(spot position, radius, feather) still belongs in the baked prefix, not the live suffix, because
"baked" means "cacheable by its own param hash," not "parameter-free" — but it does mean lens
correction's bakeability isn't a free, independent argument the way this pass originally claimed;
it rests on the same "not a live-drag-every-frame slider" architectural choice as heal/remove,
which #39 will need to confirm still holds once it designs the actual manual-slider UX (a
real-time-preview vignette-amount drag, in particular, would be a live-suffix-shaped interaction,
not a baked one).

**Conclusion: ADR-0044's original placement of lens correction in the baked prefix, before ADR-0038's
color pipeline, is confirmed on the channel-space argument (reason 1) — the strongest and only
truly independent constraint found this pass.** The linear-light argument (reason 2) is consistent
with, but doesn't independently require, that same placement. This resolves the placement question
this ticket (#191) was scoped to answer; it does not resolve #39's own broader scope (picking
`lensfun-rs` vs. embedded-NEF data as the correction-data source, verifying NIKKOR Z lens coverage,
or designing the manual distortion/vignette/defringe slider UX this pass surfaced as a real open
question), which stays open and unblocked.

### Cache key

Exactly ADR-0021's own rule, generalized from a flat per-stage list to a DAG (`hash.rs::chain`):
`blake3(sorted_upstream_cache_keys ‖ own_canonical_params_hash)`, computed recursively and memoized
per graph (`graph.rs::RenderGraph::cache_key`). A node's cache key changes iff its own params
changed or any upstream node's did — proven for both directions in
`graph.rs`'s `changing_a_baked_stage_invalidates_itself_and_every_downstream_stage` and
`changing_a_live_stage_triggers_zero_bake_dispatches`.

### Cache tiers

| Tier | Holds | Real vs hypothesis |
|---|---|---|
| VRAM | Screen-res (~3840 long edge) baked output for N±k images; full-res only for the currently-zoomed image | Byte-budgeted LRU built and tested (`cache.rs::Tier`); the *specific* k and byte budget for a real GUI are #45's to pick, this pass only proves the eviction mechanism is correct and cites real per-frame byte sizes |
| RAM ring | Same payloads, wider ±k | Same `Tier` type, different budget — no separate implementation needed |
| Disk | Compressed half-float planes + mask alpha, keyed by cache key | zstd and lz4 both measured (below); real compression ratio on real photo content is a follow-up (see Consequences) |

`Tier<V>` (`cache.rs`) is a byte-budgeted LRU generic over a `size_of` closure, so the same
eviction logic backs all three tiers. Its own tests prove: eviction keeps `used_bytes ≤
budget_bytes`, LRU order is respected, an over-budget single value is rejected rather than evicting
everything else for a payload that would just be evicted again, and re-inserting an existing key
doesn't double-count its bytes.

### Crop as geometry, not a stage

`geometry::Affine2D` maps output pixel → source pixel (`source = M · output`), composable
(`then_rotate`) so straighten-on-top-of-crop works the way a real edit stack does. The GPU twin
(`present_sample.wgsl`) reads only the already-rendered live-suffix output as a storage buffer and
remaps sample coordinates — structurally incapable of touching a Baked or Live node, since it has
no binding to their buffers at all, not just "chooses not to."

### Mask refine: guided filter, ported from `spikes/siamese`

`refine::guided_upsample` is a direct port of `spikes/siamese/src/refine.rs::guided_upsample`
(#48/ADR-0048, already proven there) onto this spike's own `Field` type — spikes don't depend on
each other, so this is a copy, not a shared dependency. The one piece also ported to GPU is the
shared `box_filter` primitive (`gpu.rs::run_box_filter`, checked against the CPU reference in
`tests/gpu_parity.rs`), proving the mechanism is GPU-portable; chaining every box-filter pass plus
the elementwise variance/covariance math into a single always-GPU guided-filter pipeline is real
additional work left to #45, not required to validate this ADR's design.

### Bake scheduling contract (for Pounce, #54)

Tapetum only defines what it asks of a scheduler, not the scheduler itself: bake jobs prioritized
by `|image_index − cursor|` (`prefetch::priority_order`), reprioritized immediately whenever the
cursor moves (a pure function of the current pending set + cursor, not a stateful queue this module
owns), and a documented "show a live render without denoise/masks, flagged stale" fallback while a
bake is still in flight (ADR-0029/#145's own stale-while-baking gap, which this ADR doesn't build,
only assumes exists).

## Measured results

**Real reference-machine numbers** (RTX 5080, driver from ADR-0016's own reference-machine table,
Vulkan backend, GPU-timestamp dispatch time only — excludes host↔device transfer and CPU-side
buffer setup, per decision rule #2/#3's own "warm, steady-state" framing):

| Kernel | Resolution | p50 | p95 |
|---|---|---|---|
| `live_suffix` (fused WB/exposure/HueSat-equivalent/tone/vibrance chain) | 3840×2560 (screen) | 0.375ms | 0.392ms |
| `live_suffix` | 8280×5520 (full, Z8 raw plane) | 1.767ms | 3.924ms |
| `present_sample` (crop/geometry) | 3840×2560 | 0.215ms | 0.222ms |
| `present_sample` | 8280×5520 | 0.511ms | 1.611ms |
| `box_filter` (mask-refine primitive, radius 2) | 512×512 field | 0.010ms | 0.023ms |

The full-resolution `live_suffix` figure (1.767ms p50 / 3.924ms p95) lands within a few percent of
ADR-0016's own comparable 45MP measurement (2.08ms p50 / 3.88ms p95, its own `live_chain` kernel on
the same reference GPU) — real corroborating evidence the fused-live-suffix approach behaves the
way ADR-0016 already predicted, not a new, unrelated result. **Decision rule #2 passes** at both
resolutions (both well inside the 4ms/16.7ms budgets, though the full-res p95 sits close enough to
the 4ms figure that a real pipeline adding HueSatMap's actual 3D-texture sample and mask compose on
top should re-check headroom, not assume it's unlimited). **Decision rule #3 passes** comfortably
(present_sample never exceeds 1.611ms even at full resolution, against a 16.7ms budget).

**A real bug this pass's own measurement caught**: the first bench run measured `live_suffix` at
~400ms p50 — roughly 1000× ADR-0016's own figure — because `run_live_suffix` rebuilt its wgpu
pipeline (including shader-module compilation, which at least one driver defers until first
dispatch) and every buffer on each timed call. `gpu::LiveSuffixKernel`/`PresentSampleKernel`/
`BoxFilterKernel` (persistent pipeline + buffers, re-dispatched via `write_buffer` + submit only)
fixed this — the same pattern `spikes/glint::LiveChainKernel` already documents hitting and fixing
for exactly this reason. Comparing against ADR-0016's own published number, rather than trusting
the first result, is what caught it.

**VRAM budget** (`cost_model.rs`): a full-res (8280×5520) RGBA16F frame is ~349MB (`
full_res_z8_frame_matches_adr_0005s_cited_figure` — within 10MB of ADR-0016's own cited ~360MB
figure), a screen-res (3840-long-edge) frame is ~75MB. A resident window of, say, N±2 screen-res
frames (5 images) is ~375MB — under 2.5% of the reference machine's 16GB VRAM even before
accounting for the GUI's own usage; full resolution is only needed for the single currently-zoomed
image (~349MB more). This pass doesn't pick a final k (that's #45's UX call), only shows the
budget has generous headroom for any k a real viewport would plausibly want.

**Disk-tier compression** (`cache::tests::disk_tier_compression_ratio_on_a_screen_res_synthetic_plane`,
`#[ignore]`d, run explicitly this pass): a 75.0MB synthetic screen-res RGBA16F plane (a smooth
gradient, not noise) compressed to 0.02MB via zstd (level 3, ~3688× ratio, 11.3ms) and 0.30MB via
lz4 (~247× ratio, 3.4ms). **These ratios are not a real-photo promise** — a smooth synthetic
gradient is far more compressible than real graded/denoised photo content, which is why this test
is explicitly `#[ignore]`d rather than asserted against; it's included to get real timing/round-trip
evidence on a realistically-sized payload, not to claim what a real frame will compress to. A real
ratio on actual baked output is a follow-up once #45 has one to measure (see Consequences).

**Bake-scheduler simulation** (`sim::simulate_hero_bake`, using each stage's real or
labelled-hypothesis cost from `cost_model.rs`): per-image bake cost `decode(1.7s, ADR-0037
midpoint) + denoise(50.9s, ADR-0040's real full-res SCUNet measurement) + mask_bake(1.0s, ADR-0048's
own labelled hypothesis) = 53.6s`, serialized on one bake worker (one shared GPU device/one `ort`
session, per ADR-0019/0016/0050's existing decisions — this sim doesn't get to assume parallel bake
workers). For the hero scenario's 50-image sync + sequential walk (`docs/benchmarks/
hero-scenario.md`): **first_image_ready = 53.6s, total sync-to-done = 2680s (44.7 minutes),
stale_at_arrival = 50/50 images** at a 100ms walk pace — i.e. baking full-resolution denoise for
every image, in nearest-to-cursor order, cannot possibly keep up with a user actually walking
through the set; every single image the user reaches is still on its live/stale render. **This is
a real, load-bearing finding, not a null result**: it means "denoise-at-full-resolution-first" is
the wrong scheduling default for the hero scenario's own bulk-sync workflow, and screen-resolution
denoise (cheaper, no measured cost yet — a real follow-up, since ADR-0040 only measured full-res
timing) should be scheduled ahead of full-res denoise, matching #145's own T2/T3-stale-until-baked
tiering rather than treating "baked" as a single all-or-nothing state per image.

## Options considered

| Option | Chosen? | Why |
|---|---|---|
| Flat per-stage list (ADR-0021's original `EditDocument` shape) reused directly as the render graph | No | Render order and multi-parent dependencies (mask bake depends on both the neutral render and its own recipe) need a real DAG, not a `BTreeMap` keyed only by stage id — ADR-0021 itself defers this to #44 |
| Single fused mega-kernel re-run on every tweak (RapidRAW's approach, `docs/research/stalk-prior-art.md`) | No | ADR-0069 already flags this as RapidRAW's "direct conflict" with Tapetum's whole design; this pass's own scheduling-sim finding (full-res denoise can't keep up with a 50-image walk) would be *worse*, not better, under a no-caching model |
| vkdt-style topological DAG with per-ROI subgraphs | Partially — the DAG/cache-key structure, not the ROI subgraph splitting | ROI-level subgraph splitting is real additional complexity `docs/research/stalk-prior-art.md` flags as vkdt's own contribution; this pass's graph is whole-frame per node, matching what ADR-0021/0016's own stage granularity already assumes — ROI splitting is a plausible future optimization, not required to validate this ADR |
| Parallel bake workers (multiple GPU devices or concurrent `ort` sessions) | No | ADR-0019/0016/0050 already committed to one shared `wgpu::Device`; revisiting that for Tapetum alone would be a bigger, separate decision this ticket isn't scoped to make |

## Consequences

- **Unblocks #45** (render engine core): the stage order, cache-key scheme, and cache-tier design
  above are #45's starting point, not a re-derivation. #45 also owns: the real execution signature
  `crates/nicti-tapetum::RenderStage` still lacks, wiring a real GUI viewport (egui/pelt-egui's
  `ViewportCallback` pattern, ADR-0068), and the actual screen-capture hero-scenario run
  (`docs/benchmarks/hero-scenario.md`'s own switch/crop/zoom protocol) this pass explicitly didn't
  attempt.
- **Unblocks #46, #49, #51, #52, #54/#55, #60, #99, #100, #101, #144, #145** — each can build against
  a settled stage-order proposal and cache-key scheme instead of an open question.
- **Real follow-up: screen-resolution-first denoise scheduling.** This pass's own sim shows
  full-res-first denoise cannot keep the hero scenario's bake queue ahead of a walking cursor.
  Filed as a follow-up (see below) rather than solved here — needs a real screen-res SCUNet timing
  measurement (ADR-0040 only measured full-res) before a scheduling policy can be picked.
  Follow-up: [#189](https://github.com/jordanfelle/nicti/issues/189).
- **Real follow-up: real-photo disk-tier compression ratio.** This pass's zstd/lz4 numbers are on a
  synthetic gradient, explicitly not a promise about real content. Filed alongside #45's real render
  output, once there's real baked output to measure against.
  Follow-up: [#190](https://github.com/jordanfelle/nicti/issues/190).
- **Resolved: lens-correction placement.** [#191](https://github.com/jordanfelle/nicti/issues/191)
  confirmed this ADR's baked-prefix placement (see "Lens-correction placement, confirmed" under
  Decision) — CA correction's channel-space requirement pins it before ADR-0038's camera→XYZ
  matrix. Bakeability itself stays conditional on #39's manual-slider UX design (LRC's own Lens
  Corrections panel has manual distortion/vignette/defringe sliders), not a given regardless of it.
  #39's broader scope (correction-data source, lens coverage, that UX question) stays open.
- **`docs/licensing.md` updated in this PR**: two new crates (`zstd`, `lz4_flex`), both permissive,
  already covered by `deny.toml`'s existing allowlist.
- **New topic `render-graph`** added to `CLAUDE.md`'s topic list and `.claude/rules/`/
  `docs/decisions/`.

## Spike: `spikes/loaf`

Name: a cat loaf (the tucked-paws resting pose) doubles as "baked" — the whole idea behind reusing
baked stage output instead of recomputing it. 35 tests (32 unit + 3 GPU-parity, the latter checked
against this sandbox's lavapipe software adapter for correctness — see the reference-machine
numbers above for real hardware timing). Modules: `hash.rs` (canonical-JSON + blake3, generalized
from `spikes/pawprint`'s one-upstream-hash version to an arbitrary-arity DAG), `graph.rs` (the
stage DAG, topological order, cache keys, invalidation sets), `cost_model.rs` (bake-stage cost
constants, each citing its source ADR), `cache.rs` (byte-budgeted LRU tiers + zstd/lz4
compression), `geometry.rs` (the affine crop/rotate/zoom/pan model + CPU sample reference),
`refine.rs` (guided-filter mask refine, ported from `spikes/siamese`), `gpu.rs` (wgpu context +
three kernels: `live_suffix`, `present_sample`, `box_filter`, each with a one-shot function and a
persistent-buffer `*Kernel` type for repeated-call timing), `prefetch.rs` (nearest-to-cursor
priority ordering), `sim.rs` (the hero-scenario bake-queue simulation). `src/bin/loaf.rs` exposes
`graph`/`bench`/`sim` subcommands — `bench` is what fills in this ADR's Measured results table on
the reference machine, writing `nicti-prowl`-format reports to `bench-results/`.
