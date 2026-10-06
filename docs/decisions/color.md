## Color pipeline

Covers #38's camera-color-profile parsing, working-space choice, and the CPU + GPU render
pipeline from linear camera RGB to a display-referred image.

- **Decision (#38)**: `docs/adr/0038-color-pipeline.md` — parse the user's own installed Adobe
  `.dcp`/`.xmp` camera profiles at runtime (never bundle one, per ADR-0018's "never bundle
  proprietary Adobe data" policy), falling back to LibRaw's built-in camera matrix when no profile
  is installed. A from-scratch DNG-spec Camera Profile tag reader (`spikes/calico/src/dcp.rs`)
  over a minimal hand-rolled TIFF/IFD parser reads `ColorMatrix1/2`, `ForwardMatrix1/2`,
  `CalibrationIlluminant1/2`, `ProfileHueSatMapDims/Data1/Data2`, `ProfileLookTableDims/Data`,
  `ProfileToneCurve`, `BaselineExposureOffset`, `ProfileHueSatMapEncoding`, and
  `ProfileLookTableEncoding` — tested only against synthetic DCPs built byte-for-byte in test code,
  never a real Adobe file.
- **Pipeline stage order**: linearize (LibRaw's own black-subtracted, 16-bit-scaled `dcraw_process`
  output — re-subtracting black/dividing by the raw sensor max on top of that double-applies the
  correction, a bug caught and fixed) → white balance (as-shot `cam_mul` — only for the
  ForwardMatrix branch below; the ColorMatrix fallback expects raw un-white-balanced input, since
  its own Bradford adaptation already corrects the illuminant, and applying both double-corrects
  it) → camera→XYZ(D50) via `cct.rs`'s CCT-interpolated matrix (DNG spec 6.3.7's dual-illuminant
  blend, using McCamy's published 1992 xy→CCT approximation as a documented stand-in for Adobe's
  own undisclosed solver; the white-point *search* itself always uses the ColorMatrix inverse,
  never ForwardMatrix, regardless of whether a ForwardMatrix exists — ForwardMatrix expects
  already-white-balanced input, which isn't known until the search converges) → ProPhoto
  intermediate → HueSatMap (hue and saturation come from **unencoded, linear** RGB, per Adobe's
  own reference implementation (`dng_reference.cpp`'s `RefBaselineHueSatMap`) — only the *value*
  coordinate runs through `ProfileHueSatMapEncoding`'s curve, linear by default, sRGB when tagged
  1; there is no "gamma 1.8" encoding, and no per-channel R/G/B encoding, anywhere in the spec — an
  earlier draft of this pipeline got both of those wrong, caught by review) → baseline exposure
  offset → LookTable (same hue/sat-unencoded, value-only-encoded treatment, per its own
  `ProfileLookTableEncoding`) → the chosen working space → tone curve (a Fritsch-Carlson monotonic
  cubic Hermite spline through the profile's `ProfileToneCurve` control points, or a
  commonly-reproduced "medium contrast" default curve when the profile has none) → sRGB for
  display/comparison output.
- **Working-space candidates, not yet decided**: linear ProPhoto/ROMM (ACR's own internal space),
  linear Rec.2020, and ACEScg (AP1 primaries) are all implemented and measured the same way; the
  decision rule (ADR-0038) picks whichever scores the lowest mean CIEDE2000 against LRC-exported
  references once that reference-machine pass runs. HueSatMap/LookTable still apply in
  ProPhoto-referenced HSV regardless of which working space wins, since that's how the DCP tables
  themselves are defined.
- **Adobe Raw "Look" `.xmp` profiles: resolved (#150), not a guess**: `crs:LookTable` turned out
  to be the table's own MD5 fingerprint rather than the data itself — the payload lives in a
  second attribute, `crs:Table_<id>`, in the DNG SDK's `dng_big_table` wire format (a Z85-like
  base85 variant + zlib around `dng_look_table::GetStream`'s tagged record; read as a spec
  reference only, never vendored). `xmp_profile.rs` decodes this and recomputes the same canonical
  re-serialization the SDK MD5-hashes for `crs:LookTable`, comparing it against the file's own ID
  as a correctness self-check — a wrong decode can't produce the right hash. Verified against all
  six real Adobe Raw profiles (Color/Landscape/Monochrome/Neutral/Portrait/Vivid): all six decode
  and fingerprint-match. Settings calico doesn't apply from a Look profile (`Clarity2012`,
  `ToneCurvePV2012`, `RGBTable`-based looks) are surfaced via `unsupported_settings` rather than
  silently dropped.
- **GPU 3D-texture kernel is a real first for this repo**: `spikes/glint`'s ADR-0016 kernels are
  storage-buffer-only by design (texture-specific concerns were explicitly left to whichever
  ticket needed them first — see `glint/src/gpu.rs`'s own scoping note). `spikes/calico/src/gpu.rs`
  is that ticket: a wgpu compute kernel applying a single `HueSatMap` via a real `Rgba16Float` 3D
  texture — width=saturation, height=hue, depth=value (matching the table's real DNG-SDK storage
  order, value outermost/hue middle/saturation innermost, with no transpose on upload), `Repeat`
  addressing on the wrapping hue axis, `ClampToEdge` on saturation/value, hardware trilinear
  filtering. Getting a real CPU/GPU parity test to pass against lavapipe (this sandbox's software
  Vulkan fallback) required finding and fixing two real bugs, not just a texture-coordinate detail:
  1. Hardware trilinear filtering treats texel `i`'s center as sitting at normalized coordinate
     `(i+0.5)/N`, not `i/N` — the CPU-side `HueSatMap::sample`/`sample_gpu_style` functions use the
     latter convention, so the GPU shader's texture-coordinate calculation needs an explicit remap
     (`gpu.rs` and `shaders/color.wgsl`'s comments carry the full derivation).
  2. `HueSatMap::sample`/`sample_gpu_style` had the saturation-axis and value-axis interpolation
     fractions swapped in their final blend step — a CPU-only bug (the GPU's hardware trilinear
     filtering interpolates all three axes correctly by construction, so it never shared this).
     The parity test's smoothly-varying synthetic data mostly masked it, which is why an earlier
     pass here attributed the resulting ~0.05-0.06 max per-channel deviation to lavapipe's own
     lower-precision filtering — plausible-sounding, but wrong; a different, deliberately
     adversarial test (a sat_divisions=1 table, where the swap's effect couldn't hide) exposed the
     real bug. A separate diagnostic (a `textureLoad`-based nearest-fetch readback, not committed
     to the repo) confirmed the texture *upload* itself — data layout,
     `bytes_per_row`/`rows_per_image` — was correct throughout, both before and after this fix.

  Fixed, the parity test's tolerance is `5e-3` and the actual measured deviation is `~1.5e-4` —
  two orders of magnitude tighter than the number this pipeline was originally measured against.
- **`retina dump-linear`, the decoder hand-off**: rather than making `spikes/calico` depend on
  `retina`'s LibRaw FFI/git-submodule (which would drag calico into the same CI path-gating retina
  needs), `retina` gained a `dump-linear` subcommand that demosaics with white balance, the color
  matrix, and gamma all disabled (LibRaw's own `output_color=0`/`gamm={1,1}`/`no_auto_bright=1`/
  `user_mul={1,1,1,1}` params), writing a 16-bit linear-camera-RGB TIFF plus a JSON metadata
  sidecar (`black`/`maximum`/`cam_mul`/`pre_mul`/`cam_xyz`/`cblack`) that calico reads directly —
  no shared Rust type between the two crates, just a documented JSON shape both sides keep in
  sync. LibRaw's demosaic here is explicitly a stand-in for this hand-off only; the real demosaic
  algorithm choice stays #40's decision, and could shift ADR-0038's measured ΔE numbers once
  decided.
- **Deferred, filed as follow-up issues** (see ADR-0038's Consequences): the reference-machine ΔE
  measurement run itself (this ADR's whole Measured-results section); the Adobe Vivid `.xmp`
  look-table decode, if the reference-machine pass confirms it doesn't parse as a DCP-style IFD;
  per-pixel black-level shading beyond the four per-channel `cblack` scalars retina already
  exposes; real GPU hardware timing for the 3D-texture kernel (correctness only was this pass's
  goal, not throughput).

## Color management (#42, ADR-0042)

`nicti-calico` gained the display/output color-management core: `space.rs` (sRGB / Display P3 /
Adobe RGB matrices from published primaries, Bradford-adapted to D50, from linear ProPhoto; sRGB
curve or 563/256 gamma), `icc.rs` (profiles generated at runtime with `moxcms`, checked against our
own matrices), `transform.rs` (`DisplayTransform { proof, kind }`) and `display_profile.rs`
(Windows `GetICMProfileW`, any failure degrades to sRGB with the reason surfaced).

Soft-proofing is analytic: working -> proof-space linear RGB -> clamp [0,1] -> back, with an exact
out-of-gamut flag (`GAMUT_EPS` = 0.002). The display stage is an exact matrix + transfer function
(default, fallback, and any monitor profile equivalent to a built-in space), or, for a different monitor profile, an analytic matrix + encode table extracted from its
colorants and TRCs (`MatrixTrc`, worst 0.8/255 for a P3 gamma-2.2 panel), with a 33^3 `Rgba16Float`
LUT baked by `moxcms` only for LUT-based profiles. It is display-only and lives
outside the render graph, so a monitor change or proofing toggle costs zero bake work. `nicti-pelt`
wires it in (`color_mgmt.rs`, `viewport.rs::set_display_transform`, `display.wgsl`), including
Shift+S gamut warning and re-resolving the profile when the window moves monitors.

**Why proofing is not baked into the LUT** (found by adversarial review of the first
implementation, which did exactly that): `moxcms` clamps f32 output to [0, 1], the sRGB gamut
boundary cuts diagonally through the ProPhoto-indexed cube, and trilinear interpolation across the
clipped nodes gave 9-12/255 error on plainly in-gamut colors (a teal 10% inside sRGB was 12/255
off); a 65^3 LUT did not help, and the gamut flag was quantised to a cell-wide false-positive band.
The Intent dropdown was also a no-op (Perceptual and Relative colorimetric are byte-identical for
matrix profiles), so it was removed.

Known limits, all recorded in ADR-0042: no black-point compensation (`moxcms` 0.9 doesn't
implement it), a non-built-in
LUT-based monitor profile carries interpolation error near its gamut boundary, Windows code is type-checked
but not run, and a real wide-gamut monitor check is still outstanding.

## JPEG-sourced previews (#319, ADR-0042 amendment)

The T0/T2 previews and grid thumbnails now convert from the JPEG's embedded ICC (else sRGB) to the
monitor profile with `nicti_calico::source_transform::SourceTransforms` (CPU, `moxcms` 8-bit,
cached per source profile, identity fast path when source and display are the same space). Done on
the Pounce worker for thumbnails (after the 256 px downsize, ~0.05 ms) and at texture upload for
the loupe/tiles (~11-13 ms for a 3840 px T2). `ColorManagement::generation` invalidates all of it
on a monitor change. `t2.rs` now keeps the source's RGB ICC profile through its re-encode (`render_hash` `v2` regenerates pre-#319 T2s). Not
proofed; real P3-monitor check still outstanding. See ADR-0042's "JPEG-sourced previews".

## DCP camera profiles in production (#42, ADR-0038 amendment)

Promoted `dcp.rs`/`cct.rs`/`huesatmap.rs` from `spikes/calico` into `nicti-calico` and added
`profile.rs`. Testing against real Adobe Z 8 profiles found the spike had two bugs: real files
start `IIRC` (magic 0x4352) and the LookTable tag ids are 50981/50982. `DcpProfile::solve` turns a
profile plus the frame's WB gains into one folded matrix and the blended tables; the live suffix
applies HueSatMap -> baseline exposure -> LookTable after the matrix. The selected profile is
stored as `CameraProfileParams` on `nicti.working_space` (content hash in the cache key). Default
stays "Matrix only". `ProfileToneCurve` and Look `.xmp` profiles are not applied yet. See ADR-0042's
"Camera profiles" section.

