# ADR-0099: Classic (non-AI) auto-tone algorithm

- **Status:** Proposed — pending #202 (reference-machine run)
- **Date:** 2026-09-27
- **Ticket:** #99 Research: classic auto-tone algorithm

## Context

LRC's one-click "Auto Settings" (Ctrl+U) is invoked constantly in real edit sessions and has no
ticket covering a Nicti equivalent. #53 is the separate, v2-track ML auto-tone trained on the
user's own edit history; this ticket is the v1, classic/algorithmic one, needed before #46 (global
adjustments) can ship an "Auto" button at all.

Two candidates from the issue body:

- **Candidate A — a histogram-percentile heuristic.** Fixed clip percentiles map to
  Exposure/Contrast/Highlights/Shadows/Whites/Blacks estimates — the commonly-documented
  approximation of Adobe's undisclosed algorithm. Simple, explainable, no training data needed.
- **Candidate B — an empirical fit.** A ridge regression from a histogram feature vector to the
  six sliders, matched against real LRC "Auto Settings" output.

Blocker #44 (stage-cached render graph) is closed (ADR-0044 exists), so nothing blocks starting
this research.

## Decision rule (stated before measuring)

Adopt **A** if its holdout error is within Exposure MAE ≤ 0.15 EV and every other slider's MAE ≤ 8
units / p95 ≤ 20 units. Otherwise adopt **B** if it meets those same thresholds. Otherwise adopt
**B** anyway (an imperfect fit beats a heuristic that also fails the bar) and file a per-slider
residual note for #46 to account for at implementation time.

## Decision

**Primary metric is per-slider value error, not a golden image.** The issue asked for "a
golden-image comparison against real LRC auto-tone output," but nothing in this repo can yet
render PV2012 sliders to pixels — that's #46's own job. Comparing rendered images now would
either fake the render (muddying the result with a second source of error) or block on #46, which
would rather consume this research than wait on it. #46's own exit criteria now owns the
rendered-image comparison once its slider-application code exists.

**The real LRC capture is deferred to #202**, a "reference-machine run" follow-up, the same shape
as #90/#149/#163/#164/#171 — this sandbox has no LRC install and can't run `spikes/retina` (its
`vendor/LibRaw` submodule isn't initialized here). What merges now is the spike, the capture
tooling, and synthetic-data validation that both candidates behave correctly; the decision rule
above and the candidates themselves are already fixed so #202 is a pure execute-and-record pass.

### Spike: `spikes/pupil`

