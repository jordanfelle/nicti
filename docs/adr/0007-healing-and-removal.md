# ADR-0007: Healing and removal

- **Status:** Proposed — pending reference-machine measurement pass
- **Date:** 2026-09-24
- **Ticket:** [#50](https://github.com/jordanfelle/nicti/issues/50) Research: healing/removal

## Context

Nicti needs a spot-removal story before #51 (shipping AI removal) can start, and before #44
(Tapetum) can settle where a heal/remove stage sits in the render-stage order. Two genuinely
different techniques answer "get rid of this thing in the photo":

- **Classic clone/heal** — clone stamp (patch copy) and gradient-domain ("Poisson") spot healing,
  both decades-old and well-understood, with a cheap auto-source-pick as a baseline for "find me
  a good source patch automatically."
- **AI-driven removal** — a segmentation model (MobileSAM-style) turns a click/box into a mask,
  then an inpainting model (LaMa-style) fills the masked region using learned image priors, well
  beyond what a purely local gradient solve can do for a genuinely complex background.

Constraints already fixed by earlier ADRs/docs:

- **ADR-0002** already commits the edit-model shape this ticket's `HealStage`/`Spot` type has to
  fit into: a `StageEntry { schema_version, params }` entry per pipeline stage, canonical
  `serde_json` + `blake3` hashing for Tapetum's cache key, and "the recipe, not the pixels" for any
  AI mask (`{model_id, model_version, params, seed?}`).
- **ADR-0004 §3** already decided AI inference loads via the `ort` crate's `load-dynamic` feature
  (`ort::init_from(path)`), not linked at build/startup time — this ADR's AI-removal scaffolding
  reuses that exact pattern rather than deciding it fresh.
- **ADR-0005** already decided `wgpu`/WGSL as the render/compute API — this ADR's GPU Poisson
  solver is a `wgpu` compute shader, following `spikes/glint`'s device/dispatch pattern.
- **ADR-0003**/`docs/licensing.md` set the bundling-vs-on-demand-download criterion for ML models
  and already flagged LaMa's `big-lama` checkpoint over its Places2 training-data provenance —
  this ADR's own research (below, and `docs/research/groom-healing-removal.md`) is the follow-up
  that flag asked for.
- **Sandbox note**, matching ADR-0005/0006's own precedent: this research pass ran in a Linux/WSL
  sandbox with no GPU-backed Vulkan/Dx12 adapter and no real ONNX model weights (obtaining actual
  LaMa/MobileSAM checkpoints is out of scope for this spike — a large download, and in LaMa's case
  gated on the licensing question this ADR is itself researching). Every CPU number below is real,
  measured data from this sandbox; every GPU/CUDA number is marked **TBD — reference machine**,
  the same convention ADR-0005/0006 use for their own pending hardware runs.

## Decision rule (stated before measuring)

- **Quality**: judged visually, plus PSNR/LPIPS on synthetic holes (a known region digitally
  removed from a real image, then compared against the healed/inpainted result) once a real
  reference-hardware pass with real model weights is possible. This spike has neither a corpus of
  real photos with known-good ground truth nor real inpainting weights, so no quality number is
  claimed here — this rule is stated for #51 to apply, not satisfied by this ADR.
- **Speed — interactive spot-heal**: **< 16ms/update on GPU** (a hypothesis carried over from
  ADR-0005's 60fps budget, *to be validated* on the reference machine — this sandbox has no GPU
  numbers for the Poisson solver at all, only CPU ones below).
- **Speed — AI removal**: a **bake-time** operation (per ADR-0002's stage model, baked once and
  cached, not recomputed per frame), with a stated latency budget of **< 2s/removal on a CUDA
  execution provider** — also *to be validated* on the reference machine; this sandbox can't run
  either MobileSAM or LaMa at all (no GPU, no real weights).
- **License**: a hard gate on **bundling** a model's weights inside Nicti's own installer, not on
  whether a model can be evaluated/researched. An unbundleable-today model (LaMa, pending Places2
  clarification) can still ship as an **on-demand download** the user fetches separately, which is
  a materially different question from "can Nicti redistribute this file in its own installer."

## Decision

**Ship both techniques, as two `SpotKind` variants of one `HealStage` (see "Spike" below), not as
competing alternatives.** Classic clone/heal is proven end-to-end in this spike (CPU reference +
`wgpu` compute shader, checked against each other within float tolerance) and is cheap enough to
run interactively with no AI dependency at all — it's the right tool for "remove a small
sensor-dust spot" or "clone out a stray hair" where a local gradient solve is genuinely sufficient.
AI-driven removal is the right tool for "remove this whole object from a complex background," but
needs real weights this sandbox doesn't have, so **this ADR proves the scaffolding
(`ort`/`load-dynamic` loading, error handling, crop/resize/feather compositing) rather than the
model itself** — the same "prove the mechanism, not the specific instance" shape ADR-0004 already
used for its dylib-ABI handshake before any real third-party plugin existed.

### Classic clone/heal

- **Clone stamp**: a radial-feathered alpha blend of a source patch onto a destination, proven in
  `spikes/groom/src/cpu_reference.rs::clone_stamp`.
- **Spot heal**: gradient-domain (Poisson, Perez et al. 2003 "seamless cloning") blending, solved
  via Jacobi iteration on `f32` linear RGBA (simulating RGBA16F storage, per this ticket's own
  instruction — half-float *storage* itself is Tapetum's concern, not this ADR's).
  `poisson_jacobi_step`'s update rule is mirrored exactly in a `wgpu` compute shader
  (`shaders/poisson_jacobi.wgsl`), dispatched once per Jacobi iteration, ping-ponging between two
  storage buffers — checked against the CPU reference within `1e-3` tolerance in
  `tests/correctness.rs`, which skips cleanly (matching `spikes/glint`'s own pattern) when no wgpu
  adapter is available.
- **Auto source pick**: a cheap SSD-over-a-ring baseline — compare a candidate source location's
  surrounding annulus against the destination's own annulus at several offsets around a search
  ring, skip any candidate whose own annulus would overlap the region being healed, return the
  lowest-SSD offset. Proven in `cpu_reference::auto_source_pick`. This is a deliberately simple
  baseline, not PatchMatch or a learned prior — good enough to seed a default suggestion, not a
  claim of state-of-the-art auto-source quality.

### AI removal scaffolding

- `MobileSamSelector`/`LamaInpainter` (`spikes/groom/src/ai.rs`) wrap `ort::init_from(dylib_path)`
  + `Session::builder()?.commit_from_file(model_path)?`, per ADR-0004 §3's already-decided
  pattern. Both return a clean `Err(GroomAiError::ModelNotFound)` — never panic — when the `.onnx`
  file doesn't exist, proven by tests that run in CI with no model file present. When a model file
  *is* present, the code attempts a real session load and a real (simplified, single-input/
  single-output) inference call — proven only as far as this sandbox can prove it, since no real
  MobileSAM/LaMa weights exist here; the corresponding tests are `#[ignore]`d with a doc comment
  naming the environment variables a reference-machine run would need.
- Crop/resize/feather-blend compositing (`spikes/groom/src/compositing.rs`) — mask bounding box,
  context-margin expansion, bilinear resize, and feathered-mask composite-back math — is pure and
  model-independent, tested against synthetic masks/crops with no model involved at all.

### Edit-model representation

`HealStage { spots: Vec<Spot> }` is the `params` payload a `"heal"` `StageEntry` (ADR-0002) would
carry. Each `Spot` has `kind: SpotKind` (`Clone`/`Heal`/`Remove`), a **destination circle**
(`center` + `radius` — chosen over a freehand brush path for this ticket's scope; see
`spikes/groom/src/spot.rs`'s doc comment for the reasoning and the compatible extension path if
#51 needs strokes), an optional `source_offset` (Clone/Heal), `feather`, `opacity`, and an optional
`mask_recipe` (`{model_id, model_version, params, seed?}`, Remove only) — "the recipe, not the
pixels" per ADR-0002. `cache_key()` follows `spikes/pawprint`'s canonical-JSON + `blake3` chaining
pattern exactly (this stage's hash chained onto a caller-supplied upstream hash).

**Stage-order proposal for #44** (proposed input, not a final decision): heal/remove runs **after
lens correction, before global tone**, in **linear space**. Reasoning: lens correction can shift
pixel geometry (distortion, chromatic-aberration correction), so a heal/clone source-offset defined
before that correction would point at the wrong post-correction pixels; running heal/remove before
any tone-curve/exposure work means the Poisson solve operates on physically-meaningful linear
light, not an already-compressed tonal range, which matters for the gradient-domain math's
correctness (Poisson blending assumes the guidance field's gradients are meaningful in the space
they're solved in). This is #44's call to finalize, not this ADR's.

## Measured results

**CPU-only** (this sandbox has no GPU-backed Vulkan/Dx12 adapter — WSL without a real Vulkan ICD).
Measured via `cargo test -p groom --test throughput --release -- --ignored --nocapture`, one
discarded warm-up run + 20 measured runs per operation, 512×512 synthetic checkerboard image,
`radius = 20`, `feather = 4`, 50 Jacobi iterations for the heal case:

| Operation | Mean time |
|---|---|
| `clone_stamp` | **0.1209 ms/op** |
| `spot_heal` (50 Jacobi iterations) | **0.2458 ms/op** |
| `auto_source_pick` (24 candidates) | **0.0542 ms/op** |
| `poisson_jacobi` GPU compute | **TBD — reference machine** |
| AI removal (MobileSAM + LaMa, CUDA EP) end-to-end latency | **TBD — reference machine** (no GPU, no real weights in this sandbox) |
| Quality (PSNR/LPIPS on synthetic holes) | **TBD — reference machine + real weights** |

`HealStage` serialized size (canonical `serde_json`, measured via
`cargo test -p groom spot::tests::spot_list_sizes_at_1_10_and_50 -- --nocapture`), a mix of
`Clone`/`Remove` spots:

| Spot count | Serialized bytes |
|---|---|
| 1 | **120 bytes** |
| 10 | **1,511 bytes** |
| 50 | **7,531 bytes** |

Informally against ADR-0002's own reference point (a 5-stage `EditDocument` at 563 bytes total):
a single heal spot (120 bytes) is a comparable order of magnitude to one stage entry's share of
that 563-byte document, and even 50 spots (7.5KB) stays small in absolute terms — consistent with
ADR-0002's "single-digit-GB across the whole 2M-asset catalog" sizing conclusion, though a
realistic heal edit is far more likely to carry a handful of spots than fifty. These are
throwaway-spike numbers for order-of-magnitude planning, exactly the caveat ADR-0002 states for its
own sizing numbers, not a commitment to `Spot`'s exact byte layout.

## Options considered

| Option | Interactive without AI? | Handles complex backgrounds? | Verdict |
|---|---|---|---|
| Classic clone/heal only | Yes — proven cheap in this spike (< 0.3ms/op CPU) | No — a local gradient solve can't invent plausible content for a large, structurally complex hole | Necessary but not sufficient on its own |
| AI removal only | No — inference latency budget (< 2s bake-time target) rules out a live-drag interaction | Yes, in principle (LaMa/MobileSAM-class models are designed for exactly this) | Necessary but not sufficient on its own |
| **Both, as `SpotKind` variants of one `HealStage` (this ADR's decision)** | Yes for Clone/Heal | Yes for Remove | Covers the full "small blemish" through "whole object" range without forcing a single technique to do both jobs |

| Inpainting model | Code license | Weights license | Training-data provenance | Verdict |
|---|---|---|---|---|
| **LaMa** (`big-lama`) | Apache-2.0 | Not separately stated, presumed Apache-2.0 | `big-lama` trained on **Places2**, whose own terms restrict use to non-commercial research and forbid redistributing the source images[^p1] — whether that restriction shadows a model merely *trained on* the data is a live, unsettled legal question this ADR does not resolve, and MIT's own Places2 page returned a connection error when re-checked live during this pass (see `docs/research/groom-healing-removal.md` for exactly what was and wasn't reachable) | **Still flagged** — no new clarity gained this pass beyond confirming Places2's own stated non-commercial/no-redistribution terms via a working mirror page[^p1] |
| **MI-GAN** (alternative researched this pass) | MIT (code)[^p2] | A separate `LICENSE-WEIGHTS` file is itself written as MIT. A now-closed GitHub issue asked whether that grant is legitimate given the training pipeline's Co-Mod-GAN teacher model, licensed NVIDIA Source Code License-NC (§3.2 requires the non-commercial term to carry over to derivatives); the maintainer confirmed the MIT weights grant but explicitly declined the deeper legitimacy question, recommending legal counsel[^p3] | Places2 **and** FFHQ[^p3] — same Places2 exposure as LaMa, no improvement there | **Not cleaner than LaMa, arguably murkier** — same Places2 exposure, plus a named legal question that was explicitly punted to legal counsel rather than resolved |

## Prior art

Desk research only (no code run) for this section, consistent with ADR-0006's own precedent for
research this sandbox can't execute directly:

- **Adobe Photoshop's Content-Aware Fill / GIMP's Resynthesizer** are the best-known examples of
  the "search the rest of the image for plausible content" family this ADR's auto-source-pick is a
  deliberately cheap ancestor of — full patch-based texture synthesis (PatchMatch-style) is a real,
  higher-effort successor to the SSD-over-a-ring baseline this spike implements, worth revisiting
  for #51 if the cheap baseline's suggestions prove too weak in practice.
- **darktable's retouch module** combines clone/heal/blur tools in one interface, using a
  drawn-mask + source-offset model broadly similar to this ADR's `Spot` shape (destination
  geometry + optional source offset), though darktable's masks are freehand rather than
  circle-only — the same brush-path extension this ADR's `spot.rs` doc comment already names as a
  compatible future addition.
- **LaMa** (Suvorov et al., WACV 2022) is the specific model ADR-0003/`docs/licensing.md` already
  flagged; this ADR's own contribution is attempting (and only partially succeeding — see the
  research doc) to resolve that flag, plus researching one alternative with different provenance.

## Consequences

- **Unblocks #51** (shipping AI removal): the `ort`/`load-dynamic` loading/error-handling shape is
  proven; #51's real work is obtaining/validating actual MobileSAM+LaMa (or an alternative) ONNX
  weights and wiring their real multi-input contracts, which this spike deliberately simplified to
  a single input/output tensor (see `ai.rs`'s module doc comment) rather than guessing at a
  contract with no real model file to validate against.
- **Feeds #44** (Tapetum): the proposed heal/remove stage-order placement (after lens correction,
  before global tone, in linear space) above is an explicit, flagged proposal for #44 to adopt or
  revise — not a binding decision this ADR is scoped to make.
- **Licensing decision deferred, not resolved**: `docs/licensing.md`'s LaMa flag stands, updated
  with what this pass actually found (Places2's terms, confirmed via a working mirror, plus a
  researched-but-worse alternative) rather than newly cleared. #51 still needs to either get an
  explicit sign-off on LaMa as an on-demand download (not bundled), find/train a
  non-Places2-provenance checkpoint, or accept the residual risk explicitly before shipping.
- **`docs/licensing.md` updated in this PR** per ADR-0003's same-PR rule: the LaMa row's provenance
  note refreshed, a new MI-GAN row added, and MobileSAM's row tagged to this ticket (#50) alongside
  its existing #48 tag.

## Spike: `spikes/groom`

Feline name: groom, as in a cat grooming debris out of its coat — the removal half of
"healing/removal." Not production code, same "don't build on top of it" status as
`spikes/glint`/`spikes/pawprint`/`spikes/sniff` (see `CLAUDE.md`'s package-map note); expect it
deleted once a future `nicti-render`-adjacent crate (or #51 directly) promotes the parts worth
keeping.

- **`src/cpu_reference.rs`**: `Image`, `clone_stamp`, `poisson_jacobi_step`/`poisson_jacobi_cpu`,
  `spot_heal`, `auto_source_pick` — the plain-`f32` reference every other implementation is
  checked against, plus a determinism test (`spot_heal_and_clone_stamp_are_deterministic`) proving
  bit-identical output across repeated runs, supporting ADR-0002's "derived, cached, never
  persisted" design.
- **`src/gpu.rs`** + **`shaders/poisson_jacobi.wgsl`**: the `wgpu` compute-shader twin of the CPU
  Jacobi solver, following `spikes/glint`'s adapter-enumeration/dispatch pattern (including its 2D
  dispatch-grid workaround for wgpu's 65535-per-dimension workgroup limit), simplified relative to
  glint by dropping the GPU-timestamp harness entirely — this ticket's perf work is CPU-only in
  this sandbox, so no GPU timing code was written only to sit unused.
- **`src/ai.rs`**: `MobileSamSelector`/`LamaInpainter`, the `ort`/`load-dynamic` scaffolding
  described above.
- **`src/compositing.rs`**: `BBox`, `mask_bounding_box`, `expand_bbox_with_margin`, `crop_image`,
  `resize_bilinear`, `feather_mask`, `composite_inpainted_crop` — pure crop/resize/feather math for
  LaMa-style compositing, tested independent of any real model.
- **`src/spot.rs`**: `Spot`/`SpotKind`/`MaskRecipe`/`HealStage`, `cache_key()` — the edit-model
  representation described above, plus the size and cache-key-chaining tests backing this ADR's
  Measured results section.
- **`src/bin/groom.rs`**: a minimal demo binary printing a small JSON summary (clone/heal/
  auto-source-pick timings on one synthetic run), in the spirit of `sniff`'s JSON-bench-output
  convention at a much smaller scale — the real perf numbers this ADR quotes come from
  `tests/throughput.rs`, not this binary.

## Footnotes

[^p1]: Places2 dataset terms ("you will use the data only for non-commercial research and
    educational purposes and will NOT distribute the images") —
    http://places2.csail.mit.edu/download-private.html, reachable via web search during this pass
    after MIT's primary `places2.csail.mit.edu` domain returned a connection error on a direct
    fetch attempt — **best-available-secondary** (a working mirror/cache of the stated terms, not
    a fresh direct fetch of the primary page in this exact session); see
    `docs/research/groom-healing-removal.md` for the full account of what was and wasn't reachable.
[^p2]: MI-GAN code license (MIT) — https://github.com/Picsart-AI-Research/MI-GAN — verified
    2026-09-24 via the repository's own listed license.
[^p3]: MI-GAN's own `LICENSE-WEIGHTS` file, fetched directly — written as an MIT-style permissive
    grant, not an explicit non-commercial license —
    https://raw.githubusercontent.com/Picsart-AI-Research/MI-GAN/main/LICENSE-WEIGHTS — verified
    2026-09-24. https://github.com/Picsart-AI-Research/MI-GAN/issues/25 is **closed** (2026-09-14):
    the maintainer confirmed the MIT grant applies to the weights, but explicitly declined the
    deeper question of whether that grant is legitimate given the Co-Mod-GAN teacher model's
    NVIDIA Source Code License-NC (§3.2's derivative-works non-commercial-carryover clause),
    recommending the asker consult a lawyer — so the grant question is answered, only the
    legitimacy question stays open, and by an explicit punt rather than silence. Places2+FFHQ
    training-data statement — the repository's own README — verified 2026-09-24,
    **best-available-secondary** for the exact NVIDIA Source Code License-NC clause text (relayed
    via the GitHub issue thread's own summary, not independently re-fetched verbatim from NVIDIA's
    license text in this pass).
