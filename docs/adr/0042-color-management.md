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

## Camera profiles (DCP) in the live suffix

The other half of #42 (ADR-0038's stage order). `nicti-calico` gained `dcp.rs`/`cct.rs`/
`huesatmap.rs` (promoted from `spikes/calico`) and `profile.rs`: `DcpProfile::solve(wb_gains)`
returns a `ProfileSolution` -- one folded camera -> linear ProPhoto matrix (the ForwardMatrix
branch folds the white-balance gains in as a diagonal, the ColorMatrix branch's Bradford step does
the balancing itself), the illuminant-blended HueSatMap, the LookTable, and `2^BaselineExposure`.
Blending the two illuminants' HueSatMaps is done on the CPU into one table per render (trilinear
sampling is linear in the table, so this equals blending the samples), and the kernel re-uploads a
table only when its content fingerprint changes. `apply_cpu` is the reference the GPU is tested
against.

`live_suffix.wgsl` applies, after the camera matrix: HueSatMap -> baseline exposure -> LookTable,
then the existing user-exposure/tone/... chain. Hue and saturation are taken from the unencoded
linear RGB; only the value coordinate goes through the table's encoding (Adobe's reference
implementation), unclamped above 1. Bindings 3/4/5 (two 3D tables, one sampler) are always bound
-- 1x1x1 dummies when absent -- because the auto-derived bind-group layout includes any binding the
shader references. The hardware lerps the hue shift linearly rather than along the shortest arc; the
CPU reference (`apply_cpu`) takes the shortest arc, so the two differ wherever adjacent table
entries' shifts differ by more than 180 degrees (most Adobe "Camera *" tables have ~350-degree
adjacent differences). Measured on seven real profiles this changed 0 of 4096 sampled pixels by more
than 0.015, so it is accepted; the DNG SDK's own reference blends shifts linearly, so the GPU is the
one matching Adobe.

