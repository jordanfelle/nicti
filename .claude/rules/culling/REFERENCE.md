---
paths:
  - "spikes/litter/**"
  - "spikes/squint/**"
  - "spikes/rosette/**"
  - "crates/nicti-pelt/src/cull/**"
  - "crates/nicti-pelt/src/grid/selection.rs"
  - "crates/nicti-lair/src/shred.rs"
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
- **YOLO (#108) is complementary to #34/#35, not redundant** — different primitive from #34's
  in-region sharpness scoring; a sequential pre-crop stage in #35's own cited Fursee prior art, not
  a competing embedding choice. License re-verified against primary source (still genuinely
  AGPL-3.0, current family YOLO26, ONNX-exportable) — no drift from the existing
  `docs/licensing.md` row. **Deferred, not integrated**: #35's crop ablation (pending #243) is the
  actual decision mechanism, so #108 doesn't duplicate it.

- **Culling UX (#32, built)** — `docs/adr/0032`: all the standard LRC keys live at once (`0-5`
  stars, `P`/`X`/`U`, `6-9` labels, Ctrl+Z/Y); no "workflow" is baked in -- filter on whatever you
  marked with (the #242 filter bar's rating/flag/label controls), Ctrl+A, Delete. Reject is `rating = -1` (replaces the
  stars), pick is `flag = 1`, they're exclusive; toggles decide once for a whole selection.
  **Gotchas**: egui's `key_pressed` *includes OS auto-repeat* and egui recomputes `repeat` from its
  own held-key state -- read raw events and ignore repeats (`cull/input.rs`), and tests must send
  release events or a second press is swallowed. egui's `key` is the *logical* key (Shift+3 = `#`),
  so the number row is bound by physical position (`binding_key`) or Shift+digit is dead. Marking never waits on the catalog: a writer
  thread owns all marker I/O and the undo ring (`cull/worker.rs`), the UI updates a cache first and
  a reply only overwrites a photo with no newer write pending. Marking never reloads the grid (list
  frozen until the filter changes). **Delete** (`nicti_lair::shred`) journals (`delete_item`, schema
  v8) *before* moving files to the Recycle Bin, judges success from what is left on disk (a batch
  bin call reports one error for the whole batch), and never removes a row unless the disk *positively*
  says the file is gone from a reachable folder -- every stat is present/absent/**unknown**
  (`Path::exists()` swallows I/O errors, so it is never used for this), and unknown keeps the row
  (unplugged drive != deleted photo). A sidecar is trashed only when ALL of these hold: its RAW is confirmed gone, the folder was listed completely, and no same-stem sibling still exists -- a sibling whose existence can't be checked, or an incomplete listing, counts as "still needed" and keeps the sidecar. Photos under a root with an unfinished folder move are skipped.
  Undo history is bounded by photos (200k), not just entries -- a select-all is a ~128 MB entry. Survey (`N`, <=16) and Compare (`C`)
  draw embedded previews only (T0, T2 for compare) -- never `DevelopView`. **Unverified**: real
  Windows Recycle Bin run, drawn pixels, real-hardware keypress latency.

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
- **`crates/nicti-pelt/src/cull/`** (#32/ADR-0032) — `keys.rs` (key table + pure marking
  semantics), `input.rs` (repeat-safe key reading), `worker.rs` + `undo.rs` (the writer thread and
  undo ring), `mod.rs` (`CullState`: optimistic cache over the worker, `should_advance`),
  `badges.rs` (marker
  drawing), `previews.rs` (survey/compare tile textures, T0 -> T2 upgrade), `tile.rs` (pure tile
  geometry), `survey.rs`, `compare.rs`, `delete.rs` (the Delete prompt/job/summary). Also
  `grid/selection.rs` (index-range multi-select, remapped by photo id on a snapshot change) and
  `crates/nicti-lair/src/shred.rs` (`Shred` delete engine, `resume_open_deletes`). The marker
  filter choices (unrated / exactly N / unflagged / no label) live in #242's `filter_bar.rs`.
