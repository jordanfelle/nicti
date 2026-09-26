# ADR-0021: Color pipeline

- **Status:** Proposed — pipeline design, DCP parser, and GPU-feasibility spike are done and
  measured against synthetic data; the actual LRC-match numbers are pending a reference-machine
  run (see Measured results)
- **Date:** 2026-09-26
- **Ticket:** [#38](https://github.com/jordanfelle/nicti/issues/38) Research: color pipeline

## Context

#38 sits at the head of Nicti's critical path: it blocks #41 (RAW → linear → working-space
pipeline on GPU), which blocks #44 (Tapetum, the stage-cached render graph), which in turn gates
roughly a dozen develop/build tickets. The question this ADR answers: how does Nicti get from
decoded camera RGB to a display-referred image that matches Lightroom Classic closely enough that
migrating existing edits doesn't mean re-editing from scratch? The user shoots Nikon Z8/D7500/D3400
bodies and mostly uses LRC's **Adobe Vivid** camera profile, so matching that specific profile is
the real target, not just a generic "close to Adobe Standard" approximation.

Constraints already fixed elsewhere:

- **ADR-0003** (`docs/adr/0003-third-party-license-policy.md`) and `docs/licensing.md` rule out
  ever bundling a real Adobe `.dcp`/`.xmp` camera profile in this repo — no redistribution grant
  was found. This ADR's parser reads profiles already installed on the user's own machine (from
  their existing Adobe Camera Raw / Lightroom Classic install), at runtime; nothing is vendored.
- **ADR-0005** picked `wgpu` for GPU compute, with `RGBA16F` intermediates and no full-frame
  host↔device round-trips as design goals. `spikes/glint`'s own kernels are storage-buffer-only
  (see its `gpu.rs` module doc) — no 3D-texture pattern existed anywhere in this repo before this
  ADR, and DNG's `HueSatMap`/`ProfileLookTableData` tables are exactly the kind of 3D LUT a texture
  unit's hardware trilinear filtering is built for, so that gap needed closing here.
- **ADR-0017** already flags a real, unsolved gap this ADR doesn't fix: the difference between a
  camera's own Picture Control JPEG rendering and Nicti's Adobe-profile-based rendering for the
  camera-JPEG-derived preview tiers (T0-T2). That mismatch is inherent to using a different
  rendering intent than the camera, not something a color-pipeline bug — noted again here so it
  isn't mistaken for one.
- **#40** (demosaic) and **#42** (ICC/soft-proofing) are explicitly out of scope — this ADR treats
  the demosaiced linear image as its input and produces a working-space linear image as its output.
  Stage *order* within the render graph is #44's decision, not this one's; this ADR only fixes the
  internal order of the color-specific sub-stages relative to each other.
- **Sandbox note**, matching ADR-0006/0007/0020's own precedent: there is no LRC install, no real
  Adobe DCP/XMP profile, and no LRC-rendered reference image anywhere in this sandbox (confirmed —
  `docs/ref-10k-manifest.csv` lists NEFs only, no JPEG/TIFF references, and `docs/licensing.md`
  forbids adding one). Every piece of this ADR that doesn't need a real profile or a real ΔE
  number against LRC is real, tested, measured evidence: 26 unit tests plus 3 integration tests
  (a full synthetic render-pipeline sanity check and a real wgpu compute-shader run against
  lavapipe, this sandbox's software Vulkan fallback — see ADR-0005's own hardware-identity caveat).
  What's pending: the actual DCP/XMP profile files, the actual ΔE-against-LRC numbers, and real
  GPU hardware timing. See Measured results.

## Decision rule (stated before measuring)

