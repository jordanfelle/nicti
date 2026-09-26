---
paths:
  - "spikes/rods/**"
  - "spikes/retina/src/classic.rs"
  - "spikes/retina/src/cfa.rs"
  - "crates/nicti-prowl/src/metrics.rs"
---

# Demosaic and Noise Reduction — Quick Reference

Full reasoning/history: `docs/decisions/denoise.md`.

- **Demosaic + NR (#40)** — `docs/adr/0024`: **Proposed** — pending real LRC comparison and a real
  Z8 tripod verification pass (weeks/months out); every quality number measured so far is
  candidate-vs-ground-truth on **RawNIND's real Nikon Z6** frames, not candidate-vs-LRC on Z8.
- **Path A (Bayer-domain joint demosaic+denoise) moved to v2, not built** — real candidates found
  (BJDD, demosaicnet_pytorch) both had genuine blockers (BJDD's own undocumented 3-channel input
  convention + Drive-hosted PyTorch weights; demosaicnet's noise-aware variant never shipped).
- **Path B pick (provisional): classic AHD demosaic (FBDD 0) + SCUNet-PSNR.** Beat NAFNet-SIDD on
  PSNR+SSIM cleanly on 2/3 scenes at full resolution; NAFNet wins both metrics on the third
  (sewingmachine). Cleanest training-data provenance of any candidate (purely synthetic
  degradations). Both `deepghs/image_restoration` ONNX re-exports (MIT), no
  PyTorch conversion needed.
- **LibRaw's wavelet denoise is broken in the vendored PR#826 fork** — any nonzero `threshold`
  corrupts `imgdata.image`'s buffer size, at every magnitude tested. Not fixed; classic-NR baseline
  uses FBDD only. Root cause: `wavelet_denoise()` operates on pre-demosaic Bayer data via a
  `BAYER()` macro, not post-demosaic.
- **Real CUDA speedup confirmed, Windows-native**: ~36x over CPU on a 256px crop, quality matched
  to 4 decimals (confirms genuine EP use, not silent CPU fallback — `ort` 2.0.0-rc.13's `Session`
  doesn't expose which EP served a call). Full 6064×4040 frame: NAFNet 30.7s, SCUNet 50.9s
  (CPU version of NAFNet never finished after 90+ minutes).
- **SCUNet needs every tile — including edge/remainder tiles — sized as multiples compatible with
  its window-attention divisibility constraint.** `spikes/rods`'s `build_padded_tile` clamp-to-edge
  pads every tile to a fixed size before inference; fixes a real ONNX reshape failure this same
  constraint caused at the image border. General, not SCUNet-specific in its code.
- **Fixed color treatment for comparison**: camera RGB → XYZ(D50) via LibRaw's no-profile matrix →
  linear sRGB (published Bradford-adapted constant) → sRGB OETF — deliberately not ADR-0021's real
  DCP pipeline (still its own open research pass).
- **Toolchain gotcha**: cross-compiling to `x86_64-pc-windows-gnu` needs rustup's toolchain
  directory first on PATH for *both* `cargo` and `rustc` — cargo shells out to a bare `rustc`
  lookup, so pointing only `cargo` at rustup while `rustc` still resolves to Homebrew's Rust (no
  Windows target) fails with a misleading "can't find crate for core/std".
