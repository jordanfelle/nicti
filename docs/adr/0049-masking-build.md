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
  caps counts (16 corrections, 16 components each, 200 000 brush points document-wide, 4096 strokes per brush component,
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
| A slider or Amount drag | nothing on the GPU — the deltas live in uniforms; zero recomposes, zero repacks. The composite and atlas *keys* are computed first and the atlas is reused on a match, so a drag never touches a composite texture even when the cache can't hold them all (tested with a 1-byte budget) |
| Move one correction's geometry | exactly that correction recomposes; the atlas repacks once |
| Paint a brush stroke | one GPU pass per frame: the field after strokes `[..n-1]` is cached |
| An AI alpha arrives | only its corrections recompose; a mask and its inverse share one guided refine |
| A new guide frame (heal edit, another photo) | AI and range masks rebuild; gradients and brushes don't |
| Undo back to an earlier state | zero recomposes (composites are still cached) |

Masks render at the frame extent **capped at 4096 px on the long edge** (~45 MB per `R32Float` field);
the bilinear sample hides it on screen. **Export builds them at the photo's own extent instead (#354)**: `ExportRenderer` lazily creates a `MaskEngine::for_export` (capped only at the device's texture limit, every cache budget zero -- one photo at a time, and a native 45 MP field is ~180 MB), `render_live_frame` bakes the frame, prepares the atlas from it and hands that same frame to `Renderer::render_live_from` (export's renderer has a zero baked-cache budget, so the "next render finds it cached" shortcut Develop uses would bake twice). The shader samples by normalized UV, so nothing else changes. The atlas is unbound and the engine released as soon as the live pass is submitted, so the masks' VRAM is not held through the tile loop or into the next photo (peak during `prepare` at 45 MP with all 16 corrections is ~4.4 GB: 16 composites at ~183 MB plus a 4-layer Rgba16Float atlas at ~1.46 GB, on top of the baked and live frames; a VRAM OOM is not handled gracefully), and `set_masks(None)` runs for every unmasked photo because the kernel is reused. AI components read their baked alphas from the Larder on the export decode step (`stash::fetch_alpha`); export never bakes, so a component with no stored alpha is skipped (as in Develop while a bake is pending) and the report warns per photo. Clarity/Texture/Dehaze bases stay at their own 2048 px cap, as in Develop.

**Brush rasterization.** The spike's kernel looped over *every dab for every pixel*. Here a stroke is a
stored polyline (small documents; dabs are derived at `radius/4` spacing, widened rather than truncated
past a cap), dabs are binned into 64 px tiles on the CPU, and one GPU pass per stroke covers only its
bounding box, reading a sampled texture and writing a fresh one.

Two bounds keep a hostile or synced document from stalling the GPU. A stroke's dab count is capped at
20 000 *dabs* (not points — points closer than the spacing produce none; an early version subtracted
the point count from the cap and collapsed a 25 000-point stroke to a single dot), and its tile list at
2 M (dab, tile) entries (8 MB, under a software adapter's 128 MB binding limit): a stroke whose radius
covers the whole mask gets proportionally fewer, wider-spaced dabs. **Residual cost, not bounded:**
a document may still hold 4096 strokes per brush component, so a hand-crafted document of huge-radius
strokes costs one full-frame pass each.

**CPU cost per frame.** `prepare` runs on every render, so it hashes every brush point to build the
composite key. That hash feeds raw bytes to blake3 (~1 ms at the 200 000-point cap) rather than
canonical JSON (~45 ms). The graph's own `apply_document` still canonicalises the *stage params* each
render (the same ~45 ms at the cap, ~4 ms at 20 000 points, which is already a lot of painting);
memoising that is #362, since it belongs to the graph, not the mask engine.

### 5. Local adjustments stack additively on the global values, as LRC does

At a pixel the effective value of each slider is `global + Σ weightᵢ · amountᵢ · deltaᵢ`, computed
in one loop over the active corrections in the fused live shader. Placement (ADR-0038 order):
`matrix → [DCP HueSatMap] → exposure* → [DCP LookTable] → dehaze* → temp/tint* → tone* → tone curve →
clarity/texture* → vibrance → HSL → saturation/hue/colour overlay*` (`*` = local). The dehaze,
clarity/texture, saturation, hue and colour-overlay steps are skipped when their stacked delta is exactly
zero; exposure, temp/tint and the tone stack still run but are identities at zero (`exp2(0) = 1`, and the
clamps are no-ops for in-range globals: contrast `-1..2`, other tone sliders `±2`, wider than the UI
allows), so a mask that selects nothing leaves pixels **bit-identical** (found because an unbaked AI mask altered them by a rounding error). Definitions a
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
model card says the model is *"trained on DIS-TR"*, matching `docs/licensing.md`; this is MIT. What was **not verified at the time** was that the third-party conversion equals the upstream checkpoint;
#348 later compared them (`bench/birefnet-verify`, result in `docs/licensing.md`): the conversion's
alpha masks closely match upstream PyTorch fp32 (mask IoU >= 0.997 on 4 images, with isolated pixel disagreements; output-level evidence, not a weight proof).

Sky is ADR-0048's interim flood-filled heuristic (#347), an ordinary provider with nothing to download,
labelled *beta*.

### 8. UI

A third tool in the Develop tool switch (`nicti-pelt/src/mask_panel.rs`). Creating Subject / Background
/ Sky / Brush / Linear / Radial / Luminance range / Colour range; a list with enable, duplicate,
duplicate-and-invert and delete; per-component op / invert / opacity; the full LRC local slider set
(most shown as −100…100; exposure is in stops, the colour tint is a hue angle plus an amount, and
Amount/opacity/range sliders use their own ranges); drag gestures selected by a "Drag edits" row (brush, linear and radial handles, a
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
the atlas is 85 MB for ≤ 4 masks and 341 MB for 16 at 45 MP; the composite cache is budgeted at 1 GiB (16 worst-case 4096² fields) so the LRU doesn't thrash once a photo has 12+ corrections. The table above times the live pass only; `prepare` (hashing, and the uniform-only drag path) is CPU work it doesn't include.

**Edits that rebuild something** (p50): move a gradient 3.2 ms (Vulkan) / 2.1 ms (Dx12); one more brush
point 3.3 ms / 2.3 ms (p95 up to 9 ms); guided refine of a 1024² AI alpha 7.8 ms / 5.3 ms; clarity +
texture + dehaze bases 10.3 ms / 18.3 ms (built once per baked frame).

**The real BiRefNet** (`crates/nicti-siamese/tests/real_models.rs`, the pinned file, run with the real
ONNX Runtime 1.28.0):

| | cold (load + SHA-256 verify + first run) | warm |
|---|---|---|
| Windows, CPU execution provider (#49, one run) | 15.3 s | **9.2 s** |
| Windows, CPU execution provider (#345 harness, p50 of 5) | 11.4 s | **≈ 6 s** |
| Linux sandbox, CPU execution provider | 30.4 s | 18.3 s |

**GPU execution providers (#345)** — RTX 5080, release build, `session.run` for the pinned model on a
1024² input unless noted; idle desktop baseline 2.7–3.2 GB VRAM and ~5 % GPU, so every figure is a
delta over that. Warm = 1 warm-up + 5 measured runs.

| Execution provider (ONNX Runtime) | model | warm p50 | cold | peak VRAM |
|---|---|---|---|---|
| DirectML (NuGet ORT 1.24.4 + DirectML 1.15.4) | fp32 | **6.1 s** (a *bake*: 6 s) | 12.3 s | ~10 GB, GPU at 100 % |
| CUDA 13, default options (ORT 1.28.0 GPU build) | fp32 | 0.24 s (bake 0.28 s) | 11.9 s | ~12.7 GB |
| CUDA 13, heuristic cuDNN search + as-requested arena | fp32 | bake 0.31 s | 4.7 s | ~12.7 GB |
| CUDA 13, same options, same input | fp32 | 0.27 s (p95 0.44 s) | 0.73 s first run | ~12.8 GB |
| CUDA 13, same options, same input | **fp16** | **0.17 s** (p95 0.20 s) | 0.85–1.4 s first run | **~7.9 GB** |
| CUDA 13, shipped path (store install, clean `PATH`, pinned cuBLAS 13.8) | fp16 | bake **0.21 s** (p95 0.22 s) | 5.9 s (incl. hashing ~1.9 GB) | 8 GiB declared |

- **DirectML is out.** It registered and kept the GPU at 100 % but is no faster than the CPU provider
  for this graph — BiRefNet's attention and deformable-convolution ops don't map well to DirectML.
  (Microsoft also stopped publishing the DirectML ORT package at 1.24.4 and has DirectML in
  maintenance; it was never going to track the 1.28.0 runtime AI removal already pins.)
- **CUDA meets the ≤ 1 s target by ~4–5×**, and **fp16 is the right model for it**: 1.6× faster, 38 %
  less VRAM, same output (mean logit −7.6340 vs −7.6296 on the probe input). The fp32 model peaks at
  ~13 GB, which would not fit beside the renderer on a 16 GB card.
- The CUDA EP's defaults (exhaustive cuDNN algorithm search, power-of-two arena growth) cost ~7 s of
  first-run time and a lot of memory; `nicti-haw::session_builder` sets heuristic search, an
  as-requested arena and no max workspace. VRAM stayed ~12.7 GB for fp32 either way — it is the
  activations of a 1024² fp32 swin backbone, not allocator slack.
- **Footprint is declared, not enforced:** `MaskBakeJob::spec().vram_bytes` is 8 GiB once the session is
  on a GPU provider (`birefnet::GPU_VRAM_BYTES`), 0 on CPU. Pounce never reserves for a Foreground
  job (`queue.rs::take_next`), so today this is bookkeeping; if bakes ever become Background jobs the
  512 MiB placeholder budget in `nicti-pelt` must grow first.
- **Safety nets:** the GPU provider is only requested when the runtime directory holds
  `onnxruntime_providers_cuda.dll` (`ExecutionProvider::for_runtime`); a provider that fails to
  register, a model that fails to load on it, or a GPU `run` that errors (cuDNN missing, out of VRAM
  beside the renderer, a driver reset) all fall back to the CPU provider on the fp32 model, and the
  declared footprint drops to 0. **Verified:** registration failure (cuDNN absent) → CPU, bake
  succeeds. **Not exercised end to end:** the mid-run retry in `BiRefNetSegmenter::segment` — the
  incomplete-cuDNN reproduction failed at registration instead, so that branch is unit-uncovered.
- **Selection rules (from the adversarial review):** the pack's runtime is pinned **per process** — the
  first time anything resolves it (`ModelStore::gpu_runtime_path`), because ONNX Runtime's environment
  can't be re-pointed, so a pack installed mid-session changes nothing until restart (otherwise every
  later mask and removal load would fail with a path mismatch). Masks *and* removal verify the runtime
  set actually loaded (`MaskModels::artifacts`/`RemovalModels::artifacts`, keyed on `gpu_runtime`, not
  on the fp16 file), and the CUDA provider is requested only when the fp16 model is installed too (an
  interrupted pack download leaves the runtime without it; fp32 on CUDA would need ~13 GB, declared as
  such if a caller supplies its own GPU runtime). AI removal stays on the CPU provider inside the CUDA
  build until #322; with only the pack installed it is not asked to download the CPU runtime.
- **Known gaps:** the pack is offered to any machine with an NVIDIA driver (`nvcuda.dll`); there is no
  probe for GPU generation (CUDA 13 needs Turing or newer) or free VRAM — the panel says so, and the
  CPU fallback covers a card that can't run it, after a wasted download. `remove()` of a multi-member
  artifact can stop partway if a DLL is locked by a live session.
- **Measured on a live desktop** (Lightroom Classic, Parsec, Nicti itself and ~40 other GPU clients
  were running, ~5 % idle load), so absolute times carry some noise; the ranking is not close.

- The tensor contract is **verified against the real weights**: it loads, the names resolve from the
  graph, the output is 1024×1024 in 0…1, and a bright synthetic subject on a dark background gets alpha
  1.000 inside and 0.000 in a corner.
- **ADR-0048's "≤ 1 s per bake" hypothesis assumed CUDA and is not met on the CPU provider — by ~9×.**
  That ADR said a CPU-provider bake is "reported, not gated"; it is reported here. A bake is a background
  job with a *Selecting…* indicator, so it is usable, but ~9 s per Select Subject is poor. **#345 closes
  the gap on NVIDIA GPUs** with the optional GPU pack below (a CPU-viable model, #349, is still open for
  everyone else).
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
  through casts, so it stays out of the default download. **Measured on CUDA in #345 and adopted there**
  (see the table above): it ships in the optional NVIDIA GPU pack, used only when the CUDA provider is live.
- **Refining the AI alpha on the CPU**: needs a full-frame readback of the baked frame.

## Consequences

- **#49 ships**: masks and local adjustments in Develop. `spikes/siamese` is deleted (its compose,
  geometry, refine and WGSL superseded by `nicti-tapetum::mask`; its segmentation scaffolding by
  `nicti-siamese`); `nicti-groom`'s cross-crate `ort` test no longer uses it as a caller.
- **The optional NVIDIA GPU pack (#345)**: `nicti_stalk::models::gpu_pack_artifacts()` — ONNX Runtime 1.28.0
  CUDA 13 build (366 MB download), cuDNN 9.27 (436 MB, NVIDIA's own PyPI wheel; NVIDIA's full redist zip
  is 1.3 GB), cuBLAS 13.8 (422 MB, NVIDIA CUDA redistributables) and the fp16 BiRefNet (490 MB) — about
  **1.7 GB to download, 1.9 GB on disk**, installed into `<store>/ort-cuda/` (one directory: ONNX Runtime
  resolves the provider's dependencies from it) and `birefnet-fp16/`. Windows + an NVIDIA driver only;
  offered by a button in the Masks panel, never automatic (ADR-0218); takes effect after a restart
  because ONNX Runtime's environment is process-wide. Needs `Payload::ZipMembers` (several members of one
  archive, a directory shared between artifacts, installed all-or-nothing). `ensure_ort_environment`
  prepends the runtime's directory to `PATH`, because ONNX Runtime loads cuDNN/cuBLAS by bare name
  (found by measurement: with a clean `PATH` cuDNN was not found beside the DLL, and the session still
  reported the CUDA provider until the first run failed). **Licences:** ORT MIT; cuDNN/cuBLAS are
  redistributable under NVIDIA's terms (`docs/licensing.md`) and are an on-demand download, never bundled.
- **Follow-ups**, each filed: ~~a GPU execution provider for masks ([#345](https://github.com/jordanfelle/nicti/issues/345))~~ (done);
  a model picker ([#346](https://github.com/jordanfelle/nicti/issues/346)); a provenance-clean sky model
  ([#347](https://github.com/jordanfelle/nicti/issues/347)); verifying the BiRefNet conversion
  ([#348](https://github.com/jordanfelle/nicti/issues/348)); a CPU-viable subject model
  ([#349](https://github.com/jordanfelle/nicti/issues/349)); Select Object / people masks
  ([#350](https://github.com/jordanfelle/nicti/issues/350)); Moiré and Defringe
  ([#351](https://github.com/jordanfelle/nicti/issues/351)); global Clarity/Texture/Dehaze
  ([#352](https://github.com/jordanfelle/nicti/issues/352)); a disk tier for baked alphas
  ([#353](https://github.com/jordanfelle/nicti/issues/353)); the remaining Dx12 audit
  ([#355](https://github.com/jordanfelle/nicti/issues/355)); unloading an idle session
  ([#356](https://github.com/jordanfelle/nicti/issues/356)); brush pressure/flow/auto-mask
  ([#357](https://github.com/jordanfelle/nicti/issues/357)); the post-lens neutral image once lens
  correction lands ([#358](https://github.com/jordanfelle/nicti/issues/358)). Notes were left on #171,
  #62, #52 and #324.
- **Not undoable yet** — like heal, mask edits write straight into the in-memory document (#324).
- **Known limits**: the mask overlay shows the *unrefined* AI alpha (the real render refines it); a
  uniformly hazy scene with no sky defeats the airlight estimate; the sky mask is a heuristic; a mask's
  edge is at most 4096 px in Develop (export builds its own at full size, #354); the neutral image is built from
  the pre-lens frame and will need the post-lens one when lens correction is real (#358).
