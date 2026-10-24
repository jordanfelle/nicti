# ADR-0439: LightCraft cull heuristics and red-eye / pet-eye as baselines

**Status:** Proposed (cull heuristics: baselines for the existing labelling passes, no feature; red-eye / pet-eye: no-go, no build ticket)
**Ticket:** #439
**Part of:** #424
**Related:** #238, #180, ADR-0034, ADR-0033, ADR-0025, ADR-0032

## Context

The LightCraft audit (#424) listed `crates/pipeline/src/cull.rs` (138 lines: a block-Laplacian p90 sharpness score, a
clipped-pixel fraction, a 64-float luminance signature with a cosine similarity for bursts) and `redeye.rs` (310 lines
plus a GPU kernel and a CPU/GPU equivalence test: automatic pupil detection inside a user ellipse, neutralise or darken,
optional catchlight, including pet-eye). Read at `265248c`. Nothing is copied, so there is no `docs/licensing.md` entry.

The ticket asks two questions: are the cull heuristics useful as a baseline against ADR-0034 / ADR-0033 / ADR-0025
approaches, and is either piece worth a build ticket.

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

- **Block-Laplacian p90** is the one idea not already covered: an aggregator between the global mean and
  `max_over_tiles`. ADR-0034's synthetic pass found that a global score misses localised misfocus and that
  `max_over_tiles` reports the frame's *sharpest* tile, so it cannot see a soft subject in front of a crisp
  background. A percentile of per-block energy is a cheap third option to include in the #238 sweep (one function in
  `sharp.rs`; blocks of 16 x 16 px at a 512 px long edge, as LightCraft does). It is judged on the same labelled
  data and the same per-degradation-class thresholds as the others; LightCraft's `0.012` calibration constant is not
  taken (it came from a small CC0 corpus).
- **Signature similarity** adds nothing over dHash/pHash. If someone wants it as a null baseline in the #180 sweep,
  it is a ten-line candidate; it is not a priority.
- **Clipped fraction** is only meaningful with an exposure label. Adding an `exposure` tag to the labelling sheet is
  cheap and would make it measurable, but exposure culling is not in ADR-0032's scope today; leave it until the
  operator asks for it.

Neither is a feature: LightCraft's score is global, uncalibrated for Nikon HE/HE* previews and knows nothing about
eyes or the AF point, which is exactly what ADR-0034 found matters.

### 2. Red-eye and pet-eye: no-go, no build ticket

- **Audience.** Nicti is aimed at convention and fursuit photography (ADR-0032's context). Red-eye needs a human or
  animal subject lit by on-axis flash; pet-eye needs a tapetum glow. Neither is the subject of that work, and the
  LightCraft ticket itself says "relevant only if wanted for fursuit/pet photography".
- **Existing tools cover the rare case.** The healing and local-adjustment path (masking, ADR-0048/0049) can
  desaturate or darken a pupil by hand; a one-click detector is a convenience, not a gap.
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
