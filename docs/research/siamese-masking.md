# Siamese: masking research write-up (#48, ADR-0024)

Feline name: Siamese/colorpoint cats carry a dark facial "mask" pattern — same naming logic as
`groom` (grooming = removal) or `sniff` (fast-preview scent-tracking angle).

This is the detailed research trail behind `docs/adr/0024-masking.md`'s Decision — read that ADR
first for the actual conclusions; this doc is what was tried and found along the way.

## Model survey

### BiRefNet (subject/background)

- License: MIT (code) + MIT (weights), per `docs/licensing.md`'s existing row (already tagged
  `#48` before this pass — confirmed still accurate, no change needed).
- Training data: DIS-TR (Dichotomous Image Segmentation training set), no stated redistribution
  restriction found.
- A full ONNX export exists publicly at `huggingface.co/onnx-community/BiRefNet-ONNX`
  (`onnx/model.onnx`, confirmed reachable, **~970MB**). Downloading and running it was judged out
  of this pass's time budget — the same "obtaining actual checkpoints is out of scope for this
  spike" call ADR-0007 made for LaMa/MobileSAM, applied here for the same reason (large download +
  CPU inference time on a model this size, in a single research pass that also had to cover
  geometry/compose/refine/GPU work). `spikes/siamese/src/segment.rs::BiRefNet` proves the loading
  shape only (`ModelNotFound` on a missing file; a real session-load attempt when a file is
  present, `#[ignore]`d without one).

### MobileSAM (interactive click/box refine)

- License: Apache-2.0 (code and weights, distilled from SAM/SA-1B) — clean, existing row.
- Real contract: **two separate ONNX sessions** — an image encoder (run once per image) and a
  lightweight prompt decoder (run once per click/box against the encoder's embedding). RapidRAW's
  own real, shipping code confirms this exact split
  (`docs/research/stalk-prior-art.md:72-73`: `sam_vit_b_01ec64_encoder/decoder.onnx`, two sessions).
  `spikes/siamese/src/segment.rs::MobileSam::encode`/`decode` mirrors that split rather than
  collapsing it into one call the way `spikes/groom/src/ai.rs` did for its own (single-model)
  healing case — the split matters here specifically because the embedding is the thing #44 bakes
  and caches, so a second/third click stays cheap.
- No real weights obtained (same time-budget reasoning as BiRefNet). The decoder's exact
  input/output tensor layout (point/box coordinates as separate named tensors, plus a low-res mask
  hint and `orig_im_size`) is unverified in this sandbox — `spikes/siamese`'s own decoder
  simplifies the prompt into one concatenated header ahead of the embedding, same "one input
  tensor" simplification `spikes/groom/src/ai.rs` uses for its own case, for the same reason (no
  real model to validate a multi-input contract against).

### SAM/SAM2 (Meta) — re-checked, not adopted

- Code/weights: Apache-2.0, already confirmed.
- **SA-V dataset license, previously flagged "not independently re-checked" in `docs/licensing.md`
  — resolved this pass.** Fetched `sav_dataset/README.md` directly:
  `https://raw.githubusercontent.com/facebookresearch/sam2/main/sav_dataset/README.md` states
  plainly: "The dataset is released under the CC by 4.0 license." No ambiguity, no secondary
  source needed — `docs/licensing.md`'s row and footnote updated accordingly.
