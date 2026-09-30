# ADR-0049: Masks and local adjustments — the build

- **Status:** Accepted
- **Date:** 2026-09-30
- **Ticket:** [#49](https://github.com/jordanfelle/nicti/issues/49) Build: masks + local adjustments
- **Builds on:** [ADR-0048](0048-masking.md) (the research this promotes),
  [ADR-0044](0044-stage-cached-render-graph.md) (where masks sit in the render graph),
  [ADR-0021](0021-non-destructive-edit-model.md) ("the recipe, not the pixels"),
  [ADR-0051](0051-healing-removal-build.md) (the model store and the async-result pattern),
  [ADR-0218](0218-local-only-ai.md) (how model weights may reach the user)

## Context

ADR-0048 settled the *design* of masking in a throwaway spike (`spikes/siamese`): BiRefNet for
subject/background, the mask-group model, brush and gradient geometry, a guided-filter refine, and
five WGSL kernels. It never ran a real model, and it left the actual local adjustments, the render
integration, the UI and the model plumbing to #49. This ADR records what was built and — more
usefully — every place the build **departed from the spike or the ADR**, and why.

## Decision

### 1. One `nicti.masks` stage; a normalized, sanitized data model

All of a photo's local corrections are one stage entry (`nicti-tapetum/src/mask/params.rs`):
`MaskParams { corrections: Vec<LocalCorrection> }`, each a `MaskGroup` (components folded left to
right) plus a `LocalAdjust` and an `amount`. One stage, not one per mask: the graph and registry are
fixed-shape (`apply_document` errors on a node id missing from the registry), invalidation
granularity is achieved *inside* the stage (the engine keys each correction's composite by its own
content hash), and a whole set copies/pastes/syncs as a unit like any other stage.

- **Normalized coordinates.** Points are `x / width, y / height` of the *uncropped* frame; lengths
  (radius, feather) are fractions of the long edge. A mask therefore means the same thing at any
  resolution, survives a crop change, and pastes onto a differently sized photo. (Heal's spots use
  source pixels; masks deliberately don't.)
- **Sanitized, because documents are untrusted** (a synced or imported edit). `MaskParams::sanitized`
  caps counts (16 corrections, 16 components each, 200 000 brush points document-wide, 4096 strokes,
  16 colour samples), clamps every number and scrubs NaN — the canonical hasher refuses JSON `null`,
  which is what a NaN serializes to. Over a limit is dropped, never a panic or an unbounded
  allocation.
- **Serde shape.** `MaskSource` is internally tagged (`kind`), so `Brush` is a struct variant: the
  spike's newtype-of-a-`Vec` could not be internally tagged at all.
- **Fold math, amended from the spike.** `Add` is a union (`max(acc, w)`), `Subtract` is
  `acc * (1 - w)`, `Intersect` is `acc * w`. The spike's `Add` was `min(acc + w, 1)`, which summed two
  overlapping feathered edges into a visible seam.

### 2. The neutral render taps post-lens, **pre-heal** — and the renderer stays a linear chain

ADR-0044 placed the AI mask bake after the heal stage ("bake boundary + default tone"). That would
re-run BiRefNet on every heal edit and every AI-removal patch arrival, so #49 taps **post-lens,
pre-heal**: `nicti.neutral` is a *keying-only* `Baked` node whose upstream is `nicti.lens`. Nothing
renders it (it is not in the baked chain); it exists so an AI bake key chains from LENS and so a tone,
white-balance, crop, local or heal edit provably cannot change it (`the_neutral_key_ignores_every_
edit_including_heal_but_follows_the_photo`).

Consistently, `Renderer` was **not** generalized into a multi-input DAG executor. The model runs as
an async Pounce job and its result is stamped into the document the render sees
(`compose::stamp_ai_alpha_state`, the heal `stamp_removal_state` pattern); mask raster/compose/apply
runs inside the fused live dispatch. `LiveExec`, `RenderRequest` and every mock are unchanged.

**`Renderer::render_baked`** was added because of a real ordering constraint: `render` records every
pass into one encoder submitted at the end, so the mask engine — which needs the *baked frame* to refine
AI alphas and build range masks — cannot read it from inside `LiveExec::encode` (anything submitted
from there runs first and reads an unwritten texture). `DevelopView::render` bakes and submits the
chain first, prepares the masks from that frame, binds them, and lets the ordinary render find every
baked stage already cached. Tested: the baked work happens exactly once.

### 3. Cache keys and the async AI flow

- **Bake key** = `hash(neutral_key, recipe)`, deliberately independent of `invert`, `opacity` and
  `op`: "Select Subject" and "Select Background" (the same recipe, inverted) share one model run.
- **An alpha arriving** is stamped into the rendered masks entry as `"ai_alphas": {bake key → content
  hash}` — only for *enabled* corrections — so it recomposes exactly the corrections that use it.
- **Bakes are requested for enabled corrections, not only active ones.** A freshly added Select
  Subject has no adjustment yet (the engine treats it as inert), but the user is looking at its
  selection; waiting for the first slider move would show them nothing.
- **A result for a photo the user has left is dropped.** Alphas are cleared on photo change.
- **Pre-existing bug fixed first.** `load_real_frame` recorded the photo's identity with
  `set_own_hash(DECODE, …)`, which the next render's `apply_document` silently overwrote, so two
  different photos at the same extent shared every baked cache key. With masks, one photo's AI alpha
  would have been applied to the next. The identity now rides in the DECODE entry of the document each
  render sees (regression test, verified to fail without the fix), and `load_real_frame` also applies it
  to the graph immediately, so `neutral_key()` is never one render stale.

### 4. The engine: every expensive thing is cached, and each edit redoes only what it changed

`MaskEngine` (`nicti-tapetum/src/mask/engine.rs`) turns the params into an `Rgba16Float` atlas layer
per four corrections (one per channel), sampled bilinearly by the live shader. Each row below is a
test asserting it with counters (`MaskStats`):

| Edit | What runs |
|---|---|
| A slider or Amount drag | nothing here — the deltas live in uniforms; zero recomposes, zero repacks |
| Move one correction's geometry | exactly that correction recomposes; the atlas repacks once |
| Paint a brush stroke | one GPU pass per frame: the field after strokes `[..n-1]` is cached |
| An AI alpha arrives | only its corrections recompose; a mask and its inverse share one guided refine |
| A new guide frame (heal edit, another photo) | AI and range masks rebuild; gradients and brushes don't |
| Undo back to an earlier state | zero recomposes (composites are still cached) |

Masks render at the frame extent **capped at 4096 px on the long edge** (~45 MB per `R32Float` field);
the bilinear sample hides it on screen. Export should build them at full size (#354).

**Brush rasterization.** The spike's kernel looped over *every dab for every pixel*. Here a stroke is a
stored polyline (small documents; dabs are derived at `radius/4` spacing, widened rather than truncated
past a cap), dabs are binned into 64 px tiles on the CPU, and one GPU pass per stroke covers only its
bounding box, reading a sampled texture and writing a fresh one.

### 5. Local adjustments stack additively on the global values, as LRC does

At a pixel the effective value of each slider is `global + Σ weightᵢ · amountᵢ · deltaᵢ`, computed
in one loop over the active corrections in the fused live shader. Placement (ADR-0038 order):
`matrix → [DCP HueSatMap] → exposure* → [DCP LookTable] → dehaze* → temp/tint* → tone* → tone curve →
clarity/texture* → vibrance → HSL → saturation/hue/colour overlay*` (`*` = local). Each local step is
skipped when its stacked delta is exactly zero, so a mask that selects nothing leaves pixels
**bit-identical** (found because an unbaked AI mask altered them by a rounding error). Definitions a
reader will want: local **temp/tint** are per-channel gains in linear working space (`2^±0.5·temp` on
red/blue, `2^-0.25·tint` on green), *not* a camera-matrix white-balance solve; **hue** rotates about the
grey axis by up to 30°; the **colour overlay** is a luma-preserving tint; contrast is clamped to
`-1..2` and the other tone sliders to `±2` after stacking.

**Spatial adjustments** need neighbouring pixels, so their inputs are precomputed and cached per
(baked frame, extent) in `mask/bases.rs` and the shader only *applies* them — a drag stays a uniform
write. Built only when some correction uses them, from the as-shot baked frame (so a global WB/tone
drag never rebuilds them), at ≤ 2048 px:

- **Clarity and texture**: the baked perceptual luminance is smoothed twice with a *self-guided*
  guided filter (fine and coarse radius); the fine band `g − base_fine` is texture, `base_fine −
  base_coarse` is clarity, applied as a chroma-preserving brightness ratio (clamped 0.25–4). Edge
  preserving, so a hard step does not ring. **The regularizers were retuned because the halo test
  failed**: the first coarse `eps` (1.6e-2) treated a 0.17 step as low-contrast detail and darkened the
  pixel beside it by 25 %. `eps` 2e-3 (coarse) / 1e-3 (fine) keeps the edge and still captures ~0.03
  mid-scale detail.
- **Dehaze**: the dark-channel prior. The airlight comes from a ~128 px GPU thumbnail read back to the
  CPU (brightest of the haziest 0.1 %), the dark channel from a separable GPU min filter, the
  transmission refined with the guided filter. A negative amount adds a uniform veil (≤ 50 %) toward the
  airlight. **The prior needs something to anchor the airlight on** (a sky or dense-haze region); a
  scene that is uniformly hazy with no such pixel gets an underestimated airlight — a known limit of the
  method, asserted by a test scene that includes a sky band.
- **Sharpness and noise** add to the global Detail-panel values *inside `detail_combine`* (local noise
  adds to the NR luminance amount, local sharpness to the sharpen amount, below zero it softens). The
  multi-pass detail path is now also taken when only a *local* sharpness/noise is set, so an untouched
  global Detail panel no longer hides a local edit; with no local detail the single-pass fast path is
  unchanged.

Not built: Moiré and Defringe (#351).

### 6. Model choice is data: a pluggable, backend-agnostic provider layer

`nicti-stalk`'s empty `ModelProvider` marker (whose signature its docs left to #49) gained
`SegmentationProvider` / `Segmenter` / `SegmentationRegistry` — plain data in (`&[f32]`), `AlphaMap`
out, **no `ort` dependency**, so a provider can wrap ONNX Runtime, another runtime, or a heuristic.
`resolve_provider` finds the provider for exactly the recipe's `(model_id, model_version)` and returns a
typed error for an unknown model, a **version mismatch** (an old edit never silently runs a newer
model), or an unsupported target. `nicti-siamese::providers::default_model_for(target)` is the one
table saying which model a *new* mask uses, so a model picker (#346) only writes a different `model_id`
into the recipe, and a newer or better model is one more `register` call.

### 7. BiRefNet as shipped, and its provenance

The default subject model is the **fp32 ONNX conversion** at `onnx-community/BiRefNet-ONNX`, pinned to
revision `534d3c82…` by size and SHA-256 (both verified against an actual download). It is an
on-demand download only (972 MB; ADR-0218), re-hashed before first load on the worker thread. The fp16
export in the same repo is deliberately not used: ONNX Runtime's CPU provider has thin fp16 kernels.
Input/output *names* are read from the loaded graph, not hard-coded. The model's version string names
the pinned revision, so swapping the export forces a new edit.

**Provenance, stated precisely** (and corrected during this work — an earlier draft of the artifact
claimed the weights were trained on "several research datasets with unreviewed terms"): the upstream
model card says the model is *"trained on DIS-TR"*, matching `docs/licensing.md`; this is MIT. What is
**not verified** is that the third-party conversion's weights equal the upstream checkpoint (#348).

Sky is ADR-0048's interim flood-filled heuristic (#347), an ordinary provider with nothing to download,
labelled *beta*.

### 8. UI

A third tool in the Develop tool switch (`nicti-pelt/src/mask_panel.rs`). Creating Subject / Background
/ Sky / Brush / Linear / Radial / Luminance range / Colour range; a list with enable, duplicate,
duplicate-and-invert and delete; per-component op / invert / opacity; the full LRC local slider set
shown as −100…100; drag gestures selected by a "Drag edits" row (brush, linear and radial handles, a
colour eyedropper); `[`/`]` resize the brush and `O` toggles the overlay only while the pointer is over
the photo. The AI download prompt states size, sources and provenance, downloads nothing until clicked,
and offers Cancel, Repair and Retry. The **overlay is a CPU preview painted by egui** (geometry, the
low-resolution AI alpha, a thumbnail for range masks) rather than a shader change, leaving the
colour-managed display shader untouched. The editing operations live in `mask_edit.rs` with no egui, so
they are unit-tested directly.

## Measured results

All on the project's reference machine, an **NVIDIA GeForce RTX 5080**, release build, cross-built and
run natively on Windows as the repo's rules require for new GPU kernels
(`mask::engine::tests::throughput`, `--ignored`).

**Live pass** — the cost a slider drag pays every frame. ADR-0044's yardstick is the 4 ms live-suffix
rule. Wall-clock, submit + device wait, p50 (p95):

| 45 MP (8280×5520) | Vulkan | Dx12 |
|---|---|---|
| no masks (baseline) | 0.92 ms | 0.76 ms |
| 16 masks, pointwise | 2.34 ms (2.43) | 2.61 ms (2.73) |
| 16 masks + clarity/texture/dehaze | 2.50 ms (2.80) | 2.89 ms (**3.19**) |

The design risk flagged before building — that 16 masks at 45 MP would break 4 ms — did **not**
materialize, so neither tile-bitmask culling nor a preview/full-res live split is needed yet. Memory:
the atlas is 85 MB for ≤ 4 masks and 341 MB for 16 at 45 MP.

**Edits that rebuild something** (p50): move a gradient 3.2 ms (Vulkan) / 2.1 ms (Dx12); one more brush
point 3.3 ms / 2.3 ms (p95 up to 9 ms); guided refine of a 1024² AI alpha 7.8 ms / 5.3 ms; clarity +
texture + dehaze bases 10.3 ms / 18.3 ms (built once per baked frame).

**The real BiRefNet** (`crates/nicti-siamese/tests/real_models.rs`, the pinned file, run with the real
ONNX Runtime 1.28.0):

| | cold (load + SHA-256 verify + first run) | warm |
|---|---|---|
| Windows, CPU execution provider | 15.3 s | **9.2 s** |
| Linux sandbox, CPU execution provider | 30.4 s | 18.3 s |

- The tensor contract is **verified against the real weights**: it loads, the names resolve from the
  graph, the output is 1024×1024 in 0…1, and a bright synthetic subject on a dark background gets alpha
  1.000 inside and 0.000 in a corner.
- **ADR-0048's "≤ 1 s per bake" hypothesis assumed CUDA and is not met on the CPU provider — by ~9×.**
  That ADR said a CPU-provider bake is "reported, not gated"; it is reported here. A bake is a background
  job with a *Selecting…* indicator, so it is usable, but ~9 s per Select Subject is poor. A GPU execution
  provider (#345) and a CPU-viable model (#349) are filed; the download prompt does not hide it.
- **Quality on real photos — fursuiters in particular — was not evaluated**; only a synthetic scene.
  That remains #171's job.

**Real-hardware finding: a Dx12 bug on `main`.** The first Dx12 run failed
`stages::tests::full_pipeline_end_to_end_produces_correctly_colored_output`, an *existing* test. Building
`main`'s own test binary and running it on the same hardware confirmed it fails there too — it is the
hazard ADR-0051 flagged as unaudited: `detail_blur.wgsl` read its input through a
`texture_storage_2d<.., read>`. #49's local sharpness/noise ride that blur path, so #49 fixed it (and
converted `live_suffix` and `detail_combine` the same way). With that, **the whole tapetum suite — 313
tests — passes on the RTX 5080 under both Vulkan and Dx12.** `present_sample.wgsl` still reads a storage
texture and is unaudited (#355).

## Options considered

- **One stage per mask** (the pawprint fixtures' `mask.subject_0` ids): needs dynamically registered
  stages and graph nodes; invalidation is better achieved inside one stage.
- **Generalize `Renderer` to a multi-input DAG** so `mask_bake` is a real node: a large change to the
  most-tested code in the crate for no benefit the stamp-into-document pattern doesn't already give.
- **Draw the overlay in the display shader**: needs an atlas binding through the colour-managed display
  pipeline, whose uniform layout has already had one size-mismatch bug. A CPU preview needs none of it.
- **fp16 BiRefNet** (490 MB): ONNX Runtime's CPU provider has thin fp16 kernel coverage and falls back
  through casts. Worth measuring on a GPU provider (#349).
- **Refining the AI alpha on the CPU**: needs a full-frame readback of the baked frame.

## Consequences

- **#49 ships**: masks and local adjustments in Develop. `spikes/siamese` is deleted (its compose,
  geometry, refine and WGSL superseded by `nicti-tapetum::mask`; its segmentation scaffolding by
  `nicti-siamese`); `nicti-groom`'s cross-crate `ort` test no longer uses it as a caller.
- **Follow-ups**, each filed: a GPU execution provider for masks ([#345](https://github.com/jordanfelle/nicti/issues/345));
  a model picker ([#346](https://github.com/jordanfelle/nicti/issues/346)); a provenance-clean sky model
  ([#347](https://github.com/jordanfelle/nicti/issues/347)); verifying the BiRefNet conversion
  ([#348](https://github.com/jordanfelle/nicti/issues/348)); a CPU-viable subject model
  ([#349](https://github.com/jordanfelle/nicti/issues/349)); Select Object / people masks
  ([#350](https://github.com/jordanfelle/nicti/issues/350)); Moiré and Defringe
  ([#351](https://github.com/jordanfelle/nicti/issues/351)); global Clarity/Texture/Dehaze
  ([#352](https://github.com/jordanfelle/nicti/issues/352)); a disk tier for baked alphas
  ([#353](https://github.com/jordanfelle/nicti/issues/353)); full-resolution refine for export
  ([#354](https://github.com/jordanfelle/nicti/issues/354)); the remaining Dx12 audit
  ([#355](https://github.com/jordanfelle/nicti/issues/355)); unloading an idle session
  ([#356](https://github.com/jordanfelle/nicti/issues/356)); brush pressure/flow/auto-mask
  ([#357](https://github.com/jordanfelle/nicti/issues/357)); the post-lens neutral image once lens
  correction lands ([#358](https://github.com/jordanfelle/nicti/issues/358)). Notes were left on #171,
  #62, #52 and #324.
- **Not undoable yet** — like heal, mask edits write straight into the in-memory document (#324).
- **Known limits**: the mask overlay shows the *unrefined* AI alpha (the real render refines it); a
  uniformly hazy scene with no sky defeats the airlight estimate; the sky mask is a heuristic; a mask's
  edge is at most 4096 px resolution until export builds its own (#354); the neutral image is built from
  the pre-lens frame and will need the post-lens one when lens correction is real (#358).
