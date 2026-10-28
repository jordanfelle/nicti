# AI Super Resolution: research for #174

Findings for [#174](https://github.com/jordanfelle/nicti/issues/174) (sibling of #40 under #7): whether and how
Nicti supports an upsampling develop step like LRC's Super Resolution. A research pass, **nothing was run or measured**
(no model was downloaded or executed; this host is a live server and the reference GPU machine was not used). Every
licence statement below was read directly from the cited page on 2026-10-28; anything not read is marked unverified.
The shipping decision belongs to the build ticket filed from this one (#612), not here.

## What LRC's Super Resolution is

- A machine-learning "Enhance" operation: the output has **2x the width and 2x the height, 4x the pixels**, like Raw
  Details but at double linear resolution. It runs on the raw data and writes a **new DNG** next to the original, which
  stays untouched; JPEG/TIFF inputs are also accepted, raw gives the best result ([Adobe's own description as quoted by
  third-party guides](https://digital-photography-school.com/lightroom-super-resolution/); Adobe's help page itself
  returned HTTP 403 to a direct fetch).
- It is rare in the real catalog: **6 of 380,307** develop-settings rows (#157's measurement).
- LRC treats it as a one-shot, per-photo, slow operation that produces a derivative, not a live slider. Nicti's LRC
  import already routes the `Super Resolution` filter entries to the provenance blob (`nicti-stray`
  `develop/filters.rs`), so **#62/#379 need no change**; nothing renders it.

## Is a from-scratch model needed? No

Off-the-shelf super-resolution networks exist with permissive code licences and ready ONNX conversions, and the project
already runs ONNX models locally (`ort`, `load-dynamic`, ADR-0019; on-demand checksummed weights, ADR-0218/`nicti-stalk::models`).
Candidates (licence rows are in `docs/licensing.md`'s ML table):

| Candidate | Scale | Code licence | Weights / provenance | Size (fp32 / fp16 ONNX) | Notes |
|---|---|---|---|---|---|
| [Real-ESRGAN](https://github.com/xinntao/Real-ESRGAN) `RealESRGAN_x2plus` (RRDBNet) | native x2 | BSD-3-Clause | "trained with pure synthetic data" (no named dataset); no separate weights licence stated | ~70 MB / ~37 MB (a community ONNX conversion of the same-architecture ai-forever x2 model; xinntao's x2plus itself was not converted or sized here) | Real-world GAN model: sharpens aggressively and can invent texture; trained for noisy/compressed inputs |
| Real-ESRGAN `realesr-general-x4v3` (SRVGGNetCompact) | x4 (to 2x by resizing) | BSD-3-Clause | same | ~5 MB / ~2.5 MB | Tiny and fast, but 4x output then halved: 4x the pixels computed for a 2x result |
| [Swin2SR](https://github.com/mv-lab/swin2sr) Classical x2 / Lightweight x2 | native x2 | Apache-2.0 (README: "also follow" the licences of Swin Transformer, SwinV2, KAIR, BasicSR, SwinIR) | DIV2K + Flickr2K (their own terms **not verified** this pass) | ~72 MB / ~40 MB (Classical x2, community conversion) | Trained on clean bicubic-downscaled images: suits an already demosaiced + denoised photo better than a noise-hallucinating real-world model |
| [HAT](https://github.com/XPixelGroup/HAT) | x4 weights only (no x2 weights named) | Apache-2.0 | ImageNet pre-train; weights on Google Drive/Baidu, licence of the weights not stated | not stated | Strongest benchmark numbers, but no x2 checkpoint and heavy; not a first spike |

Community ONNX conversions exist (e.g. `baker76/upscale-models`, 24 models). That page states each file "remains under the
license its author released the original weights under" and that for some the terms restrict commercial use, with no
per-model licence field: **never adopt a community fine-tune (UltraSharp, AnimeSharp, MewZoom, ...) from such a list
without reading its own source licence**; the three rows above are the upstream-licensed ones.

Recommendation for the first spike (build ticket): **Real-ESRGAN x2plus** (cleanest provenance: BSD-3 code, synthetic
training data) against **Swin2SR Classical x2** (Apache-2.0, trained for clean inputs); `realesr-general-x4v3` as the
speed baseline. Decide on measured output against LRC's own Super Resolution DNGs of the user's 6 catalogued images.

## Where it sits in the pipeline

The stage cannot be a live Develop stage. Arithmetic, not measurement: a Z8 frame is 8280 x 5520 (45.7 MP, ~349 MiB as
RGBA16F, ADR-0044's figure); at 2x it is 16560 x 11040 = 182.8 MP = **~1,395 MiB per frame**, before the baked-prefix
caches, the live suffix's intermediates and the crop pass, all of which would run at that size. So:

1. **Input position: after demosaic + denoise (the baked prefix), before the live suffix.** Super resolution on the
   demosaiced image reuses the existing AHD/denoise work and avoids a second Bayer-domain model (LRC works on the raw
   mosaic; Nicti's Path B keeps classic demosaic, ADR-0040). The models expect display-referred 0..1 RGB, not linear
   ProPhoto: wrap the call in a reversible encode (a per-channel transfer curve with a highlight knee so values above 1
   are not clipped, and no clip to the sRGB gamut), run, decode. Highlight and out-of-gamut handling is the main
   correctness risk to measure.
2. **Phase 1 is a derivative, like Adobe's: an export-time / "Enhance" operation, not an interactive stage.** A document
   stage `nicti.superres` (a switch plus model id and version, ADR-0021's "recipe not pixels") that only the tiled export
   path honours: tiles (e.g. 256 px with 16-32 px overlap, ~925 tiles for a Z8 frame) go through the ONNX model on the
   GPU lane, overlaps are blended, and the 2x result feeds the existing export stages; Develop's preview stays at 1x and
   shows the stage as pending. It joins the export render hash so a model upgrade is an explicit re-run.
3. **Phase 2 (only if wanted): a cached 2x derivative** the loupe can show at 100%, in the Larder's disk tier.

Weights are an on-demand, user-initiated, checksummed download (ADR-0218), exactly as BiRefNet/MobileSAM/LaMa/SCUNet.
Inference never touches the network.

## What is not known, and how the build ticket should measure it

- Tile throughput and total time per 45 MP frame on the reference RTX 5080 (CUDA EP, fp16) and on CPU; memory ceiling
  at the chosen tile size. Gate: a Z8 frame in minutes on the GPU, tens of minutes acceptable on CPU as a background job.
- Quality against LRC's own Super Resolution output on the 6 real images (PSNR/SSIM/LPIPS plus side-by-side at 100%):
  texture hallucination, sky/gradient banding, halo/ringing at tile seams, highlight roll-off.
- Whether fp16 is safe for the chosen model (some SR nets lose precision).
- The licence of the Swin2SR checkpoints' training data (DIV2K/Flickr2K) and of HAT's weights: unverified here, read
  directly before adopting either. Real-ESRGAN's weights licence is not separately stated upstream (BSD-3-Clause is the
  repository licence).

## Sources

- Adobe's description of the 2x-linear / 4x-pixel output, new DNG, raw preferred:
  [digital-photography-school.com](https://digital-photography-school.com/lightroom-super-resolution/) (secondary).
- [Real-ESRGAN README](https://github.com/xinntao/Real-ESRGAN): BSD-3-Clause, "pure synthetic data", model list.
- [Swin2SR README](https://github.com/mv-lab/swin2sr): Apache-2.0, checkpoints, DIV2K + Flickr2K.
- [HAT README](https://github.com/XPixelGroup/HAT): Apache-2.0, x4 weights only, ImageNet pre-train.
- [baker76/upscale-models](https://huggingface.co/baker76/upscale-models): ONNX conversion list, sizes, per-author licence caveat.
