# ADR-0042: Color management (display, output spaces, soft-proofing)

- **Status:** Accepted -- the mechanism is built and tested; a real wide-gamut monitor check is
  the only outstanding item (see Consequences)
- **Date:** 2026-09-29
- **Ticket:** [#42](https://github.com/jordanfelle/nicti/issues/42) Build: color management

## Context

Before #42 nothing in production code managed color. Every frame ends as linear ProPhoto (D50)
(ADR-0038, ADR-0044); the screen got a hardcoded ProPhoto -> sRGB matrix plus the sRGB curve
(`nicti-pelt`'s `display.wgsl`), and `nicti-calico`'s `ColorProfile` was an empty extension point.
ADR-0056 defers P3/AdobeRGB export and ICC embedding of anything but sRGB to this ticket.

## Decision

**Library: `moxcms`** (pure Rust, `BSD-3-Clause OR Apache-2.0`, already vetted in
`docs/licensing.md` and already used by `spikes/prey`). Little CMS 2 was the pre-approved
alternative (`docs/licensing.md`) but needs a C toolchain in CI for no capability we use.

**Output spaces** (`nicti_calico::space::OutputSpace`): sRGB, Display P3, Adobe RGB (1998).
Matrices are built from published primaries with each space's white Bradford-adapted to D50 once
(the `spikes/calico` convention); transfer functions are the sRGB curve (sRGB, P3) and the pure
563/256 gamma (Adobe RGB). ICC profiles are generated at runtime with `moxcms` (ADR-0056 rejected
vendoring `.icc` files), so the profile an export embeds is the one whose colorants the tests
check against our own matrices.

**Display-only, outside the render graph.** The display transform is applied in
`nicti-pelt`'s display pass, after Tapetum's frame. Nothing about it enters a stage's cache key,
so moving the window to another monitor or toggling soft-proofing re-uploads a texture and
re-runs zero bake or live-suffix work.

**Two independent per-pixel stages** (`nicti_calico::transform::DisplayTransform`, evaluated by
the display shader, with a CPU reference `apply` the tests measure the GPU against):

1. **Proof** (optional): working -> proof-space linear RGB -> clamp to [0, 1] -> back to working.
   For the built-in spaces this is exactly what a matrix/TRC profile does under relative
   colorimetric, so it is plain 3x3 math (`OutputSpace::from_working`/`to_working`) and the
   out-of-gamut flag is exact: a linear channel more than 0.002 outside [0, 1] (`GAMUT_EPS`).
   The gamut warning tints flagged pixels a fixed color. **There is no rendering-intent choice**:
   for matrix profiles Perceptual and Relative colorimetric produce byte-identical results
   (verified in review), so the UI does not pretend otherwise. Intents matter only for LUT-based
   proof profiles (CMYK/printer), which are out of scope for v1.
2. **Display**, three kinds, cheapest-and-most-exact first:
   - `DisplayKind::Space` -- exact matrix + built-in transfer function -- for the default, the
     sRGB fallback, and any monitor ICC profile indistinguishable from a built-in space
     (probe-grid comparison, `< 0.004`; Windows' stock sRGB profile is the usual case).
   - `DisplayKind::MatrixTrc` -- any other **matrix/TRC** monitor profile (a P3 panel with a
     gamma-2.2 curve, a custom-calibrated wide-gamut display -- nearly every real monitor):
     the profile's colorants give a 3x3 from the working space, then a per-channel encode table
     built by numerically inverting the profile's TRC (`curv` gamma/table or `para`), indexed by
     `sqrt(linear)` so near-black is sampled densely. Measured against a direct `moxcms`
     transform over a 21^3 grid of the whole working-space cube (in- and out-of-gamut colors
     alike): worst 0.8/255 for a P3 gamma-2.2 monitor, 0.2/255 for BT.2020.
   - `DisplayKind::Lut` -- only a **LUT-based** monitor profile (no matrix/TRC): a 33^3 3D LUT
     baked by `moxcms`, indexed by the working space under a gamma-1.8 shaper (ProPhoto's own
     exponent, so shaped values are exactly `ColorProfile::new_pro_photo_rgb()`'s encoded
     input), `Rgba16Float`. Rare in practice.

*Why not one LUT for everything?* The first implementation baked proofing into the LUT with a
per-node gamut flag. Adversarial review measured it: `moxcms` clamps f32 output to [0, 1], the
sRGB gamut boundary cuts diagonally through the ProPhoto-indexed cube, and trilinear interpolation
across the clipped nodes put up to ~9-12/255 of error into plainly in-gamut colors (a teal 10%
inside sRGB came out 12/255 off) -- growing the LUT to 65^3 did not fix it -- and the flag was
quantised to a cell-wide false-positive band. Analytic proofing removes all three problems. A
second review pass found the same failure for a **P3 gamma-2.2 monitor** through the LUT (14-20/255
worst case) -- the monitor case this feature exists for -- which is why `MatrixTrc` exists and why
the LUT is now the last resort.

**Monitor profile** (`nicti_calico::display_profile`): on Windows, `MonitorFromWindow` ->
`GetMonitorInfoW` -> `CreateDCW` -> `GetICMProfileW` -> read the file -> `moxcms`. `nicti-pelt`
re-resolves whenever the window's `HMONITOR` changes, and from a "Re-read display profile" menu
item. Any failure (non-Windows, no profile assigned, unreadable or unparseable file) degrades to
sRGB and surfaces the reason in the Color menu (ADR-0101).

**Export** consumes `OutputSpace::from_working`/`encode` and `icc::profile_bytes`; wiring them
into the encoders is #57. TIFF ICC embedding stays deferred with ADR-0056.

## Consequences

- **The 3D LUT is only for LUT-based monitor profiles**, where colors near the monitor's gamut
  boundary carry interpolation error from clipped nodes and near-black is lifted slightly by the
  33-node shaper. Matrix/TRC profiles never touch it.
- **Not bit-identical to before.** The default sRGB path now uses calico's primaries-derived
  ProPhoto->sRGB matrix, which differs from `nicti_tapetum::color::prophoto_to_srgb_linear_matrix`
  by ~3e-4, moving ~1.6% of pixels by one 8-bit code versus `geometry::output_encode`'s CPU
  reference. Pinned by a test at 5e-4; making calico the single source is a follow-up.
- **GPU parity tests skip without a `wgpu` adapter** (same convention as Tapetum's); they ran
  here on lavapipe, and CI runners without one silently skip them.
- **No black-point compensation.** `moxcms` 0.9's `TransformOptions` has no BPC (the field is
  commented out upstream); the monitor LUT is relative colorimetric without it.
- **LUT mode clamps in working space** before the shaper, whereas the exact path clamps after the
  matrix in display space. Tapetum's output is display-referred in [0, 1] in practice; values
  outside it would clip differently between the two paths.
- **Unmanaged surfaces.** The T0/T2 embedded-JPEG previews and grid thumbnails go straight to
  egui's sRGB textures and ignore both the monitor profile and any embedded JPEG profile. The
  Develop and Loupe *rendered* frames are managed.
- **Profile-derived caching.** The monitor-dependent half of the transform (probe + possible
  bake, 6-90 ms) is cached and rebuilt only when the resolved profile changes; toggling proofing
  or the gamut warning only swaps the proof half.
- **Working space unchanged.** Still linear ProPhoto (ADR-0038 leaves the pick to #149's
  reference-machine run). `OutputSpace::from_working` is the only place that assumes it.
- **Windows-only paths are type-checked, not run.** `display_profile.rs` compiles for
  `x86_64-pc-windows-gnu` but has only been exercised through its non-Windows fallback; a real
  wide-gamut display check (profile pick-up, moving between monitors, the gamut warning on real
  saturated photos) is still outstanding. A profile changed in Windows settings without moving
  monitors is only picked up via the "Re-read display profile" menu item.
