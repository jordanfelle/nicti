## Demosaic and noise reduction

Covers #40's demosaic + noise-reduction research (ADR-0040): the Path A/B split, the RawNIND
ground-truth substitution, the classic-pipeline wavelet-denoise fork bug, the two AI denoise
candidates measured, the alignment bugs found and fixed, and the real Windows-native CUDA numbers.

- **Demosaic + NR (#40)**: `docs/adr/0040-demosaic-and-denoise.md` — **Proposed**, pending a real
  LRC AI Denoise comparison and a real Z8 tripod verification pass (the real tripod shoot won't
  happen for weeks/months, by user decision 2026-09-26). Every quality number measured this pass is
  candidate-vs-ground-truth on RawNIND's real Nikon **Z6** frames (the same Z-mount mirrorless
  generation as the Z8, confirmed via `exiftool`), not yet candidate-vs-LRC on the real Z8.

- **RawNIND** (`doi:10.14428/DVN/DEQCIM`, CC-BY-SA-4.0) was picked over ELD (the other real public
  candidate, Nikon D850 only, a different-generation DSLR) specifically because it contains real
  Z6 raw files. 60 real NEFs total across 5 static scenes (bananapi, couch, sewingmachine, Iain01,
  Iain02), all SHA1-verified against their filename-embedded hash. **Only 3 of the 5 were
  independently confirmed Z6 and actually used for measurement**: bananapi, couch, sewingmachine,
  each a base-ISO-50 ground truth plus a full climbing ISO ladder to 51200 (confirmed via
  `exiftool`) — the same shape the original tripod-shoot plan wanted. Iain01/Iain02 were downloaded
  but never independently verified or used; RawNIND's own published composition spans more than
  one camera body, so don't assume those two match the other three's Z6/ISO range.

- **Path A (Bayer-domain joint demosaic+denoise) was investigated and moved to v2, not built.**
  Two real candidates were found — BJDD (CVPRW21, dedicated pretrained Bayer-CFA weights) and
  demosaicnet_pytorch (Gharbi et al. 2016, the seminal joint demosaic+denoise CNN) — both had
  concrete blockers, not simple absence. BJDD's weights are Google-Drive-hosted PyTorch
  checkpoints, and its own `attentionGen.py` shows a 3-channel `inputConv`, meaning it has its own
  undocumented pre-demosaic preprocessing convention that would need reading its `dataTools`
  sampling code to replicate — not a drop-in feed from a raw Bayer mosaic. demosaicnet_pytorch's
  own README FAQ states plainly that its noise-aware model "is not implemented" in this maintained
  port, pointing to an old unmaintained Caffe repo instead — it doesn't meet Path A's bar as
  shipped at all. Given the already-expected "sensor-mismatched, likely fails" prior for any
  off-the-shelf Bayer-domain model, the remaining PyTorch→ONNX conversion cost (torch install,
  reverse-engineering BJDD's preprocessing, state-dict loading, export) didn't clear its expected
  research value inside the ticket's 2-day cap.

- **Path B, provisional pick: classic AHD demosaic (FBDD 0) + SCUNet-PSNR.** Two real,
  ONNX-ready candidates were measured — NAFNet-SIDD-width64 (upstream MIT+Apache-2.0, SIDD
  real-photo noise training) and SCUNet-PSNR (upstream Apache-2.0, trained on purely synthetic
  degradations — no real-photo dataset provenance question at all, the cleanest of any candidate
  considered). Both sourced as ready ONNX exports from the public `deepghs/image_restoration` HF
  repo (MIT re-export license), sidestepping the PyTorch→ONNX conversion the plan originally
  expected to need. Both candidates beat the classic-demosaic-only baseline in every one of 3 real
  scenes measured **at the 256px-crop level** — a full-resolution no-denoise baseline was never
  measured this pass, so at full 6064×4040 resolution the comparison is NAFNet-vs-SCUNet-vs-ground-
  truth only, not vs. an undenoised baseline. At full resolution,
  SCUNet won PSNR+SSIM cleanly on 2 of 3 scenes; NAFNet won **both** PSNR and SSIM on the third
  (sewingmachine) — a clean win there, not a split. The crop-level sample for that same scene had
  shown a PSNR-vs-SSIM disagreement between the two candidates that didn't hold once measured on
  the full frame, meaning the crop was an atypical patch rather than a real, unresolved metric
  conflict.

- **LibRaw's wavelet denoise is broken in the vendored PR#826 fork, not fixed.** Any nonzero
  `threshold` corrupts `imgdata.image`'s real contents in some way at every magnitude tested (1.0,
  100.0, 400.0) — not root-caused to a specific confirmed mechanism this pass (see below), so
  `retina_libraw_process_classic` now rejects any nonzero `threshold` outright rather than trusting
  a caller to detect the corruption after the fact. `spikes/retina/vendor/LibRaw/src/postprocessing/
  postprocessing_aux.cpp` shows `wavelet_denoise()` operating on still-mosaiced Bayer data via a
  `BAYER(row,col)` macro (dcraw-legacy, pre-demosaic), not post-demosaic as the ticket's original
  design assumed — an observed processing path, not a confirmed root cause: `retina_classic_image`
  always reports a fixed `iwidth*iheight*4` length regardless of what LibRaw's own internals
  actually allocated, so this evidence doesn't by itself establish a single-channel allocation or
  that any length check actually caught one. The classic-NR baseline for this pass uses FBDD only
  (0/1/2, all confirmed working); wavelet denoise is a scoped, documented limitation, not
  root-caused further.

