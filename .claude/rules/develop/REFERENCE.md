---
paths:
  - "spikes/pupil/**"
  - "bench/lrc/auto-tone.ahk"
  - "docs/adr/0099-classic-auto-tone.md"
  - "docs/adr/0101-auto-op-graceful-degradation.md"
  - "crates/nicti-tapetum/src/perk.rs"
---

# Develop (Auto-Tone) — Quick Reference

Full reasoning/history: `docs/decisions/develop.md`.

- **Classic auto-tone (#99)** — `docs/adr/0099-classic-auto-tone.md`: **Proposed**, pending #202
  (reference-machine run — no LRC install and `spikes/retina` can't build in this sandbox).
  Candidate A (percentile heuristic) vs. candidate B (ridge-regression fit); decision rule
  (Exposure MAE ≤ 0.15 EV, other sliders MAE ≤ 8 / p95 ≤ 20) fixed before real data exists.
- **Auto-op graceful degradation (#101)** — `docs/adr/0101-auto-op-graceful-degradation.md`:
  **Accepted**. `AutoOutcome<T>` = Confident | LowConfidence | NoResult (3 states, no float score).
  Straighten: low-confidence is skipped + hint. Tone: always applies, low-confidence gets a marker.
  Never modal; batch = one summary. No-op/unchanged/skipped invocation → no history step, redo kept
  (general `History` invariant; via `apply_batch`, `control: None`, never coalesces). Build: #311 (signal + UI), #312 (History guard).
- **AI auto-tone (#53)** — `docs/adr/0053-ai-auto-tone.md`: **finding is "ridge is enough," not an
  ML model.** Trained B0 (mean)/B1 (ridge)/M1 (MLP, histogram)/M2 (MLP, histogram+thumbnail) on
  5,000 real picked/rated keepers from the user's own catalog (no reference-machine deferral — the
  catalog backup and RAW drives were reachable from this sandbox). B1 beat both MLPs on both the
  event and temporal splits; M2 (the model using the actual downsampled image tensor) underperformed
  the trivial mean baseline, likely data-starved at this sample size (3085-dim input, ~3,400 fit
  rows) rather than a hard ceiling. A v2 build should use the ridge fit, not a neural model.
- **Primary metric: per-slider value error, not a golden image** — nothing can render PV2012
  sliders to pixels yet (#46's job), so a rendered-image comparison would just add a second
  approximation-error source. #46 owns that comparison once it exists.
- **`spikes/pupil::render`** reuses `spikes/rods`' fixed "camera RGB → XYZ(D50) → linear sRGB →
  sRGB OETF" color treatment (not ADR-0038's real DCP pipeline) plus as-shot white balance, to
  reconstruct the default-render proxy LRC's own Auto Settings analyzes.
- **`input`/`truth` are small independent copies**, not path dependencies on `calico`/`shed` —
  this repo's spikes stay self-contained (same pattern `spikes/rods` already follows).
- **The real LRC capture AND the sample-set selection are both deferred to #202** — `ref-10k`
  isn't reachable from the sandbox this ADR was authored in, so `bench/lrc/auto-tone.ahk` takes
  its source directory as a CLI arg rather than reading a frozen file list.
- **#46 shipped Auto with candidate A, provisionally** — `crates/nicti-tapetum/src/perk.rs` ports
  `pupil::heuristic::estimate` onto `crate::histogram::Histogram` (production code, not a spike),
  wired to `nicti-pelt`'s Develop panel Auto button. This is a deliberate choice to unblock #46's
  Auto button on a research ticket with no fixed completion date, not a claim that ADR-0099's
  decision rule is resolved -- it isn't; #202 still owns picking the final candidate. If #202
  picks B instead, only `perk::estimate`'s body changes (same `Histogram` in, same
  `ExposureParams`/`ToneParams` out).

## Package contents

- **`spikes/pupil`** (#99/ADR-0099's classic auto-tone research) — `input` (retina dump-linear
  reader), `render` (default-render color treatment), `histogram` (percentile/mean/clip-fraction
  lookup), `sliders` (the six PV2012 targets + ranges), `heuristic` (candidate A), `fit`
  (candidate B: hand-rolled ridge regression), `truth` (LRC ground truth from a `.lrcat`), `eval`
  (per-slider MAE/p95/bias). See `docs/research/pupil-auto-tone.md` for the module breakdown and
  the RapidRAW auto-adjust study.
- **`spikes/purr`** (#53/ADR-0053's AI auto-tone research) — `catalog` (keeper-row query +
  develop-settings parsing from a `.lrcat`, a small independent copy of `pupil::truth`'s
  lock/WAL guard), `sample` (deterministic per-folder-capped sampling), `split` (event and
  temporal train/holdout splits, generic over row type), `features` (embedded-JPEG extraction via
  `nicti_cornea::embedded`, histogram feature vector, 32x32 thumbnail tensor), `dataset` (manifest/
  feature-cache scratch-file formats + parallel extraction runner), `sliders` (the eight PV2012
  targets, extending `pupil::sliders` with Saturation/Vibrance), `baseline` (B0: mean model), `fit`
  (B1: ridge regression, an 8-slider generalization of `pupil::fit`), `mlp` (M1/M2: a CPU-only
  `candle` MLP), `eval` (per-slider MAE/p95/bias + the aggregate normalized-MAE metric ADR-0053's
  decision rule reads). See `docs/research/purr-ai-auto-tone.md` for the full module breakdown.
- **Persistence (#57)**: Develop's `EditDocument` is now saved to the catalog's master edit row
  (`DevelopView::is_dirty`/`mark_saved`, `load_real_frame(frame, identity, doc)`). `has_edits` is
  test-only now. AI Remove spots' recipes persist but their patches don't (#324), so a reloaded
  photo shows no removal until it is re-run.
