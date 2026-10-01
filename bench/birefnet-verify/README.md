# birefnet-verify (#348)

Checks nicti's pinned `onnx-community/BiRefNet-ONNX` conversion against the upstream
`ZhengPeng7/BiRefNet` PyTorch checkpoint (both revisions pinned in `verify.py`): output masks on a
fixed image set, plus a layout-agnostic weight comparison. Results and their limits are recorded in
`docs/licensing.md` (BiRefNet row, footnote m1). Not part of the build or CI; needs PyTorch.

```
python bench/birefnet-verify/verify.py --out result.json
```
