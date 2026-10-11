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
the destination and source patches; for `Heal`, a starting guess and N Jacobi sweeps (none for
`Clone`); then a feathered composite copied back into the frame.

**The Jacobi solve must not start from the destination.** The adversarial review of this change
found that the first version did, and that it left a blemish essentially in place whenever the
blemish filled more than about half the spot — i.e. Heal was close to a no-op for real use. Jacobi
needs on the order of `side²` sweeps to smooth a blemish away, and only 50–400 are affordable. The
fix (`boundary_mean` + `init_heal`) starts the interior at *source + the mean (destination − source)
offset on the ring just outside the spot*, so the blemish never enters the solve and Jacobi only
refines the smooth boundary variation. The lesson generalises: the GPU-vs-CPU parity tests could not
see this, because the reference shared the algorithm and so agreed with the shader while both were
wrong. `heal_actually_removes_the_blemish` now asserts the *outcome* (a known blemish on a flat
field ends up at the flat value, on both GPU and reference) and is mutation-checked against reverting
the initialisation. Reads and writes to the frame happen in different passes, so
no `read_write` storage-texture feature is needed. Semantics, shared by the shader and the CPU
reference used in the parity tests: centers round to whole pixels, out-of-frame reads clamp to the
edge (the spike zero-filled, which would have painted black into a heal near a border), only
in-frame patch pixels are written back, and spots apply in list order.

**Dx12 correctness — found only by running on real hardware.** The whole heal suite passed on
Linux's software rasteriser and on Vulkan, then failed on the RTX 5080 under Dx12 (a Jacobi chain
came back as garbage from the third sweep on; Clone was flaky in the same way once the scratch
textures were reused). Reproduced in isolation: 1–2 chained sweeps were right, 3+ were wrong. Two
changes made it correct on Vulkan, Dx12 and lavapipe:

1. The shader reads its inputs through ordinary sampled textures (`texture_2d` + `textureLoad`) and
   only *writes* storage textures. Declaring the inputs `texture_storage_2d<.., read>` (as the
   crate's other shaders do) gave wgpu's Dx12 backend a resource state that disagreed with the
   shader's binding.
2. Scratch textures are reset by a copy from a never-written zero texture at the start of each spot,
   and each Jacobi sweep is copied back rather than swapping the two textures' roles. A copy is a
   transfer use, so it forces an explicit state transition (`clear_texture` would need a device
   feature we don't require).

The *mechanism* is inferred from the symptoms and the experiments (bind-group reuse/freshness made
no difference; copies did), not confirmed in wgpu's source — but the behaviour is pinned by tests.
`each_heal_pass_matches_the_reference_in_isolation` compares each pass on its own, and the whole
`heal::` suite was run on the RTX 5080 under both backends. **CI's Windows job runs on `windows-latest`
(WARP/Dx12), so this class of bug fails the required check rather than reaching users** — and the
other crates' shaders that read `texture_storage_2d<.., read>` (`detail_blur`, `present_sample`, …)
were audited in #355: `detail_blur` (#49) and `present_sample` are now `texture_2d` + `textureLoad`, and no
read-mode storage texture remains.

`impl_version` is a real value here, so a change to the algorithm invalidates cached bakes. A render
applies at most `MAX_SPOTS` (256) spots and `MAX_JACOBI_PASSES` (12 000) Jacobi passes in total —
a heal spot that would exceed the budget is skipped — and centres are clamped to ±10⁹ px, because
a hand-edited or imported document must not be able to queue millions of GPU passes or overflow
integer arithmetic.

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
   O(r²)-per-pixel feather was explicitly "not the production algorithm"). The feather is capped at
   6 px: outside the hole the fill is the model's copy of the *original*, round-tripped through a
   512 px resize and the display mapping (slightly blurred, highlights clipped), so a wide ring
   would visibly soften real detail around every removal. Fill values are clamped so extreme
   white-balance gains cannot overflow f16 to infinity.

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

`status()` only compares sizes (cheap enough to call every frame), but the ONNX Runtime library is
native code loaded into the process and the models are parsed by it, so the removal backend
re-hashes every pinned file **before the first load**, on the job's worker thread (hashing ~250 MB
must not stall the UI), and again on every retry until the engine is loaded — a failed check can't
be bypassed by trying twice. A same-size corrupt file would otherwise look installed forever, so a
failed check surfaces a **Repair models** button (`InstallModelsJob::new_repair`), which hashes what
is installed, deletes the files that fail, and downloads only those.

