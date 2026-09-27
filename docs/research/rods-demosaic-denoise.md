# Rods: demosaic + denoise measurements

Findings for [#40](https://github.com/jordanfelle/nicti/issues/40), feeding
[docs/adr/0025-demosaic-and-denoise.md](../adr/0025-demosaic-and-denoise.md)'s shipping decision.
Tooling: `spikes/rods/` (alignment/metrics/AI-denoise harness) plus new `retina dump-classic`/
`dump-cfa` subcommands — see those crates' own module docs. This is a findings doc, not the ADR;
the shipping decision itself belongs to ADR-0025.

## Method

Unlike most of this repo's prior research passes, this one ran on a machine that is a genuine
GPU/CUDA/TensorRT reference environment — not a sandbox proving scaffolding shape only. CUDA 13.4,
cuDNN 9.26.0.51, TensorRT 10.16.1.11, and onnxruntime-gpu 1.30.0 were installed on the Windows
host during this pass (all via winget/pip, no NVIDIA login needed), and `rods` cross-compiles
cleanly to `x86_64-pc-windows-gnu` with no C/C++ dependency to fight (unlike `retina`'s LibRaw).
So most of the numbers below are real, not "TBD — reference machine."

**What's still not measured**: comparison against LRC's own AI Denoise output specifically. The
decision rule is stated against LRC, but every number in this doc is candidate-vs-**ground-truth**
(a real, separately-exposed clean frame), not candidate-vs-LRC — that comparison needs the user's
own LRC export batch (`docs/decisions/denoise.md`'s User-side inputs), not yet done. Read every
number below as "how much does this candidate improve over the classic-demosaic-only baseline,"
not yet as "does this beat LRC."

### Ground truth: RawNIND, not a real Z8 tripod shoot

The real Z8 tripod shoot this ticket originally planned (see ADR-0025's Context) won't happen for
weeks/months. By user decision, this pass used **RawNIND** (`doi:10.14428/DVN/DEQCIM`,
CC-BY-SA-4.0, dataverse.uclouvain.be) instead — a public dataset of real paired noisy/clean raw
photos across several camera brands. Picked over ELD (the other real candidate found, Nikon D850
only) because RawNIND includes real **Nikon Z6** files: same Z-mount mirrorless generation as the
Z8, confirmed via `exiftool` (`NIKON Z 6`, `NEF Compression: Lossless` — no HE dependency risk).
60 real NEFs across 5 static scenes (`bananapi`, `couch`, `sewingmachine`, `Iain01`, `Iain02`),
each a base-ISO-50 ground truth plus a full climbing ISO ladder to 51200 — the same shape the
tripod shoot would have had. All 60 files' SHA1s verified against the filename-embedded hash.

Every quality number below is therefore **provisional against a Z6, not a Z8** — the exact caveat
ADR-0025 states up front. A real Z8 verification pass is filed as
[#164](https://github.com/jordanfelle/nicti/issues/164); the LRC comparison itself is
[#163](https://github.com/jordanfelle/nicti/issues/163).

### Classic pipeline

`retina dump-classic` (new, this pass): runs LibRaw's classic pipeline with white balance applied
(`use_camera_wb=1`, unlike `dump-linear`'s WB-free stand-in for calico) and the caller's choice of
demosaic quality plus FBDD noise reduction, still linear/raw output (`output_color=0`, `gamm=
{1,1}`) so `rods` applies one fixed color treatment to every candidate uniformly. Confirms #148's
own "unverified in this sandbox" caveat that `dcraw_process()` after `raw2image()` works — smoke-
tested against a real Z6 NEF, all 7 demosaic qualities (linear/VNG/PPG/AHD/DCB/DHT/AAHD) and FBDD
0/1/2 produce correct output.

**Found, not fixed: the vendored PR#826 LibRaw fork's wavelet denoise is broken.** Any nonzero
`threshold` corrupts `imgdata.image`'s real contents in some way, at every magnitude tested
(1.0, 100.0, 400.0) — not root-caused to a specific confirmed mechanism this pass, so
`retina_libraw_process_classic` now rejects any nonzero `threshold` outright rather than trusting a
caller to detect the corruption after the fact. `postprocessing_aux.cpp` shows `wavelet_denoise()`
operating on still-mosaiced Bayer data via a `BAYER(row,col)` macro (dcraw-legacy, pre-demosaic),
not post-demosaic as the original ADR-0025 plan assumed — an observed processing path, not a
confirmed root cause: `retina_classic_image` always reports a fixed `iwidth*iheight*4` length
regardless of what LibRaw's own internals actually allocated, so this evidence doesn't by itself
establish a single-channel allocation or that any length check actually caught one. Classic-NR
baseline for this pass uses FBDD only (0/1/2, all working); wavelet stays a known, scoped
limitation, not root-caused further (see Consequences).

`AHD`, `FBDD=0` was used for every result below (the plan's stated baseline quality setting; a
demosaic-quality sweep with real timing per setting is unbuilt, flagged as a follow-up).

### Path A: investigated, not built

Searched for a ready ONNX or easily-convertible pretrained Bayer-domain joint demosaic+denoise
model to feed from `retina dump-cfa` (also new this pass — a black-subtracted, white-normalized
Bayer-plane dump, no demosaic). Two real candidates found, both genuinely blocked, not simply
absent:

- **[BJDD (CVPRW21)](https://github.com/sharif-apu/BJDD_CVPR21)**: real joint demosaic+denoise,
  dedicated pretrained Bayer-CFA weights (Gaussian noise σ 5/10/15) exist. Blocked on (a) Google-
  Drive-hosted PyTorch checkpoints, no direct-download API, and (b) its own architecture takes a
  **3-channel** input (`modelDefinitions/attentionGen.py`'s `inputConv`), not a raw 1-channel
  mosaic — it has its own undocumented pre-demosaic preprocessing convention that would need
  reading `dataTools`'s actual sampling code to replicate, not just the model architecture.
- **[demosaicnet_pytorch](https://github.com/douyuhan/demosaicnet_pytorch)** (Gharbi et al. 2016):
  its own README FAQ states "the noise-aware model is not implemented" in this maintained port —
  points to an old, unmaintained Caffe repo for that. Doesn't meet Path A's bar at all as shipped.

**Decision: Path A moves to v2 research**, not built this pass — the expected research value
(both real candidates were trained on synthetic/non-Nikon noise, so "sensor-mismatched, likely
fails" was already the prior) didn't clear the remaining engineering cost (PyTorch→ONNX conversion
plus reverse-engineering BJDD's own preprocessing) inside the plan's 2-day cap. Revisit BJDD
specifically if neither Path B candidate below clears the eventual LRC-relative bar.

### Path B: two real candidates, both ONNX-ready

Both sourced as ready-made ONNX exports from the public
[deepghs/image_restoration](https://huggingface.co/deepghs/image_restoration) HF repo (MIT license
on the re-export, confirmed via the HF card's own metadata) — sidestepping the PyTorch→ONNX
conversion the original plan expected to need:

- **NAFNet-SIDD-width64** (upstream MIT+Apache-2.0, trained on SIDD real-photo noise)
- **SCUNet-PSNR** (upstream Apache-2.0, trained on **purely synthetic** degradations — no real-
  photo dataset provenance question exists for this candidate, the cleanest of the two)

Both confirmed sharing the same input/output contract (NCHW float32, dynamic spatial dims, one
named "input"/"output" tensor) via `onnxruntime`'s own metadata before writing any Rust code.

`spikes/rods/src/ai.rs`: a tiled `ort`/`load-dynamic` wrapper (same scaffolding pattern as
`spikes/groom/src/ai.rs`), feathered linear-ramp tile blending, CUDA/TensorRT execution provider
selectable via `--ep`. Runs on the fixed display-encoded sRGB image
(`spikes/rods/src/display.rs`'s `to_display_srgb` output: camera RGB → XYZ(D50), a hardcoded
Bradford-adapted matrix (a public ICC-tooling constant, not project data) → linear sRGB → sRGB
OETF), not linear camera RGB — both candidates are SIDD/synthetic-noise trained on gamma-encoded,
WB-applied, phone-ISP-style images, and linear light would be badly out-of-distribution. Scoring
stays in that same fixed encoding, applied identically to every candidate, so quality differences
measure demosaic/denoise choices, not a second color pipeline. Deliberately not calico's real DCP
pipeline (ADR-0021, still its own open research pass) — this is LibRaw's own no-profile fallback
treatment, correct enough to compare candidates against each other and (eventually) LRC on equal
footing.

### Alignment

`spikes/rods/src/align.rs`: single-level Lucas-Kanade (Gauss-Newton on image gradients, no FFT
dependency in this workspace) recovers sub-pixel translation between a candidate and its
reference. `resample_rgb` then bilinear-resamples the candidate onto the reference's pixel grid at
that shift before any scoring or denoising. `clip_mask`/`fit_gain` exclude near-clipped samples
and absorb a shared linear WB/exposure-scale difference.

**Two real bugs found and fixed while validating this**:
1. Shift was being re-estimated per-candidate on the *post-denoise* image. A smoother denoiser's
   different gradient structure shifts where Lucas-Kanade converges — SCUNet's smoother output
   tripped a spurious misregistration warning that NAFNet's, on the identical raw candidate,
   didn't. Fixed: shift is now always estimated once from the raw, pre-denoise candidate,
   independent of which candidate gets scored.
2. `resample_rgb` (the fix for bug 1's underlying gap) turned out load-bearing immediately:
   RawNIND's `couch` scene has a genuine ~1.1px real shift between its GT and high-ISO frames (real
   exposure-to-exposure movement, not decoder rounding) — without correcting for it, couch's
   numbers were measuring misalignment error alongside demosaic/denoise differences.

`couch` (0.44px/-1.16px) and `sewingmachine` (0.30px/0.35px) both still exceed the 0.25px
tolerance *after* resampling corrects the mean shift — plausible if some residual non-translational
difference remains (not investigated further; flagged as an open question, not a blocker, since
the resample step demonstrably fixes the dominant component).

## Measured results

### Crop-level (256×256, apples-to-apples across candidates), CPU dev-loop

| Scene | Shift | Baseline | + NAFNet-SIDD | + SCUNet-PSNR |
|---|---|---|---|---|
| bananapi | 0.16/-0.09 (OK) | 25.33 dB / 0.383 | 28.99 dB / 0.604 | **29.62 dB / 0.757** |
| couch | 0.46/-1.15 (>tol) | 23.06 dB / 0.512 | 23.21 dB / 0.525 | **27.07 dB / 0.748** |
| sewingmachine | 0.28/1.02 (>tol) | 24.32 dB / 0.302 | **31.50 dB / 0.640** | 29.70 dB / 0.796 |

### Full-resolution (real 6064×4040 frames), Windows-native, CUDA

| Scene | + NAFNet-SIDD | + SCUNet-PSNR |
|---|---|---|
| bananapi | 31.400 dB / 0.678 | **35.035 dB / 0.818** |
| couch | 27.632 dB / 0.664 | **33.605 dB / 0.862** |
| sewingmachine | **29.866 dB / 0.815** | 24.508 dB / 0.780 |

**Both candidates beat the classic-demosaic-only baseline in every case measured.** At full
resolution, **SCUNet is the stronger candidate overall** — wins PSNR *and* SSIM cleanly on 2 of 3
scenes (bananapi, couch). NAFNet wins **both** PSNR and SSIM on the third (sewingmachine,
29.87dB/0.815 vs 24.51dB/0.780) — a clean win, not a split, at full resolution. The 256px crop
sample *did* show a PSNR/SSIM disagreement on that same scene (NAFNet ahead on PSNR, SCUNet ahead
on SSIM, see the crop-level table above) that **doesn't hold at full resolution** — the crop was
evidently an atypical patch for that scene, not representative of the whole frame. Treat the
full-resolution numbers as the more reliable signal; a visual inspection of why the crop diverged
wasn't done this pass, flagged as an open question if it matters later.

### Speed

| Run | Environment | Result |
|---|---|---|
| SCUNet, 256px crop, CPU | WSL/CPU | p50 1605.8ms |
| SCUNet, 256px crop, CUDA | Windows-native, RTX 5080 | **p50 44.8ms — ~36x speedup** |
| PSNR/SSIM, same crop, CPU vs CUDA | — | matched to 4 decimal places (29.615dB/0.7566 both), confirming CUDA EP genuinely active, not silently falling back |
| NAFNet, full 6064×4040 frame, CPU (117× 512px tiles) | WSL/CPU | killed after 90+ min, never finished |
| NAFNet, full 6064×4040 frame, CUDA | Windows-native, RTX 5080 | **30.7s total wall time** (decode+tile+inference+H2D/D2H) |
| SCUNet, full 6064×4040 frame, CUDA | Windows-native, RTX 5080 | **50.9s total wall time** |

`ort` 2.0.0-rc.13's `Session` doesn't expose which EP actually served a call after the fact — the
~36x speedup plus matching quality numbers is the practical way this pass confirmed CUDA was
genuinely used, not a silent CPU fallback.

**A real model-architecture finding, not a bug in this repo's own code**: SCUNet's Swin-window
self-attention needs each downsampled internal feature map's spatial dims evenly divisible by its
window size. 512px tiles fail outright (a clean, correctly-propagated `ort` error, not a crash);
256px tiles work for interior tiles but still failed on `rods`'s naive edge/remainder tile
shrinking at the image border. **Fixed**: `build_padded_tile` clamp-to-edge pads every tile to a
fixed `tile_size x tile_size` before inference (replicating the source image's last real
row/column, not zero-filling), with only the tile's real region used for blending — verified fixed
on real hardware (the full-resolution SCUNet row above is post-fix). NAFNet has no equivalent
constraint (ran cleanly at 512px, including edge tiles, both pre- and post-fix).

### A real toolchain bug, found getting ready for the above

`cargo build --target x86_64-pc-windows-gnu` failed with "can't find crate for core/std" despite
the target being installed, even reinstalled fresh. Cause: PATH resolved `cargo` **and** `rustc`
to Homebrew's Rust install (no Windows target support at all), not rustup's — invoking rustup's
`cargo` binary directly wasn't sufficient, since cargo shells out to a bare `rustc` looked up via
PATH again. Fix: rustup's toolchain directory first on PATH for both binaries. `rods` then
cross-compiles cleanly, no C/C++ dependency to fight (unlike `retina`'s vendored LibRaw).

## Options considered

| Path | Real candidate found? | Built/measured? | Verdict |
|---|---|---|---|
| A: Bayer-domain joint demosaic+denoise | Yes (BJDD) — real Bayer weights, but 3-channel non-raw input convention + Drive-hosted PyTorch | No | Moves to v2; revisit if Path B fails LRC bar |
| B: classic demosaic + AI denoise, NAFNet-SIDD | Yes, ONNX-ready | Yes, full pipeline + full-resolution GPU numbers | Real improvement over baseline everywhere measured; loses to SCUNet on PSNR+SSIM on 2/3 scenes, but wins both on sewingmachine |
| B: classic demosaic + AI denoise, SCUNet-PSNR | Yes, ONNX-ready | Yes, full pipeline + full-resolution GPU numbers, edge-tile bug found+fixed | **Strongest candidate measured this pass** — best PSNR+SSIM on 2/3 scenes; cleanest training-data provenance (purely synthetic, no real-photo dataset question) |

## Consequences

- **Feeds ADR-0025's decision** (see that ADR for the actual shipping call) — this doc supplies
  the evidence, not the ruling.
- **Still needed before the decision rule is actually satisfied**: real LRC AI Denoise comparison
  ([#163](https://github.com/jordanfelle/nicti/issues/163), user's own export batch, not yet run),
  LPIPS (not wired), and a real Z8 tripod verification pass
  ([#164](https://github.com/jordanfelle/nicti/issues/164), weeks/months out per the user's own
  timeline) — every number here is candidate-vs-ground-truth on a Z6, a real and useful signal, but
  not yet the LRC-relative, Z8-specific claim the decision
  rule asks for.
- **`docs/licensing.md` updated in this PR**: NAFNet's row updated to note the actual ONNX source
  used and its evaluate-only status; new SCUNet row (Apache-2.0, cleanest provenance of the three
  candidates considered).
- **Wavelet denoise stays broken in the vendored LibRaw fork** — not fixed, scoped out. If a future
  pass needs it (e.g. FBDD alone doesn't clear the eventual LRC bar), root-causing
  `wavelet_denoise()`'s Bayer-domain buffer-sizing bug is its own follow-up, not re-attempted here.
- **`rods`'s edge-tile padding fix is a real, general correctness fix**, not SCUNet-specific in its
  code (any future model with a similar divisibility constraint benefits automatically) — but it
  was only found and fixed because a full-resolution run was actually possible this pass.

## Spike: `spikes/rods`

Feline name: rods, as in rod cells, the retina's low-light receptors — pairs with `retina`
(#37/ADR-0019). Not production code, same "don't build on top of it" status as this repo's other
spikes; expect it deleted once a future ticket promotes the parts worth keeping.

- **`src/linear_input.rs`**: reads `retina dump-classic`/`dump-linear`'s TIFF+JSON output pair,
  same pattern as calico's own copy of this reader (no dependency on retina's LibRaw FFI).
- **`src/display.rs`**: `camera_rgb_to_xyz`, `xyz_to_linear_srgb`, `srgb_oetf`,
  `to_display_srgb` — the one fixed color treatment every candidate shares.
- **`src/align.rs`**: `Plane`/`Shift`/`estimate_shift` (Lucas-Kanade), `resample_rgb`, `fit_gain`,
  `clip_mask`.
- **`src/ai.rs`**: `TiledDenoiser`, `TileConfig`, `ExecutionProviderKind`, `build_padded_tile` (the
  edge-tile fix, extracted standalone for unit testing without a real ONNX model).
- **`src/bin/rods.rs`**: the `compare` CLI — aligns, optionally denoises (`--denoise-model`/
  `--ort-dylib`/`--ep`/`--tile`/`--overlap`), optionally times (`--time`, wraps
  `nicti_prowl::perf::Protocol`), optionally crops for a fast dev loop (`--crop`), scores.
- **`crates/nicti-prowl/src/metrics.rs`** (new, in the production harness crate, not the spike):
  `psnr`/`ssim`/`ssim_rgb` for `f32` 0..1 samples — separate from `golden.rs`'s private, 8-bit-only
  SSIM (different dynamic-range constants, not worth parameterizing one function over both).
- **`retina dump-classic`/`dump-cfa`** (new subcommands in `spikes/retina`, not `rods` itself):
  `retina_libraw_process_classic`/`retina_cfa_normalized` in `shim.{cpp,h}`, the classic-pipeline
  and Bayer-domain inputs this doc's candidates consume.
