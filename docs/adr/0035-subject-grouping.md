# ADR-0035: Subject grouping

- **Status:** Proposed — measurement pending
- **Date:** 2026-09-27
- **Ticket:** [#35](https://github.com/jordanfelle/nicti/issues/35) Research: face/subject
  grouping

## Context

#35 (Part of epic [#6](https://github.com/jordanfelle/nicti/issues/6), blocks
[#36](https://github.com/jordanfelle/nicti/issues/36)'s AI culling assist integration and
[#108](https://github.com/jordanfelle/nicti/issues/108)'s YOLO-as-culling-candidate research)
started as a one-line migrated stub: cluster images by person/subject for review-per-subject.

**Human face-recognition models don't apply here.** Most subjects at a Shutterpaws con shoot are
fursuiters, not bare human faces — InsightFace/RetinaFace are excluded on license grounds anyway
(`docs/licensing.md`'s row for both: non-commercial research only), but even a licensable
face-recognition model would fail on a fursuit head, which has none of the landmark geometry those
models are trained to find. `docs/adr/0018-third-party-license-policy.md` and `docs/licensing.md`
already steer #35 toward general image embeddings (DINOv2/OpenCLIP) instead, and a comment on the
issue (2026-09-27) found a directly on-topic reference confirming this: [Fursee: Hybrid
YOLO-DINOv3 Framework for Fursuit Identity Retrieval and
Clustering](https://arxiv.org/html/2606.22872v1) reports a detect→embed→cluster pipeline
(YOLO26l head-crop → DINOv3 ViT embedding → DBSCAN with silhouette-guided adaptive hyperparameter
selection) beating zero-shot VLM prompting on exactly this fursuit-identity task (93.33% retrieval
hit rate / 0.8755 clustering F1, vs. 85% / 0.7043 for a general-purpose VLM). The same comment
noted the full YOLO detection stage may be overkill for photobooth-style crops where the subject
is already framed — an open question this ADR treats as an ablation to measure, not an assumption
either way.

**Reused prior art.** ADR-0033 (litter, #33, burst/duplicate grouping) already built and ran a
DINOv2 ViT-S/14 embedder against a real ONNX export, and its own Consequences section says #35
"can reuse `embed.rs`'s DINOv2 wrapper directly — same embedding, different downstream clustering
question." Spikes can't depend on other spikes (this repo's own convention), so `spikes/rosette`
adapts (copies, then extends) rather than imports: `embed.rs`'s preprocessing/session-loading
shape, `metrics.rs`'s B-cubed/ARI scoring verbatim, and `nef.rs`/`decode.rs`/`source.rs` for
reading a NEF's T0 preview unchanged.

**Why not litter's own grouping algorithm.** `spikes/litter/src/group.rs` is sequence-constrained
— it only ever links a frame to a nearby one in capture order, which is the right shape for
burst/duplicate detection but the wrong shape for subject grouping: the same subject can reappear
at any point across a whole shoot, hours apart, with no timestamp relationship at all. `spikes/
rosette` implements DBSCAN over cosine distance instead (matching the Fursee paper's own choice of
clustering algorithm), with `eps` chosen by a silhouette-coefficient sweep so no fixed cluster
count needs to be picked up front.

**User decisions (2026-09-27):**

- Compare three embedding backbones: DINOv2, OpenCLIP, and DINOv3.
- Ablate full-frame vs. a padded subject-crop, rather than assuming either wins.
- Defer the labelled accuracy measurement to a follow-up issue — same "spec + tooling built, real
  ground truth pending a real shoot" shape ADR-0033 already used (that one deferred to
  [#180](https://github.com/jordanfelle/nicti/issues/180)).

## Decision rule (stated before measuring)

- **Precision bar**: B-cubed precision ≥ 0.90. Over-merging is the costly error class here too —
  merging two different subjects into one group hides a distinct person's photos inside someone
  else's review set, which is worse than the merely-annoying alternative of splitting one subject
  across two groups (recall loss).
- **Recall bar**: B-cubed recall ≥ 0.75 — looser than precision, matching the same
  cost-asymmetry reasoning ADR-0033 used for its own tight/set bars.
- **Cost**: embedding runs as a background job ([#54](https://github.com/jordanfelle/nicti/issues/54)/Pounce),
  not inline during ingest — same call ADR-0033 already made for its own DINOv2 signal, since a
  ViT-scale embedding is too slow to run inline at ingest time regardless of which candidate wins.
- **Tie-break**: the cheapest backbone/crop configuration that clears both bars wins, preferring a
  bundle-OK license (see the license findings below) over one that needs a per-user download gate.

## Decision

**New spike: `spikes/rosette`** (a big cat's individually-identifying rosette coat markings —
used by wildlife biologists for real animal-identity clustering — continuing the project's
feline-anatomy/behavior naming convention with a naming angle specific to *identity*, distinct
from litter's "sibling frames" angle). Pure Rust, lib+bin split, not path-gated in CI (same
posture as `litter`/`siamese`/`groom`), depends on no other spike crate.

- **`embed.rs`** — an `Embedder` trait (`embed(&RgbImage) -> Vec<f32>`, `dim()`, `name()`) with
  three implementations, so `cluster.rs` and the CLI compare them interchangeably:
  - **`Dinov2Embedder`** — ported from `spikes/litter/src/embed.rs` unchanged (224px resize +
    center-crop + ImageNet normalize, CLS-token `last_hidden_state[:, 0, :]`, 384-dim). Already
    run against a real `onnx-community/dinov2-small` export in ADR-0033's own pass; not
    re-verified independently here, same input/output contract.
  - **`Dinov3Embedder`** — same CLS-token convention, ViT-S/16. License-gated (see below).
  - **`OpenClipEmbedder`** — written against `open_clip`'s documented pooled-and-L2-normalized
    image-tower output (`[1, dim]`, CLIP's own normalization constants, not ImageNet's). **No
    real ONNX weights obtained this pass** — see License findings below for why.
- **`crop.rs`** — the full-frame-vs-crop ablation. Takes a precomputed alpha mask (same `Alpha`
  shape as `spikes/siamese/src/segment.rs::Alpha`, duplicated rather than imported), finds its
  bounding box, pads it 20% on each side (a crop tight to the mask risks clipping ear/edge
  context an embedder would otherwise use), and crops. **No BiRefNet inference is wired in
  directly** — `siamese` itself has no real weights obtained either (its own doc comment: ~970MB
  export, out of that pass's time budget), so both arms of this ablation are exercised against
  externally-supplied masks (`<nef_dir>/masks/<stem>.mask.json`) in this pass, with a full-frame
  fallback (and a warning, not a hard error) when no mask file exists for a given photo.
- **`cluster.rs`** — DBSCAN on cosine distance (`dbscan`), a silhouette-coefficient sweep over a
  fixed `eps` grid with `min_samples` held constant (`dbscan_with_eps_sweep`), matching the Fursee
  paper's own "silhouette-guided adaptive hyperparameter selection" description. **HDBSCAN is not
  implemented this pass** — see Not adopted below.
- **`collapse.rs`** — optionally collapses a burst set (frames of the same pose, e.g. from
  litter's own tight/set grouping) into one mean embedding before clustering, so a 10-frame burst
  doesn't dominate a DBSCAN neighborhood relative to a subject with only a couple of photos. Takes
  plain `&[usize]` group ids rather than depending on `litter` directly — a real
  litter→rosette integration is a follow-up wiring question, not asserted here.
- **`metrics.rs`** — B-cubed precision/recall/F1 and ARI, copied verbatim from
  `spikes/litter/src/metrics.rs` (general clustering-comparison metrics, not specific to burst
  grouping's tight/set structure).
- **`label.rs` + CLI `rosette draft <nef-dir> --work <dir>`** — extracts T0 previews, embeds +
  clusters with a chosen backbone, and writes a local (not published) contact sheet grouping
  photos visually by predicted subject id, with an editable id field per photo — structurally
  simpler than litter's tight/set boundary-click UI, since subject clusters aren't
  sequence-constrained (no "adjacent boundary" exists to click). Same privacy/size rationale as
  litter's page for staying local-only: real third-party photos, con scale.
- **CLI `rosette eval --nef-dir <dir> --labels labels.json --backbone <name>`** — scores a
  backbone against human-corrected `labels.json`, over both crop ablation arms in one run.

**Not adopted / deferred:**

- **A pure timestamp/sequence-constrained grouping algorithm** (litter's own `group.rs`) — ruled
  out directly by the Context section above: the same subject can reappear anywhere across a
  whole shoot, so a sequence constraint would systematically miss most real re-appearances.
- **HDBSCAN** — mentioned in the original plan as a comparison point. No pure-Rust HDBSCAN crate's
  license was independently re-verified against `deny.toml` this pass, and a from-scratch
  implementation (mutual reachability + minimum spanning tree + condensed cluster tree) is a
  meaningfully larger undertaking than DBSCAN. Deferred to a follow-up, not silently dropped.
- **A YOLO head-detection stage** — the Fursee paper's own first pipeline step. Left to
  [#108](https://github.com/jordanfelle/nicti/issues/108) (already scoped to "Ultralytics YOLO as
  a culling/detection candidate") rather than duplicated here; this ADR's own crop ablation uses a
  precomputed mask instead, decoupling "does cropping help" from "which detector produces the
  crop."

## License findings (this pass)

- **DINOv3 (Meta DINOv3 License)**, verified against `facebookresearch/dinov3/LICENSE.md` and
  `ai.meta.com/resources/models-and-libraries/dinov3-license` (2026-09-27): commercial use is
  permitted under a non-exclusive, worldwide, non-transferable, royalty-free license.
  Redistribution is allowed if the Agreement text travels with the materials, **plus a "Built
  with DINOv3" attribution** displayed on any related UI/about-page/product docs — a real
  bundling condition, the same shape as this repo's existing NVIDIA cuDNN/TensorRT
  attribution-notice rows in `docs/licensing.md`. Acceptable-use restrictions (no
  military/ITAR/nuclear/espionage/weapons use) don't affect Nicti. **Verdict: ✅ bundle OK with
  the attribution notice + Agreement text shipped** — but the `onnx-community` exports are gated
  behind a Hugging Face click-through (an individual account, no scripted/CI download), so in
  practice this is a one-time manual fetch rather than an automated model-registry pull, fitting
  ADR-0218's on-demand-download model either way. ONNX export confirmed available:
  `onnx-community/dinov3-vits16-pretrain-lvd1689m-ONNX`.
- **OpenCLIP**: no ready-made ONNX export of a clean-license, non-LAION checkpoint was found. The
  one community ONNX family found, Apple's MobileCLIP2, is **research/non-commercial only** per
  `apple/ml-mobileclip`'s `LICENSE_MODELS` (explicitly excludes "any commercial product or
  service") — excluded, same posture as InsightFace. No torch/Python is available in this sandbox
  to export a checkpoint. **A reproducible export recipe is documented instead** (see
  `docs/research/rosette-subject-grouping.md`): `open_clip` + `torch.onnx.export` on a non-LAION
  DataComp checkpoint (e.g. `ViT-B-32` on `datacomp_xl_s13b_b90k`, MIT-equivalent). `embed.rs`'s
  `OpenClipEmbedder` is written against that recipe's expected output shape but has no real model
  to run against this pass — its own test is `#[ignore]`d/TBD, same posture as DINOv3's gated
  download and the con-shoot ground truth below.
- `docs/licensing.md` gets a new row for DINOv3 (no existing row covers it) in the same PR as this
  ADR, per ADR-0018's own process rule.

## Measured results

**Pending both a real unculled/labelled con shoot and real model files** — neither exists in this
sandbox (same constraint ADR-0033 documented: no unculled shoot on disk yet, and here additionally
no DINOv3/OpenCLIP `.onnx` file obtained — DINOv3's HF gate needs a manual accept, OpenCLIP has no
ready export at all). Not fabricated here.

- **Real, tested this pass**: 48 unit tests across `cluster.rs` (DBSCAN correctness on synthetic
  blobs, silhouette-based eps selection, noise handling), `collapse.rs` (mean-embedding
  collapse + assignment expansion round-trip), `crop.rs` (bbox-from-mask, padding, clamping),
  `metrics.rs` (copied, same coverage as ADR-0033), `label.rs` (contact-sheet generation +
  round-trip, including the same `</script>`-escaping regression test litter's own page needed),
  and `nef.rs`/`decode.rs`/`source.rs` (copied unchanged from litter, same coverage). `cargo
  clippy` and `cargo fmt --all` both clean.
- **Real DINOv2 inference**: not re-run in this pass (would reuse ADR-0033's own verified
  `onnx-community/dinov2-small` result unchanged); `embed.rs`'s `#[ignore]`d test documents the
  same env vars (`NICTI_TEST_DINO_ONNX`, `NICTI_TEST_ORT_DYLIB`) litter's own test uses.
- **Procedure, once ground truth + model files exist** (parallel to ADR-0033's own procedure):
  1. Accept DINOv3's Hugging Face gate and download the `.onnx` file; export an OpenCLIP
     checkpoint per this ADR's documented recipe.
  2. `rosette draft <shoot-dir> --work <scratch-dir> --backbone <name> [--crop]` for each
     backbone/crop combination — extracts previews, clusters, opens `label.html`.
  3. The user corrects subject ids by hand in the browser, exports `labels.json`.
  4. Commit `labels.json` (filename/sha256/subject_id only, never image content, per ADR-0018 and
     plain privacy) to a location the eval command can read.
  5. `rosette eval --nef-dir <dir> --labels labels.json --backbone <name> --model-onnx <path>
     --ort-dylib <path>` on the Windows reference machine — fills in the table below for both crop
     ablation arms.
  6. Move this ADR to Accepted if the decision rule's bars are met by at least one
     backbone/crop combination, or record where every combination falls short and why.

| Backbone | Crop | Precision | Recall | F1 | ARI | ms/photo |
|---|---|---|---|---|---|---|
| dinov2 | full-frame | TBD | TBD | TBD | TBD | TBD |
| dinov2 | crop | TBD | TBD | TBD | TBD | TBD |
| dinov3 | full-frame | TBD | TBD | TBD | TBD | TBD |
| dinov3 | crop | TBD | TBD | TBD | TBD | TBD |
| openclip | full-frame | TBD | TBD | TBD | TBD | TBD |
| openclip | crop | TBD | TBD | TBD | TBD | TBD |

## Consequences

**Feeds [#36](https://github.com/jordanfelle/nicti/issues/36)** (AI culling assist integration),
which per ADR-0033 also depends on the burst-grouping measurement pass
([#180](https://github.com/jordanfelle/nicti/issues/180)) — #36 shouldn't integrate an unmeasured
signal choice from either research ticket.

**Feeds [#108](https://github.com/jordanfelle/nicti/issues/108)** (YOLO as a culling/detection
candidate) via this ADR's crop ablation: if the measurement pass shows cropping meaningfully
improves clustering accuracy, that's a concrete reason for #108 to pursue a real detector; if
full-frame performs comparably, #108's YOLO question becomes lower-priority for subject grouping
specifically (it may still matter for #108's own culling-assist scope).

**Follow-up filed**: the post-con labelling + measurement pass is
[#243](https://github.com/jordanfelle/nicti/issues/243) (Part of #6) — kept distinct from #180
rather than folded into it, since #180's own ground truth (tight/set burst membership) is a
different label set from #243's (subject identity per photo), even though both may end up reading
photos from the same real con shoot.

**A real, flagged risk, not silently assumed away**: `docs/licensing.md`'s DINOv3 row commits
Nicti to displaying a "Built with DINOv3" attribution notice if this backbone is the one adopted
in #36 — a real, if small, UI/about-page obligation to remember at that point, not automatically
satisfied by anything in this spike.
