# A personal crop-style model: research for #170

Findings for [#170](https://github.com/jordanfelle/nicti/issues/170) (under #8): learn from the user's own crop history
to suggest, or in an opted-in batch auto-apply, crops on new imports. A research pass over the code and the earlier
auto-tone work; **no crop data was read and no model was trained** (the real `.lrcat` is not on this host). Thresholds
below are proposals, marked as such. The honest summary: the idea is plausible but its value is unmeasured, and the
cheapest baselines may already cover most of it, so the first step is a measurement, not a model.

## What the data would be, and what is wrong with the obvious source

LRC stores a crop as `HasCrop`, `CropLeft/Top/Right/Bottom` (0..1 fractions of the **oriented** frame), `CropAngle` and
`CropConstrainAspectRatio` in each photo's develop settings. Nicti's importer already reads them (`nicti-stray`
`develop/crop.rs`) but **deliberately translates only upright originals with no angle**; rotated originals and any
non-zero `CropAngle` are skipped and kept verbatim in provenance, because the orientation and sign conventions are not
yet pinned against real renders. So the training set must come from LRC's raw fields (or the provenance blob), not from
the translated `CropParams`, and needs the same orientation care that #341, #550/#604 and #561 are working through (a
fraction of the oriented frame only means something once the frame's orientation is known).

Four ways the raw data misleads, each to be handled in the dataset builder:

1. **Straighten is not framing.** A small `CropAngle` makes LRC shrink the rectangle (Nicti's own fit does the same,
   `fit_crop_to_rotated`). Compare each crop with the rectangle the angle alone would produce; a crop within a small
   tolerance of it is "no framing decision" and must not be a positive example.
2. **Selection bias.** Only photos the user chose to crop have a crop. The "left uncropped" photos are equally informative
   (should this photo be cropped at all?) and must be in the set as explicit negatives, not missing rows. Which photos
   count follows #53's rule: keepers (picked or rated), not every file (ADR-0053 rejected "every edited image as a label").
3. **Aspect locks.** `CropConstrainAspectRatio` means many crops are a fixed aspect chosen for the delivery (a print
   ratio, a social crop), so the useful target is *where* a rectangle of the locked aspect sits and how large, not a free
   rectangle. Aspect must be an input or a separate prediction.
4. **Style is per event.** A user crops tightly for portraits and loosely for wide scenes, and re-uses one framing for a
   whole shoot. Any split must be by event/folder (`purr`'s `split::by_folder`), or per-event leakage will make a weak
   model look good.

The data volume is **unknown**: no existing measurement counts how many of the 380,228 photos carry a framing crop (as
opposed to none, or angle-only). That count is the first deliverable, one read-only query over the closed `.lrcat` backup.

## Model scope: small is plausible, but the context matters more than the model

The ticket asks for subject detection plus an offset regression. Findings from what exists:

- **Subject features without a big model.** BiRefNet (972 MB, ~9 s warm on CPU, `docs/research/cpu-subject-model.md`) is
  far too slow to run per import photo on 1000+ images; the smaller CPU models measured there are licence- and quality-
  qualified. A crop model needs a *rough* subject box, not a matte, so cheap saliency, a coarse low-resolution run of a
  smaller subject model, or a 32x32 thumbnail feature (as `purr` uses) are the right input class. Nothing in-tree yields a
  rough subject box without BiRefNet today (`crates/nicti-siamese` is the masking crate, promoted from the spike, to extend). Faces are constrained:
  InsightFace is non-commercial and excluded (`docs/licensing.md`), MediaPipe/YuNet are not yet verified there.
- **Pure-Rust local trainer.** ADR-0053 (via ADR-0218) already settled that training on a user's library is offline,
  local and pure Rust, with the manifest and feature cache never committed. `spikes/purr` (`dataset` -> `extract` -> `train`
  -> `report`, MLP/ridge baselines, by-folder split) is the harness to extend; #99/#53's "personalize a generic
  auto-adjustment" pattern is shared at the harness level (dataset builder, event split, evaluation), not as one model.
- **The strongest signal is probably the previous crop in the same event, not the image.** People re-use a framing across
  a session. That is a *few-shot, per-event* problem: condition on the last N accepted crops of the event (their size,
  aspect and offsets relative to the subject box) rather than fit one global style. It also means a part of the feature
  already exists: copy/paste settings (#52) applies the previous photo's `Crop and straighten` to a selection today.

## Baselines the model must beat (build these first, no ML)

1. No crop (the answer for most photos, and the denominator for any acceptance rate).
2. Copy the previous photo's crop in the same event (what #52 paste does by hand).
3. Rule-of-thirds / subject-centred crop around the rough subject box, at the user's median tightness.
4. The event's median crop (insets and aspect).

Metrics, against the user's real crop: intersection-over-union and per-edge error as a percentage of the frame; the
**acceptance rate under the real UI** (the suggestion is within a tolerance, a proposal of about 2% of the frame per edge,
so one click would be accepted without adjustment); and the **false-suggestion rate** on photos the user would not crop.
A model is worth building only if it beats the best baseline on acceptance by a margin (proposal: at least 10 points)
with a false-suggestion rate low enough to leave "no suggestion" the default.

## Interaction

Suggest, never silently change: a ghost crop overlay in Develop and the grid with one-click accept, mirroring #36's caution
about silently altering. Batch auto-apply only for a selection the user explicitly opts into, shown as a review grid and
applied as one undoable batch (the `knead::batch` machinery). Always an ordinary, non-destructive crop edit. Abstain by
default: no suggestion when the confidence or the P(crop) head is low.

## Recommendation

1. **Measure first** (small ticket): from the closed `.lrcat` backup, count photos with a framing crop (after excluding
   angle-only), per event, the aspect-ratio distribution, and the share that are locked-aspect. Stop here if framing
   crops are rare or concentrated in a few events.
2. **Baselines and evaluation harness** (extend `purr`'s dataset/extract/report; no model): compute the four baselines and the
   metrics above on an event split. If baseline 2 or 4 already gives a high acceptance rate, ship that as a feature and
   stop: it needs no learning.
3. **Only then** a small model (gradient-boosted trees or the existing MLP), per-event conditioned, trained locally.

## Not done / unknown

The number and distribution of real framing crops; whether #341/#561 make the oriented-frame fractions usable for portrait
shots; whether cheap subject boxes are good enough on the user's subjects (crowded halls, fursuits); how much of the
benefit the per-event baselines already capture.
