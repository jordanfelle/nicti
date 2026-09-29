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

**Two shapes** (`nicti_calico::transform::DisplayTransform`):

- `Direct(space)` -- the monitor is a built-in space and there is no proof. The shader applies an
  exact matrix plus transfer function. This is the default and the sRGB fallback.
- `Lut(Lut3d)` -- a real monitor ICC profile and/or soft-proofing. A 33^3 3D LUT indexed by the
  working space encoded with a gamma-1.8 shaper (ProPhoto's own exponent, so the shaped values
  are exactly `moxcms::ColorProfile::new_pro_photo_rgb()`'s encoded input), holding display-encoded
  RGB in `rgb` and an out-of-proof-gamut flag in `a`. Uploaded as `Rgba16Float` (filterable
  everywhere; `Rgba32Float` needs an optional feature).

**Soft-proofing** is working -> proof space (chosen intent) -> clamp to [0, 1] -> monitor
(relative colorimetric). The gamut flag is computed from the proof space's own matrix (exact for
the built-in spaces): a color is out of gamut if any linear channel leaves [0, 1] by more than
0.002. The gamut warning tints flagged pixels a fixed color in the shader. Only the built-in
spaces are offered as proof targets in v1; a CMYK/printer ICC proof would need a ΔE-based flag.

**Monitor profile** (`nicti_calico::display_profile`): on Windows, `MonitorFromWindow` ->
`GetMonitorInfoW` -> `CreateDCW` -> `GetICMProfileW` -> read the file -> `moxcms`. `nicti-pelt`
re-resolves whenever the window's `HMONITOR` changes, and from a "Re-read display profile" menu
item. Any failure (non-Windows, no profile assigned, unreadable or unparseable file) degrades to
sRGB and surfaces the reason in the Color menu (ADR-0101).

**Export** consumes `OutputSpace::from_working`/`encode` and `icc::profile_bytes`; wiring them
into the encoders is #57. TIFF ICC embedding stays deferred with ADR-0056.

## Consequences

- **No black-point compensation.** `moxcms` 0.9's `TransformOptions` has no BPC (the field is
  commented out upstream). Relative colorimetric without BPC is what the LUT builder does;
  revisit if a real monitor profile shows crushed blacks.
- **Unmanaged surfaces.** The T0/T2 embedded-JPEG previews and grid thumbnails go straight to
  egui's sRGB textures and ignore both the monitor profile and any embedded JPEG profile. The
  Develop and Loupe *rendered* frames are managed.
- **LUT accuracy.** A 33^3 LUT agrees with the exact sRGB path to about 1.5% worst case, at
  saturated colors near a gamut edge where a node channel clips (`transform::tests::
  lut_for_srgb_display_matches_the_direct_path`). The common case (sRGB monitor, no proof) never
  uses the LUT.
- **Working space unchanged.** Still linear ProPhoto (ADR-0038 leaves the pick to #149's
  reference-machine run). `OutputSpace::from_working` is the only place that assumes it.
- **Needs a physical check** on a real wide-gamut display before this is called verified:
  profile pick-up, a monitor move, and the gamut warning on real saturated photos.
