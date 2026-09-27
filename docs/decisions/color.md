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
