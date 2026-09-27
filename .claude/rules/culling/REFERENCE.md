---
paths:
  - "spikes/litter/**"
  - "spikes/squint/**"
  - "spikes/rosette/**"
---

# Culling — Quick Reference

Full reasoning/history: `docs/decisions/culling.md`.

- **Burst/duplicate grouping (#33)** — `docs/adr/0033`: con-day duplicates are pose sets 2-30s
  apart, not sub-second bursts — time is a *constraint*, not the decision; a visual-similarity
  signal decides. Four candidates (dHash/pHash/SSIM/DINOv2), same grouping algorithm.
- **Two-level groups: tight (pick one) nested inside set (collapsible)** — nesting is structural
  (`group_sets` only ever merges whole tight groups), verified by test, not just asserted.
- **`nef.rs` verified against 37 real Z8 NEFs** (XMP-sidecar cross-check) — `ShutterCount`/
  `SerialNumber` unencrypted on Z8, capture time within 1ms (LRC's own XMP-rounding quirk, not a
  bug). **Unverified**: D7500/D3400 encryption of these tags.
- **Real DINOv2 model run, not `#[ignore]`d-and-never-tried** — real ONNX Runtime 1.19.2 +
  `onnx-community/dinov2-small`. **Gotcha**: the test process segfaults on exit (an `ort`/
  `load-dynamic` teardown-ordering quirk) even though the test's own assertions pass — run the
  compiled binary directly to see the real signal, `cargo test`'s harness reports the child crash
  as a failure regardless.
- **No unculled shoot exists on disk** — every large con folder is already culled (20-40% frame
  density); `E:\cf\*.dd` is blank. Real measurement waits on the user's next con — see #180.
- **`litter draft`'s `label.html` is local-only, never published** — real third-party photos +
  past artifact size limits at con scale.
- **Blur/misfocus/eye detection (#34)** — `docs/adr/0034`: every sharpness candidate scores a
  frame's *sharpest tile*, not a whole-frame average (shallow-DoF safe). Same con-shoot gap as
  #33; substituted with synthetic defocus/motion-blur/misfocus degradation of a synthetic keeper
  image set this pass (8 generated images, not real photos), which also gives a real (not
  fabricated) **keeper false-flag rate** on that set — 0.0% for all three candidates. **Gotcha**:
  one threshold calibrated on defocus severity didn't transfer to detecting motion blur in this
  pass's measurement, even though motion blur's own relative score drop was severe — calibrate
  per-degradation-type, don't assume one cutoff generalizes. Nikon `AFInfo2` AF-area reader
  (`af.rs`) targets the real Z8/Z9 `"0400"` version (an adversarial review + primary-source lookup
  caught an earlier draft reading the wrong `"0100"`/`"0101"` offsets) but is still unverified
  against a real NEF. **No eye-detection candidate shipped this pass** — real candidates
  (MediaPipe/YuNet/OWLv2/DINOv2-probe) were license-checked but not run against real weights; see
  `eyes.rs`'s doc comment rather than trusting a fabricated result.
- **Subject grouping (#35) is NOT sequence-constrained** — reuses litter's DINOv2 embedder but
  needs its own clustering (DBSCAN + silhouette-guided eps), since the same subject can reappear
  anywhere in a shoot, not just nearby in capture order.
- **DINOv3 license-gated + HF manual-download gate; no clean OpenCLIP ONNX export found** — both
  real weights are TBD for #35, same as litter's own con-shoot ground truth.

## Package contents

- **`spikes/litter`** (#33/ADR-0033's burst/duplicate-grouping research) — EXIF/Nikon-MakerNote
  capture-time reader, dHash/pHash/SSIM/DINOv2 similarity signals, two-level
  sequence-constrained grouping. Real, tested (35 unit/integration tests), not path-gated,
  pending the reference-labelling measurement pass ADR-0033 describes. See
  `docs/research/litter-burst-grouping.md`.
- **`spikes/squint`** (#34/ADR-0034's blur/misfocus/eye-detection research) — Laplacian
  variance/Tenengrad/real-2D-FFT/structure-tensor sharpness candidates, disk/motion-blur synthetic
  degradation, an AF-region-aware misfocus ratio, a Nikon `AFInfo2` AF-area reader, and a labelling/
  eval harness. Real, tested (32 unit tests), not path-gated, pending the real con-card
  measurement pass (#238) ADR-0034 describes. `eyes.rs` is research-only (no bundled candidate).
  See `docs/research/squint-blur-eye-detection.md`.
- **`spikes/rosette`** (#35/ADR-0035's subject-grouping research) — DINOv2/OpenCLIP/DINOv3
  embedding backbones (DINOv2 adapted from litter), DBSCAN + silhouette-guided eps clustering,
  full-frame-vs-crop ablation, burst-collapse helper, and a labelling/eval harness. Real, tested
  (48 unit tests), not path-gated, pending both a real labelled shoot and DINOv3/OpenCLIP model
  files. See `docs/research/rosette-subject-grouping.md`.
