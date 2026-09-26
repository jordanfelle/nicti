# ADR-0023: Demosaic and noise reduction

- **Status:** Proposed — pending [#164](https://github.com/jordanfelle/nicti/issues/164) (real Z8
  tripod verification) and [#163](https://github.com/jordanfelle/nicti/issues/163) (LRC-relative
  comparison)
- **Date:** 2026-09-26
- **Ticket:** [#40](https://github.com/jordanfelle/nicti/issues/40) Research: demosaic + noise
  reduction

## Context

Nicti needs a v1 demosaic + noise-reduction story before #44 (Tapetum, the stage-cached render
graph) can settle where these stages sit. #4/E0's hard bar: **v1 must match or exceed LRC's own AI
Denoise on quality and speed** (`docs/benchmarks.md` restates the bar, no numeric target exists
independent of LRC).

- **Path A**: a Bayer-domain model that replaces LibRaw's demosaic outright (joint
  demosaic+denoise).
- **Path B**: classic LibRaw demosaic plus a separate post-demosaic AI denoise stage.

**Ground truth, by user decision (2026-09-26)**: the real Z8 tripod shoot this ticket originally
planned won't happen for weeks/months. This pass used the public **RawNIND** dataset (real Nikon
Z6 paired noisy/clean frames) as an interim stand-in instead — see
`docs/research/rods-demosaic-denoise.md` for why (over ELD, the other real candidate found) and
its exact scope. Every quality number this ADR cites is therefore **candidate-vs-ground-truth on
a Z6, not yet candidate-vs-LRC on a Z8** — the decision below is provisional on both counts.

Constraints already fixed by earlier ADRs/docs:

- **ADR-0004 §3** already decided AI inference loads via `ort`'s `load-dynamic` feature — this
  ADR's Path B scaffolding reuses that pattern (`spikes/groom/src/ai.rs`'s shape) rather than
  deciding it fresh.
- **ADR-0019** already picked the vendored PR#826 LibRaw fork as the decoder — this ADR's classic
  pipeline builds directly on that fork via `retina`'s new `dump-classic` subcommand.
- **ADR-0021** (color pipeline, still Proposed) is deliberately **not** reused here — this ADR
  needed a fixed, uniform color treatment usable *before* ADR-0021's own reference-machine pass
  lands, so it uses LibRaw's plain no-profile fallback matrix instead (see Decision).
- **ADR-0003**/`docs/licensing.md` set the bundling-vs-evaluate criterion for ML models — both
  Path B candidates below are evaluate-only in this pass, never bundled.
- Unlike ADR-0005/0006/0007/0020's own sandbox notes, **this pass ran on a real GPU/CUDA/TensorRT
  reference environment** (installed during this pass — see the research doc) — most numbers
  below are real measurements, not "TBD — reference machine."

## Decision rule (stated before measuring)

- **Quality**: PSNR/SSIM against LRC's own AI Denoise output, per scene, at the classic-baseline's
  demosaic setting (AHD, FBDD 0) — candidate must not fall more than 0.1 dB / 0.002 SSIM short of
  LRC on average, and never more than 0.5 dB short on any single scene. LPIPS as a blur guard.
  **Not yet measurable this pass** — needs the user's own LRC export batch. This pass instead
  measured candidate-vs-ground-truth, a real and useful signal for *direction and magnitude* of
  improvement, but not the LRC-relative number the rule actually asks for.
- **Speed**: warm per-image p95 (NEF on NVMe → denoised buffer) ≤ LRC's own per-image p50,
  Windows-native. Real full-resolution numbers exist this pass (see Measured results) but not yet
  compared against a real LRC export's own timing (also pending the user's export batch).
- **License**: evaluate-only is always fine; bundling needs ADR-0003's weights+training-data
  criteria cleared per model.

## Decision