Render each of a handful of LRC-exported reference images (see Measured results for the exact
set) with calico and compare against the matching LRC export via CIEDE2000, at 1/4 resolution (so
the demosaic stand-in's edge differences don't dominate a comparison this ADR isn't trying to
settle). **"LRC-transparent"** means mean ΔE00 ≤ 2.0 and p95 ΔE00 ≤ 5.0 across every reference
image, for both the Adobe Standard/Color baseline and (if decodable — see below) Adobe Vivid.
Whichever working-space candidate (ProPhoto / Rec.2020 / ACEScg) scores lowest mean ΔE00 is
adopted; if none meets the bar, the gap and its likely source (demosaic stand-in, tone-curve
approximation, or something else) get written up as a Deferred follow-up rather than silently
accepted.

## Decision

**Profile source: parse the user's own installed Adobe DCP/XMP profiles at runtime, falling back
to LibRaw's built-in camera matrices when none is installed.** This keeps ADR-0003's "never
bundle" intact (nothing Adobe's is redistributed) while still hitting the actual target — matching
a profile the user already owns and uses. `docs/licensing.md`'s footnote `[^dcp1]` (a conservative,
not-primary-sourced inference that DCPs can't be redistributed) is amended below to record this
runtime-parse reasoning explicitly.

**New spike: `spikes/calico`** (a calico cat is tri-color, fitting for a color spike). Pure Rust,
no FFI, no git submodule — unlike `retina`, it needs no CI path-gating and runs in the workspace's
normal `clippy`/`test` jobs.

- **`dcp.rs`** — a from-scratch DNG-spec Camera Profile tag reader over a minimal hand-rolled
  TIFF/IFD parser (a DCP is TIFF-structured — no sub-IFDs, strips, or tiles, so a full TIFF
  library wasn't needed). Reads `ColorMatrix1/2`, `ForwardMatrix1/2`, `CalibrationIlluminant1/2`,
  `ProfileHueSatMapDims/Data1/Data2`, `ProfileLookTableDims/Data`, `ProfileToneCurve`,
  `BaselineExposureOffset`, `ProfileName`. Tested only against synthetic DCPs built byte-for-byte
  in test code — never a real Adobe file, per ADR-0003.
- **`xmp_profile.rs`** — Adobe Raw "Look" `.xmp` profile parsing (e.g. the user's installed Adobe
  Vivid preset). **This is the one piece of the plan that had to be timeboxed, not completed**:
  the embedded look-table's exact binary encoding inside these files is undocumented, and with no
  real sample file available in this sandbox (ADR-0003 forbids adding one, and reverse-engineering
  an undocumented format from zero real samples risks silently producing plausible-but-wrong
  colors — worse than refusing), this parser reads the RDF/XML container and attempts to decode an
  embedded base64 look table as a DCP-style IFD (plausible, since Adobe is known to reuse DNG tag
  semantics for these), but returns a clear, typed `UnrecognizedTableFormat` error rather than a
  guess when that doesn't parse. See Deferred below and the filed follow-up issue.
- **`cct.rs`** — DNG's dual-illuminant CCT-based matrix interpolation (`solve_camera_to_xyz`):
  iteratively estimates the shooting illuminant's correlated color temperature from the as-shot
  neutral, using McCamy's published 1992 cubic xy→CCT approximation as a documented stand-in for
  Adobe's own (undisclosed) solver, then blends `ColorMatrix`/`ForwardMatrix` by inverse-CCT
  weight per the DNG spec.
- **`matrix.rs`** — 3x3/vector math, Bradford chromatic adaptation (used to re-reference a
  ColorMatrix-only camera's XYZ output, and to adapt each working space's native white to D50).
- **`workspace.rs`** — three working-space candidates measured against each other (linear ProPhoto
  = ACR's own internal space, linear Rec.2020, ACEScg/AP1), each as an XYZ(D50)↔RGB matrix pair,
  plus sRGB for display output.
- **`huesatmap.rs`** — `HueSatMap`/`ProfileLookTableData`'s trilinear (hue, sat, val) lookup, with
  shortest-path angle interpolation on the hue-shift channel across the 360°/0° wraparound seam —
  the one place a naive linear interpolation gets visibly wrong (confirmed by a dedicated test:
  identical adjacent hue-shift entries of +179°/-179° interpolate to ~0° via the short arc, not a
  swing through 180°/0°).
- **`tonecurve.rs`** — `ProfileToneCurve`, a monotonic Fritsch-Carlson cubic Hermite spline through
  the profile's control points (Adobe's own spline construction is undisclosed; Fritsch-Carlson is
  a standard, published method guaranteeing no ringing between points). Falls back to a commonly
  reproduced "medium contrast" default curve when a profile has none — sourced from public
  raw-processing discussions, not an Adobe primary document, and documented as an approximation.
- **`pipeline.rs`** — the CPU reference, in this stage order: linearize (black/white-level scale)
  → white balance (as-shot `cam_mul`) → camera→XYZ(D50) (`cct.rs`'s illuminant-interpolated
  matrix) → working space → HueSatMap (in gamma-encoded linear-ProPhoto RGB, a documented 1/1.8
  power-curve approximation of ACR's own undisclosed encoding — the DNG spec is clear that
  HueSatMap/LookTable operate in *some* ProPhoto-referenced perceptual space, just not exactly
  which) → baseline exposure offset → LookTable (same HSV representation) → tone curve → sRGB.
- **`deltae.rs`** — CIEDE2000 (Sharma, Wu & Dalal 2005) plus sRGB→Lab conversion, tested against
  that paper's own published near-identical-color test pairs (the classic ΔE00≈1.0000 hue-wrap
  edge case several independent implementations get wrong).
