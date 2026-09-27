# ADR-0038: Color pipeline

- **Status:** Proposed — pipeline design, DCP parser, and GPU-feasibility spike are done and
  measured against synthetic data; the actual LRC-match numbers are pending a reference-machine
  run (see Measured results)
- **Date:** 2026-09-26
- **Ticket:** [#38](https://github.com/jordanfelle/nicti/issues/38) Research: color pipeline
- **Formerly:** ADR-0021 (sequential numbering, pre-#183)

## Context

Issue #38 sits at the head of Nicti's critical path: it blocks #41 (RAW → linear → working-space
pipeline on GPU), which blocks #44 (Tapetum, the stage-cached render graph), which in turn gates
roughly a dozen develop/build tickets. The question this ADR answers: how does Nicti get from
decoded camera RGB to a display-referred image that matches Lightroom Classic closely enough that
migrating existing edits doesn't mean re-editing from scratch? The user shoots Nikon Z8/D7500/D3400
bodies and mostly uses LRC's **Adobe Vivid** camera profile, so matching that specific profile is
the real target, not just a generic "close to Adobe Standard" approximation.

Constraints already fixed elsewhere:

- **ADR-0018** (`docs/adr/0018-third-party-license-policy.md`) and `docs/licensing.md` rule out
  ever bundling a real Adobe `.dcp`/`.xmp` camera profile in this repo — no redistribution grant
  was found. This ADR's parser reads profiles already installed on the user's own machine (from
  their existing Adobe Camera Raw / Lightroom Classic install), at runtime; nothing is vendored.
- **ADR-0016** picked `wgpu` for GPU compute, with `RGBA16F` intermediates and no full-frame
  host↔device round-trips as design goals. `spikes/glint`'s own kernels are storage-buffer-only
  (see its `gpu.rs` module doc) — no 3D-texture pattern existed anywhere in this repo before this
  ADR, and DNG's `HueSatMap`/`ProfileLookTableData` tables are exactly the kind of 3D LUT a texture
  unit's hardware trilinear filtering is built for, so that gap needed closing here.
- **ADR-0029** already flags a real, unsolved gap this ADR doesn't fix: the difference between a
  camera's own Picture Control JPEG rendering and Nicti's Adobe-profile-based rendering for the
  camera-JPEG-derived preview tiers (T0-T2). That mismatch is inherent to using a different
  rendering intent than the camera, not something a color-pipeline bug — noted again here so it
  isn't mistaken for one.
- **#40** (demosaic) and **#42** (ICC/soft-proofing) are explicitly out of scope — this ADR treats
  the demosaiced linear image as its input and produces a working-space linear image as its output.
  Stage *order* within the render graph is #44's decision, not this one's; this ADR only fixes the
  internal order of the color-specific sub-stages relative to each other.
- **Sandbox note**, matching ADR-0068/0050/0071's own precedent: there is no LRC install, no real
  Adobe DCP/XMP profile, and no LRC-rendered reference image anywhere in this sandbox (confirmed —
  `docs/ref-10k-manifest.csv` lists NEFs only, no JPEG/TIFF references, and `docs/licensing.md`
  forbids adding one). Every piece of this ADR that doesn't need a real profile or a real ΔE
  number against LRC is real, tested, measured evidence: 26 unit tests plus 3 integration tests
  (a full synthetic render-pipeline sanity check and a real wgpu compute-shader run against
  lavapipe, this sandbox's software Vulkan fallback — see ADR-0016's own hardware-identity caveat).
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
to LibRaw's built-in camera matrices when none is installed.** This keeps ADR-0018's "never
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
  in test code — never a real Adobe file, per ADR-0018.
- **`xmp_profile.rs`** — Adobe Raw "Look" `.xmp` profile parsing (e.g. the user's installed Adobe
  Vivid preset). **Resolved in #150**, once the user's real installed profiles (reachable from this
  WSL sandbox via the Windows side, `/mnt/c/...`) turned out to make the sample-file blocker moot.
  `crs:LookTable` is the table's own MD5 fingerprint, not the data; the actual payload is a second
  attribute, `crs:Table_<id>`, encoded in the DNG SDK's `dng_big_table` wire format (a Z85-like
  base85 variant + zlib, read as a spec reference only, never vendored) wrapping
  `dng_look_table::GetStream`'s tagged record. The parser recomputes the same canonical
  re-serialization the SDK hashes for `crs:LookTable` and compares it against the ID in the file —
  a wrong decode can't produce the right hash, the strongest correctness proof available without an
  LRC render. Verified against all six real Adobe Raw profiles (Color/Landscape/Monochrome/
  Neutral/Portrait/Vivid): all six decode and fingerprint-match. `Clarity2012`/`ToneCurvePV2012`/
  `RGBTable` look settings calico doesn't apply are surfaced via
  `LookProfile::unsupported_settings` rather than silently dropped.
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
- **`pipeline.rs`** — the CPU reference, in this stage order: linearize (LibRaw's own
  black-subtracted, 16-bit-scaled output, per `linear_input.rs`) → white balance (as-shot
  `cam_mul`, only for the ForwardMatrix branch below) → camera→XYZ(D50) (`cct.rs`'s
  illuminant-interpolated matrix — the white-point *search* always uses the ColorMatrix inverse,
  regardless of ForwardMatrix availability, per the DNG spec; the *final* matrix uses ForwardMatrix
  when both illuminants have one, which then expects white-balanced input, or the ColorMatrix
  fallback, which expects raw un-white-balanced input since its Bradford adaptation already
  corrects the illuminant) → ProPhoto intermediate → HueSatMap (hue/sat from unencoded linear
  ProPhoto RGB, per Adobe's own reference implementation -- only the *value* coordinate is run
  through `ProfileHueSatMapEncoding`'s curve, linear when absent/0, sRGB-encoded when 1; there is
  no "gamma 1.8" encoding, and no per-channel R/G/B encoding, in the DNG spec) → baseline exposure
  offset → LookTable (same hue/sat-unencoded, value-only-encoded treatment, per its own
  `ProfileLookTableEncoding`) → selected working space → tone curve → sRGB.
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
  — getting it to pass required finding and fixing two real bugs: a texel-center
  coordinate-mapping bug (hardware trilinear filtering centers texel `i` at normalized coordinate
  `(i+0.5)/N`, not `i/N`; the fix and its derivation are in `gpu.rs`'s and `shaders/color.wgsl`'s
  comments), and a CPU-only bug in `huesatmap.rs`'s trilinear interpolation (the saturation- and
  value-axis fractions were swapped) that this test's own smoothly-varying synthetic data mostly
  masked — a separate texel-exact `textureLoad` readback check confirmed the texture *upload*
  itself was always correct, but an earlier pass here wrongly attributed the resulting ~0.05-0.06
  max deviation to lavapipe's filtering precision rather than to that swap. Fixed, the tolerance
  is `5e-3` and the real measured deviation is `~1.5e-4`.

