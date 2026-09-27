# #38: Color pipeline (`spikes/calico`)

See `docs/adr/0021-color-pipeline.md` for the decision and its rationale. This document is the
write-up: what was built, how to run it, and what's still pending.

## Sandbox constraint

This research pass ran in a Linux/WSL sandbox with no Lightroom Classic install, no real Adobe
`.dcp`/`.xmp` camera profile file, and no LRC-rendered reference image — none of these can exist
here (ADR-0003's licensing policy forbids adding a real Adobe profile to this repo, and
`docs/ref-10k-manifest.csv` only ever listed NEFs, never JPEG/TIFF references). What's real: the
DCP/DNG-tag parser, the CCT/matrix math, the HueSatMap/tone-curve implementations, the CIEDE2000
metric, and — the one part of this research that specifically needed real hardware feedback, not
just type-checking — a wgpu compute-shader 3D-texture kernel, tested for CPU/GPU parity against
this sandbox's lavapipe (software Vulkan) fallback. All of that is backed by unit and integration
tests (`cargo test -p calico`), not just written-and-hoped-correct.

**Update (#150)**: the Adobe Raw "Look" `.xmp` decode below (originally timeboxed as
`UnrecognizedTableFormat`) turned out to be solvable in-sandbox after all — the user's real,
installed Adobe Raw profiles are reachable from this WSL environment via the Windows side
(`/mnt/c/ProgramData/Adobe/CameraRaw/Settings/Adobe/Profiles/Adobe Raw/`). `xmp_profile.rs` now
decodes the real `dng_big_table` container (base85 + zlib around a `dng_look_table` tagged
record) and self-checks by recomputing the profile's own MD5 fingerprint, verified against all
six real profiles (Color/Landscape/Monochrome/Neutral/Portrait/Vivid). See
`docs/adr/0021-color-pipeline.md`'s `xmp_profile.rs` bullet for the format details. Steps 4/6/87-90
below (the `UnrecognizedTableFormat` contingency) are now historical — Vivid is in scope for the
reference-machine pass like Standard/Color always was.

**Two real bugs were found and fixed via that GPU test and its own review process**, not just
written and assumed right:

1. An initial version of `shaders/color.wgsl`'s hue/sat/val texture-coordinate math didn't account
   for hardware trilinear filtering treating texel `i`'s center as sitting at normalized coordinate
   `(i+0.5)/N`, not `i/N` — the CPU sampler in `huesatmap.rs` uses the latter convention. The fix
   (and a from-scratch derivation of why it's correct, cross-checked by hand against the exact
   failing test case) is in `gpu.rs`'s and `shaders/color.wgsl`'s comments.
2. `huesatmap.rs`'s `sample`/`sample_gpu_style` had the saturation-axis and value-axis
   interpolation fractions swapped in the final two `lerp` calls — a CPU-only bug the GPU's own
   hardware trilinear filtering never shared, since it interpolates all three axes correctly by
   construction. The parity test's smoothly-varying synthetic data mostly masked this (the two
   fractions were often numerically close, so swapping them barely changed the result), which is
   why an earlier pass here attributed the ~0.05-0.06 max ΔRGB deviation to lavapipe's own
   lower-precision filtering — a plausible-sounding but wrong explanation, corrected once a
   different test (`pipeline.rs`'s `encoding_choice_does_affect_value_scaling`, using a
   sat_divisions=1 table where the swap's effect couldn't hide) exposed the real bug. Fixed, the
   parity test's tolerance is now `5e-3`, and the actual measured deviation is `~1.5e-4`.

A follow-up dedicated diagnostic (a `textureLoad`-based nearest-fetch readback, not committed — see
the ADR) additionally confirmed the texture *upload* itself (data layout,
`bytes_per_row`/`rows_per_image`) was always correct.

## What's real vs. what's pending

**Real, tested, in this sandbox:**

- `spikes/calico`'s DCP/DNG-tag parser, CCT solver, matrix math, HueSatMap/tone-curve/ΔE
  implementations, and CPU render pipeline — all real, unit-tested.
- The wgpu 3D-texture HueSatMap kernel and its CPU/GPU parity test — real, runs against lavapipe.
- A full synthetic end-to-end pipeline sanity check (`tests/pipeline_synthetic.rs`): a known
  neutral camera-RGB value through a synthetic (hand-computable) DCP profile, checked against a
  hand-derived expected sRGB output.

**Pending the user's reference-machine pass** (real DCP/XMP files, real LRC exports, real NEFs):

- `spikes/retina`'s new `dump-linear` subcommand (extends the existing LibRaw shim with a
  demosaic-only decode path — no WB/color-matrix/gamma applied). **Not compiled or tested in this
  sandbox**: retina's own `vendor/LibRaw` git submodule isn't checked out here (same constraint
  every other retina session hits — `git submodule update --init spikes/retina/vendor/LibRaw`
  first), and retina is excluded from the workspace's normal `clippy`/`test` jobs for exactly this
  reason (see CLAUDE.md's CI path-gating note). The Rust/C++ additions were written and reviewed
  by hand against the existing shim's own conventions, and are syntax-checked via `cargo fmt
  --check -p retina` (which parses the file without needing the C++ build), but not run.

1. Export 6-10 Nikon Z8 NEFs from the user's own source NEF folder (mixed HE/HE*/Lossless, varied
   scenes including saturated colors and skin/fur tones).
2. In Lightroom Classic, export each as a 16-bit sRGB TIFF, twice — once with **Adobe Standard**
   (or Adobe Color) applied, once with **Adobe Vivid** — all develop sliders zeroed, As Shot white
   balance, no lens corrections/sharpening/noise reduction/crop. Save both sets to the user's
   local export folder.
3. `git submodule update --init spikes/retina/vendor/LibRaw`, then for each NEF:
   `cargo run -p retina --bin retina -- dump-linear <nef> --out <dir>`.
4. Locate the installed DCP file(s) for the camera model(s) used (typically under
   `%LOCALAPPDATA%\Adobe\CameraRaw\CameraProfiles\` or similar) and, separately, the installed
   Adobe Vivid `.xmp` look profile (under `...\CameraRaw\Settings\Adobe\Profiles\`).
5. For each candidate working space (`prophoto`, `rec2020`, `acescg`):
   `cargo run -p calico --bin calico -- render <dir>/<stem>.linear.tiff <dir>/<stem>.meta.json
   --dcp <path-to-dcp> [--look <path-to-vivid-xmp>] --space <candidate> --out <render>.png`.
   **Before trusting the ΔE numbers from this step**, sanity-check `dcp.rs`'s unverified
   HueSatMap/LookTable entry-nesting-order assumption (see the ADR's own flagged risk): render a
   scene with a strong, known hue-dependent effect and confirm it looks plausible, not scrambled.
6. `cargo run -p calico --bin calico -- compare <render>.png <lrc-export>.tiff --heatmap
   <heatmap>.png` against the matching LRC export, for each working space and each profile
   (Standard/Color and, if step 4's `--look` decodes successfully, Vivid).
7. Fill in ADR-0021's Measured results table with the mean/p95/max ΔE00 per candidate, and move
   its Status to Accepted if the decision rule's bar is met (mean ≤ 2.0, p95 ≤ 5.0, every
   reference image, every profile actually measured).

(Historical: step 4/6's `--look` used to be expected to fail with `UnrecognizedTableFormat`,
falling back to a Standard/Color-only measurement. #150 resolved that decode against all six real
Adobe Raw profiles, so `--look <path-to-vivid-xmp>` should now succeed during the
reference-machine pass; if it still fails, that's a new, distinct bug worth its own issue rather
than the original research-risk case.)