- **`gpu.rs` + `shaders/color.wgsl`** — the GPU-feasibility half of this research: a wgpu compute
  kernel applying a single `HueSatMap` via a real 3D texture (`Rgba16Float`, `Repeat` addressing on
  the wrapping hue axis, `ClampToEdge` on sat/val, hardware trilinear filtering). Deliberately
  narrower than the full CPU pipeline — dual-illuminant map blending and the hue-wrap-aware angle
  interpolation stay CPU-only, since a hardware sampler's linear filtering doesn't do
  shortest-path interpolation across the hue seam. **A real, working CPU/GPU parity test
  (`tests/gpu_parity.rs`) runs against this sandbox's lavapipe software Vulkan adapter and passes**
  — getting it to pass required finding and fixing a real texel-center coordinate-mapping bug
  (hardware trilinear filtering centers texel `i` at normalized coordinate `(i+0.5)/N`, not `i/N`;
  the fix and its derivation are in `gpu.rs`'s and `shaders/color.wgsl`'s comments) and confirming,
  via a separate texel-exact `textureLoad` readback check, that the remaining ~0.05-0.06 max
  deviation is lavapipe's own lower-precision fixed-point filtering weights, not a real bug.

**Not adopted / considered and rejected:**

- **`dssim-core`** for the ΔE comparison — not needed; `deltae.rs`'s from-scratch CIEDE2000 is a
  small, well-specified formula, and (per `nicti-prowl`'s own precedent, ADR-0017/#17) an SSIM- or
  ΔE-style perceptual library isn't worth a new dependency for a formula this size. Also, unlike
  `nicti-prowl`, the comparison here needs Lab-space CIEDE2000 specifically (the ADR's own decision
  rule), not a generic image-similarity score.
- **Reproducing Adobe's exact tone-curve spline / HueSatMap perceptual encoding** — both are
  undisclosed; this ADR uses documented, standard stand-ins (Fritsch-Carlson monotonic spline;
  1/1.8 gamma encoding) and lets the ΔE measurement (not a claim of bit-exact reproduction) be the
  actual bar.

## Measured results

**Pending the user's reference-machine pass** (real DCP/XMP profile files, real LRC exports —
neither can exist in this sandbox, see Context). Not fabricated here.

- **Reference NEFs**: 6-10 Z8 files (mixed HE/HE*/Lossless, varied scenes including saturated
  colors and skin/fur tones) pulled from
  `H:\Photos\Furries\Socials\2025\2025-12-27 - RAWs`.
