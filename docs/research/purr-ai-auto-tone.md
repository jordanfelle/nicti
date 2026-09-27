# `spikes/purr`: AI auto-tone (#53, ADR-0053)

Full decision and measured results: `docs/adr/0053-ai-auto-tone.md`. This doc is the module
breakdown and pipeline walkthrough; the ADR is the record of what was decided and why.

**Naming**: a cat's purr is one of the most individual, recognizable-per-cat vocal signatures in
its behavioral repertoire — a fitting name for a model predicting *this specific user's* own
editing signature, not a general algorithm (contrast with `pupil`/#99's classic auto-tone, which
reproduces LRC's own documented behavior). `knead` was this spike's original name before a naming
collision with `bench/knead` (#45's unrelated real-NEF perf harness, merged to `main` while this
research was in progress) forced a rename — see the naming-convention section of `CLAUDE.md`.

## Pipeline

```
purr dataset  --lrcat <path> --out manifest.json
purr extract  --manifest manifest.json --out features.bin
purr train    --features features.bin --model b0|b1|m1|m2 --split event|temporal
purr report   --features features.bin
```

1. **`dataset`** (`catalog.rs` + `sample.rs`): queries every real keeper (`pick = 1 OR rating >=
   1`, NEF, at least one of the eight PV2012 targets non-default) out of a real `.lrcat`, then
   samples down to a per-folder-capped working set (`--per-folder-cap`, `--target-total`) so a
   single large event can't dominate the training distribution. Writes a JSON manifest — the
   user's own real catalog paths and edit values, **never committed** (see the Privacy section
   below).
2. **`extract`** (`features.rs` + `dataset.rs::extract_all`): for each manifest row, resolves the
   LRC drive-letter path onto this sandbox's `/mnt/<letter>` mount (`catalog::resolve_path`), pulls
   the NEF's largest embedded JPEG via `nicti_cornea::embedded` (pure Rust, no LibRaw/vendor
   submodule needed), decodes it (`zune-jpeg`), and computes two feature representations: a
   13-value histogram feature vector (percentiles/mean/clip-fractions, the same shape as
   `pupil::fit::features`) and a 32x32 RGB thumbnail (`fast_image_resize`). Runs on a 12-worker
   pool (the WSL 9p I/O ceiling `ref-10k` copies elsewhere in this repo measured flat past 12
   workers). A file that can't be resolved or fails to decode is skipped and counted, not fatal to
   the run. Writes a bincode feature cache — also never committed.
3. **`train`/`report`** (`fit.rs`, `mlp.rs`, `baseline.rs`, `eval.rs`, `split.rs`): splits the
   feature cache by event (`split::by_folder`, primary) or by capture time (`split::temporal`,
   secondary), fits each of the four models on the training side, and reports per-slider MAE/p95/
   bias plus the aggregate range-normalized MAE on the untouched holdout side. `report` runs all
   four models across both splits in one pass — the table ADR-0053's Measured results section is
   built from.

## Models

- **B0 (`baseline.rs`)**: predicts the training-set mean for every input. The floor every real
  model must beat.
- **B1 (`fit.rs`)**: ridge regression from the 13-value histogram vector to all eight sliders — a
  small independent copy of `pupil::fit`'s hand-rolled normal-equations solve (no linear-algebra
  crate, per ADR-0018), generalized from six outputs to eight.
- **M1/M2 (`mlp.rs`)**: a small CPU-only MLP (`candle-core`/`candle-nn`), two hidden ReLU layers
  and a `tanh` output bounded to each slider's documented range. M1 trains on the 13-value
  histogram vector alone; M2 trains on the histogram vector plus the flattened 32x32 thumbnail
  (3072 floats) — the issue's own "downsampled image tensor." Early-stopping validation is carved
  from the training rows only (never the holdout set) so the reported holdout error is never seen
  during training.

## Splits

- **Event** (primary, `split::by_folder`): a deterministic SplitMix64 hash of `AgLibraryFolder`'s
  id assigns every row from one folder to the same side, at roughly the requested holdout
  fraction. An image-level split would leak synced batch edits across train and holdout — LRC
  users commonly select a whole folder and apply one develop setting to every image in it, so two
  images from the same folder can be near-duplicate label pairs.
- **Temporal** (secondary, `split::temporal`): the oldest captures train, the newest hold out — the
  realistic personal-model deployment shape (train on edit history so far, predict on what comes
  next).

## Why the embedded JPEG, not a LibRaw linear render

`pupil::render` (ADR-0099) reconstructs a fixed default-render proxy from LibRaw's linear output
specifically to match what LRC's own "Auto Settings" analyzes. #53 has no such constraint — it's
predicting *this user's own edits*, not reproducing LRC's algorithm — so the input only needs to be
a reasonable visual summary of the photo, and the embedded JPEG is dramatically cheaper (no LibRaw
FFI, no vendored submodule, no per-file RAW decode cost) while still being a real camera-rendered
preview of the same frame.

## Privacy

Per ADR-0061's own privacy note and `CONTRIBUTING.md`'s `ref-10k` handling: the manifest (real
catalog paths, capture times, and slider values) and the feature cache (extracted pixel-derived
features and labels) are both derived from the user's own real photo library and **must never be
committed**. `spikes/purr` writes them only to a path the caller supplies (this pass used a
scratch directory outside the repo entirely). The trained model weights themselves are likewise
scratch state, not a repo artifact — see ADR-0053's own note on what a future v2 build ticket would
need to ship a real per-user weight-training flow (ADR-0218's user-initiated, checksummed weight
delivery, applied to weights this user trains themselves rather than downloads).