| Artifact | Source | Size | License |
|---|---|---|---|
| MobileSAM encoder + decoder | `Acly/MobileSAM` (ONNX export) | 28 MB + 17 MB | Apache-2.0 |
| LaMa `big-lama` fp32 | `Carve/LaMa-ONNX` | 208 MB | Apache-2.0 code; **weights trained on Places2** |
| ONNX Runtime 1.28.0 (CPU) | `microsoft/onnxruntime` GitHub release | 79 MB zip → 16 MB DLL | MIT |

**ONNX Runtime is the CPU build, not DirectML.** The plan was DirectML so removal would run on any
DX12 GPU, but DirectML ships through NuGet (with a separate `DirectML.dll` package) rather than as
a single pinnable release asset, and it was not evaluated. The CPU build is one official,
hash-pinnable zip. The cost is speed (see Measured results); the
`ExecutionProviderKind`-style seam is left for a GPU build to slot in. **(#345 update: the GPU build that slotted in is CUDA, not DirectML -- DirectML measured no faster than the CPU provider for BiRefNet, and its NuGet package stops at ORT 1.24.4. It is the optional NVIDIA GPU pack; when installed, its ONNX Runtime also serves removal, which stays on the CPU provider until #322 moves it. See ADR-0049's measured results.)** The zip carries debug
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
`heal::tests::throughput` (Vulkan and Dx12 both measured; they land in the same ranges). Median
of 5 after 1 warm-up, **range over several runs**: this is a shared
desktop GPU and run-to-run variance is large (one run measured the unchanged AI-patch row at 25 ms
against 4.7 ms in the others), so treat these as ranges, not points:

| | 3840×2560 | 8280×5520 |
|---|---|---|
| frame copy only (0 spots) | 0.2–0.3 ms | 0.5–0.7 ms |
| 1 heal, r=24 | 1.1–2.2 ms | 1.2–2.1 ms (one 5.6 ms outlier) |
| 10 heal, r=24 | 8.9–17 ms | 7.4–18 ms |
| 1 heal, r=100 | 3.1–7.9 ms | 3.1–6.7 ms |
| 1 heal, r=300 | 7.8–17 ms | 7.7–17 ms (28 ms in one noisy run) |
| 1 clone, r=100 | 0.4–0.8 ms | 0.5–1.7 ms |
| 10 clone, r=24 | 0.7–3.0 ms | 1.0–3.4 ms |
| 1 AI patch, 513×513 | 3.5–10 ms | 3.9–25 ms |

A single heal spot up to r≈100 is well inside ADR-0050's 16 ms interactive budget. **A very large
spot (r=300) and ten heal spots each reach or exceed it** on a busy GPU, so dragging one of those
would stutter; a heal spot's cost is mostly its Jacobi passes (a clone spot costs a fraction).
These figures are after the review fix that added two passes per heal spot (r=24 was ~1 ms
before). The AI-patch row was dominated by the CPU converting the patch to f16 on every rebake,
not by the GPU; `RemovalPatch` now stores its texels pre-converted (#325), so a rebake only uploads
them. That row predates the change and is to be re-measured with `heal::tests::throughput` on a
real GPU (not available where #325 was implemented).

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
- **Deferred, tracked as follow-up issues**: a GPU execution provider for removal
  ([#322](https://github.com/jordanfelle/nicti/issues/322)); evaluating removal quality on real
  photos ([#323](https://github.com/jordanfelle/nicti/issues/323)); undo/redo and persistence of the
  edit document, which Develop-wide work removal patches (recomputed, not stored) will depend on
  ([#324](https://github.com/jordanfelle/nicti/issues/324)); pre-converting the patch's f16 upload
  ([#325](https://github.com/jordanfelle/nicti/issues/325), done: `RemovalPatch` stores f16 texels).
- **Freehand brush geometry** for spots remains the additive `Geometry` enum ADR-0050 sketched;
  circles only for now. PatchMatch-style auto-source is likewise still a possible upgrade over the
  SSD baseline.
- **Known limits**: classic heal quality was verified on synthetic blemishes (a flat field with a
  known dark disc), not real textured photos; a very large spot pays for hundreds of Jacobi passes; an object larger than 1024 px is refused; a very large crop is inpainted at
  512×512 and upsampled, so the fill is softer than the surrounding photo detail; the mask comes
  from a single click/box with no refine step.
