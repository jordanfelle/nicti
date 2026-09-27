# ADR-0048: Masking

- **Status:** Proposed — pending a reference-machine pass (real BiRefNet/MobileSAM weights, real
  photos including fursuiters, and #44's own gating)
- **Date:** 2026-09-26
- **Ticket:** [#48](https://github.com/jordanfelle/nicti/issues/48) Research: masking
- **Formerly:** ADR-0024 (sequential numbering, pre-#183)

## Context

#48 is the last open research ticket (alongside #40, in progress) blocking #44 (Tapetum, the
stage-cached render graph), which in turn gates roughly a dozen develop/build tickets (#45, #46,
#49, #51, #52, #54/#55, #60, #99, #100, #101). It asks for three things: (1) evaluate BiRefNet /
SAM-family models for subject/sky/background masks, (2) design the brush + gradient
local-adjustment mask model, and (3) the inverse-mask (`1.0 - alpha`) case the hero scenario
exercises directly — `docs/benchmarks/hero-scenario.md`'s edit stack includes both "Mask 1: Select
Subject" and "Mask 2: Select Subject, Invert", synced across all 50 images.

Constraints already fixed elsewhere, that this ADR has to fit inside rather than re-decide:

- **ADR-0021** (`docs/adr/0021-non-destructive-edit-model.md:117-123`): "A mask stage stores
  `{model_id, model_version, params, seed?}` for AI-generated masks ... and vector geometry for
  brush/gradient masks — never the derived pixel mask itself, which is Tapetum's job to compute and
  cache."
- **ADR-0019 §3 / ADR-0016**: AI inference loads via `ort`'s `load-dynamic` feature
  (`ort::init_from(path)`), CUDA/TensorRT execution providers with CPU fallback; `wgpu`/WGSL for
  GPU compute, with a bake-time-only host↔device round-trip (never per-frame) and a 2D dispatch
  grid to avoid wgpu's 65535-per-dimension workgroup limit.
- **ADR-0018**/`docs/licensing.md`: a model needs a licensing.md row (code/weights/provenance) in
  the same PR; bundle only if both the weights license and training-data provenance allow it,
  otherwise it ships as an on-demand download.
- **#44's own body** already expects, of whatever this ADR proposes: AI masks baked and cached
  while local-adjustment math stays live in the shader; masks generated at preview resolution
  first, refined to full resolution via an edge-aware (guided-filter) upsample only when
  zoomed/exporting; a disk cache tier for compressed mask alpha.
- **`docs/benchmarks.md:17-18`**: slider→preview ≤16.7ms (60fps at screen resolution);
  switch-between-images <100ms when warm (warm = the AI-mask/denoise stage cache is populated).
  **No mask-specific latency target exists anywhere in the repo before this ADR** — the Decision
  rule below states one, matching ADR-0050's own precedent of stating hypotheses before measuring.

**Two conflicts this research pass found between existing spike data, that this ADR resolves
rather than leaves for a later ticket to trip over:**

1. **`model_version`'s type.** `spikes/pawprint/tests/sizing.rs`'s synthetic mask stage uses a
   string (`"0.4.1"`); `spikes/groom/src/spot.rs::MaskRecipe` (ADR-0050, Accepted) uses a `u32`.
   This ADR picks **`String`** — a segmentation-model release is a semver-ish string upstream
   (BiRefNet/MobileSAM/SAM2 tags aren't sequential integers) — and groom/#51 should align to this
   on their own next touch, not the reverse.
2. **The inverse-mask double-recipe problem.** `spikes/pawprint/tests/sizing.rs`'s
   `mask.inverse_subject_0` stores a *second, separate* `{model_id, model_version, ...}` recipe
   rather than referencing the subject mask it inverts — literally the hero scenario's own "Select
   Subject" + "Select Subject, Invert" pair, which would otherwise bake the same model twice for
   every one of the 50 hero-set images. This ADR makes **inverse a property of the mask
   *component*** (an `invert: bool` flag), not a second recipe: two components can carry the exact
   same `AiRecipe`, one `invert: false` and one `invert: true`, and the recipe's own **bake key is
   defined independently of `invert`/`opacity`** — see Decision below. Both components resolve to
   the same bake key, so the model runs once and `1.0 - alpha` happens live in the shader.

## Decision

### Model choice: BiRefNet (subject) + MobileSAM (interactive click/box refine)

Both are already tagged `#48` in `docs/licensing.md` with a clean bundle verdict (BiRefNet
MIT/MIT; MobileSAM Apache-2.0, distilled from SA-1B) — no new licensing question to resolve for
adopting them. SAM2 was re-checked this pass too (its SA-V dataset license, previously flagged
"not independently re-checked", is confirmed CC-BY-4.0 directly from `sav_dataset/README.md` — see
`docs/licensing.md`'s updated row) but not adopted for v1: it's a heavier, video-oriented model
where MobileSAM's smaller interactive decoder already covers the click-refine case #48 asks for,
and BiRefNet already covers one-shot "Select Subject" at higher matting-style edge quality than
SAM's own mask output. Re-open SAM2 if MobileSAM's edge quality proves insufficient once real
weights are measured.

**Sky segmentation**: no clean, purpose-built model was adopted this pass. RapidRAW's own choice
(a community `skyseg` fine-tune of U-2-Net, downloaded from `CyberTimon/RapidRAW-Models` at
runtime — `docs/research/stalk-prior-art.md:72-74`) has an unverified third-party checkpoint
provenance (base U-2-Net is Apache-2.0, trained on the public DUTS-TR benchmark — see
`docs/licensing.md`'s new row — but the specific `skyseg` fine-tune's own training data wasn't
independently re-checked this pass). Instead, `spikes/siamese/src/sky.rs` implements a **classic
heuristic baseline**: a per-pixel blue-dominant/bright luminance test, flood-filled from the top
row so an unrelated bright/blue region elsewhere in the frame (a white wall, say) that isn't
reachable from the top edge through other sky-like pixels doesn't get picked up — same "define sky
by connectivity, not just color" principle a naive per-pixel threshold misses. This is the
fallback the Decision rule below expects to lose to a real model on quality; it ships as the
interim answer, with re-adopting U-2-Net's `skyseg` fine-tune (once its own provenance is
confirmed) or MobileSAM-with-a-sky-prompt as open follow-ups.

### The mask-group model: `spikes/siamese/src/compose.rs`

Named `MaskGroup`/`MaskComponent` to match Lightroom Classic's own `MaskGroupBasedCorrections`
shape (`spikes/shed/src/develop.rs`; `docs/adr/0061-lrc-catalog-import-mapping.md:93`), so #49/#62's
importer maps onto it directly rather than a differently-shaped Nicti-only structure:

```rust
struct AiRecipe { model_id: String, model_version: String, params: Value, seed: Option<u64> }
enum MaskSource { Ai(AiRecipe), Geometry(Geometry) }
enum Op { Add, Subtract, Intersect }
struct MaskComponent { source: MaskSource, op: Op, invert: bool, opacity: f32 }
struct MaskGroup { components: Vec<MaskComponent> }
```

Composition folds components left to right: each component's raw weight (a baked AI alpha looked
up by its recipe's bake key, or a geometry rasterized live) is optionally inverted, scaled by
`opacity`, then combined into the running composite via `op`. **`ai_bake_key()` hashes only
`{model_id, model_version, params, seed}` plus the caller-supplied upstream-model-input hash — not
`invert`/`opacity`/`op`** — so a mask and its inverse share one bake key
(`bake_keys_dedupe_the_shared_recipe_between_a_mask_and_its_inverse`,
`compose_inverts_the_same_baked_alpha_for_the_inverse_component` in
`spikes/siamese/src/compose.rs`'s tests), while the whole-group hash (what actually gates a visible
recompose) still changes when `invert` does. A `Geometry` source has no bake key at all — it
rasterizes live every frame, proven not to touch the bake-alpha lookup at all
(`geometry_component_bypasses_the_bake_lookup_entirely`).

### The AI model input is decoupled from tone sliders — the single most important cache-key
### decision this ADR makes

An AI mask (BiRefNet/MobileSAM) infers on a **fixed neutral render** — the image after lens
correction, at a fixed default tone map — not on the user's live, currently-adjusted look.
Otherwise every exposure/tone slider drag would invalidate and re-bake every AI mask in the frame,
and the hero scenario's bulk-synced masks would re-run the model 50 times over instead of once.
This is what `ai_bake_key`'s `upstream_model_input_hash` parameter is: the hash of that fixed
neutral render, not of the live edit stack.

### Vector local-adjustment geometry: `spikes/siamese/src/geometry.rs`

`LinearGradient { p0, p1, invert }`, `RadialGradient { center, radii, angle, feather, invert }`,
and `Brush(Vec<Stroke>)` where each `Stroke` is an ordered list of `Dab { center, radius, feather,
flow }` plus its own `erase: bool`. Brush strokes fold sequentially — dabs within one stroke
combine via `max` (overlapping dabs don't double-darken,
`brush_dab_blends_with_max_not_addition`), and a later erase stroke subtracts from what came before
(`erase_stroke_subtracts_from_a_prior_add_stroke`) — matching how a real brush tool behaves stroke
by stroke, not how a naive per-dab accumulation would.

### Preview-to-full-res refinement: `spikes/siamese/src/refine.rs`

A **guided filter** (He, Sun & Tang 2010/2012's fast variant — filter at low resolution, upsample
the linear coefficients rather than the alpha itself), using the full-resolution photo's own
luminance as the edge guidance signal. Proven to sharpen a softly-sampled low-res mask boundary
back toward the guidance image's real edge
(`a_real_edge_in_guidance_sharpens_a_softly_sampled_low_res_boundary`) rather than a plain
bilinear alpha upsample's uniform blur across the boundary.

### GPU: WGSL twins for every per-frame kernel, CPU-checked

Per #44's own expectation ("local-adjustment math stay[s] live in the shader"), four kernels have
`wgpu` compute-shader twins, each checked against its CPU reference within `1e-4` tolerance in
`spikes/siamese/tests/gpu_parity.rs` (9 tests, passing against this sandbox's lavapipe software
adapter — see the reference-machine follow-up below for real hardware numbers):
`gradient_linear.wgsl`/`gradient_radial.wgsl` (rasterize a gradient mask directly on GPU),
`brush.wgsl` (the same stroke-ordered add/erase fold as the CPU reference, via a flat dab list
tagged with `stroke_id`), `compose.wgsl` (one component's invert→opacity→op fold, called once per
component exactly like the CPU `compose()` loop), and `masked_adjust.wgsl` (a local-exposure delta
scaled by mask weight, in linear light — `out = image * 2^(ev * mask)` — proving that a local
adjustment itself needs no bake step at all, only the AI alpha feeding it does).

### AI inference scaffolding: `spikes/siamese/src/segment.rs`

Same `ort`/`load-dynamic` pattern as `spikes/groom/src/ai.rs` (ADR-0050): a process-global
`OnceLock`-guarded `ort::init_from(dylib_path)`, `ModelNotFound` returned cleanly (never a panic)
when the model file is absent — proven by tests that run in CI with no model file present.
**No real ONNX weights are committed or downloaded in this sandbox.** A full BiRefNet ONNX export
exists publicly (~970MB, `huggingface.co/onnx-community/BiRefNet-ONNX`) but downloading and
running it was out of this pass's time budget, the same "obtaining actual checkpoints is out of
scope for this spike" call ADR-0050 made for LaMa/MobileSAM. `BiRefNet` is genuinely
single-input/single-output (no simplification needed there, unlike groom's healing case).
`MobileSam`, unlike groom's single-tensor collapse, is split into **`encode()`/`decode()`** methods
matching MobileSAM's real two-session contract (an expensive image encoder run once per image,
producing an embedding; a cheap prompt decoder run once per click/box against that embedding) —
because the split *is* the point for #44: the embedding is what gets baked and cached, so a second
or third click after the first only re-runs the cheap decoder, not the whole model. Exact
input/output tensor names and shapes are still unverified against real weights, same caveat class
as groom's `outputs[0]` risk.

## Decision rule (stated before measuring, per ADR-0050's precedent)

- **Quality**: judged against a hand-labeled set once real weights and a reference-machine pass
  exist — real photos, **must include fursuiters**, not just human faces or COCO-style object
  classes (#4's explicit v1 requirement, and the same bar #34/#35's cull research already commits
  to). LRC itself has no exportable ground-truth mask to diff against, so the real comparison is
  visual, side-by-side with LRC's own "Select Subject" on the hero set. Not measured in this
  sandbox at all (no real weights) — this is a hypothesis for the reference-machine follow-up
  below, not a result.
- **Speed** (hypotheses, not yet measured on real hardware): AI bake ≤1s/image at preview
  resolution on a CUDA execution provider (so the hero scenario's 50-image bulk-mask bake stays
  under a minute); a MobileSAM click-refine ≤100ms once the embedding is already cached; the
  gradient/brush/compose/masked-adjust-apply GPU kernels stay within the 16.7ms frame budget
  (`docs/benchmarks.md:17`) — plausible given ADR-0050's own GPU Poisson-solve result (0.386ms p50
  on the reference RTX 5080) for a comparably simple per-pixel kernel, but unmeasured here; a
  CPU-execution-provider bake is reported, not gated, matching ADR-0050's own CPU-fallback stance.
- **License**: gate on bundling only (ADR-0018) — an unbundleable model still ships as an
  on-demand download, a materially different question from redistributing it inside Nicti's own
  installer.

## Measured results

**CPU-only, synthetic data — this sandbox has no real BiRefNet/MobileSAM weights and no
GPU-backed Vulkan/Dx12 adapter (WSL without a real Vulkan ICD, same as every prior GPU-research ADR
in this repo).** Every CPU-reference correctness/parity claim above is real, proven by the 32
passing tests in `spikes/siamese` (23 unit + 9 GPU-parity, the latter against lavapipe). Every
quality/GPU-hardware-speed/AI-inference-latency number is **TBD — reference machine**, matching
ADR-0016/0068/0050/0038's own established convention for exactly this gap.

`MaskGroup`/`MaskComponent` serialized size (canonical `serde_json`, one `Ai` component): informal
comparison against ADR-0021's own 563-byte/5-stage reference document — a single mask component
carrying one AI recipe is a comparable order of magnitude to one stage entry's share of that
document, consistent with ADR-0021's "single-digit-GB across the whole 2M-asset catalog" sizing
conclusion; not a hard byte-count assertion, same caveat ADR-0050's own sizing table states.

## Options considered

| Option | Handles subject/sky/background? | Interactive refine? | Verdict |
|---|---|---|---|
| BiRefNet only | Subject/background, high edge quality (matting-style) | No (one-shot, no click prompt) | Necessary but not sufficient alone |
| MobileSAM only | Yes, via click/box prompts | Yes, cheap decoder-only refine | Weaker one-shot "just select the subject" default than BiRefNet |
| **BiRefNet (default subject) + MobileSAM (click refine), this ADR's decision** | Both | Yes | Covers the "just work" default and the "let me correct it" interactive case |
| SAM2 | Yes, and video-capable | Yes | Heavier, video-oriented; no need identified beyond what MobileSAM already covers for stills |
| A single recipe field with a manual `invert` flag folded into `params` (rejected) | — | — | Would require re-parsing `params`'s untyped JSON to detect the inverse case, instead of a typed `invert: bool` the compose step reads directly — no real benefit, more fragile |

## Prior art

`docs/research/stalk-prior-art.md:69-89` already surveyed RapidRAW's real, shipping masking stack
(SAM-ViT-B split into separate encoder/decoder ONNX sessions — the same two-session shape this
ADR's `MobileSam::encode`/`decode` mirrors — plus a U-2-Net `skyseg` variant, both downloaded
on-demand from a HuggingFace model repo at runtime) and ADR-0069 already recommends reading that
code as a reference, not for adoption (RapidRAW's own edit storage is an untyped JSON blob, no
catalog DB, incompatible with ADR-0021's typed stage model). This ADR's own contribution is the
model-recipe/inverse-sharing/bake-key design ADR-0021 left open, and the geometry+compose model
LRC's own `MaskGroupBasedCorrections` shape informs but doesn't fully specify (LRC's exact
structure is third-party reverse-engineered, per ADR-0021's own `crs:` caveat).

## Consequences

- **Unblocks #44** (Tapetum): this ADR's proposed stage placement is "AI masks bake as a distinct
  stage after color/tone, before any masked local adjustment reads them" — flagged as a proposal
  for #44 to adopt or revise, not a binding decision this ADR is scoped to make (same posture
  ADR-0050 took for its own heal/remove stage-order proposal).
- **Unblocks #49** (the real masking build ticket) and **#51** (healing/removal's own AI
  scaffolding) — both can build on `compose.rs`'s `MaskGroup` shape and `segment.rs`'s
  `encode`/`decode` split rather than re-deriving them; **#51/groom's own `MaskRecipe.model_version`
  should switch from `u32` to `String`** to match this ADR's resolution of conflict 1, filed as a
  follow-up issue.
- **Range masks (luminance/color) are a real gap this ADR doesn't cover.** #48's own scope was
  literally "the brush & gradient local-adjustment mask model" — `MaskSource::Geometry` only has
  `LinearGradient`/`RadialGradient`/`Brush` variants, no luminance-range or color-range selection.
  LRC's `RangeMaskMapInfo` develop key is already classified under the `Masks` owner category by
  `spikes/shed/src/develop.rs::classify_key` (ADR-0061), which maps to **#49** — so this isn't an
  orphaned gap needing a new ticket, but #49 needs to add a `RangeMask` (or similar) `Geometry`
  variant of its own alongside the three this ADR ships, not assume the enum is already complete.
- **`docs/licensing.md` updated in this PR** per ADR-0018's same-PR rule: SAM2's SA-V flag
  resolved (CC-BY-4.0, confirmed directly), a new U-2-Net row added (the sky-model candidate
  RapidRAW itself uses), BiRefNet/MobileSAM rows left as-is (already clean, already tagged #48).
- **Reference-machine pass still needed** before this ADR can move to Accepted — filed as a
  follow-up issue (`**Part of:** #8`), same "spec + tooling merged, baseline measurement deferred"
  shape as #90/#97/#149/#163/#164.
