import glob, json, sys, time
import numpy as np
import onnxruntime as ort
from PIL import Image

import os
S = os.environ.get("BENCH_DIR", ".")  # holds img/p*.jpg and models/*.onnx
IMN = ((0.485, 0.456, 0.406), (0.229, 0.224, 0.225))
# name: (file, size, mean, std, activation)
MODELS = {
    "birefnet_fp32": ("birefnet_fp32.onnx", 1024, *IMN, "sigmoid"),
    "birefnet_fp16": ("birefnet_fp16.onnx", 1024, *IMN, "sigmoid"),
    "birefnet_lite_fp32": ("birefnet_lite_fp32.onnx", 1024, *IMN, "sigmoid"),
    "birefnet_lite_fp16": ("birefnet_lite_fp16.onnx", 1024, *IMN, "sigmoid"),
    "isnet_general": ("isnet-general-use.onnx", 1024, (0.485, 0.456, 0.406), (1.0, 1.0, 1.0), "minmax"),
    "u2net": ("u2net.onnx", 320, *IMN, "minmax"),
    "u2netp": ("u2netp.onnx", 320, *IMN, "minmax"),
}
only = sys.argv[1:] or list(MODELS)
imgs = sorted(glob.glob(S + "/img/p*.jpg"), key=lambda p: int(p.split("/p")[-1][:-4]))
threads = 8


def prep(img, size, mean, std):
    a = np.asarray(img.convert("RGB").resize((size, size), Image.BILINEAR), dtype=np.float32) / 255.0
    a = (a - np.array(mean, np.float32)) / np.array(std, np.float32)
    return a.transpose(2, 0, 1)[None].astype(np.float32)


def post(out, act):
    o = out.astype(np.float32)[0, 0]
    if act == "sigmoid":
        return 1.0 / (1.0 + np.exp(-o))
    mn, mx = o.min(), o.max()
    return (o - mn) / (mx - mn + 1e-8)


res = {}
for name in only:
    f, size, mean, std, act = MODELS[name]
    so = ort.SessionOptions()
    so.intra_op_num_threads = threads
    t0 = time.time()
    sess = ort.InferenceSession(f"{S}/models/{f}", so, providers=["CPUExecutionProvider"])
    load = time.time() - t0
    inp = sess.get_inputs()[0]
    fp16 = "float16" in inp.type
    outs = []
    lat = []
    for i, p in enumerate(imgs):
        x = prep(Image.open(p), size, mean, std)
        if fp16:
            x = x.astype(np.float16)
        t = time.time()
        o = sess.run(None, {inp.name: x})
        lat.append(time.time() - t)
        m = post(o[0] if act == "sigmoid" else o[0], act)
        m = np.asarray(Image.fromarray((m * 255).astype(np.uint8)).resize((512, 512), Image.BILINEAR), np.float32) / 255
        outs.append(m)
    res[name] = {"lat": lat, "load": load, "masks": outs, "in": inp.name, "type": inp.type}
    print(name, "load %.1fs" % load, "first %.2fs" % lat[0], "warm median %.2fs" % np.median(lat[1:]), flush=True)

np.save(S + "/masks_" + "_".join(only) + ".npy", {k: v["masks"] for k, v in res.items()}, allow_pickle=True)
json.dump({k: {"lat": v["lat"], "load": v["load"]} for k, v in res.items()}, open(S + "/lat_" + "_".join(only) + ".json", "w"))
