# ADR-0053: AI auto-tone (MLP, per-user edit history)

- **Status:** Proposed
- **Date:** 2026-09-27
- **Ticket:** #53 Research: AI auto-tone

## Context

A lightweight model predicting the core develop sliders (Exposure2012, Contrast2012,
Highlights2012, Shadows2012, Whites2012, Blacks2012, Saturation, Vibrance) from a downsampled image
tensor, trained on the user's own LRC edit history. Explicitly v2-track, not part of the v1
must-have feature bar. Unblocked: its one prerequisite, #61 (LRC catalog schema mapping), has
landed.

This is a separate, deliberately distinct question from #99's classic auto-tone (ADR-0099):
#99 reproduces LRC's own "Auto Settings" algorithm as a heuristic/ridge-fit pair, evaluated against
LRC's own Auto output as ground truth. #53 instead predicts *this user's own actual final edits* —
noisier, more personal, and not trying to match any documented algorithm. ADR-0099 explicitly
defers this to #53 and treats it as unaffected by its own decision.

Unlike #99, whose real LRC comparison had to be deferred to a reference-machine follow-up (#202) —
no LRC install, and `spikes/retina`'s `vendor/LibRaw` submodule not initialized in that sandbox —
**this pass had both the closed LRC catalog backup and the RAW drives reachable from the sandbox**
(the user's own backup at `G:\Backups\...\Lightroom Catalog-2-2-v13-3.zip`, extracted to a scratch
directory outside the repo, and the RAW files themselves mounted at `/mnt/c` through `/mnt/k`). So
this ADR trains and measures against real data directly, with no reference-machine deferral.

## Scope decisions (made before measuring)

- **Input: the NEF's embedded JPEG**, not a LibRaw linear render. Unlike #99 (which needs to match
  what LRC's own algorithm analyzes), #53 only needs a reasonable visual summary of the photo — the
  embedded JPEG is pure Rust (`nicti_cornea::embedded`, no LibRaw FFI/vendored submodule/C++
  compile) and dramatically cheaper per image.
- **Training framework: `candle-core`/`candle-nn`, CPU only.** Per ADR-0218, training on a user's
  own library must run locally, offline, on the user's own machine by default — no Python, no
  hosted training service, nothing this repo could ship without bundling a Python runtime. `candle`
  is the only ML-training framework in this repo's dependency graph (see `docs/licensing.md`'s
  2026-09-27 update); `ort`'s existing use elsewhere (masking/culling/healing) is inference-only.
- **Labels: picks/rated keepers only, with a real non-default edit.** Filtering to `pick = 1 OR
  rating >= 1` (both real `Adobe_images` columns) restricts training data to images the user
  actually cared enough about to flag — the careful edits, not the 90%+ of a large event that never
  gets touched. Requiring at least one of the eight targets to differ from its default excludes
  rows LRC never actually developed (an all-default row carries no real editing signal).

## Decision rule (stated before measuring)

Looser than #99's rule — a user's own edits are noisier and less consistent than LRC's own
algorithmic Auto Settings, so a tight per-slider bound isn't a fair standard here:

1. The best ML model (M1 or M2) beats B1 (ridge) by **≥15% on mean range-normalized MAE**
   (`eval::EvalReport::mean_normalized_mae` — each slider's MAE divided by its documented range
   width, averaged across all eight, so Exposure2012's 10-unit range and Contrast2012's 200-unit
   range are comparable in one number) on the **event** split's holdout set.
2. Exposure2012 MAE ≤ 0.25 EV, and every other slider's MAE ≤ 10 / p95 ≤ 25.

If both hold: adopt the winning ML model for a v2 build ticket. If only (1) holds: record ML as
promising over the ridge baseline, with per-slider residual notes, but not yet meeting an absolute
bar worth shipping. If (1) fails: the finding is "ridge is enough" — a v2 auto-tone build should use
a per-user ridge fit on `pupil`-style histogram features rather than a neural model.

## Decision

**Primary split is by event (folder), not by image.** LRC users commonly select a whole folder and
apply one develop setting to every image in it — an image-level split would let synced batch edits
leak near-duplicate label pairs across train and holdout, inflating apparent accuracy. `split.rs`'s
`by_folder` hashes `AgLibraryFolder.id_local` (SplitMix64, deterministic, no RNG dependency) to
assign every row from one folder to the same side. A **temporal** split (oldest captures train,
newest hold out) is measured as a secondary check — the realistic personal-model deployment shape
(train on history so far, predict on what comes next).

