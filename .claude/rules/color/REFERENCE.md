---
paths:
  - "spikes/calico/**"
  - "spikes/retina/**"
  - "crates/nicti-color/**"
---

# Color Pipeline — Quick Reference

Full reasoning/history: `docs/decisions/color.md`.

- **Color pipeline (#38)** — `docs/adr/0038`: **Proposed**, pending a reference-machine ΔE
  measurement run against real LRC exports (no LRC install / real Adobe profile / reference image
  exists in this sandbox — ADR-0018 forbids adding one). Stage order: linearize (LibRaw's own
  black-subtracted, 16-bit-scaled output) → WB (`cam_mul`, ForwardMatrix branch only) →
  camera→XYZ(D50) (CCT-interpolated `ColorMatrix`/`ForwardMatrix`, DNG spec 6.3.7 — white-point
  *search* always uses ColorMatrix, ForwardMatrix only for the final matrix, expects
  white-balanced input; ColorMatrix fallback expects raw input) → ProPhoto intermediate →
  HueSatMap (hue/sat from unencoded linear RGB, per Adobe's reference implementation — only the
  *value* coordinate goes through `ProfileHueSatMapEncoding`: linear when absent/0, sRGB when 1 —
  no "gamma 1.8", no per-channel R/G/B encoding, in the spec) → baseline exposure → LookTable
  (same treatment, `ProfileLookTableEncoding`) → selected working space → tone curve
  (Fritsch-Carlson monotonic spline) → sRGB.
- **Profile source: parse the user's own installed Adobe `.dcp`/`.xmp` at runtime**, never bundle
  one (ADR-0018) — falls back to LibRaw's built-in camera matrix when none is installed. Adobe
  Raw "Look" `.xmp` profiles (e.g. Adobe Vivid) resolved in #150: `crs:LookTable` is an MD5
  fingerprint, the payload is `crs:Table_<id>` in the DNG SDK's `dng_big_table` wire format
  (base85 + zlib); `xmp_profile.rs` decodes it and self-checks by recomputing that same
  fingerprint. Verified against all six real Adobe Raw profiles.
- **Working-space candidates**: linear ProPhoto (ACR's own), Rec.2020, ACEScg — picked by lowest
  measured ΔE00 against LRC exports, not decided yet.
- **HueSatMap storage order**: value outermost, hue middle, saturation innermost (DNG SDK's
  `dng_hue_sat_map::SetDivisions`) — `huesatmap.rs`'s `index()` reads the parsed bytes directly in
  this order, no transpose in `dcp.rs`.
- **3D-texture GPU kernel**: first 3D-texture pattern in this repo (glint's own kernels are
  storage-buffer-only, ADR-0016). Texture axes: width=saturation, height=hue, depth=value (matches
  the storage order above with no transpose on upload); sampler Repeat on height (hue wraps),
  ClampToEdge on width/depth. Hardware trilinear filtering centers texel `i` at `(i+0.5)/N`, not
  `i/N` — `huesatmap.rs`'s CPU sampler uses the latter; `gpu.rs`/`shaders/color.wgsl` remap
  coordinates accordingly. `huesatmap.rs`'s `sample`/`sample_gpu_style` also had the sat/value
  interpolation fractions swapped (fixed) — this, not lavapipe precision, was the real source of
  an earlier ~0.05-0.06 max ΔRGB; CPU/GPU parity now holds to `5e-3` (~1.5e-4 measured), verified
  via a separate `textureLoad` nearest-fetch readback check on the texture upload itself.
- **`retina dump-linear`**: new subcommand, demosaic-only (WB/color-matrix/gamma disabled via
  LibRaw's own `user_mul={1,1,1,1}`/`output_color=0`/`gamm={1,1}` params) — hands linear camera RGB
  + metadata (black/max/cam_mul/pre_mul/cam_xyz/cblack) to calico without calico depending on
  retina's LibRaw FFI/submodule.

## Package contents

- **`spikes/calico`** (#38/ADR-0038's color-pipeline research) — a from-scratch DNG-spec Camera
  Profile (`.dcp`) tag reader over a hand-rolled TIFF/IFD parser, DNG-spec CCT-based
  dual-illuminant matrix interpolation, three working-space candidates (ProPhoto/Rec.2020/ACEScg),
  a HueSatMap/LookTable trilinear HSV implementation with hue-wrap-aware interpolation, a
  Fritsch-Carlson monotonic tone-curve spline, CIEDE2000 comparison tooling, a CPU reference
  pipeline, and — this repo's first 3D-texture wgpu kernel (`spikes/glint`'s own kernels are
  storage-buffer-only, ADR-0016) — a GPU port of the HueSatMap lookup with a real CPU/GPU parity
  test passing against lavapipe; pure Rust, no FFI, not path-gated. Real, tested (39 unit tests +
  3 integration tests, plus a local-only `--ignored` test that verifies `xmp_profile.rs`'s Adobe
  Raw "Look" `.xmp` decode against the user's real installed profiles, #150), pending only the
  reference-machine ΔE-against-LRC measurement pass ADR-0038 describes. See
  `docs/research/calico-color-pipeline.md`.