- **Real Windows-native CUDA numbers, not sandbox scaffolding.** This is the first Nicti research
  pass on a genuine GPU/CUDA/TensorRT reference environment (installed during this pass: CUDA
  13.4, cuDNN 9.26.0.51, TensorRT 10.16.1.11, onnxruntime-gpu 1.30.0 — all via winget/pip, no
  NVIDIA login needed). SCUNet on a 256px crop: 44.8ms (CUDA) vs 1605.8ms (CPU), a ~36x speedup,
  quality matched to 4 decimal places (confirms genuine EP use — `ort` 2.0.0-rc.13's `Session`
  doesn't expose which EP served a call after the fact, so the speedup plus matching quality is
  the practical confirmation). Full 6064×4040 frame: NAFNet 30.7s total wall time (the CPU version
  never finished after 90+ minutes); SCUNet 50.9s.

- **A real model-architecture constraint was found and fixed, not a bug in Nicti's own design.**
  SCUNet's Swin-window self-attention needs each downsampled internal feature map's spatial
  dimensions evenly divisible by its window size. 512px tiles fail outright with a clean,
  correctly-propagated `ort` reshape error (not a crash); 256px tiles work for interior tiles but
  still failed on `rods`'s own naive edge/remainder-tile shrinking at the image border. Fixed via
  `build_padded_tile`: every tile is now clamp-to-edge padded to a fixed size before inference
  (replicating the source image's last real row/column, never zero-filled), with only the tile's
  real region used for blending — a general fix, not SCUNet-specific in its code, verified on real
  hardware post-fix. NAFNet has no equivalent constraint.

- **Two real alignment bugs were found and fixed while validating this.** First: the sub-pixel
  shift estimate was being re-computed per-candidate on the *post-denoise* image, and a smoother
  denoiser's different gradient structure shifts where Lucas-Kanade converges — SCUNet's smoother
  output tripped a spurious misregistration warning that NAFNet's didn't, on the identical raw
  candidate. Fixed: shift is now always estimated once from the raw, pre-denoise candidate,
  independent of which candidate gets scored. Second: the fix for the first bug (always measuring
  shift pre-denoise) surfaced that resampling the candidate onto the reference's pixel grid before
  scoring — previously flagged as an unimplemented gap — was in fact load-bearing: RawNIND's couch
  scene has a genuine ~1.1px real shift between its ground truth and high-ISO frames (real
  exposure-to-exposure movement, not decoder rounding). `resample_rgb` (Lucas-Kanade-estimated
  shift, bilinear resample) now runs before any scoring or denoising for every comparison.

- **Fixed color treatment used for scoring**: camera RGB → XYZ(D50) via LibRaw's own no-profile
  camera matrix → linear sRGB (a hardcoded, published Bradford-adapted matrix, a standard
  ICC-tooling constant, not project data) → sRGB OETF, applied identically to every candidate.
  Deliberately not ADR-0021's real DCP/HueSatMap/LookTable color pipeline, which is still its own
  open research pass this ticket didn't want to block on or duplicate.

- **A real Rust toolchain-shadowing bug was found and fixed getting Windows-native testing
  working.** `cargo build --target x86_64-pc-windows-gnu` failed with "can't find crate for
  core/std" despite the target being installed (confirmed via a fresh reinstall too). Cause: PATH
  resolved both `cargo` and `rustc` to Homebrew's Rust install, which has no Windows target support
  at all, rather than rustup's — pointing only `cargo` at rustup's toolchain wasn't sufficient,
  since cargo itself shells out to a bare `rustc` looked up via PATH again. Fix: rustup's toolchain
  directory first on PATH for both binaries. `rods` then cross-compiles to Windows cleanly, with no
  C/C++ dependency to fight (unlike `retina`'s vendored LibRaw submodule).
