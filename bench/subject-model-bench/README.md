# subject-model-bench (#349)

CPU latency + mask agreement (IoU vs BiRefNet fp32) for candidate "Select Subject" models. Results
and the verdict: `docs/research/cpu-subject-model.md`. Not part of the build or CI; needs
`onnxruntime`, `numpy`, `pillow`, `exiftool`.

```
python bench/subject-model-bench/extract.py <dir> <a.NEF> <b.NEF> ...   # EXIF-upright <dir>/img/p<N>.jpg
BENCH_DIR=<dir> python bench/subject-model-bench/bench.py [model ...]  # needs <dir>/models/*.onnx
```

Always extract upright: sideways inputs badly understate the small models. Model files: `birefnet_fp32/fp16.onnx`,
`birefnet_lite_fp32/fp16.onnx` (onnx-community `BiRefNet-ONNX` / `BiRefNet_lite-ONNX`, `onnx/model*.onnx`),
`isnet-general-use.onnx`, `u2net.onnx`, `u2netp.onnx` (rembg release `v0.0.0`). Writes `masks_run.npy` and
`lat_run.json` into `BENCH_DIR`.
