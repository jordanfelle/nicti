# #33: burst/duplicate grouping (`spikes/litter`)

Full write-up backing `docs/adr/0025-burst-duplicate-grouping.md`. See that ADR for the decision
rule and Consequences; this doc is the method and reproduction detail.

## Method

Con-day duplicates aren't sub-second camera bursts — they're pose sets a few seconds to ~30s
apart, produced by a photographer re-shooting the same subject/setup. A gap histogram over a real
con day (Anthrocon 2026-07-03, 1,368 frames, computed from each file's XMP sidecar's
`exif:DateTimeOriginal`) confirms this:

| Gap | Frame count |
|---|---|
| <=0.5s | 23 |
| <=1s | 129 |
| <=2s | 218 |
| <=5s | 455 |
| <=10s | 268 |
| <=30s | 202 |
| <=60s | 23 |
| <=300s | 28 |
| >300s | 21 |

Run-length histogram of consecutive gaps <=1s: 1,083 singletons, 119 runs of 2, 12 runs of 3, one
run of 5, one run of 6. Most of the day's real duplication lives in the 2-10s band, not the <=1s
band a naive "burst" detector would target.

## Candidates measured (unit-level, synthetic + real-file where noted)

- **`time+dhash`** — `image_hasher`'s Gradient hash algorithm over the T0 preview. Cheapest,
  CPU-only, no ML dependency.
- **`time+phash`** — `image_hasher`'s Mean hash + DCT preprocessing. Same cost class as dHash.
- **`time+ssim`** — reuses `nicti_prowl::golden::ssim` (the same hand-rolled single-scale SSIM
  #17/ADR-0017 already built), resized to equal dimensions when the two T0s differ (a
  mixed-camera pair).
- **`time+dino`** — DINOv2 ViT-S/14 global embedding (CLS token), cosine similarity. Costs more
  per frame than a hash but ran successfully on CPU in this pass (see Real DINOv2 run below) —
  feasible as a background job, not necessarily as an inline ingest-time signal.

Every candidate shares the same sequence-constrained two-level grouping algorithm
(`spikes/litter/src/group.rs`) — only the pairwise similarity function changes, so a fair
apples-to-apples comparison is just "swap the closure."

## Real-file cross-check: `nef.rs` against 37 real Z8 NEFs

`tests/real_nef_cross_check.rs`, gated on `NICTI_TEST_REAL_NEF_DIR` (not run in CI — no such
folder exists there), run against
`H:\Photos\Furries\Socials\2025\2025-12-27` (37 real NEF+XMP pairs):

```
NICTI_TEST_REAL_NEF_DIR=/mnt/h/Photos/Furries/Socials/2025/2025-12-27 \
  cargo test -p litter --test real_nef_cross_check -- --nocapture
```

Result: **37/37 pass.** `ShutterCount`/`SerialNumber` match `aux:ImageNumber`/`aux:SerialNumber`
exactly (both come through unencrypted on this Z8 body — no key-derivation decryption needed,
confirmed by direct comparison, not assumed). Capture time matches `exif:DateTimeOriginal` to
within 1ms on every file — not always bit-exact: one file (`DSC_9711.NEF`) has EXIF
`SubSecTimeOriginal="56"`, which both this reader and `exiftool` independently expand to `.560`,
while Lightroom's own XMP-writing rounds the same value to `.559` — a 1ms LRC-internal rounding
quirk, not a parsing bug (confirmed against exiftool as a second, independent implementation), and
irrelevant to grouping decisions at this resolution.

**Unverified**: whether Nikon's D7500/D3400 bodies encrypt `ShutterCount`/`SerialNumber` the way
some Nikon DSLRs do (using a key derived from the serial number and a counter tag) — this reader
implements no such decryption. Worth a real D-series file cross-check before trusting these fields
on non-Z8 input.

## Real DINOv2 run

Obtained (network access confirmed available, same as #48/ADR-0024's own context):

- A real ONNX Runtime 1.19.2 Linux shared library:
  `https://github.com/microsoft/onnxruntime/releases/download/v1.19.2/onnxruntime-linux-x64-1.19.2.tgz`
- The real `onnx-community/dinov2-small` ONNX export (based on `facebook/dinov2-small`,
  Apache-2.0 — an already-approved standard DINOv2 checkpoint per `docs/licensing.md`'s DINOv2
  row): `https://huggingface.co/onnx-community/dinov2-small/resolve/main/onnx/model.onnx`

```
NICTI_TEST_DINO_ONNX=<path-to-dinov2-small.onnx> \
NICTI_TEST_ORT_DYLIB=<path-to-libonnxruntime.so> \
  cargo test -p litter -- --ignored dinov2_embeds_two_similar_frames_closer_than_a_different_one
```

**Passes** — a real 384-dim CLS-token embedding, correct similarity ordering (two near-identical
synthetic frames embed closer than a clearly different one).

**One real caveat, not a defect in this crate's own code**: running this test through `cargo
test`'s own harness process reports a failure, because the *process* segfaults during exit/
teardown — after the test's own assertions have already passed and printed `ok`. Confirmed by
running the compiled test binary directly (`./target/debug/deps/litter-<hash> --ignored
dinov2_embeds... --nocapture`), which prints the same passing result before segfaulting on exit.
This is a known class of issue with `ort`'s `load-dynamic` feature (a static-destructor-ordering
race between the dynamically loaded ONNX Runtime library and Rust's own exit path), not something
`spikes/litter`'s `embed.rs` can fix from its own code. Worth knowing before #48/#51 (which also
use `ort load-dynamic`) or a future production integration relies on a clean process exit around
any `ort` session.

## Labelling workflow

`litter draft <nef-dir> --work <dir>` extracts every T0 preview, runs the default `dhash`
candidate (cheapest signal, no measured winner exists yet), and writes `<dir>/draft.json` plus a
**local-only** `<dir>/label.html` contact sheet. Deliberately not a published Artifact: the page
embeds real photos of real people (a privacy concern, and past a published Artifact's size limit
at con scale). No server needed — thumbnails are plain `<img>` files the page loads via a relative
path (which works fine under a `file://` origin, unlike `fetch()`), and the export button builds a
`Blob`/`URL.createObjectURL` download entirely client-side.

In the browser: click the gap between two thumbnails to split/merge a tight group; shift-click to
split/merge a set group (only where a tight boundary already exists there — enforces the nesting
invariant in the UI, not just in `group.rs`'s own algorithm). Export downloads `labels.json`.

`litter eval --nef-dir <dir> --labels labels.json --candidate all --sweep` scores every candidate
against the exported labels, sweeping a small `(max_gap_secs, min_similarity)` grid per level and
reporting the best-F1 operating point per candidate — see ADR-0025's Measured-results table.

## Reproducing

Every command above uses placeholder paths, not real ones — `NICTI_TEST_REAL_NEF_DIR` is a real
private photo folder (not committed), and the DINOv2 weights/ONNX Runtime library are downloaded
artifacts (not committed either, per `docs/licensing.md`'s "download on demand, never bundle"
policy for ML runtimes/weights).