**Not adopted / considered and rejected:**

- **`dssim-core`** for the ΔE comparison — not needed; `deltae.rs`'s from-scratch CIEDE2000 is a
  small, well-specified formula, and (per `nicti-prowl`'s own precedent, ADR-0029/#17) an SSIM- or
  ΔE-style perceptual library isn't worth a new dependency for a formula this size. Also, unlike
  `nicti-prowl`, the comparison here needs Lab-space CIEDE2000 specifically (the ADR's own decision
  rule), not a generic image-similarity score.
- **Reproducing Adobe's exact tone-curve spline** — undisclosed; this ADR uses a documented,
  standard stand-in (Fritsch-Carlson monotonic spline) and lets the ΔE measurement (not a claim of
  bit-exact reproduction) be the actual bar. (The HueSatMap/LookTable representation itself is
  *not* a stand-in — `ProfileHueSatMapEncoding`/`ProfileLookTableEncoding` are real DNG-spec tags,
  parsed directly; an earlier draft of this ADR incorrectly described a "1/1.8 gamma" guess here,
  caught during review before this ADR's first merge.)

## Measured results

**Pending the user's reference-machine pass** (real DCP/XMP profile files, real LRC exports —
neither can exist in this sandbox, see Context). Not fabricated here.

- **Reference NEFs**: 6-10 Z8 files (mixed HE/HE*/Lossless, varied scenes including saturated
  colors and skin/fur tones) pulled from the user's own source NEF folder (local path, not
  committed here — see `docs/research/calico-color-pipeline.md`).
- **LRC exports**: 16-bit TIFF, sRGB, **two sets** — Adobe Standard (or Adobe Color) and Adobe
  Vivid — all develop sliders zeroed, As Shot white balance, no lens corrections/sharpening/NR/
  crop. Exported to the user's local export folder (not committed here).
- **Procedure**: `retina dump-linear <nef> --out <dir>` for each reference NEF, then
  `calico render <tiff> <json> --dcp <path-to-installed-dcp> [--look <path-to-vivid-xmp>] --space
  <candidate> --out <png>` for each working-space candidate, then `calico compare <ours.png>
  <lrc-export.tiff> --heatmap <path>` against the matching LRC export.
- **To fill in**: mean/p95/max ΔE00 per working-space candidate, per profile (Standard/Color and
  Vivid both in scope now that #150 resolved `xmp_profile.rs`'s decode), and the winning candidate
  per the decision rule above. Move this ADR to Accepted once filled in and the bar is met (or
  record why it wasn't, and what follow-up that implies).

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

- **Reference-machine ΔE measurement run** (this ADR's own Measured results, above) — filed as
  [#149](https://github.com/jordanfelle/nicti/issues/149), same pattern as #90/#97's
  reference-machine follow-ups, Part of #7.
- ~~**Adobe Raw `.xmp` "Look" profile decode**~~ — resolved in
  [#150](https://github.com/jordanfelle/nicti/issues/150); see the `xmp_profile.rs` bullet above.
  Remaining gap: `crs:RGBTable`-based looks use a separate `dng_rgb_table` container this parser
  doesn't decode (none of the six real Adobe Raw profiles checked use it, so it's untested either
  way) — a future issue if a look profile using it ever needs support.
- **Per-pixel black-level shading** (`retina`'s `cblack` pattern map beyond the four per-channel
  scalars) — out of scope for this pass, noted in `shim.h`.
- **Real GPU hardware timing** for the 3D-texture HueSatMap kernel — this ADR only establishes
  correctness (the CPU/GPU parity test), not throughput; a real-hardware pass is #44/Tapetum's
  concern once the render graph exists to measure end-to-end.
- **Demosaic quality** — `retina dump-linear` uses LibRaw's demosaic as a stand-in for this
  hand-off only; the real demosaic algorithm choice belongs to #40, and could shift the ΔE numbers
  once decided.
- **ADR-0029's Picture-Control-vs-Nicti-default mismatch** (camera JPEG vs. Adobe-profile
  rendering for T0-T2 preview tiers) stays exactly as unsolved as ADR-0029 left it — not this
  ADR's problem to fix, restated here only so it isn't mistaken for a regression this ADR
  introduced.
