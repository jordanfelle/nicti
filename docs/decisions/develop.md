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

## AI auto-tone

Covers #53's research (ADR-0053): an MLP predicting all eight PV2012 sliders from a downsampled
embedded-JPEG preview, trained on the user's own real LRC edit history — a separate, v2-track model
from #99's classic auto-tone, not built on its output.

- **Real data this pass, no reference-machine deferral.** Unlike #99, the closed LRC catalog backup
  and the RAW drives were both reachable from this sandbox, so #53 trains and measures against real
  data directly instead of deferring to a follow-up ticket. See ADR-0053's Measured results.

- **Input is the NEF's embedded JPEG, not a LibRaw linear render.** #53 isn't trying to reproduce
  LRC's own algorithm the way #99 is — it only needs a reasonable visual summary of the photo, so
  it uses the pure-Rust `nicti_cornea::embedded` extraction (no LibRaw FFI, no vendored submodule,
  no per-file RAW decode cost) rather than `pupil::render`'s fixed linear-to-sRGB treatment.

- **Training framework is `candle` (CPU only), per ADR-0218.** Training on a user's own library
  must run locally and offline by default — no Python, no hosted training service. `candle` is the
  first ML-training framework in this repo's dependency graph; `ort`'s existing masking/culling/
  healing use is inference-only, a different case.

- **Labels are picks/rated keepers with a real non-default edit**, not every edited image — a bulk
  sync or import-time preset produces a large volume of near-identical, low-signal edits that would
  dilute the training signal relative to the user's actual careful edits.

- **Split by event (folder), not by image**, as the primary holdout — an image-level split would
  leak synced batch edits (a whole folder selected and one setting applied to all of it) across
  train and holdout, inflating apparent accuracy. A temporal split (oldest trains, newest holds out)
  is measured as a secondary check, matching the realistic personal-model deployment shape.

- **Decision rule is looser than #99's** — a user's own edits are noisier than LRC's own
  algorithmic Auto Settings — and requires the best ML model to beat the ridge baseline by ≥15% on
  mean range-normalized MAE, not just clear an absolute per-slider bound alone.

## Presets, copy/paste, sync

Covers #52 (ADR-0052): reusing one photo's develop settings across others.

- **Presets, copy/paste, sync (#52)**: `docs/adr/0052-presets-copy-paste-sync.md` — **Accepted**. Per-stage, absolute semantics: a checked stage replaces the target's `StageEntry` wholesale, a checked stage absent from the source is removed from the target (a reset), an unchecked stage is never touched. Checklist = Develop's "Reset all" stage ids plus the camera profile; crop, heal and masks start unchecked because they describe one photo's content, and the camera profile does too because a `.dcp` is camera-specific (a target from another camera rejects it). Masks travel as the single `nicti.masks` stage; geometry masks are normalized so they transfer, AI masks re-bake lazily when each photo opens (the batch pre-bake this deferred landed in #353, ADR-0353: baked alphas now have a disk tier and a paste/sync/preset pre-bakes the touched photos in the background). One batch write (`get_master_edits`/`put_master_edits`, single transaction, serialise-then-write so a bad document writes nothing). A target whose document would not change is not written and gets no undo entry (ADR-0101 rule 6); the result is one summary line (rule 5). Presets live in `<catalog>.develop-presets.json` (export-presets pattern), no built-ins, duplicate/empty names refused, an unreadable or partly unusable file is copied to `<file>.bad` (`.bad1`, ...) before the first save replaces it, applying a preset never resets a stage it doesn't hold. Undo is session-local (previous documents in memory) and skips any photo whose document is no longer what the batch wrote; real History is #324. The loaded photo is flushed before and re-read after a batch (`DevelopView::replace_document`), else the per-frame autosave overwrites the batch. Relative paste via `apply_relative` is deferred.

## Auto-op graceful degradation

Covers #101 (ADR-0101): what an automatic, non-interactive develop op (auto-straighten #47, auto-tone #99) does on failure or low confidence.

- **Auto-op graceful degradation (#101)**: `docs/adr/0101-auto-op-graceful-degradation.md` — **Accepted**. Shared `AutoOutcome<T>` (`Confident`/`LowConfidence`/`NoResult` + `AutoReason`), a discrete signal rather than a float score since thresholds are untuned (#273) and calibration does not carry across degradation types (ADR-0034). Straighten skips on low confidence (a wrong rotation is worse than none) and shows a non-modal hint; tone always applies, marking low confidence. Never a modal dialog; batch/sync (#52) shows a single summary with a jump-to-skipped filter. An unchanged, `NoResult`, or skipped invocation creates no history step and keeps redo (a general `History` invariant, #312); an applied result is one entry via `apply_batch`. Partial decode → `NoResult(DecodeIncomplete)`. Implementation: #311, #312 (no-op compares effective params, absent == stage default; `compact` drops net-zero runs; `apply_batch` uses `control: None`, so it never coalesces).
  - **#311 landed**: `nicti-tapetum/src/auto.rs` (`AutoOutcome`/`AutoReason`); `autolevel::detect_level_angle` and `perk::estimate` return it. `DevelopView::apply_auto_tone`/`apply_auto_straighten` (`nicti-pelt/src/render.rs`) return `AutoApplied`: `Unchanged` (result equals the effective current params) and `Skipped` write nothing, and both refuse an incomplete frame (`DecodeIncomplete`). The hints and the `⚠` marker beside Auto are `develop_panel::AutoHintUi` (non-modal, transient). Thresholds are untuned starting points: the single-line synthetic fixture yields exactly one Hough detection, so `MIN_SUPPORTING_LINES` is 1 and line disagreement (> 3°) is the live straighten low-confidence trigger; auto-tone is degenerate at ≥ 50% clipped or ≥ 90% of pixels in one luma bin. #273 and #202 own tuning.
