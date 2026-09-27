# ADR-0034: Blur/misfocus/eye detection

- **Status:** Proposed — sharpness/misfocus candidates and the synthetic-measurement pass are
  built, tested, and run against real synthetic data; the AF-area reader is built but unverified
  against a real NEF; eye detection is research-only this pass (no bundled candidate)
- **Date:** 2026-09-27
- **Ticket:** [#34](https://github.com/jordanfelle/nicti/issues/34) Research: blur/misfocus/closed-
  eye detection

## Context

#34 (Part of epic [#6](https://github.com/jordanfelle/nicti/issues/6), blocks
[#36](https://github.com/jordanfelle/nicti/issues/36)'s AI culling assist integration and
[#108](https://github.com/jordanfelle/nicti/issues/108)'s YOLO research) started as a one-line
migrated stub: auto-flag blur, motion blur, misfocus, and closed eyes for fast rejection, validated
on fursuiters, not just human faces. Nothing in the repo did sharpness or eye analysis before this
pass.

**Same ground-truth gap as #33/ADR-0033**: no unculled con shoot exists on disk (every large con
folder found is already culled), so real blurry/misfocused/closed-eye rejects mostly don't exist to
label either — a real photographer's own kept files are, almost by definition, sharp and eyes-open.

**User decisions (2026-09-27):**

- **Ground truth this pass**: synthetic degradation of a stand-in keeper image set, plus a
  measurement this enables directly without waiting on real photos — the **keeper false-flag
  rate**: how often a candidate wrongly flags an undegraded image from that set. This pass's own
  keeper set is 8 generated synthetic images (see Measured results), not real photographs; the
  false-flag *methodology* is real (a genuine measurement, not asserted prose), but its numbers are
  a synthetic-set result, not yet a real-photo one. Real-reject accuracy on actual photos (the
  #33/#180-style pass) is deferred to a follow-up issue against the user's next unculled con card,
  sharing that labelling session with #180.
- **Eye scope**: human closed-eye detection **and** fursuit "eyes obscured" detection (head turned
  away, eyes hidden by hair/hand/prop) — not fursuit "closed eyes," since fixed/painted fursuit-head
  eyes can't blink. The fursuit check is "can the eyes be seen," not "are they open."
- **AF metadata**: the Z8's own AF-area position (Nikon `AFInfo2`) is one misfocus candidate,
  measured alongside image-only ones — sharpness inside the AF area vs. the frame's sharpest region
  elsewhere.

## Decision rule (stated before measuring)

- **Keeper false-flag rate <= 2%** on real kept photos — the costly error class (hiding a real
  keeper behind a false reject), and the number this pass can actually measure honestly.
- **Blur/misfocus recall >= 0.80** on real labelled rejects — deferred to the follow-up con-card
  pass; this pass instead reports synthetic detection rate at several severities as a proxy.
- **Eyes**: no bar set this pass — no candidate was run against real data (see below), so a
  target would be a guess, not a decision.
- **Cost**: every sharpness candidate here is pure-Rust, sub-millisecond per frame at
  256x256-scale tiles (see Measured results) — comfortably inline during ingest, unlike #33's
  DINOv2 candidate which needed to run as a background job.
- **Tie-break**: the cheapest candidate that clears the bars wins.

## Decision

**New spike: `spikes/squint`** (a cat squints to bring things into focus). Pure Rust, lib+bin
split (matching `spikes/litter`'s shape), not path-gated, depends on `nicti-prowl` for
`perf::Protocol` timing but no other spike crate.

- **`af.rs`** — a from-scratch TIFF/EXIF/Nikon-MakerNote reader (re-implemented rather than
  depending on `spikes/litter` or `nicti-cornea`, per CLAUDE.md's "spikes don't depend on other
  spikes" note): `ExposureTime`/`FocalLength`/`ISO` (a cheap motion-risk prior), SubIFD0's
  full-resolution `JpgFromRaw` preview (8256x5504 per `docs/research/sniff-embedded-jpeg.md`'s own
  Z8 measurement, used for AF-region-level detail), the Nikon MakerNote's smaller PreviewIFD (for
  cheap global scoring), and Nikon `AFInfo2` (tag `0x00B7`) — the AF area the camera itself
  selected. **`AFInfo2`'s layout is reconstructed from published third-party documentation of
  Nikon's format, not a Nikon spec, and is unverified in this sandbox** — no real Z8 NEF exists
  here to cross-check against (the same gap ADR-0033 flagged for `spikes/litter`'s own MakerNote
  fields). `af::AfArea::rescale_to` maps the AF-sensor coordinate space onto any decoded/preview
  frame size.
- **`sharp.rs`** — sharpness/motion/misfocus candidates, every one a pure function over a
  grayscale frame, scored on its *sharpest tile* (not a whole-frame average) so a shallow-
  depth-of-field portrait isn't penalized for its own intentional background blur:
  - `laplacian_variance_tiled` — variance of a discrete 3x3 Laplacian (Pech-Canul et al.'s standard
    blur metric).
  - `tenengrad_tiled` — mean squared Sobel gradient magnitude (Krotkov 1988), a first-derivative
    focus measure distinct from the Laplacian's second-derivative one.
  - `fft_high_freq_ratio_tiled` — a **real 2D FFT** (via `rustfft`, row-then-column decomposition,
    not a Gaussian-pyramid approximation) split into a low-frequency disc and everything outside
    it; returns the high-frequency energy fraction.
  - `structure_tensor_anisotropy` — motion-vs-defocus discriminator: eigenvalue anisotropy of the
    averaged structure tensor over the sharpest tile. Directional energy loss (one eigenvalue much
    larger) indicates motion blur along that axis; isotropic loss indicates defocus.
  - `af_region_misfocus_ratio` — composes with any of the above: sharpness inside the (rescaled)
    AF rectangle divided by the frame's own sharpest-tile score. Near 1.0 means properly focused;
    well below 1.0 means something else in the frame is sharper than where the camera focused —
    back-/front-focus.
- **`synth.rs`** — synthetic degradation of a keeper image set: a disk (pillbox) kernel for
  uniform defocus at several radii, a linear-PSF kernel for motion blur at several lengths/angles,
  and region-confined defocus (`convolve_region`) for synthetic misfocus (blur only a subject/AF
  region, background stays sharp). `default_sweep()` is the severity grid ADR-0034's Measured
  results table reports against.
- **`eval.rs`** — the actual measurement methodology (stated before any numbers exist, same
  discipline ADR-0033 followed): for each candidate, calibrate a decision threshold from the
  median score across every keeper degraded at one concrete reference severity (`defocus_r6` —
  chosen to look unambiguously out-of-focus in a manual spot-check), then report (a) the
  **detection rate** at every other severity in the sweep against that same threshold, and (b) the
  **keeper false-flag rate**: the fraction of undegraded images in this pass's keeper set whose
  score falls below that same threshold (this pass's own set is 8 generated synthetic images, not
  real photographs — see Measured results). Tying the threshold to one inspectable severity (rather
  than an arbitrary percentile) means a reader can sanity-check "is this threshold reasonable"
  against an image they can look at.
- **`label.rs` + CLI `squint draft`** — extracts every NEF's preview, computes every *global*
  candidate's score (not the AF-region-aware one, since `af::AfArea` is unverified against a real
  file), and writes a **local, not published** contact-sheet page (`label.html`, same reasoning as
  `spikes/litter`'s own: real third-party photos, past a published Artifact's size limit). The user
  tags each frame (sharp/motion_blur/defocus/misfocus/eyes_closed/eyes_obscured/not_applicable) and
  exports `labels.json`.
- **CLI `squint labels --work <dir> --labels labels.json`** — reports each candidate's mean score
  grouped by real human tag, a diagnostic rather than a hard precision/recall verdict (that verdict
  needs the real con-card pass, see Consequences).
- **`eyes.rs`** — **no bundled runnable candidate this pass.** Real candidates were license-checked
  but not run against real weights, unlike `spikes/litter`'s DINOv2 pass:
  - Human closed-eye: MediaPipe Face Landmarker (Apache-2.0, ships TFLite not ONNX — would need
    either a new TFLite-runtime dependency class or a re-verified community ONNX re-export) or
    YuNet (OpenCV Zoo, Apache-2.0, ~340KB ONNX, 5-point landmarks but no blink signal on its own).
  - Fursuit eyes-obscured: an open-vocabulary detector (OWLv2, Apache-2.0 per its model card;
    Grounding DINO, license varies by fork/checkpoint) prompted with text queries like `"fursuit
    head"`/`"visible eyes"`, or a DINOv2 patch-feature linear probe reusing #33's own embedding
    (needs real labels to train on before it's anything but an unfitted architecture choice).
  - `EyeTag`/`EyeCandidate` (a trait, no implementation) are the only real code here — enough for a
    future candidate to plug into the same eval harness `sharp`'s candidates use.
  - **Explicitly not InsightFace/RetinaFace** — already excluded by `docs/adr/0018` on license
    grounds (non-commercial-only), independent of #34's own scope.

**Not adopted / deferred:**

- A single global (non-tiled) sharpness score — rejected outright without measuring: it would
  penalize any intentionally shallow-depth-of-field shot, a huge fraction of real con/portrait
  work.
- Fabricating an eye-detection accuracy number without running a real model — ADR-0033's own
  framing ("everything not gated on X is real, tested, measured evidence") ruled this out; see
  `eyes.rs`'s module doc comment for the full reasoning.

## Measured results

**Synthetic pass (done, not pending)** — via `squint synthetic --keepers-dir <dir>` against 8
synthetic "keeper" images (textured 120x120 center region simulating in-focus subject detail
against a smooth background gradient, not real photographs — real-photo numbers are the follow-up
con-card pass's job, see Consequences):

| Candidate | Keeper false-flag rate | Detection @ defocus r1 | @ r3 | @ r6 (= threshold ref) | @ r10 | @ localized misfocus r6 | @ any motion blur (l3-16, 0/45/90°) | ms/frame (p50) |
|---|---|---|---|---|---|---|---|---|
| `laplacian_variance` | 0.0% | 0% | 0% | 50% | 100% | 0% | 0% | 0.15 |
| `tenengrad` | 0.0% | 0% | 0% | 50% | 100% | 0% | 0% | 0.18 |
| `fft_high_freq_ratio` | 0.0% | 0% | 0% | 50% | 100% | 0% | 0% | 0.17 |

**A real methodological finding, not a bug**: every candidate's `mean_score_ratio` (degraded score
÷ original score) drops sharply under every motion-blur variant tested — as low as 0.3% of baseline
at the most severe length/angle — yet `detection_rate` (whether that drop crosses the *absolute*
threshold calibrated from `defocus_r6`) stays at 0% for every motion-blur case, on this synthetic
image set. A defocus-calibrated absolute threshold does not automatically transfer to motion blur,
even though the relative degradation is severe — this synthetic test set's baseline sharpness
happens to sit well above the defocus-derived threshold, so a large relative drop still lands above
it. **Consequence for the real pass**: don't assume one threshold serves every degradation type;
the real con-card labelling pass should calibrate per degradation class (or use a multi-class
classifier) rather than reusing a single defocus-derived cutoff.

**A second real finding, from adding `Misfocus` to the sweep**: localized misfocus (blurring only
the centered ~22%-of-area subject region, background untouched) is detected 0% of the time by
every *global* candidate here, even though its score ratio drops as sharply as `defocus_r6`/`r10`
(0.1-0.5% of baseline). This isn't a threshold-calibration gap like the motion-blur finding above —
it's structural: `max_over_tiles` reports the frame's *sharpest* tile, and a small localized
misfocus region leaves most of the frame's tiles fully sharp, so the frame-level score never drops
regardless of how out-of-focus the subject itself is. **This is exactly why `af_region_misfocus_ratio`
exists as its own candidate** (AF-region sharpness relative to the frame's own max, not the frame's
max alone) rather than being redundant with the global candidates above — a whole-frame max-tile
score structurally cannot detect this failure mode at all, at any threshold.

The 50%/100% split at r6/r10 is a sanity check on the methodology itself, not a finding: the
threshold *is* the median r6 score by construction, so ~50% of r6-degraded images are always
expected to fall below it.

**Pending the real con-card labelling pass** (real photos, real blur/misfocus/eye-state
ground truth) — see Consequences.

**AF-area reader**: unit-tested against a synthetic fixture matching the real Z8's declared
dimensions (8256x5504) and a plausible `AFInfo2` payload; **not yet cross-checked against a real
NEF** (no such file exists in this sandbox — see Context).

## Consequences

**Feeds [#36](https://github.com/jordanfelle/nicti/issues/36)** (AI culling assist integration) once
the real con-card pass lands, and **[#108](https://github.com/jordanfelle/nicti/issues/108)** (YOLO
research) can reference this ADR's finding that a single-region-detector primitive (YOLO-style
bounding boxes) is a different question from *scoring* sharpness within a region — the two are
complementary, not redundant.

**Follow-up filed**: the real con-card labelling + measurement pass (blur/misfocus/eye-state ground
truth, replacing this pass's synthetic numbers with real ones, and cross-checking `af.rs`'s
`AFInfo2` reader against a real Z8 NEF) is
[#238](https://github.com/jordanfelle/nicti/issues/238) (Part of #6), sharing its labelling session
with #180 since both need the same next unculled con card. Added to #36's `**Blocked by:**` line.

**Follow-up filed**: an AF-point viewer overlay in nicti's own UI (showing the camera's own AF-area
selection over a photo, using `af::AfArea::rescale_to`'s same coordinate mapping this ADR's
misfocus candidate relies on) is
[#239](https://github.com/jordanfelle/nicti/issues/239) — raised by the user while this research was
in progress, scoped separately since it's a UI feature, not a culling-signal question. Blocked on
#238's own real-file cross-check, since showing an unverified AF position directly to the user is a
different risk bar than only feeding it into an internal scoring signal.

**A real, flagged risk, not silently assumed away**: `AFInfo2`'s binary layout is reconstructed from
published third-party documentation, not verified against a real file — if the actual on-disk
layout differs from the documented version-`"0100"`/`"0101"` fixed layout (e.g. a firmware/body
difference), `af::parse_af_info2`'s fields would silently return wrong values with no parse error.
Worth checking against a real Z8 NEF (and exiftool's own independent parse, the same cross-check
`spikes/litter`'s `nef.rs` used) before trusting `AfArea` on real input.