- Not adopted for v1 anyway: SAM2 is heavier and video-oriented (temporal masklets, per
  `sav_dataset/README.md`'s own schema — `masklet`, `masklet_visibility_changes`, etc., none of
  which Nicti's single-image editing needs). MobileSAM's own decoder already covers the
  click/box-refine case #48 asks for at a smaller model size; BiRefNet already covers one-shot
  subject selection at higher edge quality than SAM's own mask head is generally reported to
  produce (a matting-style network vs. a segmentation-style one). Re-open if MobileSAM's real edge
  quality proves insufficient once weights are actually measured.

### Sky segmentation — no clean model adopted this pass

- **RapidRAW's own choice**: a community `skyseg` fine-tune of U-2-Net, pulled at runtime from
  `CyberTimon/RapidRAW-Models` on HuggingFace (`docs/research/stalk-prior-art.md:72-74`,
  `ai_processing.rs:31-38`).
- **U-2-Net itself**: Apache-2.0
  (`https://raw.githubusercontent.com/xuebinqin/U-2-Net/master/LICENSE`, verified directly this
  pass), general model trained on DUTS-TR (a standard public salient-object-detection benchmark —
  the repo's own README documents this, no unusual restriction found). New row added to
  `docs/licensing.md`.
- **The specific `skyseg` fine-tune** RapidRAW downloads is a separate community checkpoint, not
  the base U-2-Net release — its own training-data provenance wasn't independently re-verified
  this pass (would mean tracing back through whichever fork/dataset that specific fine-tune used,
  out of this pass's scope). Flagged in `docs/licensing.md`'s new row rather than assumed clean.
- **This pass's interim answer**: a classic (non-AI) heuristic in `spikes/siamese/src/sky.rs` —
  luminance + blue-dominance per-pixel test, flood-filled from the top row so a disconnected
  bright/blue region elsewhere in the frame doesn't get picked up just because it passes the color
  test in isolation (proven in `sky_only_at_the_top_does_not_leak_into_a_disconnected_bright_patch_below`).
  This is explicitly the fallback the ADR's own Decision rule expects to lose to a real model on
  quality — it's the "ship something now, replace it once a model is properly vetted" answer, not
  a claim of matching BiRefNet/SAM-quality sky selection.

### EfficientSAM — desk research only

Not evaluated hands-on this pass (time budget); flagged as an open alternative to re-check if
MobileSAM's real measured latency doesn't hit the ADR's `<100ms` decode hypothesis once weights are
obtained — EfficientSAM's own claim is a lighter decoder at close-to-SAM quality, which would be a
direct answer to that specific risk if it holds up.

## LRC's own mask storage, cross-checked against this ADR's shape

Per `spikes/shed/src/develop.rs` (#61/ADR-0023's own catalog-schema research) and
`docs/adr/0002-non-destructive-edit-model.md`'s footnote on `crs:MaskGroupBasedCorrections`: LRC's
real catalogs (measured against a 380,307-asset real catalog) show `hasMasks` on 13,258 rows and
`hasAIMasks` on 12,015 — AI masking is real, common usage, not a rare edge case, consistent with
the hero scenario building it into the very first two mask ops of its edit stack. `#49`/`#62`'s
future importer maps LRC's own `MaskGroupBasedCorrections` shape onto this ADR's `MaskGroup` —
named identically on purpose so that mapping is closer to a rename than a re-derivation.

## Cache-key design: why the model input can't be the live edit stack

An earlier framing this pass considered and rejected: bake the AI mask against whatever the image
currently looks like after the user's full edit stack (so the model "sees what the user sees").
Rejected because:

1. Every exposure/tone-curve slider drag would then invalidate every AI mask's bake key,
   defeating the entire point of baking them separately from the live per-frame render — the
   render graph would re-run BiRefNet/MobileSAM on every slider tick, not just re-touch the cheap
   local-adjustment math.
2. The hero scenario syncs the same edit stack across 50 images — a bulk-mask-generation pass —
   which would then mean 50 separate model runs whose result depends on incidental tone-slider
   state, rather than 50 runs of "find the subject," decoupled from how the user is currently
   grading the photo.

This ADR's answer: masks infer against a **fixed neutral render** (lens-corrected, default tone),
independent of the live edit stack. `ai_bake_key()`'s `upstream_model_input_hash` parameter is
exactly this — the hash of that fixed neutral render, not of whatever the user's current sliders
produce.

## What was and wasn't reachable this pass

- Reachable, fetched directly: SAM2's `sav_dataset/README.md` (CC-BY-4.0 confirmed), U-2-Net's
  `LICENSE` file (Apache-2.0 confirmed) and README (DUTS-TR training data confirmed), a public
  BiRefNet ONNX export's existence and size (confirmed reachable, 970MB, via a `HEAD` request —
  not downloaded).
- Not attempted: downloading and running any real model weights (BiRefNet ONNX, MobileSAM
  encoder/decoder ONNX, ONNX Runtime's own prebuilt shared library) — all judged out of this
  pass's time budget rather than technically blocked; network access itself was available in this
  sandbox (confirmed via the HEAD requests above), unlike the no-GPU/no-real-weights sandbox
  limitation ADR-0005/0006/0007/0021 all cite. The reference-machine follow-up issue should
  attempt real inference, not just real hardware timing, since nothing here actually blocks it
  beyond time.
