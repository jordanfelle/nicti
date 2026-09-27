## Culling

Covers burst/duplicate grouping (#33), blur/misfocus/eye detection (#34), and subject grouping
(#35).

- **Burst/duplicate grouping (#33)**: `docs/adr/0033-burst-duplicate-grouping.md` — con-day
  duplicates are pose sets 2-30s apart, not sub-second bursts (measured on a real con day: most
  gaps land at 2-10s, only 152/1,368 are <=1s), so a pure timestamp threshold misses most real
  duplicates. `spikes/litter` implements sequence-constrained two-level (tight/set) grouping,
  where time acts as a constraint and a visual-similarity signal (dHash/pHash/SSIM/a real DINOv2
  embedding, all four measured under the identical grouping algorithm) makes the actual link
  decision. Nesting invariant (every set group is a union of whole tight groups) is structural,
  not just asserted — a dedicated test tries to break it under adversarial similarity functions.
  A from-scratch EXIF/Nikon-MakerNote reader (capture time, shutter count, serial, PreviewIFD)
  cross-checks byte-exact against 37 real Z8 NEFs' own XMP sidecars. A real ONNX Runtime library
  and a real DINOv2 export were obtained and run end-to-end (not left as an untested `#[ignore]`)
  — found one real caveat along the way: a segfault-on-exit that's an `ort`/`load-dynamic`
  teardown-ordering quirk, not a bug in this crate. **Proposed, not Accepted** — the actual
  accuracy numbers wait on a real unculled con shoot (none exists on disk yet; every large con
  folder found is already culled), tracked in the labelling/measurement follow-up (#180).
- **Blur/misfocus/eye detection (#34)**: `docs/adr/0034-blur-misfocus-eye-detection.md` — every
  sharpness candidate (Laplacian variance, Tenengrad, a real 2D FFT high-frequency ratio,
  structure-tensor motion/defocus discriminator) scores a frame's *sharpest tile*, not a whole-
  frame average, so a shallow-depth-of-field shot isn't penalized for its own bokeh. Same
  ground-truth gap as #33 (no unculled con shoot on disk), substituted this pass with synthetic
  degradation of a synthetic keeper image set — which also enables a real (not fabricated)
  measurement on that set: the keeper false-flag rate (all three candidates: 0.0% on this pass's
  8-image synthetic set, not yet measured on real photos). A real finding: a single threshold
  calibrated on defocus severity didn't transfer to detecting motion blur on this synthetic set,
  even though motion blur's own relative score drop was severe — worth per-degradation calibration
  once real labels exist. A Nikon `AFInfo2` AF-area reader (`af.rs`) targets the real Z8/Z9
  `"0400"` version (an adversarial review + primary-source lookup caught an earlier draft reading
  the wrong `"0100"`/`"0101"` offsets) and composes with any sharpness candidate for a misfocus
  signal, but is unverified against a real NEF. Eye detection (human closed-eye, fursuit "eyes
  obscured") is research-only this pass — real candidates were license-checked but none was run
  against real weights, so no bundled candidate ships rather than fabricating a result. **Proposed,
  not Accepted** — real numbers wait on the next unculled con card (#238), same as #33's own #180.
- **Subject grouping (#35)**: `docs/adr/0035-subject-grouping.md` — clusters a shoot's photos by
  subject/person for review-per-subject. Human face-recognition models don't apply: most subjects
  are fursuiters, and InsightFace/RetinaFace are license-excluded anyway (non-commercial only).
  `spikes/rosette` uses general image embeddings instead (DINOv2, reused from litter's own
  embedder; OpenCLIP; DINOv3), clustered with DBSCAN over cosine distance and a
  silhouette-coefficient-guided `eps` sweep — not litter's own sequence-constrained grouping,
  which only links nearby frames in capture order and would miss a subject reappearing elsewhere
  in the shoot. A full-frame-vs-subject-crop ablation is included (crop source: a precomputed
  mask, same shape as `spikes/siamese`'s BiRefNet output). **DINOv3 is license-gated** (Meta's
  DINOv3 License requires a "Built with DINOv3" attribution if adopted) and its ONNX export sits
  behind a Hugging Face manual-accept gate; **no clean-license OpenCLIP ONNX export was found**
  (the one community export, Apple's MobileCLIP2, is research-only) — its embedder is implemented
  against a documented self-export recipe, untested against a real model this pass. **Proposed,
  measurement pending** — same "no real unculled/labelled shoot exists yet" constraint as #33,
  plus no DINOv3/OpenCLIP model file obtained in this pass.
- **Subject grouping (#35)**: `docs/adr/0035-subject-grouping.md` — clusters a shoot's photos by
  subject/person for review-per-subject. Human face-recognition models don't apply: most subjects
  are fursuiters, and InsightFace/RetinaFace are license-excluded anyway (non-commercial only).
  `spikes/rosette` uses general image embeddings instead (DINOv2, reused from litter's own
  embedder; OpenCLIP; DINOv3), clustered with DBSCAN over cosine distance and a
  silhouette-coefficient-guided `eps` sweep — not litter's own sequence-constrained grouping,
  which only links nearby frames in capture order and would miss a subject reappearing elsewhere
  in the shoot. A full-frame-vs-subject-crop ablation is included (crop source: a precomputed
  mask, same shape as `spikes/siamese`'s BiRefNet output). **DINOv3 is license-gated** (Meta's
  DINOv3 License requires a "Built with DINOv3" attribution if adopted) and its ONNX export sits
  behind a Hugging Face manual-accept gate; **no clean-license OpenCLIP ONNX export was found**
  (the one community export, Apple's MobileCLIP2, is research-only) — its embedder is implemented
  against a documented self-export recipe, untested against a real model this pass. **Proposed,
  measurement pending** — same "no real unculled/labelled shoot exists yet" constraint as #33,
  plus no DINOv3/OpenCLIP model file obtained in this pass.
