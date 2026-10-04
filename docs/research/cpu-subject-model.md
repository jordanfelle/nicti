# Research: a CPU-viable subject model (#349)

Follow-up to #49. BiRefNet (Swin-L, fp32, 972 MB) takes ~9 s warm on the CPU provider, so Select Subject
is not interactive without a GPU execution provider (#345). Question: does a smaller, licence-clear
model make it interactive on CPU, and is fp16/int8 BiRefNet any faster?

## Method

`bench/subject-model-bench/bench.py`. 16 photos from the NictiBench subset (anthrocon 2024/2025, mff2024,
Nikon Z 8 NEFs, a mix of single fursuiters, groups and crowded halls), using each NEF's embedded
full-size JPEG (8256x5504, unrotated). Each model gets its own documented preprocessing
(1024 px for BiRefNet/IS-Net, 320 px for U²-Net), ONNX Runtime CPU provider, 8 intra-op threads,
one session per model, first run excluded from the warm median. Agreement = IoU of the >0.5 mask
against BiRefNet fp32 at 512x512, plus a visual contact sheet of the worst/median/best images.

**Caveats.** Run on the WSL2 dev box (32 logical cores), not the Windows reference machine -- but BiRefNet
fp32 measured 7.9 s warm here vs 9.2 s there, so absolute numbers carry over within ~15%. There is no
hand-labelled ground truth: IoU measures agreement with BiRefNet, not correctness, and a "subject"
in a crowded hall is ambiguous. Previews are unrotated (orientation does not affect the models'
relative behaviour). 16 images is a smoke test, not a benchmark; #171 remains the real-quality pass.

## Results

| Model | Size | Warm median | vs BiRefNet | IoU mean (min) |
|---|---|---|---|---|
| BiRefNet fp32 (shipped) | 973 MB | 7.9 s | 1.0x | 1.00 |
| BiRefNet fp16 | 490 MB | 10.0 s | 0.8x (slower) | 1.00 (0.998) |
| BiRefNet-lite fp32 | 224 MB | 4.8 s | 1.7x | 0.88 (0.50) |
| BiRefNet-lite fp16 | 115 MB | 5.3 s | 1.5x | 0.88 (0.50) |
| IS-Net general-use | 179 MB | 0.42 s | 19x | 0.71 (0.11) |
| U²-Net | 176 MB | 0.24 s | 33x | 0.62 (0.09) |
| U²-Netp | 4.6 MB | 0.12 s | 66x | 0.69 (0.01) |

No int8 BiRefNet exists in the onnx-community conversions (only fp32/fp16); quantizing it ourselves
would be a separate piece of work and was not attempted.

## Findings

- **fp16 is a loss on CPU**, as #49 predicted: the ORT CPU provider has thin fp16 kernels, so it is
  *slower* (10.0 vs 7.9 s) for half the disk size, with masks identical to fp32 (IoU >= 0.998). Not worth
  shipping. (fp16 only makes sense on a GPU provider -- fold into #345.)
- **BiRefNet-lite is the only candidate that matches BiRefNet's edge quality** (crisp, binary, fur-aware
  boundaries; on the 5 sheet images it was near-identical on the single-subject shots). Its low-IoU
  cases are crowded scenes where it also segments a second person/group that BiRefNet drops -- a
  different notion of "subject", not garbage masks. But at ~4.8 s warm it is only ~40% faster: still not
  interactive.
- **The interactive models fail on the subject we care about.** IS-Net, U²-Net and U²-Netp run in
  0.1-0.4 s but produce soft, unsaturated alphas, miss or fragment fursuiters (fur interiors go
  semi-transparent, a heavily-furred subject can nearly vanish), and have min IoU of 0.01-0.11. They
  would need heavy post-processing and would still be visibly worse than the current mask.
- Nothing is both fast enough (< ~1 s) and good enough. Per the ticket ("if one qualifies, register it")
  **no model is registered**; the registry stays additive for when one appears.

## Recommendation

1. Treat #345 (GPU execution provider) as the path to interactive Select Subject; fold fp16 BiRefNet into
   that ticket's measurements.
2. Optionally offer BiRefNet-lite as a "faster, slightly rougher" model for no-GPU machines: a ~1.7x
   speedup and a 4x smaller download, behind the existing `SegmentationProvider` registry. This needs a
   pinned artifact + `docs/licensing.md` row (MIT per the onnx-community card; confirm training-data
   provenance first) and a #171-style quality pass on real photos before it is worth shipping --
   not done here.
3. Re-open if a distilled/smaller matting-class model with BiRefNet-grade edges appears; the harness
   makes a re-run one command.

Not licence-reviewed (no row added to `docs/licensing.md`): none of these models is adopted. IS-Net and
U²-Net are Apache-2.0 upstream; BiRefNet-lite is MIT per its card. Verify before any adoption.
