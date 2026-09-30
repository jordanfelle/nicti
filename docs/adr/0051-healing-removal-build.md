# ADR-0051: Healing and removal — the build

- **Status:** Accepted
- **Date:** 2026-09-29
- **Ticket:** [#51](https://github.com/jordanfelle/nicti/issues/51) Build: healing/removal tools
- **Builds on:** [ADR-0050](0050-healing-and-removal.md) (the research this promotes),
  [ADR-0218](0218-local-only-ai.md) (how model weights may reach the user),
  [ADR-0044](0044-stage-cached-render-graph.md) (where the heal stage sits)

## Context

ADR-0050 proved the techniques in a throwaway spike (`spikes/groom`) but stopped short of anything
shippable: its GPU Poisson solver ran on storage buffers against a private device, and its AI
wrappers collapsed MobileSAM and LaMa to a fake single-tensor contract because no weights were
available. #51 turns it into the real feature — spot heal, clone stamp and AI object removal in the
Develop view — and with it Nicti's first real AI model, which also means building the model-install
path ADR-0218 only specified.

## Decision

### 1. Classic clone and heal run on the GPU inside the Tapetum heal stage

`nicti-tapetum::heal` replaces the passthrough `nicti.heal` slot. It stays a `Baked` stage after lens
correction and before the live suffix (ADR-0044), so it works in linear camera RGB. The params are
`coat::HealParams { spots }`, promoted from the spike's `HealStage` (`Spot`, `SpotKind`,
`MaskRecipe`), in source-pixel coordinates like `CropParams`.

Each spot is a sequence of compute passes on `Rgba16Float` textures (`shaders/heal.wgsl`): extract
the destination and source patches, N Jacobi sweeps for `Heal` (none for `Clone`), then a feathered
composite copied back into the frame. Reads and writes to the frame happen in different passes, so
no `read_write` storage-texture feature is needed. Semantics, shared by the shader and the CPU
reference used in the parity tests: centers round to whole pixels, out-of-frame reads clamp to the
edge (the spike zero-filled, which would have painted black into a heal near a border), only
in-frame patch pixels are written back, and spots apply in list order.

`impl_version` is a real value here, so a change to the algorithm invalidates cached bakes.

### 2. AI removal: a click becomes a `RemovalPatch`

`nicti-groom` (promoted from the spike, keeping its feline name) runs the pipeline in
`remove.rs`:

1. MobileSAM turns the click/box into an object mask, limited to the spot's circle (so the spot's
   size control is also "how much to remove").
