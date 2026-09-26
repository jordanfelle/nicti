---
paths:
  - "spikes/calico/**"
  - "spikes/retina/**"
  - "crates/nicti-color/**"
---

# Color Pipeline — Quick Reference

Full reasoning/history: `docs/decisions/color.md`.

- **Color pipeline (#38)** — `docs/adr/0021`: **Proposed**, pending a reference-machine ΔE
  measurement run against real LRC exports (no LRC install / real Adobe profile / reference image
  exists in this sandbox — ADR-0003 forbids adding one). Stage order: linearize (LibRaw's own
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
  one (ADR-0003) — falls back to LibRaw's built-in camera matrix when none is installed. Adobe
  Raw "Look" `.xmp` profiles (e.g. Adobe Vivid) have an undocumented embedded look-table encoding
  — `xmp_profile.rs` attempts a DCP-style IFD decode and fails cleanly
  (`UnrecognizedTableFormat`) rather than guessing; unresolved pending a real sample file.
- **Working-space candidates**: linear ProPhoto (ACR's own), Rec.2020, ACEScg — picked by lowest
  measured ΔE00 against LRC exports, not decided yet.
- **HueSatMap storage order**: value outermost, hue middle, saturation innermost (DNG SDK's
  `dng_hue_sat_map::SetDivisions`) — `huesatmap.rs`'s `index()` reads the parsed bytes directly in
  this order, no transpose in `dcp.rs`.
- **3D-texture GPU kernel**: first 3D-texture pattern in this repo (glint's own kernels are
  storage-buffer-only, ADR-0005). Texture axes: width=saturation, height=hue, depth=value (matches
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
