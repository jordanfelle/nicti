# HDR merge and panorama: research for #442

Findings for [#442](https://github.com/jordanfelle/nicti/issues/442) (under #424): should Nicti build photo merge
(HDR brackets, panoramas) from LightCraft's `crates/merge`? A read of that crate (at `ebe24771`; the audit was at
`265248c`, so line counts below are from the newer commit) against what Nicti already has. **Nothing was copied, built
or run**; LightCraft was read, not executed, and no brackets or pano sets were tested. The ticket's own verdict stands:
this is outside the Lightroom-replacement MVP and stays v2. This note records what a future ticket would be starting
from, so it does not have to repeat the read.

## What LightCraft has

`lightcraft-merge` (4,385 lines, `forbid(unsafe_code)`, MIT OR Apache-2.0, "our own implementation from published
papers; no GPL code consulted") is layer L3 with no UI dependency. Input is scene-linear `Frame`s from raw or standard
images; output is a linear DNG (16-bit float for HDR) with the source's colour tags.

| Piece | Lines | Method |
|---|---|---|
| `hdr.rs` | 533 | Order by brightness, middle frame is reference. Exposure ratios by median log-ratio over pixels well exposed in both (EXIF fallback). Weight `k * (1 - smoothstep(0.80..0.96 clip))` on the max channel over a 1 px neighbourhood, so a clipped channel never shifts colour. Deghost None/Low/Medium/High: per-pixel log-radiance consistency against the reference on a <= 2048 px proxy, masks dilated and feathered, union exposed as an overlay. |
| `align.rs`, `refine.rs`, `flow.rs` | 153 + 359 + 239 | Features on exposure-normalised luma at <= 1200 px, RANSAC homography (falls back to similarity, translation, identity); full-resolution refinement; then a tile-grid local flow (masked correlation, outlier rejection, 3x3 box smoothing) to absorb parallax, lens distortion and rolling shutter. The flow comment records a real 33 MP bracket where the global fit left a building 2.3 px off. |
| `features.rs` | 312 | MOPS: Harris on a pyramid, ANMS, 8x8 oriented descriptors, brute-force L2 with Lowe's ratio and mutual check. |
| `ransac.rs`, `linalg.rs` | 291 + 247 | Normalised DLT homography, RANSAC with adaptive iteration count, fixed-seed xorshift; small dense solvers. |
| `pano.rs`, `pano/{camera,project,blend,finish}.rs` | 489 + 451 + 83 + 300 + 210 | Brown & Lowe: pairwise match, RANSAC, largest connected set, focal from homographies, rotation + focal bundle adjustment (Levenberg-Marquardt, Huber, numeric Jacobian), auto-straighten, spherical / cylindrical / perspective chosen from FOV, gain compensation, Voronoi-like seams, multi-band blend in log space, boundary warp, auto crop, push-pull fill. |
| `frame.rs`, `output.rs`, `synth.rs` | 411 + 158 + 86 | Raw/standard loaders, DNG writer, synthetic scenes. |

Quality caveat, from the ticket and the tree: tests are thin (6 HDR and 5 pano integration tests at `ebe24771`, the ticket counted 2 and 3 at
`265248c`, plus the synthetic scenes) and the algorithms have not been validated on a real bracket or pano set beyond the one 33 MP case noted in
`flow.rs`. The code is a design to read, not a result to trust.

## What Nicti already has, and what is missing

- **Reusable already:** `nicti_tapetum::upright` has its own deterministic RANSAC (xorshift, vanishing directions) and a
  `Homography` type (ADR-0427); `nicti-cornea` decodes raw/DNG (through LibRaw); `nicti-tapetum` has Lanczos-class resampling and the render
  graph. None of this needs LightCraft code.
- **Missing, and the bulk of the work:** feature detection and descriptors (Upright uses line segments, not points);
  a normalised-DLT homography fit (Upright solves rotations, not a general 3x3 from point pairs); the bundle adjuster
  and its linear algebra; a scene-linear multi-frame input path (the render graph is single-image, so a merge is a
  batch job that produces a new asset, not a develop stage); a linear-DNG writer; multi-band blending; and the
  catalog side: a merged result is a new file that must be stacked with or linked to its sources, imported, and named.
- **Where the output goes** is the real design question, not the maths: a linear DNG that re-enters the pipeline as a
  raw-like asset (LightCraft's choice, and the right one: edit headroom is the point of HDR) needs a DNG writer and a
  decision on camera-profile tags for a file with no CFA.

## Licensing and freedom to operate

- Code licence is fine: MIT OR Apache-2.0, the same as the LightCraft pieces already ported (`docs/licensing.md`, #425,
  #428). A port would add a `docs/licensing.md` row with the commit, files and copyright line, as those did.
- The methods are old published ones (MOPS 2005, Brown & Lowe 2007, Burt & Adelson 1983, Hartley & Zisserman,
  Fischler & Bolles). That is a good sign but **not a clearance**: LightCraft's roadmap lists the freedom-to-operate review
  as still open for local Laplacian filters, PatchMatch and HEVC, and notes it for some pano/local-Laplacian techniques.
  Of these only multi-band blending is in this crate's path (Laplacian-pyramid, not the patented local-Laplacian filter),
  and no patch synthesis is used (fill is push-pull). Nobody on this project has done the review; a build ticket must
  do it first, and say so in the ADR rather than assume it.

## Recommended scope if it is ever prioritised

Re-implement against `nicti-tapetum`, do not port wholesale; take the design, port small pure pieces only where a test
can prove them. In build order, each independently shippable and measurable:

1. **Bracket HDR, no pano.** Highest user value per line. Needs: bracket detection (the stacking work, #53-style
   grouping by EXIF exposure and time), exposure normalisation + saturation-aware weights (`hdr.rs`'s steps 1, 3, 4: about
   150 lines of idea), a homography-only aligner (needs features + DLT), linear-DNG output. Skip deghost and local flow in
   slice 1; ship "tripod or near-tripod" and measure alignment error on real handheld brackets before adding either.
2. **Deghost + local flow**, only if slice 1's handheld results show the residual LightCraft's `flow.rs` documents.
3. **Panorama.** Largest piece (about 1,500 lines of ideas, plus bundle adjustment). Cylindrical / perspective first
   (no spherical); skip boundary warp and fill (a crop is enough); multi-band blend last.

Shared groundwork worth building once for both and for Upright: a point-feature + match module and a general homography
RANSAC. That is what the ticket meant by "reusable (see also Upright)".

## Measurements before any build

Nothing here was measured. A build ticket's first step is a bench on real data, not a port:

- 20 real handheld 3- and 5-frame brackets and 10 pano sets from the owner's library (not synthetic scenes): alignment
  error in pixels against a hand-marked reference, and visual ghosting rate per deghost level.
- Wall time and peak memory at 24 and 45 MP (the `hdr.rs` proxy sizes of 1200 and 2048 px were chosen for speed; check
  them against Nicti's targets in `docs/benchmarks.md`).
- Pano: count of sets where the largest connected component drops frames, and seam / exposure failures.

## Decision requested

None. Keep #442 parked (v2). If merge is prioritised, open a build ticket for slice 1 above, with the FTO review and the
DNG-output decision as its first two steps, and link this note.