**Four models compared**: B0 (predict the training mean — the floor), B1 (ridge on the 13-value
histogram feature vector, an 8-slider generalization of `pupil::fit`'s hand-rolled
normal-equations solve), M1 (a small `candle` MLP on the same 13-value histogram vector), M2 (the
same MLP architecture, on the histogram vector plus a flattened 32x32 RGB thumbnail — the issue's
own "downsampled image tensor"). Early-stopping validation for M1/M2 is carved from the training
rows only, never the holdout set.

### Spike: `spikes/purr`

See `docs/research/purr-ai-auto-tone.md` for the full module/pipeline breakdown. Summary:
`catalog` (keeper-row query + `agprefs` develop-settings parsing, a small independent copy of
`pupil::truth`'s lock/WAL guard — spikes stay self-contained per this repo's convention), `sample`
(deterministic per-folder-capped sampling), `split` (event/temporal splits, generic over row type),
`features` (embedded-JPEG extraction + histogram/thumbnail feature computation), `dataset`
(manifest/feature-cache scratch-file formats + parallel 12-worker extraction runner), `sliders`
(the eight PV2012 targets), `baseline`/`fit`/`mlp` (the four models), `eval` (per-slider MAE/p95/
bias + the aggregate metric this decision rule reads).

## Measured results

Real data, sampled from the user's own closed LRC catalog backup (per-folder cap 300, target 5000
rows) and extracted from the corresponding RAW files at their real `/mnt/<letter>` paths:

- **52,495 real keeper rows** found before sampling (picked/rated NEFs with a real non-default
  PV2012 edit), sampled down to **5,000 rows across 34 distinct folders/events** (the per-folder
  cap concentrated the sample into large events; a future run should lower the cap or raise
  `--target-total` for a broader folder spread, at the cost of a much longer extraction pass — see
  below).
- **Extraction: 5,000/5,000 succeeded, 0 unreachable, 0 failed** — every sampled path resolved onto
  this sandbox's `/mnt/<letter>` mounts and decoded cleanly. Wall time: **17m26s** (12-worker pool,
  146 CPU-minutes of user time), consistent with this repo's other measurements of WSL's `/mnt` 9p
  protocol as the bottleneck, not CPU (`project_lightroom_replacement`'s `ref-10k` copy measured a
  similar flat ceiling past 12 workers).
