# ADR-0427: Upright (automatic and guided perspective correction)

- **Status:** Accepted
- **Date:** 2026-10-23
- **Ticket:** #427 Upright: auto-perspective correction (vertical/level/full/guided)

## Context

Nicti had auto-level only (Canny + Hough, #47/#273) and no perspective correction. The LightCraft
audit (#424) pointed at its `upright.rs` (LSD-style segments, RANSAC vanishing points, virtual
rotation). That checkout was not available when this was built, so the implementation here is
original; nothing is ported and no `docs/licensing.md` entry is needed.

## Decision

- **`nicti_tapetum::upright`** (CPU, resolution independent, no WGSL for the solve):
  1. `detect_segments`: 800px luma proxy, Sobel gradient, strongest-first region growing on line
     orientation (22.5 deg tolerance, magnitude cut `max(0.35*p90, 0.02)`), magnitude-weighted PCA
     per region, kept when long (>= 4% of the long edge) and elongated (>= 6:1). Returned in source
     pixels.
  2. Segments become plane normals in ray space (`(x-cx)/f, (y-cy)/f, 1`), split into near-vertical
     and near-horizontal groups (40 deg), and each group gets a RANSAC vanishing direction
     (deterministic xorshift, 300 hypotheses, 2 deg inlier angle) refined by the smallest
     eigenvector of the inliers' `sum w n n^T`. The horizontal VP must be within 10 deg of
     orthogonal to the vertical one.
  3. Modes are rotations: **Level** = roll; **Vertical** = roll + pitch (vertical VP -> image y);
     **Full** = + yaw (horizontal VP -> image x); **Auto** = Full, else Vertical, else Level,
     whichever the evidence supports. A correction above 40 deg on any axis is refused.
  4. The result is `K R^T K^-1` as a `Homography` (output -> input, like `Affine2D`), zoomed about
     the centre by the largest factor that keeps every output corner inside the source (no empty
     corners). A correction needing a zoom below `MIN_ZOOM` (0.3) is refused (`NoResult`), and a
     stored value that would need it renders as the identity rather than a collapsed image.
     Level takes its roll from the vertical VP when there is one (a horizontal VP's direction mixes
     in pitch/yaw).
  5. **Guided** (`analyze_guides`, 1-4 guides, each tagged vertical or horizontal by drag axis): two
     or more guides of a kind give that kind's VP; a lone guide only rolls, and a mode the guides cannot
     support downgrades (Full -> Vertical -> Level).
- **Storage**: `CropParams.upright: Option<UprightParams>` (roll, pitch, yaw, focal as a fraction of
  the image diagonal). Angles, not a pixel matrix, so one value serves a preview, a tile and a full
  export; omitted from the JSON when absent so existing documents keep their hash. Documents are
  untrusted: `UprightParams::sanitized` clamps everything.
- **Pipeline**: the homography is applied by `present_sample.wgsl` *after* the crop affine, so the
  crop rect and straighten angle live in the corrected frame (as in LRC). `CropKernel::set_upright`
  holds it (bound every render by `RenderInputs::bind_effects`, tiles share it); effects stay in
  crop-normalized coordinates and never see it. `geometry::sample_bilinear_upright` is the CPU
  reference the GPU parity test checks against.
- **Develop UI**: Crop & Straighten gains Auto / Level / Vertical / Full / Off, and a Guided toggle
  (Crop tool; drag lines on the uncorrected frame). The Crop tool's whole-frame view keeps the
  correction (`uncropped_keeps_upright`); heal and masks keep showing the uncorrected frame because
  their coordinates are source coordinates. Low-confidence or empty results write nothing (ADR-0101).

## Known gaps / follow-ups

- **Focal length**: no EXIF focal length reaches `LinearFrame` yet, so `DevelopDoc::focal_35mm` is
  `None` and Upright assumes a 28mm-equivalent. Perspective strength is wrong for other lenses until
  that is plumbed (the angles are only as good as `f`).
- **Thresholds are untuned** (gradient cut, region tolerance, inlier angle, minimum lengths); tests
  are synthetic. A real-photo tuning pass is owed, the same exercise as #273 (`needs-physical-testing`).
- The Heal/Mask/Point Color overlays and the eyedropper outside the Crop tool assume source
  coordinates; the eyedropper maps through the homography, the others deliberately show the
  uncorrected frame.
