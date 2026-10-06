# ADR-0380: Global Presence (texture, clarity, dehaze, saturation) and post-crop Effects (vignette, grain)

**Status:** Accepted
**Ticket:** #380
**Part of:** #11
**Related:** #46, #49, #62, #352, #47, #355

## Context

ADR-0062's LRC import left `Texture`/`Clarity2012`/`Dehaze`/`Saturation`, grain and the post-crop
vignette untranslated (verbatim in `lrc_provenance`) because nicti had no *global* stage for them.
Only the per-mask `LocalAdjust` fields existed (ADR-0049), and its spatial ones (clarity, texture,
dehaze) read bases that were built only when a mask needed them. #352 asked for the same three as
global sliders; grain and the vignette had no code anywhere.

## Decision

### 1. One `nicti.presence` Live stage, *summed with* the local deltas

`PresenceParams { texture, clarity, dehaze, saturation }`, all -1..1 and a no-op at 0 (Vibrance stays
its own stage). In `live_suffix.wgsl` each global value is **added to the stacked local delta of the
same name** at the point ADR-0049 already applies that local one (dehaze after exposure/Look,
clarity/texture after the tone curve, saturation after HSL), reusing `apply_dehaze`/`apply_bands`/
`saturate_chroma` unchanged. So a global +0.3 and a local +0.2 act as +0.5 there, and an unmasked
photo with a zero presence still runs the exact earlier path (every step is skipped at exactly
zero). The CPU twin is `mask/local.rs::live_pixel`, which takes the same `PresenceParams`.

### 2. The bases are built for a global-only edit, from the same cache

`MaskEngine::prepare` takes the document's `PresenceParams` (`MaskInputs.presence`). It returns
`None` only when no correction is active **and** the presence needs no spatial base
(`PresenceParams::needs_bases`); otherwise it builds the clarity/texture bands and/or dehaze
transmission+airlight (same per-(guide, extent) caches, same ADR-0049 "a slider never rebuilds a
base" rule) and, with no correction, hands back a `MaskFrame` with a 1x1 placeholder atlas and zero
corrections. The shader gates base reads on the header's bands/haze flags, no longer on "a mask is
active". Saturation is per-pixel and needs no base.

Every caller that renders a photo does the bake-then-prepare step when masks are active **or** the
presence needs bases: Develop (`DevelopView::render`), export, and rendered previews (#145). A preview
passes an *empty* mask set, so it shows global clarity/dehaze while local corrections stay out and it
stays marked partial (#399).

### 3. `nicti.effects`: a Geometry node evaluated in crop-normalized coordinates

`EffectsParams` (post-crop vignette: amount/midpoint/feather/roundness/highlights + style
Highlight Priority / Color Priority / Paint Overlay; grain: amount/size/roughness/seed) is a second
Geometry node (`spine::GEOMETRY_IDS = [CROP, EFFECTS]`, one fused dispatch). It runs inside
`present_sample.wgsl`, after the bilinear sample, so editing it re-runs only that pass (never a bake
or the live suffix) -- pinned by a Develop test on `RenderStats`.

The vignette is *post-crop*, so it must follow the crop; the grain must be the same pattern in a
preview, a full-size export and every export tile. Both are therefore functions of **crop-normalized
position**, not of the output pixel: the kernel is given the affine from a source coordinate to
`(u, v)` in 0..1 across the crop rect (`effects::crop_norm`: the inverse of the crop transform over
the crop size). The *untiled, unscaled* crop transform is used on purpose; a tile's offset transform
and a screen-size preview's scaled one only change which source position a pixel samples, and the
effects read that position. `RenderInputs::bind_effects` sets them once per photo (not per tile) and
clears the previous photo's (the kernel is reused).

Grain is value noise on a lattice laid across the crop's long edge (300..1500 cells by size), hashed
with PCG so the CPU reference and WGSL agree to the last bit of the integer part, two octaves mixed by
roughness, applied as a midtone-weighted brightness ratio in cube-root luma (chroma preserved,
ratio clamped). The vignette is a superellipse distance (roundness blends oval -> circle in pixel
space, or raises the exponent toward a square), `smoothstep(midpoint, feather)`, applied as
exposure with highlight protection (Highlight Priority), exposure plus a re-saturation (Color
Priority), or a blend toward black/white (Paint Overlay). Vignette then grain.

**These formulas are this repo's approximation of LRC's Effects panel.** There was no LRC to compare
against, only its parameter ranges/defaults, so nothing claims a pixel match; the import carries LRC's
slider values across faithfully and a visual comparison pass is a follow-up.

### 4. LRC import

`basic.rs` maps `Texture`/`Clarity2012`/`Dehaze`/`Saturation` (-100..100 -> -1..1, the same scale
as `Vibrance`; the `Local*` equivalents are already -1..1, so the sum is consistent). `effects.rs`
maps `PostCropVignette{Amount,Midpoint,Feather,Roundness,Style,HighlightContrast}` and
`Grain{Amount,Size,Frequency,Seed}`, gated by `EnableEffects`. **A slider whose amount is zero is
ignored**: LRC writes a random per-image `GrainSeed` and the shape sliders at their defaults into
every image, so taking them would make every untouched photo import as edited. `OverrideLookVignette`
(a vignette baked into a Look profile) is not modelled and stays in the untranslated list when real.

## Consequences

- Both stages are copied/pasted/synced/presets as ordinary stages (`knead::GROUPS`, on by default)
  and reset by "Reset all".
- A document with a huge global dehaze costs a transmission build per photo even with no masks; it is
  cached per baked frame like the local one. An export bakes first for it (the baked cache has a zero
  budget there), once.
- `present_sample.wgsl` is still unaudited on the real RTX 5080 under Dx12 (#355); the new code adds
  integer hashing there, so the reference-machine pass for #355 should include it
  (`NICTI_WGPU_BACKEND=vulkan|dx12`).
- Not done: Look-profile vignettes (`OverrideLookVignette`), LRC-pixel comparison of the effects, the
  Develop canvas not resizing to the crop (#272: the effects follow the crop region the preview
  samples, so they cover the crop-sized area of an oversized canvas, not the whole canvas).
