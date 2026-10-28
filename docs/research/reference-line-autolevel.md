# Per-session auto-level from one reference line: research for #169

Findings for [#169](https://github.com/jordanfelle/nicti/issues/169) (under #8): straighten a whole batch shot from one
fixed setup (a photobooth, a stage) from a reference vertical/horizontal line the user confirms once, instead of
re-running blind Canny/Hough per image (#47) and hoping it locks onto the right line. A research pass over the code that
exists today; **no photos were processed and nothing was measured** (no real session batch is available on this host).
Numbers below that look like thresholds are proposals for the build ticket to tune, marked as such.

## The short answer: no training, and most of it already exists

- It is **not a learning problem**. For a camera that does not move, the correct rotation is one number, computed once
  from the line the user drags (`geometry::straighten_delta_degrees`, the #47 gesture), and reused verbatim. "Trainable"
  in the title is a misnomer; there is nothing to train.
- **Applying the angle to a group already works today** through copy/paste settings (#52, `nicti-pelt/src/knead/`):
  straighten one photo with the Ctrl-drag gesture, `Ctrl+Shift+C`, tick "Crop and straighten", select the group (a keyword,
  a collection, a folder, or any filter result), `Ctrl+Shift+V`. It is per-stage and absolute, undoable as one batch, and
  `Crop and straighten` is unchecked by default because it "describes one specific photo's content" (`knead/mod.rs`).
- What is *missing* is (a) transferring only the **angle**, not the whole crop rectangle; (b) a **drift check** so a
  bumped tripod or a mixed-up group is caught instead of silently mis-levelled; (c) the plumbing to fit each target's crop
  without decoding it (below).

## Open questions, answered

**1. One angle for the group, or per-image re-detection anchored to the reference?** One angle, plus a *verifier*.
Per-image detection adds estimation noise to a quantity that is constant on a tripod: the detector works on an 800 px
downscale with median-of-lines and untuned thresholds, and no real-photo noise figure exists yet (`autolevel.rs`, #273), so
re-detecting every frame can only add scatter to a batch whose true angle does not change. Use detection where it helps: record the reference line
(two endpoints in the displayed frame's normalised coordinates, plus its angle) and, per photo, run the existing
Canny/Hough primitives only in a narrow band around that line with an angle prior (a proposal: band about 2-3% of the long
edge, angle within about 1.5 degrees of the reference). Outcomes, in `AutoOutcome`'s existing vocabulary (ADR-0101):
edge found at the expected angle (apply the reference angle unchanged); found with a consistent residual (a step
change: the camera was bumped, so split the batch into **runs** and give each run its own angle rather than a per-frame
wobble); not found (flag for review, not apply). That is also how a mixed-room selection is caught: its photos fail the
band test, instead of being assumed to share a line.

**2. How is the group scoped?** An **explicit selection** (any keyword, collection, folder or filter result the user has
on screen), no stored scope in v1. Reasons: keywords (#23) are organisational, not geometric, so a multi-room event under
one tag would silently share an angle; burst grouping (#33) is the wrong unit because a session spans many bursts; a stored
per-tag setting is hidden state that new photos only join if something re-applies it. A later enhancement can offer "re-apply
to photos added to this keyword" using the saved reference line (the same record the verifier needs). Multi-room collisions
are handled by the verifier flagging outliers, not by trusting the tag.

**3. UI and persistence?** No persistent "model". The reference line lives with the operation (and in the undoable
batch's record), not in a global setting; the angle is ordinary `CropParams::rotation_degrees` on each photo, so it is
non-destructive, visible in Develop, and syncs/exports like any edit. Storing the reference per keyword is the deferred
enhancement above.

## Real constraints the build must handle

1. **Fitting the crop without decoding each photo.** After setting a rotation, each photo's crop must be fitted so no
   corner samples outside the frame (`CropParams::with_rotation_fitted`, #557/#589), which needs that photo's true source
   extent. A batch over a thousand photos must not decode a thousand RAWs. Today the catalog's `width`/`height` for a RAW
   are the embedded preview's declared size (#550/#604), not the frame's: the same aspect for Nikon, but the wrong pixel
   scale for a crop rectangle stored in source pixels. Options: read the true dimensions from the RAW header (a cheap IFD
   read, as righting does for Orientation, #561); store a normalised crop; or fit **at resolve time** (`spine::resolve_inputs`)
   so every consumer (Develop, export, grid, prebake) agrees and a batch writes only the angle. The review of #589 found
   exactly this class of bug (fit applied in one consumer only), so resolve-time fitting deserves a decision of its own.
2. **Orientation.** The reference angle is measured in the *displayed* frame. Loupe/Develop do not yet apply the EXIF
   Orientation on screen (#341), so a portrait shot's stored frame is rotated 90 degrees from what the user judged. Until
   #341, restrict the batch to photos with the same Orientation as the reference (a fixed booth: all of them) and say so
   when the selection mixes them.
3. **Sign and composition.** The new angle is added to the photo's existing rotation the way the gesture does
   (`straighten_from_drag`), clamped to `MAX_STRAIGHTEN_DEGREES`; the batch should offer "set" (absolute) as well as
   "add", since the group's photos may already carry a manual angle.
4. **Parity with LRC.** LRC's Sync Settings lets the straighten angle travel apart from the crop; recalled from memory, not
   verified on a current build. Rotation-only transfer is the Nicti equivalent.

## Recommendation: build tickets

- **A (small, #614): "Apply this angle to the selection"**: a rotation-only batch action next to Paste (set or add, undoable as one
  batch via the `knead::batch` machinery), restricted to matching Orientation, fitting each target using the chosen extent
  source above. This alone covers the photobooth case with no detection.
- **B (needs data, #615): the reference-line verifier**: record the reference line, run the banded Canny/Hough check per
  photo, split into runs, flag failures in the grid/Develop using the existing auto-hint low-confidence presentation.
  Measure on a real session batch first: the distribution of per-photo residual angles on a fixed camera (expected near
  zero within detector noise), and how often a bumped tripod shows as a step. Thresholds above are untuned proposals.
- **C (decision): resolve-time crop fit** versus per-writer fits, since A and B both depend on it.

## Not done / unknowns

Real-batch residual statistics; detector noise on the user's actual booth lighting; whether the booth ever shoots
portrait; whether LRC's own behaviour matches the recollection in point 4.