**User Exposure and baseline exposure are one stage** (Adobe's `dng_render`), applied *before* the
LookTable: `rgb *= profile.baseline * user_exposure`, then the LookTable. With no profile this is
just the user exposure, as before. The ColorMatrix (no-ForwardMatrix) branch is normalized so a
neutral lands at the same brightness as the ForwardMatrix branch (unnormalized it drifted ~0.35 EV
between illuminants A and D65 on a real profile), and a ForwardMatrix present for only one
illuminant is used as-is rather than discarded.

**Selection and the cache key.** The choice lives on the `nicti.working_space` stage as
`coat::CameraProfileParams { name, path, content_hash }` (blake3 of the file), so it flows into
that node's hash and therefore the live-output cache key; no profile serializes to the stage's
historical `{}`, leaving existing documents' hashes unchanged. `nicti-pelt` discovers the user's
own Adobe profiles at runtime (`camera_profiles.rs`; never bundled, ADR-0018), matches them to the
frame by `<MAKE> <MODEL>` file-name prefix (the rest of the name must begin `Adobe ` or `Camera `,
so `Z 6` doesn't claim the Z 6II's `Z 6 2 ...` files; LibRaw's `Z 6_2` and multi-word makes like
`OM Digital Solutions` are normalized) and re-checks the file's `UniqueCameraModel`, and offers
them in the Develop panel's Basic section. **Default is "Matrix only"** -- silently changing every
photo's look on load is a decision for the reference-machine ΔE pass (#149), not this PR.

**Hostile `.dcp` files** are bounded: only the 17 tags the parser reads are decoded, total decoded
bytes are budgeted against the file size (entries may legally overlap, so an unbudgeted parse
multiplied memory ~1 GB per MB), table axes are capped at 256 (3D texture limits are 2048 on common
adapters), a singular ColorMatrix is refused at parse time, and the CCT solver degrades rather than
panics on a matrix pair that is singular only when blended.

**Not done here:** `ProfileToneCurve`, `DefaultBlackRender` (most Adobe "Camera *" profiles carry
one or both, so those profiles render close to, but not exactly, Adobe's look) and the Look `.xmp` profiles (`xmp_profile.rs` stays in the
spike), per-photo persistence of the choice (edits are not yet catalog-persisted), and the working
space pick (#149).

## JPEG-sourced previews (#319)

The grid thumbnails, the loupe's T0/T2/rendered-tier fallback and the survey/compare tiles are
display-referred 8-bit JPEGs that never enter the render graph, so `DisplayTransform` (linear
ProPhoto -> display) can't take them. On a wide-gamut monitor they disagreed with the managed
rendered frame.

- **`nicti_calico::source_transform::SourceTransforms`** converts 8-bit RGBA from a source profile
  to the display profile on the CPU (`moxcms` 8-bit transform, relative colorimetric, same options
  as the display probe). The source is the JPEG's embedded ICC profile; no profile, an unparseable
  one, a panicking parse or a non-RGB one means sRGB. One transform is built per distinct source
  profile and cached (bounded). Source and display being the same space (judged with the same
  `equivalent_space` probe the display path uses) is a no-op, so the common case -- untagged
  JPEGs on an sRGB monitor -- costs nothing and is byte-identical to before.
- **Soft-proofing is not applied** to previews: they show the photo, proofing stays on the
  rendered frame.
- **Where it runs.** Thumbnails convert on the Pounce worker *after* the 256 px downsize
  (`grid/jobs.rs::make_thumbnail`); the loupe and tiles convert in `cull/previews.rs::
  preview_texture` on the UI thread, once per photo (the texture is cached).
- **Invalidation.** `ColorManagement` bumps a `generation` whenever the display profile is
  re-resolved and rebuilds its `SourceTransforms`. `PeltApp::sync_preview_color` hands both to
  `GridSession::set_color` (drops thumbnail textures and decoded-but-unuploaded results, cancels
  in-flight batches; snapshot, failure sets and edited flags are kept), `TilePreviews::set_color`
  and the loupe's cached texture.
- **T2 keeps the source profile.** `t2.rs::resize_and_encode` used to drop the embedded ICC when
  it re-encoded, so a Display P3 original would have been read back as sRGB. It now carries an RGB
  profile over to the T2 JPEG. T2s stored before #319 have no profile and are treated as sRGB
  (the pre-#319 behaviour); they refresh when their cache entry is regenerated.
- **Measured** (release, `cargo test -p nicti-calico --release -- --ignored --nocapture
  convert_cost`): converting a 256x170 thumbnail costs ~0.05 ms (ADR-0029's per-thumbnail decode is
  ~2-3 ms, so the 1M-asset grid budget is unaffected; untagged-on-sRGB is free); a 3840x2560 T2
  costs ~11-13 ms when it does convert, on top of its JPEG decode, still under one 16 ms frame.
- **Not verified here:** the issue's "done when" -- a Display P3 JPEG looking the same in the grid,
  the loupe fallback and the rendered frame on a real P3 monitor -- needs physical testing.

## Consequences

- **The 3D LUT is only for LUT-based monitor profiles**, where colors near the monitor's gamut
  boundary carry interpolation error from clipped nodes and near-black is lifted slightly by the
  33-node shaper. Matrix/TRC profiles never touch it.
- **Single matrix source (#318).** The default sRGB path uses calico's primaries-derived
  ProPhoto->sRGB matrix. Tapetum's former published-constant copy differed by ~3e-4 (~1.6% of
  pixels off by one 8-bit code versus the display); #318 deleted it and `geometry::output_encode`
  now uses calico's, so display, export and CPU readback agree. Pinned by a test at 1e-6.
- **GPU parity tests skip without a `wgpu` adapter** (same convention as Tapetum's); they ran
  here on lavapipe, and CI runners without one silently skip them.
- **No black-point compensation.** `moxcms` 0.9's `TransformOptions` has no BPC (the field is
  commented out upstream); the monitor LUT is relative colorimetric without it.
- **LUT mode clamps in working space** before the shaper, whereas the exact path clamps after the
  matrix in display space. Tapetum's output is display-referred in [0, 1] in practice; values
  outside it would clip differently between the two paths.
- **Preview surfaces are managed too (#319, see "JPEG-sourced previews" below).** The T0/T2
  embedded-JPEG previews and grid thumbnails used to go straight to egui's sRGB textures; they now
  convert from the JPEG's embedded ICC profile (else sRGB) to the monitor profile on the CPU.
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
