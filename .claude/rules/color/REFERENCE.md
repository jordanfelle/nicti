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
  exists in this sandbox — ADR-0003 forbids adding one). Stage order: linearize → WB (`cam_mul`) →
  camera→XYZ(D50) (CCT-interpolated `ColorMatrix`/`ForwardMatrix`, DNG spec 6.3.7) → working space
  → HueSatMap (gamma-encoded linear-ProPhoto RGB, 1/1.8 approximation) → baseline exposure →
  LookTable → tone curve (Fritsch-Carlson monotonic spline) → sRGB.
- **Profile source: parse the user's own installed Adobe `.dcp`/`.xmp` at runtime**, never bundle
  one (ADR-0003) — falls back to LibRaw's built-in camera matrix when none is installed. Adobe
  Raw "Look" `.xmp` profiles (e.g. Adobe Vivid) have an undocumented embedded look-table encoding
  — `xmp_profile.rs` attempts a DCP-style IFD decode and fails cleanly
  (`UnrecognizedTableFormat`) rather than guessing; unresolved pending a real sample file.
- **Working-space candidates**: linear ProPhoto (ACR's own), Rec.2020, ACEScg — picked by lowest
  measured ΔE00 against LRC exports, not decided yet.
- **3D-texture GPU kernel**: first 3D-texture pattern in this repo (glint's own kernels are
  storage-buffer-only, ADR-0005). Hardware trilinear filtering centers texel `i` at `(i+0.5)/N`,
  not `i/N` — `huesatmap.rs`'s CPU sampler uses the latter; `gpu.rs`/`shaders/color.wgsl` remap
  coordinates accordingly. CPU/GPU parity confirmed on lavapipe (~0.05-0.06 max ΔRGB, attributed
  to lavapipe's own lower-precision filtering, not a bug — verified via a separate `textureLoad`
  nearest-fetch readback check).
- **`retina dump-linear`**: new subcommand, demosaic-only (WB/color-matrix/gamma disabled via
  LibRaw's own `user_mul={1,1,1,1}`/`output_color=0`/`gamm={1,1}` params) — hands linear camera RGB
  + metadata (black/max/cam_mul/pre_mul/cam_xyz/cblack) to calico without calico depending on
  retina's LibRaw FFI/submodule.