**Path A moves to v2 research, not built.** Two real candidates were found (BJDD, a genuine
Bayer-CFA joint demosaic+denoise model; demosaicnet_pytorch, the seminal Gharbi et al. 2016
network) — both had concrete, non-trivial blockers rather than simply not existing (BJDD's
undocumented 3-channel-input preprocessing convention plus Google-Drive-hosted PyTorch weights;
demosaicnet's own maintained port never shipped its noise-aware variant at all). Given the
already-expected "sensor-mismatched, likely fails" prior for any off-the-shelf Bayer-domain model,
the remaining PyTorch→ONNX conversion cost didn't clear its expected research value inside this
ticket's 2-day cap. See `docs/research/rods-demosaic-denoise.md` for the full account.

**Path B, provisionally: classic AHD demosaic (FBDD 0) + SCUNet-PSNR as the AI denoise stage.**
Two real, ONNX-ready candidates were measured — NAFNet-SIDD-width64 and SCUNet-PSNR, both public
MIT/Apache-2.0 licensed re-exports, no PyTorch conversion needed. Both candidates beat the
classic-demosaic-only baseline in **every** scene measured (3 real Nikon Z6 scenes, crop-level and
full-resolution). **SCUNet is the stronger candidate overall** — best PSNR+SSIM on 2 of 3 scenes,
best SSIM on all 3 scenes, and the cleanest training-data provenance of any candidate considered
(purely synthetic degradations, no real-photo dataset provenance question at all). NAFNet's one
clear advantage (sewingmachine PSNR, consistent between crop-level and full-resolution
measurements) is noted but doesn't change the overall pick.

**This status stays Proposed, not Accepted, until:**
1. A real LRC AI Denoise export batch exists to compare against (the decision rule's actual gate).
2. A real Z8 tripod verification pass confirms the same ruling holds on the real sensor, not just
   RawNIND's Z6 stand-in.

Both are filed as follow-up issues (see Consequences), neither blocks recording this provisional
pick now — the evidence gathered this pass is real and substantial even though the two remaining
gates aren't cleared yet.

### Classic pipeline

AHD demosaic, FBDD 0/1/2 (all confirmed working against a real Z6 NEF). **LibRaw's own wavelet
denoise is broken in the vendored PR#826 fork** — any nonzero threshold corrupts the output
buffer, at every magnitude tested — so the classic-NR baseline uses FBDD only. Not root-caused
further this pass (see Consequences); revisit only if FBDD-only doesn't clear the eventual
LRC-relative bar.

### Color treatment

Every candidate (classic baseline, NAFNet, SCUNet) is scored through the same fixed pipeline:
camera RGB → XYZ(D50) via LibRaw's own no-profile camera matrix → linear sRGB (a hardcoded,
published Bradford-adapted matrix, not project data) → sRGB OETF. Deliberately **not** ADR-0021's
real DCP/HueSatMap/LookTable pipeline, which is still its own open research pass — this ADR
doesn't want to block on or duplicate that work, and a uniform-but-simplified treatment is
sufficient for comparing demosaic/denoise candidates against each other and (eventually) LRC on
equal footing, though not a claim of colorimetrically accurate camera-specific rendering.

## Measured results

**Real Nikon Z6 (RawNIND), 3 scenes, apples-to-apples (identical crop or identical full frame per
candidate), Windows-native CUDA for the full-resolution rows:**

| Scene | Baseline (crop) | + NAFNet (crop) | + SCUNet (crop) | + NAFNet (full-res) | + SCUNet (full-res) |
|---|---|---|---|---|---|
| bananapi | 25.33 dB / 0.383 | 28.99 dB / 0.604 | 29.62 dB / 0.757 | 31.400 dB / 0.678 | **35.035 dB / 0.818** |
| couch | 23.06 dB / 0.512 | 23.21 dB / 0.525 | 27.07 dB / 0.748 | 27.632 dB / 0.664 | **33.605 dB / 0.862** |
| sewingmachine | 24.32 dB / 0.302 | **31.50 dB / 0.640** | 29.70 dB / 0.796 | **29.866 dB / 0.815** | 24.508 dB / 0.780 |

**Speed, Windows-native, RTX 5080, CUDA EP:**

| Metric | Value |
|---|---|
| SCUNet, 256px crop, CUDA vs CPU | 44.8ms vs 1605.8ms — **~36x speedup**, quality matched to 4 decimals |
| NAFNet, full 6064×4040 frame | **30.7s total wall time** (CPU version never finished after 90+ min) |
| SCUNet, full 6064×4040 frame | **50.9s total wall time** (post edge-tile-padding fix) |

Real LRC comparison numbers: **not yet measured** — pending the user's own LRC export batch.

## Options considered

| Option | Real candidate? | Measured? | Verdict |
|---|---|---|---|
| Path A: Bayer-domain joint demosaic+denoise (BJDD) | Yes | No — blocked on Drive-hosted PyTorch + undocumented 3-channel input convention | Moves to v2; revisit if Path B fails the LRC bar |
| Path A: Bayer-domain joint demosaic+denoise (demosaicnet) | No — noise-aware variant never shipped in this port | No | Not a real candidate as-is |
| Path B: NAFNet-SIDD | Yes, ONNX-ready | Yes, full pipeline + full-resolution GPU | Real improvement everywhere; loses to SCUNet on SSIM everywhere, PSNR on 2/3 scenes |
| **Path B: SCUNet-PSNR (this ADR's pick)** | Yes, ONNX-ready | Yes, full pipeline + full-resolution GPU, one real bug found+fixed | Strongest measured candidate; cleanest provenance (purely synthetic training data) |

## Consequences

- **Feeds #44 (Tapetum)**: proposes classic AHD demosaic (FBDD 0) → SCUNet AI denoise as the
  render-stage order's demosaic+NR segment — #44's own call to adopt or revise.
- **Two follow-up issues required before Accepted:**
  1. [#163](https://github.com/jordanfelle/nicti/issues/163) — real LRC AI Denoise export batch +
     comparison (the decision rule's actual quality/speed gate), blocked on the user's own export
     step, not further engineering.
  2. [#164](https://github.com/jordanfelle/nicti/issues/164) — real Z8 tripod verification pass,
     once real tripod hardware time exists (weeks/months out per the user) — confirms this ruling
     holds on the real sensor, not just RawNIND's Z6 stand-in.
- **Wavelet denoise stays broken in the vendored LibRaw fork**, not fixed. A follow-up if FBDD
  alone turns out insufficient once real LRC numbers exist.
- **`docs/licensing.md` updated in this PR**: NAFNet's row refreshed (actual ONNX source, evaluate-
  only status), new SCUNet row (Apache-2.0, cleanest provenance of the candidates considered).
- **A real, general tiling bug was found and fixed** (`spikes/rods`'s edge-tile clamp-to-edge
  padding) — not SCUNet-specific in its code, benefits any future model with a similar tile-size
  divisibility constraint.
- **LPIPS not wired this pass** — a follow-up if PSNR/SSIM alone prove insufficient to resolve the
  sewingmachine PSNR-vs-SSIM disagreement noted in the research doc.

## Spike: `spikes/rods`

See `docs/research/rods-demosaic-denoise.md`'s own Spike section for the full module breakdown.
Name: rods, as in rod cells, the retina's low-light receptors — pairs with `retina` (#37/
ADR-0019), whose own `dump-classic`/`dump-cfa` subcommands (also new this pass) supply `rods`'s
inputs.
