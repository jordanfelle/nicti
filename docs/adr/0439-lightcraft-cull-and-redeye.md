# ADR-0439: LightCraft cull heuristics and red-eye / pet-eye as baselines

**Status:** Proposed (cull heuristics: baselines for the existing labelling passes, no feature; red-eye / pet-eye: no-go, no build ticket)
**Ticket:** #439
**Part of:** #424
**Related:** #238, #180, ADR-0034, ADR-0033, ADR-0025, ADR-0049

## Context

The LightCraft audit (#424) listed `crates/pipeline/src/cull.rs` (138 lines: a block-Laplacian p90 sharpness score, a
clipped-pixel fraction, a 64-float luminance signature with a cosine similarity for bursts) and `redeye.rs` (310 lines
plus a GPU kernel and a CPU/GPU equivalence test: automatic pupil detection inside a user ellipse, neutralise or darken,
optional catchlight, including pet-eye). Read at `265248c`. Nothing is copied, so there is no `docs/licensing.md` entry.

The ticket asks two questions: are the cull heuristics useful as a baseline against ADR-0034 / ADR-0033 / ADR-0025
approaches (ADR-0034, ADR-0033), and is either piece worth a build ticket.

## What Nicti already has

The culling spikes already measure more than LightCraft does:

- **Sharpness** (`spikes/squint/src/sharp.rs`, ADR-0034): `laplacian_variance`, `tenengrad`, `fft_high_freq_ratio`,
  `structure_tensor_anisotropy` (motion versus defocus) and `af_region_misfocus_ratio` (Nikon `AFInfo2`-aware). The tiled
  variants aggregate with `max_over_tiles` (the sharpest tile). LightCraft's score is global and has no AF awareness,
  as its own header says.
- **Similarity** (`spikes/litter/src/signals.rs`, ADR-0033): dHash, pHash, SSIM and a DINOv2 embedding, with capture-time
  and shutter-count constraints and B-cubed/ARI scoring. LightCraft's 8 x 8 zero-mean luminance signature with cosine
  similarity is the same family as dHash/pHash, with no sequence constraint.
- **Clipping:** not measured by either spike. The ADR-0034 label set is sharp / motion_blur / defocus / misfocus /
  eyes_closed / eyes_obscured / not_applicable (#238), so there is no exposure ground truth.

## Decision

### 1. Cull heuristics: take as baselines, do not build a feature

- **Block-Laplacian p90** is the one aggregator not already covered: the 90th percentile of per-block energy, a
  hot-pixel-robust cousin of `max_over_tiles` (which takes the single sharpest tile). **It does not solve localised
  misfocus.** A p90 is a high-order statistic: if crisp background covers more than about a tenth of the blocks it lands
  in background energy however soft the subject is, and LightCraft built it for the opposite purpose (its comment: "a
  sharp subject on a soft background still reads as sharp"). ADR-0034's finding stands, that no frame-level high-end
  score catches a soft subject on a crisp background, and the answer there is `af_region_misfocus_ratio`. So p90 is
  worth one row in the #238 sweep as a robustness comparison against `max_over_tiles`, nothing more (one function in
  `sharp.rs`; 16 x 16 px blocks at a 512 px long edge, as LightCraft does; previews smaller than 512 px would make the
  blocks cover more of the frame, so check the Z8 preview size). It is judged on the same labelled data and
  per-degradation-class thresholds as the others; LightCraft's `0.012` calibration constant is not taken (it came from a
  small CC0 corpus).
- **Signature similarity** adds nothing over dHash/pHash. If someone wants it as a null baseline in the #180 sweep,
  it is a ten-line candidate; it is not a priority.
- **Clipped fraction** is only meaningful with an exposure label. Adding an `exposure` tag to the labelling sheet is
  cheap and would make it measurable, but nothing in the culling ADRs asks for exposure marking today; leave it until the
  operator asks for it.

Neither the sharpness score nor the clip fraction is a feature: LightCraft's score is global, uncalibrated for Nikon HE/HE* previews and knows nothing about
eyes or the AF point, which is exactly what ADR-0034 found matters.

### 2. Red-eye and pet-eye: no-go, no build ticket

- **Audience.** Nicti's culling work targets convention and fursuit photography (ADR-0034 scopes eye detection to
  fursuiters as well as people; ADR-0033 targets con-day duplicates). Red-eye needs a human or
  animal subject lit by on-axis flash; pet-eye needs a tapetum glow. Neither is the subject of that work, and the
  LightCraft ticket itself says "relevant only if wanted for fursuit/pet photography".
- **Existing tools cover the rare case.** Local masks carry saturation and exposure steps (ADR-0049, LRC
  `LocalSaturation` / `LocalExposure2012`), so a brush or radial mask can desaturate or darken a pupil by hand. Whether
  that is practical on a 5-10 px pupil is untested; that is a judgment, and a one-click detector is a convenience
  rather than a gap. (Healing is clone/remove, not recolour, and does not apply.)
- **Cost.** The feature is a detector plus a GPU kernel plus a CPU/GPU equivalence test (LightCraft's `redeye.rs` alone is
  310 lines, plus its kernel and test) and needs a UI for the ellipse. That is a build ticket's worth of work for a case the operator has not
  asked for.

Revisit only if the operator asks for it. If so, the design to start from is LightCraft's: an ellipse, then a fixed
grid of samples in normalised coordinates (resolution independent), a red-dominance score `(R - max(G, B)) / (R + G + B)`
for red-eye and relative brightness for pet-eye, then a weighted centroid and area; re-derive the constants.

## Alternatives rejected

- **Adopt `cull.rs` as the culling feature.** Global metrics and an uncalibrated threshold would regress the
  AF-aware, per-class design of ADR-0034.
- **Build red-eye now for completeness.** See section 2.

## Consequences

- **#238:** add block-Laplacian p90 as one more sharpness candidate in the real-card sweep. **XS**, done with that pass.
- **#180:** optional null baseline (8 x 8 signature) in the similarity sweep.
- **No build tickets** from this ADR; red-eye / pet-eye is parked until asked for.
- Nothing in the shipped code or the spikes changes in this PR.
