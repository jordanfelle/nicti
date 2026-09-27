# #35: subject grouping (`spikes/rosette`)

Full write-up backing `docs/adr/0035-subject-grouping.md`. See that ADR for the decision rule and
Consequences; this doc is the method, license findings, and reproduction detail.

## Method

Cluster a shoot's photos by subject/person so review can happen per-subject. Most subjects are
fursuiters, not bare human faces, so a face-recognition model (InsightFace/RetinaFace) is both
license-excluded (`docs/licensing.md`: non-commercial research only) and the wrong tool even if it
weren't — those models are trained to find human facial landmark geometry, which a fursuit head
doesn't have.

A comment on the issue (2026-09-27) found a directly on-topic reference: [Fursee: Hybrid
YOLO-DINOv3 Framework for Fursuit Identity Retrieval and
Clustering](https://arxiv.org/html/2606.22872v1). Its pipeline: YOLO26l crops the fursuit head
region out of full frames, a DINOv3 ViT embeds the crop (attention layers fine-tuned with ArcFace
loss for angular discrimination between similar-looking fursuits), and DBSCAN with
silhouette-coefficient-guided adaptive hyperparameter selection clusters the embeddings — no fixed
cluster count needed. Reported results: 93.33% retrieval hit rate (vs. 85% for a general-purpose
VLM prompted zero-shot) and clustering F1 0.8755 (vs. 0.7043 for zero-shot prompting) — a
purpose-built detect→embed→cluster pipeline meaningfully beats prompting a general vision-language
model on this task.

This spike adopts the embed→cluster shape (DINOv2/DINOv3/OpenCLIP → DBSCAN + silhouette sweep) but
treats the detection stage as an ablation, not a given: the issue comment itself noted that a full
YOLO detection stage may be overkill when the subject is already framed by a photobooth-style shot
(unlike Fursee's own dataset, which presumably includes wider con-floor candids where a subject
occupies only part of the frame). ArcFace fine-tuning is noted as a possible future refinement (it
needs a labeled fursuit dataset to fine-tune against) but isn't attempted this pass — plain
pretrained embeddings are the baseline being measured first.

## Why not litter's own grouping algorithm

`spikes/litter/src/group.rs` links frame *i* to a following frame within a `(max_gap_secs,
min_similarity, max_lookahead)` budget — sequence-constrained, by design, since burst/duplicate
detection only ever needs to look a few frames ahead in capture order. Subject grouping has no
such constraint: the same person can appear at the start of a shoot, then again three hours later,
with dozens of unrelated photos in between. `spikes/rosette` implements DBSCAN over the full
pairwise cosine-distance matrix instead (`cluster.rs`), which has no notion of "nearby in time" at
all — exactly the shape this problem needs, and the same clustering family Fursee itself uses.

## Backbones compared

- **DINOv2 ViT-S/14** — adapted unchanged from `spikes/litter/src/embed.rs::Dinov2Embedder` (224px
  resize + center-crop, ImageNet normalization, CLS-token `last_hidden_state[:, 0, :]`, 384-dim).
  Already run against a real `onnx-community/dinov2-small` export in ADR-0033's own pass.
- **DINOv3 ViT-S/16** — same CLS-token convention. License-gated (see below); ONNX export
  confirmed to exist (`onnx-community/dinov3-vits16-pretrain-lvd1689m-ONNX`) but not obtained this
  pass (Hugging Face gate requires a manual, individual accept — no scripted/CI download exists).
- **OpenCLIP** — no ready-made ONNX export of a clean-license, non-LAION checkpoint exists (see
  License findings). `embed.rs::OpenClipEmbedder` is implemented against the expected export
  shape (a `[1, dim]` pooled, L2-normalized image embedding, CLIP's own normalization constants —
  not ImageNet's), documented below as a reproducible export recipe, not run this pass.

## License findings

- **DINOv3 (Meta DINOv3 License)** — read directly from `facebookresearch/dinov3/LICENSE.md` and
  `ai.meta.com/resources/models-and-libraries/dinov3-license` (2026-09-27, both fetched since
  neither alone clearly surfaced every clause): commercial use permitted, a non-exclusive,
  worldwide, **non-transferable**, royalty-free limited license. Redistribution is allowed if the
  Agreement text ships alongside the materials, **plus a "Built with DINOv3" attribution**
  displayed on any related website/UI/about-page/product documentation — a real condition, not a
  formality, and the kind of thing worth remembering at the point #36 actually adopts a backbone.
  Acceptable-use restrictions (no military/ITAR/nuclear/espionage/weapons use) don't affect Nicti.
  **Verdict: bundle OK with the attribution notice + Agreement text shipped.** Practically,
  though, the `onnx-community` ONNX exports on Hugging Face are gated behind Meta's own
  click-through acceptance (an individual HF account, no scripted/CI path) — fetching the file is
  a one-time manual step either way, which fits ADR-0218's on-demand-download model for AI
  features regardless of the license verdict.
- **OpenCLIP** — the one community ONNX export family found via search (`RuteNL/MobileCLIP2-*-
  OpenCLIP-ONNX`, re-exporting Apple's MobileCLIP2) turned out to be built on
  `apple/ml-mobileclip`, whose `LICENSE_MODELS` is **research/non-commercial only**: "license...
  exclusively for Research Purposes," where "Research Purposes does not include any commercial
  exploitation, product development or use in any commercial product or service." Redistribution
  is permitted, but only under the same restriction — every downstream recipient inherits it.
  **Excluded**, same category as InsightFace/RetinaFace. No torch/Python was available in this
  sandbox to export a different, clean-license OpenCLIP checkpoint, so `OpenClipEmbedder` in this
  spike is implemented against an *expected* export shape rather than a real, run model.

### OpenCLIP export recipe (documented, not run this pass)

A standard, non-LAION-trained OpenCLIP checkpoint (MIT-equivalent per `docs/licensing.md`'s own
OpenCLIP row) can be exported to ONNX with `open_clip`'s own PyTorch model and a plain
`torch.onnx.export` call — no custom export tooling exists upstream, but none is needed:

```python
import torch
import open_clip

# ViT-B-32 on datacomp_xl_s13b_b90k: a standard size/architecture, DataComp- not LAION-trained.
model, _, preprocess = open_clip.create_model_and_transforms(
    "ViT-B-32", pretrained="datacomp_xl_s13b_b90k"
)
model.eval()

class ImageTower(torch.nn.Module):
    def __init__(self, clip_model):
        super().__init__()
        self.clip_model = clip_model

    def forward(self, pixel_values):
        # encode_image already L2-normalizes internally for open_clip's own zero-shot usage;
        # this wrapper exposes exactly the pooled [1, dim] embedding `OpenClipEmbedder` expects.
        features = self.clip_model.encode_image(pixel_values)
        return torch.nn.functional.normalize(features, dim=-1)

wrapper = ImageTower(model)
dummy_input = torch.randn(1, 3, 224, 224)
torch.onnx.export(
    wrapper,
    dummy_input,
    "rosette-openclip.onnx",
    input_names=["pixel_values"],
    output_names=["image_embeds"],
    dynamic_axes={"pixel_values": {0: "batch"}, "image_embeds": {0: "batch"}},
    opset_version=17,
)
```

Preprocessing to match: CLIP's own normalization constants (`embed.rs::CLIP_MEAN`/`CLIP_STD`,
distinct from DINOv2/DINOv3's ImageNet constants), 224×224, matching `open_clip`'s own
`preprocess` transform for `ViT-B-32`. `ViT-B-32`'s image tower outputs 512-dim embeddings — pass
`--openclip-dim 512` to `rosette`'s CLI (the default) when using this exact checkpoint; a
different model size needs a different `--openclip-dim`.

Once exported, `openclip_embeds_two_similar_frames_closer_than_a_different_one` in `embed.rs`
(currently `#[ignore]`d) can run unchanged against `rosette-openclip.onnx` via
`NICTI_TEST_OPENCLIP_ONNX`.

## Ablation: full-frame vs. subject crop

`crop.rs` takes a precomputed alpha mask (same shape as `spikes/siamese/src/segment.rs::Alpha`),
finds its tight bounding box, pads it 20% on each side, and crops. No BiRefNet inference is wired
in directly — `siamese` itself never obtained real weights either (its own doc comment: a ~970MB
export, out of that pass's time budget) — so this pass proves the crop-geometry logic against
synthetic masks (unit-tested: bbox-from-mask, padding, clamping at image edges) rather than a
real subject-detection result. `rosette eval` runs both arms (full-frame, then crop) for a given
backbone in one invocation, falling back to full-frame with a warning for any photo missing a
precomputed mask file (`<nef_dir>/masks/<stem>.mask.json`).

## Burst collapse

`collapse.rs` optionally averages a burst set's embeddings into one representative before
clustering, so a large burst doesn't dominate a DBSCAN neighborhood relative to a subject with
only one or two photos. It takes plain `&[usize]` group ids rather than depending on `litter`
directly (spikes can't depend on other spikes) — wiring litter's own tight/set output in as the
group-id source is a real integration point, left to whoever picks up the measurement pass or
#36's own production integration.

## What's real vs. pending

**Real, tested this pass**: 48 unit tests (`cluster.rs`'s DBSCAN + silhouette-sweep correctness on
synthetic Gaussian-style blobs, `collapse.rs`'s averaging + round-trip expansion, `crop.rs`'s
bbox/padding/clamping geometry, `metrics.rs` copied unchanged from litter, `label.rs`'s contact
sheet generation including the same `</script>`-escaping regression test litter's page needed,
and `nef.rs`/`decode.rs`/`source.rs` copied unchanged). `cargo clippy -p rosette --all-targets` and
`cargo fmt --all -- --check` both clean. `cargo deny check licenses` passes with no new allowlist
entry needed (the crate carries the workspace's own AGPL-3.0-or-later license, unlike `nicti`/
`bench/whisker`, which needed a `[licenses.private]` exemption for having none).

**Adversarial review, before opening the PR**: a fresh agent reviewed the diff hostilely.
CONFIRMED: `bbox_from_alpha` index-panicked on a mask JSON whose `data` length didn't match
`width * height` (a hand-supplied `<nef_dir>/masks/<stem>.mask.json` can trivially trigger this) —
fixed to return `None` (full-frame fallback) instead, with a regression test. SPECULATIVE, fixed
anyway since both were cheap: `mean_silhouette`'s `(b - a) / a.max(b)` could compute `NaN` when
both means are exactly `0.0` (not reachable through this crate's own `dbscan_with_eps_sweep`, but
a real gap in a `pub` function a future caller — e.g. #36's own integration — could hit); and
`Dinov2Embedder`/`Dinov3Embedder`'s CLS-token slice could index-panic against a malformed `.onnx`
model declaring a zero sequence length, untested since neither DINOv3 nor OpenCLIP has run against
a real model yet. Both fixed with regression tests (see `cluster.rs`/`embed.rs`). The DBSCAN
algorithm itself, the ADR's/licensing.md's factual claims, and the noise-singleton scoring scheme
in `main.rs::eval` were all checked against the actual code/behavior and found correct — the
review's own verification re-ran `cargo test`/`clippy`/`fmt`/`cargo deny check licenses` rather
than trusting this doc's claims.

**Pending**: DINOv3's Hugging Face gate hasn't been accepted (no `.onnx` file obtained); no
OpenCLIP checkpoint has been exported (no torch/Python in this sandbox); no real labelled con shoot
exists to measure accuracy against, same constraint ADR-0033 already documented for its own
ground truth. See ADR-0035's Measured results section for the exact procedure once both
preconditions are met.

## Reproducing

```bash
# Once a DINOv2/DINOv3/OpenCLIP .onnx file and a real ONNX Runtime shared library exist:
cargo test -p rosette -- --ignored

# Draft a clustering for manual correction:
cargo run -p rosette -- draft <nef-dir> --work <scratch-dir> --backbone dinov2 \
    --model-onnx <path-to-dinov2.onnx> --ort-dylib <path-to-libonnxruntime.so>

# After correcting labels.json in the browser:
cargo run -p rosette -- eval --nef-dir <nef-dir> --labels labels.json --backbone dinov2 \
    --model-onnx <path-to-dinov2.onnx> --ort-dylib <path-to-libonnxruntime.so>
```
