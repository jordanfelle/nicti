---
paths:
  - "spikes/calico/**"
  - "spikes/retina/**"
  - "crates/nicti-calico/**"
  - "crates/nicti-pelt/src/color_mgmt.rs"
  - "crates/nicti-pelt/src/viewport.rs"
  - "crates/nicti-pelt/shaders/display.wgsl"
---

# Color Pipeline — Quick Reference

Full reasoning/history: `docs/decisions/color.md`.

- **One ProPhoto→sRGB matrix (#318)** — `nicti_calico::space::OutputSpace::from_working` is the sole source
  (display shader, export, and `nicti_tapetum::geometry::output_encode`'s CPU readback); do not
  re-introduce a published-constant copy. Pinned by `viewport.rs`'s `export_and_display_matrix_is_the_cpu_references_matrix` (1e-6).
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
  (base85 + zlib); `nicti-calico/src/xmp_profile.rs` (promoted from the spike by #321) decodes it and self-checks by recomputing that same
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
- **Scope split with #46** (added 2026-09-27): #42 (this topic) owns the DCP-profile machinery
  above — camera-profile HueSatMap/LookTable, the working-space pick, display/export color
  management. #46 (not this ticket) owns the *user-facing* WB temp/tint, tone curve, and HSL
  sliders as `crates/nicti-tapetum`'s own render stages (`coat.rs`/`color.rs`), using a single-
  matrix WB approximation (`color::wb_gains_for_temp_tint`) rather than this crate's dual-
  illuminant DNG solve until a DCP profile is actually loaded. See
  [`render-graph`](../render-graph/REFERENCE.md)'s own package-contents entry for #46's stages.
- **Color management (#42)** — `docs/adr/0042`: **Accepted**. `nicti-calico` owns it: `space.rs`
  (`OutputSpace` sRGB/Display P3/Adobe RGB; matrices from primaries + Bradford to D50, from linear
  ProPhoto), `icc.rs` (runtime `moxcms` profiles, never vendored), `transform.rs`
  (`DisplayTransform { proof, kind }`: **proof is analytic** — matrix to proof space, clamp, back;
  exact gamut flag, no intent choice; **display** is `Space` exact matrix+TRC; `MatrixTrc` analytic
  matrix + sqrt-indexed encode table for a matrix/TRC monitor ICC that isn't a built-in space;
  33³ RGBA16F `Lut` only for a LUT-based monitor ICC), `display_profile.rs` (Windows
  `GetICMProfileW`, degrades to sRGB). **Do not put proofing — or a matrix/TRC monitor — into a LUT**: moxcms clamps node
  output, and interpolating across the clipped nodes cost ~9-20/255 in-gamut error (review).
  **Display-only, outside the render graph** — no cache-key involvement. `nicti-pelt`:
  `color_mgmt.rs` (Color menu, Shift+S gamut warning, follows the window across monitors),
  `viewport.rs` `set_display_transform`, `display.wgsl` (proof stage, then mode 0/1). **No BPC**
  (`moxcms` 0.9 lacks it). Working space still ProPhoto
  (#149).
- **JPEG previews are managed (#319)** — `nicti_calico::source_transform::SourceTransforms`
  (embedded ICC else sRGB -> monitor, 8-bit CPU, cached per source profile, identity when same
  space, no proofing). Grid thumbnails convert on the Pounce worker after the 256 px downsize
  (`grid/jobs.rs::make_thumbnail`, ~0.05 ms); loupe/tiles at upload (`cull/previews.rs::
  preview_texture`, ~12 ms at 3840 px). `ColorManagement::generation` -> `PeltApp::sync_preview_color`
  -> `GridSession::set_color`/`TilePreviews::set_color`/loupe texture. `t2.rs` keeps the RGB ICC
  through its re-encode (`render_hash` `v2` regenerates pre-#319 T2s). Real P3-monitor check outstanding. Shared decode:
  `nicti-pelt/src/preview_color.rs`.
- **DCP camera profiles in the live suffix (#42)** — `nicti-calico` `dcp.rs`/`cct.rs`/
  `huesatmap.rs` promoted from `spikes/calico`; `profile.rs` `DcpProfile::solve(wb_gains)` →
  `ProfileSolution` (folded matrix, blended HueSatMap, LookTable, baseline exposure) with an
  `apply_cpu` reference. #321 adds `tone_lut` (profile `ProfileToneCurve` or ACR default, 1024 entries in sqrt space, hue-preserving RGB tone via `apply_cpu_toned`), `black_render` (parsed, Auto not applied), and `with_look` for a Look `.xmp` layered after the LookTable. **Real Adobe files start `IIRC` (magic 0x4352) and use LookTable tags
  50981/50982** — the spike had both wrong. Matched to a frame via `UniqueCameraModel`. Tapetum:
  `live_suffix.wgsl` bindings 3/4/5, `LiveParams.camera_profile`, tables re-uploaded only on
  fingerprint change. Selection = `coat::CameraProfileParams` on `nicti.working_space` (blake3 of
  the file → cache key; none = `{}`). `nicti-pelt` `camera_profiles.rs` discovers the user's own
  profiles; **default is Matrix only**. Not applied yet: `ProfileToneCurve`, Look `.xmp`.

## Package contents

- **`spikes/calico`** (#38/ADR-0038's color-pipeline research; DCP parser/CCT/HueSatMap promoted to `crates/nicti-calico` by #42, `xmp_profile.rs`/`tonecurve.rs` promoted by #321 and the spike's copy is now a shim; the spike stays for its ΔE tooling and CLI until #149's run is done) — a from-scratch DNG-spec Camera
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