2. A square crop with context (75% of the object's size on each side, between 160 px and 2047 px)
   is cut around it. Objects over 1024 px across are refused rather than stretched thin.
3. The mask is grown a few pixels (`geom::dilate`) and LaMa inpaints the 512×512 resize.
4. The result is resized back, mapped into the heal stage's color space, and weighted by a feathered
   copy of the mask (`geom::feathered_weight`, an exact Euclidean distance transform — the spike's
   O(r²)-per-pixel feather was explicitly "not the production algorithm").

The output is a `nicti_tapetum::heal::RemovalPatch`: a square, odd-sided patch of fill pixels with
the fill weight in alpha. The GPU blends it with one extra pass (`composite_patch`). The document
stores only the recipe — the MobileSAM prompt in `MaskRecipe.params`, with `model_id`/
`model_version` pinned — never the pixels (ADR-0021).

**Real tensor contracts**, read from the ONNX files rather than assumed (the spike's contracts were
guesses): MobileSAM's encoder takes a raw 0–255 `[H, W, 3]` image resized to a 1024 longest side and
returns `[1, 256, 64, 64]` embeddings; its decoder takes `point_coords`/`point_labels` (a click is
padded with a `(0,0)` label `-1` point; a box is two corners labelled 2 and 3), `mask_input`,
`has_mask_input` and `orig_im_size`. LaMa takes `image [B,3,512,512]` in 0..1 and `mask
[B,1,512,512]` and returns 0..255. The wrappers cache the SAM embedding per photo, so a second click
re-runs only the ~40 ms decoder.

### 3. Getting a patch into the cache key

The render graph rebakes a stage when its `own_hash` changes, and `apply_document` computes that
from the document's params. A patch arriving is not a params change, so `heal::stamp_removal_state`
adds a `"removals": {spot key → patch content hash}` field to the heal entry of the document the
render sees (not the stored one). `HealParams` ignores the unknown field when parsing; the hash
covers it. The patch therefore invalidates through the normal path — no second hash, no side
channel that `apply_document` would overwrite on every render. A Remove spot without a ready patch
passes through unchanged.

### 4. Color space

The heal stage sees linear camera RGB — dark and green-heavy — but the models expect display-referred
sRGB photos. `nicti-groom::space::SpaceMap` is a per-photo, exactly invertible stand-in for "what the
photo looks like": as-shot white-balance gains, one exposure scale from the 99th-percentile
highlight, then the sRGB curve. It is deliberately not the real render pipeline (a DCP profile, tone
curves): the models only need a natural image, and the round trip has to be exact so unmasked pixels
come back unchanged. The image is read lazily through `PixelSource`/`FramePixels`; a full-resolution
frame as `[f32; 4]` would be 730 MB.

### 5. Model delivery (`nicti-stalk::models`, ADR-0218)

Nothing is fetched unless the user clicks Download. Each artifact in the compiled-in manifest has a
URL pinned to an immutable revision, an exact size and a SHA-256; a download streams to a temp file
(cut off at the declared size), is verified, and is renamed into place atomically, so an interrupted
or tampered download can never look installed. It runs as a Pounce job (`InstallModelsJob`), one
artifact per chunk, cancellable, with live byte progress for the UI.

| Artifact | Source | Size | License |
|---|---|---|---|
| MobileSAM encoder + decoder | `Acly/MobileSAM` (ONNX export) | 28 MB + 17 MB | Apache-2.0 |
| LaMa `big-lama` fp32 | `Carve/LaMa-ONNX` | 208 MB | Apache-2.0 code; **weights trained on Places2** |
| ONNX Runtime 1.28.0 (CPU) | `microsoft/onnxruntime` GitHub release | 79 MB zip → 16 MB DLL | MIT |

**ONNX Runtime is the CPU build, not DirectML.** The plan was DirectML so removal would run on any
DX12 GPU, but DirectML ships through NuGet (with a separate `DirectML.dll` package) rather than as
a single pinnable release asset, and it was not evaluated. The CPU build is one official,
hash-pinnable zip. The cost is speed (see Measured results); the
`ExecutionProviderKind`-style seam is left for a GPU build to slot in. The zip carries debug
symbols, hence 79 MB for a 16 MB DLL. The store exposes `NICTI_ORT_DYLIB` and `NICTI_MODELS_DIR`
overrides for development and non-Windows machines.

### 6. Licensing sign-off

ADR-0050 left LaMa's Places2 training-data question unresolved and required one of: an explicit
sign-off on an on-demand download, a non-Places2 checkpoint, or accepting the risk. **The project
owner signed off on 2026-09-29 for on-demand download only, never bundled.** That is an accepted
residual risk, not a legal conclusion — nothing here settles whether Places2's non-commercial terms
reach a model trained on it. The download prompt shows the user the license note and sources before
they agree, and `docs/licensing.md` records the decision. If Nicti ever needs to bundle inpainting
weights, or ship commercially, the question reopens.

### 7. UI

A Crop | Heal tool switch in the Develop panel. The heal tool shows the *uncropped* image
(`DevelopView::uncropped_preview`), so a click maps to source pixels by the same plain stretch the
crop tool uses. A click places a spot of the chosen kind (or selects the spot under it); the selected
spot's destination and source handles drag; `[`/`]` resize and Delete removes. A Clone/Heal spot's
source is chosen by `groom::source::auto_source_pick` (the spike's SSD-over-a-ring baseline,
extended to several rings and to read lazily), falling back to a fixed offset clamped inside the
frame. A removal runs as a `RemoveJob` on Pounce's GPU lane — the first real job that lane has run —
and a failed one (no object at the click, region too large) removes its placeholder spot and says
why instead of leaving a spot pending forever.

## Measured results

**Classic heal, end to end** (submit + GPU fence, frame copy included), release build cross-compiled
for Windows and run on the real reference machine — NVIDIA GeForce RTX 5080, Vulkan — via
`heal::tests::throughput`. Median of 5 after 1 warm-up:

| | 3840×2560 | 8280×5520 |
|---|---|---|
| frame copy only (0 spots) | 0.19 ms | 0.53 ms |
| 1 heal, r=24 | 0.87 ms | 1.44 ms |
| 10 heal, r=24 | 7.7 ms | 13.7 ms |
| 1 heal, r=100 | 2.5 ms | 3.0 ms |
| 1 heal, r=300 | 6.6 ms | 7.4 ms |
| 1 clone, r=100 | 0.8 ms | 0.7 ms |
| 10 clone, r=24 | 2.4 ms | 1.1 ms |
| 1 AI patch, 513×513 | 8.7 ms | 4.4 ms |

A single spot is well inside ADR-0050's 16 ms interactive budget at any size measured. Ten heal
spots at r=24 approach it: a heal spot's cost is mostly its ~50 Jacobi passes (~0.8–1.4 ms each
regardless of radius at this size), whereas a clone spot costs a fraction of that. A stage this
cheap is fine to rebake on every drag, but a document with dozens of heal spots would not be. The AI-patch
row is dominated by the CPU converting and uploading the patch as f16 on every rebake, not by the
GPU; storing it pre-converted is an easy win (follow-up).