- **LRC exports**: 16-bit TIFF, sRGB, **two sets** — Adobe Standard (or Adobe Color) and Adobe
  Vivid — all develop sliders zeroed, As Shot white balance, no lens corrections/sharpening/NR/
  crop. Exported to `G:\Export\nicti`.
- **Procedure**: `retina dump-linear <nef> --out <dir>` for each reference NEF, then
  `calico render <tiff> <json> --dcp <path-to-installed-dcp> [--look <path-to-vivid-xmp>] --space
  <candidate> --out <png>` for each working-space candidate, then `calico compare <ours.png>
  <lrc-export.tiff> --heatmap <path>` against the matching LRC export.
- **To fill in**: mean/p95/max ΔE00 per working-space candidate, per profile (Standard/Color vs.
  Vivid, the latter only if `xmp_profile.rs` successfully decoded the installed Vivid `.xmp` —
  record whether it did), and the winning candidate per the decision rule above. Move this ADR to
  Accepted once filled in and the bar is met (or record why it wasn't, and what follow-up that
  implies).

## Consequences

**Unblocks #41** (RAW → linear → working-space pipeline on GPU): #41 can now build directly on
`pipeline.rs`'s stage order and working-space choice instead of starting from zero, once the
reference-machine pass picks a working space.

**Feeds #46** (global develop adjustments): the WB temp/tint sliders map onto `cct.rs`'s
AsShotNeutral/CCT machinery; #46's own auto-tone research (#99) is unaffected — this ADR fixes
color pipeline stages only, not tone/exposure adjustment UI.

**A real risk this pass could not resolve, flagged rather than guessed past:** `dcp.rs`'s
`HueSatMap`/`ProfileLookTableData` table parsing assumes a specific entry-nesting order (hue
slowest-varying, matching `HueSatMap::index`'s layout) recalled from memory, not cross-checked
against a real DCP file — none can exist in this sandbox. If the real nesting is reversed, every
table entry silently lands at the wrong (hue, sat, val) grid point, with no parse error to catch
it (the entry *count* would still match). **This must be verified during the reference-machine
pass** before trusting any ΔE number that involves a profile with a real HueSatMap/LookTable —
e.g. by checking the rendered result's behavior at a hue/saturation combination the profile is
known to visibly affect. See `dcp.rs`'s `hue_sat_map` closure for the full note.

**Deferred, each as its own follow-up issue:**

- **Reference-machine ΔE measurement run** (this ADR's own Measured results, above) — filed as a
  GitHub issue, same pattern as #90/#97's reference-machine follow-ups, Part of #7.
- **Adobe Raw `.xmp` "Look" profile decode** (`xmp_profile.rs`'s `UnrecognizedTableFormat` path) —
  filed as its own research issue if the reference-machine pass confirms the installed Adobe Vivid
  `.xmp` doesn't parse as a DCP-style IFD; needs a real sample file and a way to validate against
  it, neither available in this sandbox.
- **Per-pixel black-level shading** (`retina`'s `cblack` pattern map beyond the four per-channel
  scalars) — out of scope for this pass, noted in `shim.h`.
- **Real GPU hardware timing** for the 3D-texture HueSatMap kernel — this ADR only establishes
  correctness (the CPU/GPU parity test), not throughput; a real-hardware pass is #44/Tapetum's
  concern once the render graph exists to measure end-to-end.
- **Demosaic quality** — `retina dump-linear` uses LibRaw's demosaic as a stand-in for this
  hand-off only; the real demosaic algorithm choice belongs to #40, and could shift the ΔE numbers
  once decided.
- **ADR-0017's Picture-Control-vs-Nicti-default mismatch** (camera JPEG vs. Adobe-profile
  rendering for T0-T2 preview tiers) stays exactly as unsolved as ADR-0017 left it — not this
  ADR's problem to fix, restated here only so it isn't mistaken for a regression this ADR
  introduced.
