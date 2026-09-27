## Classic (non-AI) auto-tone

Covers #99's research (ADR-0099): the histogram-percentile heuristic vs. empirical-fit comparison
for a classic auto-tone algorithm matching LRC's "Auto Settings," and why the reference-machine
measurement pass is deferred.

- **Classic auto-tone (#99)**: `docs/adr/0099-classic-auto-tone.md` — **Proposed**, pending #202
  (a real LRC comparison; this sandbox has no LRC install and `spikes/retina` can't build here
  either, its `vendor/LibRaw` submodule not initialized). Two candidates: **A**, a
  histogram-percentile heuristic (median → exposure toward 18% mid-gray, interquartile spread →
  contrast, highlight/shadow clip-mass fractions → Highlights/Shadows/Whites/Blacks); **B**, a
  hand-rolled ridge regression from a 13-feature histogram vector to the six PV2012 sliders. The
  decision rule (Exposure MAE ≤ 0.15 EV, other sliders MAE ≤ 8 / p95 ≤ 20) is fixed in the ADR
  before any real data exists.

- **Primary metric is per-slider value error, not a golden image.** The issue's own wording asked
  for "a golden-image comparison against real LRC auto-tone output," but nothing in this repo can
  render PV2012 sliders to pixels yet — that's #46's job (global adjustments), not #99's. Faking a
  renderer here to produce a golden-image number would add a second source of approximation error
  on top of the sliders' own error, muddying exactly the comparison the ADR needs to be clean.
  #46's own exit criteria now explicitly owns the rendered-image comparison once its
  slider-application code exists.

- **Default-rendered proxy, not raw camera values.** LRC computes "Auto Settings" against the
  image at its default (all-sliders-zero) render, so the histogram this research analyzes has to
  come from that render, not straight off the sensor. `spikes/pupil::render` reuses the exact
  "fixed color treatment for comparison" `spikes/rods` established for #40 — camera RGB →
  XYZ(D50) → linear sRGB → sRGB OETF, deliberately not ADR-0038's real DCP pipeline — plus as-shot
  white balance (`cam_mul`) before the matrix step, since #40's own harness compared already
  white-balanced inputs and didn't need that step itself.

- **Spikes stay self-contained, never depend on each other.** `spikes/pupil::input` is a small
  independent copy of `calico::linear_input`'s TIFF+JSON reader, and `spikes/pupil::truth` is a
  small independent copy of `shed::develop`'s `agprefs`/`Adobe_imageDevelopSettings` reading — both
  deliberately *not* path dependencies on `calico`/`shed`. `spikes/rods` already established this
  pattern (it doesn't depend on `calico`/`retina` either): a spike is deleted once its own ticket
  promotes it, and a spike-on-spike dependency would break the moment either side is deleted or
  restructured.

- **Ridge regression via hand-rolled Gaussian elimination, no linear-algebra crate.** Candidate B
  has six outputs and about a dozen features — small enough that a `nalgebra`/`ndarray` dependency
  (and the license-review surface ADR-0018 would then require) wasn't worth it. Fits per-slider
  independently: `(X^T X + lambda*I) w = X^T y`, solved with partial pivoting.

- **The real LRC capture and the sample-set selection are both deferred to #202**, not this PR —
  same "spec + tooling merged, baseline measurement deferred" shape as #90/#149/#163/#164/#171.
  `ref-10k` and its manifest aren't reachable from the sandbox ADR-0099 was authored in
  (`NICTI_REF10K` unset, the dataset itself absent — see `docs/benchmarks.md`), so
  `nicti-prowl::refset::select`'s deterministic bucket picker can't be run here either. Committing
  a fabricated sample list against a dataset this sandbox can't see would be worse than committing
  none — `bench/lrc/auto-tone.ahk` takes its source directory as a CLI argument (same convention
  as `bench/lrc/setup.ahk`) rather than reading a frozen file, so #202 supplies it once it can
  actually reach `ref-10k` on the reference machine.