**AI removal**, real MobileSAM/LaMa weights, ONNX Runtime 1.28.0 **CPU** execution provider, Linux,
release build, session load excluded (`crates/nicti-groom/tests/real_models.rs`):

| | |
|---|---|
| MobileSAM, encoder + decoder (first click on a photo) | 1.18 s |
| MobileSAM, decoder only (embedding cached) | 40 ms |
| LaMa, one 512×512 inpaint | 3.3 s |
| End to end, 1600×1200 photo, first removal | 4.3 s |
| End to end, embedding cached | 3.0 s |

**This misses ADR-0050's <2 s/removal target, which assumed a CUDA execution provider** — the
target was not evaluated on a GPU EP, and the CPU build is not expected to meet it. Removal is
a background job with a pending indicator, not a live-drag interaction, so ~3–4 s is usable, but it
is slower than intended.

**Quality was checked on synthetic scenes only**: MobileSAM segmented a flat 80×80 square with IoU
0.996; LaMa returned the unmasked region unchanged (error 0.0000) and filled a hole in a smooth
gradient to 0.0037 mean absolute error; end to end, the fill of a removed square matched the true
background to 0.0053 versus 0.38 for the object it replaced. No real photograph — and in particular
no fursuit photography, the actual use case — was evaluated, and no PSNR/LPIPS was computed. A
gradient with a flat object is the easy case for inpainting; these numbers prove the plumbing and
the tensor contracts, not that removals look good on real photos.

## Consequences

- **#51 ships**: spot heal, clone stamp and AI removal in Develop. `spikes/groom` is deleted, its
  code promoted to `nicti-groom`/`nicti-tapetum::heal`; its cross-crate `ort` environment regression
  test (#179/#229) moved to `nicti-groom/tests/ort_cross_module.rs` so it isn't lost with the spike.
- **First real GPU-lane Pounce job** (`RemoveJob`) and first `JobKind::Download`. With the CPU
  execution provider it declares no VRAM; a GPU EP build must declare the sessions' footprint.
- **#62 (LRC catalog import)** still owns translating LRC's `RetouchInfo`/`RemoveAreas`/People
  Removal entries into `HealParams`; nothing here parses those.
- **Deferred, tracked as follow-up issues**: a GPU execution provider for removal; evaluating
  removal quality on real photos (needs the reference machine and real images); pre-converting the
  patch's f16 upload. Also deferred without an issue because they are Develop-wide rather than
  heal-specific: undo/redo (Develop doesn't route edits through `History` yet) and persistence of
  the edit document, on which removal patches (recomputed, not stored) will depend.
- **Freehand brush geometry** for spots remains the additive `Geometry` enum ADR-0050 sketched;
  circles only for now. PatchMatch-style auto-source is likewise still a possible upgrade over the
  SSD baseline.
- **Known limits**: an object larger than 1024 px is refused; a very large crop is inpainted at
  512×512 and upsampled, so the fill is softer than the surrounding photo detail; the mask comes
  from a single click/box with no refine step.
