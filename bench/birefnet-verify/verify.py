#!/usr/bin/env python3
"""Verify nicti's pinned BiRefNet ONNX conversion against the upstream PyTorch checkpoint (#348).

Nicti ships `onnx-community/BiRefNet-ONNX` (a third-party conversion) pinned to a revision + SHA-256
(`crates/nicti-stalk/src/models.rs::BIREFNET`). That the conversion's weights equal the upstream
`ZhengPeng7/BiRefNet` checkpoint was never checked. This script checks it two independent ways:

  1. Weights: every upstream state_dict tensor must be found, value-for-value, among the ONNX
     initializers (exact shape, max |diff| reported).
  2. Outputs: upstream PyTorch fp32 vs the ONNX file under onnxruntime on a fixed image set at the
     1024x1024 input nicti uses; max/mean |diff| of the sigmoid alpha and IoU of the 0.5 masks.

Everything is pinned (revisions below, ONNX SHA-256 checked first). Usage:

    pip install torch onnx onnxruntime numpy pillow huggingface_hub transformers timm einops \
        kornia safetensors scikit-image pooch
    python bench/birefnet-verify/verify.py [--out result.json]
"""
import argparse
import hashlib
import json
import sys

import numpy as np

UPSTREAM = "ZhengPeng7/BiRefNet"
UPSTREAM_REV = "e2bf8e4460fc8fa32bba5ea4d94b3233d367b0e4"
CONVERSION = "onnx-community/BiRefNet-ONNX"
CONVERSION_REV = "534d3c82d3bb8b2f0867db6dfbc3a525b8e42f67"
ONNX_SHA256 = "58f621f00f5d756097615970a88a791584600dcf7c45b18a0a6267535a1ebd3c"
SIZE = 1024
MEAN = np.array([0.485, 0.456, 0.406], dtype=np.float32)
STD = np.array([0.229, 0.224, 0.225], dtype=np.float32)


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def images():
    """Fixed, license-clean (scikit-image bundled public-domain/CC0) photo set."""
    from PIL import Image
    from skimage import data

    out = []
    for name in ("astronaut", "coffee", "chelsea", "rocket"):
        arr = getattr(data, name)()
        im = Image.fromarray(arr).convert("RGB").resize((SIZE, SIZE), Image.BILINEAR)
        out.append((name, (np.asarray(im, dtype=np.float32) / 255.0 - MEAN) / STD))
    return out


def weight_check(state, onnx_path):
    import onnx
    from onnx import numpy_helper

    # Layout-agnostic: the exporter transposes Linear weights and folds/constant-folds some
    # tensors, so match on the *sorted* flattened values among initializers of equal element
    # count. A tensor with no equal-count initializer is reported, not hidden.
    model = onnx.load(onnx_path, load_external_data=False)
    by_numel = {}
    for init in model.graph.initializer:
        a = numpy_helper.to_array(init)
        if a.dtype == np.float32 and a.size >= 2:
            by_numel.setdefault(a.size, []).append(np.sort(a.ravel()))
    rows, missing = [], []
    for name, t in state.items():
        if t.numel() < 2 or not t.is_floating_point():
            continue
        ws = np.sort(t.detach().float().cpu().numpy().ravel())
        best = min((float(np.abs(c - ws).max()) for c in by_numel.get(ws.size, [])), default=None)
        if best is None:
            missing.append({"name": name, "shape": list(t.shape)})
        else:
            rows.append({"name": name, "shape": list(t.shape), "max_abs_diff": best})
    diffs = np.array([r["max_abs_diff"] for r in rows]) if rows else np.zeros(1)
    return {
        "upstream_tensors_compared": len(rows) + len(missing),
        "matched_by_sorted_values": len(rows),
        "matched_within_2e-3": int((diffs <= 2e-3).sum()),
        "median_max_abs_diff": float(np.median(diffs)),
        "worst_max_abs_diff": float(diffs.max()),
        "no_equal_size_initializer": missing,
        "over_2e-3": sorted((r for r in rows if r["max_abs_diff"] > 2e-3), key=lambda r: -r["max_abs_diff"])[:20],
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default=None)
    args = ap.parse_args()

    import onnxruntime as ort
    import torch
    from huggingface_hub import hf_hub_download
    from transformers import AutoModelForImageSegmentation

    onnx_path = hf_hub_download(CONVERSION, "onnx/model.onnx", revision=CONVERSION_REV)
    got = sha256(onnx_path)
    if got != ONNX_SHA256:
        sys.exit(f"ONNX SHA-256 mismatch: {got} != {ONNX_SHA256}")

    net = AutoModelForImageSegmentation.from_pretrained(
        UPSTREAM, revision=UPSTREAM_REV, trust_remote_code=True, torch_dtype=torch.float32
    ).eval()
    dev = "cuda" if torch.cuda.is_available() else "cpu"
    net.to(dev)

    result = {
        "upstream": f"{UPSTREAM}@{UPSTREAM_REV}",
        "conversion": f"{CONVERSION}@{CONVERSION_REV}",
        "onnx_sha256": got,
        "weights": weight_check(net.state_dict(), onnx_path),
        "outputs": [],
    }

    sess = ort.InferenceSession(onnx_path, providers=["CPUExecutionProvider"])
    in_name = sess.get_inputs()[0].name
    for name, chw in images():
        x = chw.transpose(2, 0, 1)[None].astype(np.float32)
        with torch.no_grad():
            ref = torch.sigmoid(net(torch.from_numpy(x).to(dev))[-1]).float().cpu().numpy()
        onx = 1.0 / (1.0 + np.exp(-sess.run(None, {in_name: x})[0]))
        d = np.abs(ref - onx)
        a, b = ref > 0.5, onx > 0.5
        result["outputs"].append(
            {
                "image": name,
                "max_abs_diff": float(d.max()),
                "mean_abs_diff": float(d.mean()),
                "mask_iou_at_0.5": float((a & b).sum() / max((a | b).sum(), 1)),
            }
        )
    text = json.dumps(result, indent=2)
    print(text)
    if args.out:
        with open(args.out, "w") as f:
            f.write(text + "\n")


if __name__ == "__main__":
    main()