- **`input`** — reads `retina dump-linear`'s TIFF + JSON sidecar. A small independent copy of
  `calico::linear_input`, not a path dependency (see the module's own doc comment) — this repo's
  spikes stay self-contained, the same pattern `spikes/rods` already follows.
- **`render`** — the default-rendered proxy image "Auto Settings" is computed against: as-shot
  white balance (`cam_mul`) + camera RGB → XYZ(D50) → linear sRGB → sRGB OETF, the same fixed
  "not ADR-0038's real DCP pipeline, correct enough to compare candidates on equal footing"
  treatment `spikes/rods` established for #40, copied here for the same reason.
- **`histogram`** — a sorted-sample nearest-rank percentile/mean/clip-fraction lookup. No image
  histogram utility existed anywhere in this repo before this ticket.
- **`sliders`** — the six PV2012 targets (Exposure2012/Contrast2012/Highlights2012/Shadows2012/
  Whites2012/Blacks2012) with their documented ranges and a clamp.
- **`heuristic`** — candidate A: median → exposure (targeting 18% mid-gray), interquartile spread
  → contrast, and highlight/shadow clip-mass fractions → Highlights/Shadows/Whites/Blacks.
- **`fit`** — candidate B: hand-rolled ridge regression (normal equations via Gaussian elimination
  with partial pivoting) over a 13-feature vector (percentiles, mean, clip fractions). No
  linear-algebra crate dependency — six outputs, a dozen features, not worth the license-review
  surface (ADR-0018).
- **`truth`** — reads LRC's own Auto slider values back out of a `.lrcat` (read-only + immutable,
  same convention as `spikes/shed`'s `open.rs`): `Adobe_imageDevelopSettings.text` parsed via
  `agprefs`, joined to `AgLibraryFile` by base name + extension. A small independent copy of the
  `agprefs`/`Adobe_imageDevelopSettings` reading `spikes/shed` already does (same
  self-contained-spike reasoning as `input`), not a dependency on `shed`.
- **`eval`** — per-slider MAE, p95 absolute error, and signed bias between predicted and ground
  truth.
- **`pupil` CLI** — `auto` (candidate A on one image), `fit` (train B on a deterministic 50/50
  split, report holdout error), `eval --candidate a|b` (compare a candidate against a `.lrcat`'s
  ground truth, fitting B on the training half first).

### Capture tooling for #202

- **`bench/lrc/auto-tone.ahk`** — modeled on `bench/lrc/hero.ahk`/`setup.ahk`, same `<source-dir>`
  CLI-argument convention (not a frozen file list this PR can't validate — see below). Creates a
  fresh **throwaway** catalog (never the user's real one), imports the sample set, selects all in
  Library Grid, then a manual (not scripted) Quick Develop "Auto" click applies Auto Tone to the
  whole selection — **not** a scripted Ctrl+U: a reported, current (2026) LRC regression (Adobe
  Community: "Batch editing in Library module (presets, AI updates, auto tone) only applies to the
  first selected photo in Lightroom Classic 15.3") means batch Ctrl+U over multiple selected photos
  can silently apply to only the first one, which would corrupt ground truth with no visible
  error — not independently reproduced against this repo's own LRC install (no LRC in this
  sandbox), but multiple real user reports are consistent with each other. No Ctrl+S/XMP write
  needed — results are read
  straight out of the throwaway `.lrcat` via `pupil::truth`, once Lightroom has fully exited (its
  own guard refuses a catalog with a live lock file or pending WAL frames).
- **The sample set itself is #202's job, not this PR's.** `nicti-prowl::refset::select` (the
  existing bucket picker) needs `ref-10k` and its manifest on disk (`NICTI_REF10K`), and ref-10k
  isn't present in this sandbox — see `docs/benchmarks.md`. #202 runs on the reference machine
  where ref-10k is reachable, picks a sample there (a deterministic 50/50 train/holdout split via
  `select`), copies it to a source directory, and passes that to `auto-tone.ahk`. Committing a
  fabricated sample list here, against a dataset this sandbox can't see, would be worse than no
  list at all.

## Measured results

Synthetic-data sanity checks only (see `spikes/pupil`'s tests and `tests/smoke.rs`):

- A dark synthetic frame gets positive `Exposure2012`; a frame with a partially blown highlight
  corner gets negative `Highlights2012`; a flat, low-contrast frame gets positive `Contrast2012`.
  Every slider stays within its documented range under adversarial (all-0/all-1) input.
- `fit` recovers a planted linear mapping to within 0.05 of ground truth on synthetic data, and
  clamps sanely on out-of-distribution input.
- **Real LRC comparison numbers: not yet measured — TBD, see #202.**

## Options considered

- **Golden-image comparison now, with a stub renderer.** Rejected: a stub slider-application
  renderer would add its own approximation error on top of the sliders' own error, making the
  metric harder to interpret than direct slider comparison, for no real benefit over waiting for
  #46's real renderer.
- **Depend on `calico`/`shed`/`retina` directly instead of copying their small reader
  functions.** Rejected: this repo's established convention (`spikes/rods` vs. `spikes/calico`/
  `spikes/retina`) is that spikes stay self-contained and depend only on crates.io crates, not on
  each other — a spike is deleted once its own ticket promotes it, and a spike-on-spike dependency
  would break the moment either side is deleted or restructured.
- **A neural/ML fit instead of ridge regression for candidate B.** Rejected for this ticket: #53
  already owns the ML-model auto-tone track (a different model trained on the user's own edit
  history, v2). Candidate B here stays a classical linear fit so it's a fair "classic" alternative
  to compare against A, not a smaller version of #53.

## Consequences

- #46 (global adjustments) consumes `pupil::heuristic`/`pupil::fit`'s output as the "Auto"
  button's slider targets once #202 picks a winner, and #46's own exit criteria takes on the
  rendered-image golden comparison this issue originally asked for.
- #53 (AI auto-tone) is unaffected — it remains a separate v2-track model, not built on this
  ticket's output.
- #202 tracks the deferred reference-machine run; until it lands, this ADR's Status stays
  Proposed and its decision rule is unresolved.

**Update (#46 shipped, provisional)**: #46 didn't wait for #202 to ship an Auto button at all —
`crates/nicti-tapetum/src/perk.rs` ports candidate A (the heuristic) into production code now,
explicitly flagged provisional in its own doc comment and in the `develop` topic's own
REFERENCE.md. This ADR's Status stays Proposed and its decision rule stays unresolved either
way — shipping A now is a scheduling choice (an unblocked Auto button beats an indefinitely
blocked one), not a claim that A has won the comparison #202 still owns.
