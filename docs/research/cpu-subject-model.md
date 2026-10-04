# Research: a CPU-viable subject model (#349)

Follow-up to #49. BiRefNet (Swin-L, fp32, 972 MB) takes ~9 s warm on the CPU provider, so Select Subject
is not interactive without a GPU execution provider (#345). Question: does a smaller, licence-clear
model make it interactive on CPU, and is fp16/int8 BiRefNet any faster?

## Method

`bench/subject-model-bench/` (`extract.py` builds the inputs, `bench.py` runs them). 16 photos from the
NictiBench subset (anthrocon 2024/2025, mff2024; Nikon Z 8 NEFs; single fursuiters, groups, crowded
halls), using each NEF's embedded full-size JPEG (45 MP) **rotated upright per the NEF's EXIF
Orientation** (13 of the 16 are portrait; an earlier run that skipped this fed them sideways and badly
understated the small models -- IS-Net's mean IoU went 0.71 -> 0.90 once upright). Each model gets its
documented preprocessing (1024 px square for BiRefNet/IS-Net, 320 px for U²-Net; aspect squashed),
ONNX Runtime CPU provider, 8 intra-op threads, one session per model, first run excluded from the warm
median. Agreement = IoU of the >0.5 mask against BiRefNet fp32 at 512x512, plus a visual contact sheet of
the worst/median/best images. IS-Net/U²-Net outputs are min-max rescaled per image (rembg's
convention); thresholding the raw sigmoid instead gives the same IoU to 3 digits.

**Caveats.** Run on the WSL2 dev box (32 logical cores), not the Windows reference machine. BiRefNet fp32
measured 8.0 s warm here vs 9.2 s there (~15% apart), so model-to-model *ratios* should carry over; absolute
times on the reference machine are not established. No hand-labelled ground truth: IoU measures agreement
with BiRefNet, not correctness, and "the subject" in a crowded hall is ambiguous. 16 images is a smoke
test; #171 remains the real-quality pass. Inputs need the NictiBench NEFs (not in the repo).

## Results

| Model | Size | Warm median | Speedup | IoU vs BiRefNet, mean / median / min |
|---|---|---|---|---|
| BiRefNet fp32 (shipped) | 973 MB | 8.0 s | 1.0x | -- |
| BiRefNet fp16 | 490 MB | 9.9 s | 0.8x (slower) | 1.00 / 1.00 / 1.00 |
| BiRefNet-lite fp32 | 224 MB | 4.9 s | 1.6x | 0.95 / 0.99 / 0.51 |
| BiRefNet-lite fp16 | 115 MB | 5.2 s | 1.5x | 0.95 / 0.99 / 0.51 |
| IS-Net general-use | 179 MB | 0.56 s | 14x | 0.90 / 0.92 / 0.66 |
| U²-Net | 176 MB | 0.34 s | 24x | 0.81 / 0.92 / 0.37 |
| U²-Netp | 4.6 MB | 0.14 s | 57x | 0.83 / 0.88 / 0.53 |

No int8 BiRefNet exists in the onnx-community conversions (fp32/fp16 only); quantizing it ourselves is a
separate piece of work and was not attempted.

## Findings

- **fp16 is a loss on CPU**, as #49 predicted: the ORT CPU provider has thin fp16 kernels, so BiRefNet fp16
  is *slower* (9.9 vs 8.0 s) for half the size, with the same masks. Not worth shipping on CPU.
- **BiRefNet-lite is near-identical to BiRefNet on 15 of 16 images** (median IoU 0.99, same crisp fur-aware
  edges) at 1.6x the speed and a quarter of the download -- but 4.9 s is still not interactive. Its one
  weak image (IoU 0.51) covers about half BiRefNet's area (5% vs 10% of the frame), i.e. it drops part of
  the subject.
- **IS-Net general-use is the only interactive candidate (0.56 s) and it picks the right subject**
  (mean IoU 0.90, worst 0.66), but its mask is visibly worse than BiRefNet's: soft edges (5.5% of
  pixels between 0.1-0.9 alpha vs 0.7%), grey/leaky interiors on dark or busy fursuits, and halo around
  limbs. U²-Net and U²-Netp are faster still but fragment or miss more (min IoU 0.37 / 0.53).
- So the real tradeoff is **BiRefNet-lite (quality, ~5 s) vs IS-Net (rough, ~0.5 s)**; nothing is both
  fast and BiRefNet-grade.

## Recommendation

1. **Don't replace BiRefNet.** The GPU execution provider (#345) remains the route to interactive,
   full-quality Select Subject; fold fp16 BiRefNet into that ticket's measurements (it only helps on GPU).
2. **Two optional, additive providers** behind the existing `SegmentationProvider` registry, for machines
   without a capable GPU -- neither registered here because each needs a pinned artifact, a
   `docs/licensing.md` row (confirm training-data provenance first; IS-Net/U²-Net are Apache-2.0 upstream,
   BiRefNet-lite MIT per its card) and a #171-style real-photo quality pass first:
   - *BiRefNet-lite* as a "faster, same-looking" model (1.6x, 224 MB).
   - *IS-Net general-use* as a "quick rough mask" mode for interactive previews. Its soft edges may
     clean up under the engine's existing guided-filter refine against the photo (untested -- worth a
     spike before judging it).
3. Re-run the harness if a smaller matting-class model with BiRefNet-grade edges appears.