- **M2's naive first run diverged**, not a real model-capacity result: at the same learning rate
  (0.005) that trains M1 cleanly, M2's 236x-wider input (3085 histogram+thumbnail floats vs. M1's
  13) drove every output to a saturated `tanh` extreme within a few epochs (e.g. Contrast2012 MAE
  105 against a 200-wide range — a collapsed constant prediction, not noise). A learning-rate sweep
  (0.001/0.0005/0.0001) found 0.0005 trains without collapsing; `purr report` uses 0.0005 for M2
  and 0.005 for M1 (see `bin/purr.rs`'s `m1_config`/`m2_config`).
- **Corrected after adversarial review: every model now fits on the same rows.** The first working
  version gave B0/B1 the *entire* `train` split while carving 15% off `train` as an M1/M2-only
  early-stopping validation slice — an undisclosed ~15%-more-data advantage for the baselines that
  confounded the B1-vs-M1 comparison below. `fit_val_split` (`bin/purr.rs`) now deterministically
  shuffles `train` (fixed seed, so it reproduces) before carving the 85/15 fit/val slice — the
  shuffle matters on its own: `train` arrives folder/id_local-ordered, so an unshuffled positional
  cut would have concentrated `val` in whichever folder happened to sort last, not a representative
  sample. All four models below now fit on the identical 85% `fit` slice; only M1/M2 additionally
  see the 15% `val` slice, for early stopping only, never for computing a gradient. The finding is
  unchanged by this fix (see below) — B1 was never relying on the extra data to win.

**Per-split, per-model mean range-normalized MAE** (`purr report`, lower is better; `fit`/`val`/
`holdout` counts shown once per split since every model shares the same three-way split):

| Split (fit/val/holdout) | B0 (mean) | B1 (ridge) | M1 (MLP, hist) | M2 (MLP, hist+thumb) |
|---|---|---|---|---|
| Event (3396/600/1004) | 0.0221 | **0.0169** | 0.0178 | 0.0275 |
| Temporal (3400/600/1000) | 0.0303 | **0.0241** | 0.0246 | 0.0320 |

B1 (ridge) is the best model on **both** splits, and its own numbers barely moved after the fairness
fix (0.0169/0.0241, unchanged to 4 decimal places on the event split) — confirming B1 was never
winning on a data-quantity advantage. Neither MLP variant beats it: M1 comes within 5% of B1 on the
event split and within 2% on the temporal split, but is still worse on both; M2 (the model actually
using the downsampled image tensor the issue asked about) is worse than the trivial mean baseline B0
on the event split and close to it on the temporal split — *more* data-starved than before the
fairness fix, since M2 now trains on 3,396 rows (down from 3,996) against its 3,085-dimensional
input. A larger sample (raising `--target-total`, at the cost of a much longer extraction pass) is
the most likely lever to change this finding, not further learning-rate tuning.

Full per-slider MAE/p95/bias for every model x split combination is in this ADR's PR — see
`purr report`'s output; not reproduced in full here since it's long and secondary to the aggregate
metric the decision rule reads.

**Decision-rule outcome: rule (1) fails — B1 is the finding, not an ML model.** No ML model beats
B1 by the required ≥15% on mean normalized MAE on the event split; B1 in fact beats both ML models
outright. Rule (2) is checked for completeness on the actual winner (B1): Exposure2012 MAE 0.155
(event)/0.205 (temporal), both ≤ 0.25 ✓; every other slider's MAE stays ≤ 10 on both splits, but
**Highlights2012's p95 on the temporal split is 27.6, over the ≤ 25 bound** — a real, if modest,
miss even for the winning model. Per the decision rule's own fallback: **the finding is "ridge is
enough."** A v2 auto-tone build should use a per-user ridge fit on `pupil`-style histogram features
(`fit.rs`'s `RidgeModel`, generalized to eight sliders) rather than a neural model — and should
account for Highlights2012's temporal-split residual (a real, if modest, drift as the user's editing
style evolves over time) at implementation time, the same way ADR-0099 hands #46 a residual note
rather than treating it as this ADR's own problem to solve.

Synthetic-data sanity checks (see each module's own tests, `cargo test -p purr`): the ridge fit
recovers a planted linear mapping to within 0.05 of ground truth; the MLP recovers a planted
mapping on synthetic data and its output stays within every slider's documented range even on
out-of-distribution input; the event split keeps every row of one folder on the same side and lands
within 5 percentage points of the requested holdout fraction at scale; the temporal split correctly
orders the newest rows into holdout.

## Options considered

- **A neural/ML fit for #99's candidate B instead of a separate #53.** Rejected in ADR-0099 itself:
  #99's candidate B stays a classical ridge fit so it's a fair "classic" alternative to compare
  against candidate A, not a smaller version of #53's own model.
- **PyTorch training + ONNX export for inference.** Rejected: per ADR-0218, training on a user's own
  library must be local and offline by default — bundling a Python training pipeline (even one that
  only runs once, locally) is a materially larger shipping surface than a pure-Rust CPU trainer, and
  this repo has no other Python dependency anywhere in its production path.
- **LibRaw linear render as the model input, matching `pupil::render`.** Rejected: #53 isn't trying
  to match LRC's own algorithm the way #99 is, so there's no reason to pay LibRaw's FFI/vendored-
  submodule/per-file-decode cost when the embedded JPEG is a perfectly good visual summary for this
  purpose.
- **Every edited image as a label, not just picks/rated keepers.** Rejected: LRC's own default
  render + a bulk-sync preset produces a large volume of near-identical, low-signal edits (batch
  white-balance corrections, import-time defaults) that would dilute the training signal relative
  to the user's actual careful edits.

## Consequences

- A future v2 build ticket picks up **B1 (ridge)**, not an ML model, per the Measured results above
  — `fit.rs`'s `RidgeModel` and `features.rs`'s histogram extraction generalize directly, and are
  fast enough to train per-user, on-device, with no GPU. It wires into a real "Auto" suggestion in
  the develop UI, following ADR-0218's user-initiated training flow (the user's own trained weights
  are scratch state produced on their own machine, not a downloaded/bundled model — training runs
  only when the user asks for it), and should account for Highlights2012's temporal-split p95
  residual noted above.
- **This finding could change with more training data.** M2 (the model using the actual downsampled
  image tensor the issue asked about) underperformed even the trivial mean baseline with ~3,400 fit
  training rows against a 3,085-dimensional input — a data-starved regime, not necessarily a ceiling
  on what a neural approach could do with `ref-10k`-scale data. If a future pass revisits the ML
  track, raising `--target-total` well past 5,000 (at the cost of a much longer extraction pass,
  ~17 minutes for this run's 5,000 rows) is the first lever to try, not further architecture or
  learning-rate changes.
- #99 (classic auto-tone) is unaffected — it remains ADR-0099's own separate decision, not built on
  this ticket's output, and vice versa.
- The manifest and feature-cache scratch files this pass produced, and the trained model weights
  themselves, are **not committed** to the repo (see `docs/research/purr-ai-auto-tone.md`'s Privacy
  section) — they're derived from the user's own real, private photo library.
