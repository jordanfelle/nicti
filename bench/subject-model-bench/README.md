# subject-model-bench (#349)

CPU latency + mask agreement (IoU vs BiRefNet fp32) for candidate "Select Subject" models. Results
and the verdict: `docs/research/cpu-subject-model.md`. Not part of the build or CI; needs
`onnxruntime`, `numpy`, `pillow`.

```
BENCH_DIR=<dir with img/p*.jpg and models/*.onnx> python bench/subject-model-bench/bench.py [model ...]
```

Model files (see the research doc for sources): `birefnet_fp32/fp16.onnx`, `birefnet_lite_fp32/fp16.onnx`
(onnx-community), `isnet-general-use.onnx`, `u2net.onnx`, `u2netp.onnx` (rembg release `v0.0.0`).
