## Culling

Covers burst/duplicate grouping (#33) and subject grouping (#35). Blur/misfocus detection (#34)
is a separate research ticket, not yet covered here — this file grows as it lands.

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
