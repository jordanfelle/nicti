# ADR-0033: Burst/duplicate grouping

- **Status:** Proposed — spike, decision rule, and every signal candidate are built and unit-
  tested; the real con-scale measurement pass is pending real ground truth (see Measured results)
- **Date:** 2026-09-26
- **Ticket:** [#33](https://github.com/jordanfelle/nicti/issues/33) Research: burst/duplicate
  grouping
- **Formerly:** ADR-0025 (sequential numbering, pre-#183)

## Context

#33 (Part of epic [#6](https://github.com/jordanfelle/nicti/issues/6), blocks
[#36](https://github.com/jordanfelle/nicti/issues/36)'s AI culling assist integration) started as
a one-line migrated stub: group near-identical frames (timestamp clustering + perceptual hash, or
embeddings) so the user picks one per group. Nothing in the repo read capture time, shutter count,
or burst tags before this pass; nothing did perceptual hashing or embeddings either
(`crates/nicti-ai::ModelProvider` has no methods yet).

**Con duplicates are pose sets, not sub-second bursts.** A gap histogram over a real con day
(Anthrocon 2026-07-03, 1,368 frames) shows most consecutive gaps at 2-10s; only 152 of 1,368 gaps
are <=1s. A pure timestamp threshold would miss most real duplicates — time has to act as a
*constraint* (never link across a gap bigger than the level's own budget), with a visual-
similarity signal making the actual link/no-link decision.

**XMP sidecars carry ground-truth-quality metadata.** Real Z8 files' Lightroom-written `.xmp`
sidecars carry `exif:DateTimeOriginal` (millisecond precision), `aux:ImageNumber` (shutter count),
and `aux:SerialNumber` — independently verified against this ADR's own EXIF/MakerNote parsing on
37 real files (see Measured results).

**No unculled con shoot exists on disk.** Every large con folder on the user's drives keeps only
20-40% of its original frame-number range (already culled), and the one CFexpress card image
found (`E:\cf\*.dd`, ~1TB) is entirely `0xFF` — blank, not a real capture. A real unculled shoot
won't exist until the user's next con (within the next ~1-2 weeks, per the user's own steer). This
is the same "spec + tooling merged, measurement deferred" shape as
[#38](https://github.com/jordanfelle/nicti/issues/38) → [#149](https://github.com/jordanfelle/nicti/issues/149)
and [#68](https://github.com/jordanfelle/nicti/issues/68) → [#90](https://github.com/jordanfelle/nicti/issues/90).

**Sandbox note**, matching those same ADRs' precedent: everything not gated on a real unculled con
shoot is real, tested, measured evidence here — a from-scratch EXIF/Nikon-MakerNote reader
cross-checked byte-exact against 37 real Z8 NEFs' own XMP sidecars, a real ONNX Runtime 1.19.2
shared library and a real `onnx-community/dinov2-small` export (Apache-2.0, standard checkpoint)
both obtained and run end-to-end (not `#[ignore]`d-and-never-run — see Measured results for the
one real caveat found running it), and 35 passing unit/integration tests. What's pending: labelled
ground truth from a real unculled shoot, and the actual accuracy numbers that requires.

**User decisions (2026-09-26):**

- **Groups have two levels**: *tight* groups (interchangeable near-duplicates, pick one) nested
  inside *set* groups (same subject/setup, seconds to ~30s apart, collapsible in the UI).
- **Ground truth**: the next unculled con card, labelled by the user correcting this spike's own
  candidate grouping in a local contact-sheet page (`litter draft`), not labelled from scratch.

## Decision rule (stated before measuring)

- **Tight level**: B-cubed precision >= 0.95 (over-merging hides a distinct keeper inside another
  group — the costly error class) and recall >= 0.85.
- **Set level**: B-cubed precision >= 0.90 (looser — a set is meant to bundle related-but-distinct
  poses, so some under-merging is tolerable; over-merging unrelated subjects into one set is not).
- **Cost**: the chosen signal must fit the ingest budget in `docs/benchmarks.md` (10k NEFs
  grid-browsable <60s from NVMe). Time+hash signals (dHash/pHash/SSIM, all CPU-only, single-digit
  milliseconds per frame pair) run inline during ingest. The DINOv2 embedding signal is
  CPU-feasible (confirmed: real inference ran successfully in this pass, no GPU required) but
  costs meaningfully more per frame than a hash — it runs as a background job
  ([#54](https://github.com/jordanfelle/nicti/issues/54)/Pounce) after the grid is browsable, not
  inline, regardless of which signal wins on accuracy.
- **Tie-break**: the cheapest signal that clears both bars wins.

## Decision

**New spike: `spikes/litter`** (a litter = sibling frames, continuing the feline-anatomy naming
convention with a feline-behavior one instead). Pure Rust, lib+bin split (like `spikes/homing`, so
CLI-unwired helpers don't trip `dead_code`), not path-gated in CI (like `sniff`/`calico`/`homing`/
`shed`), depends on `nicti-prowl` for its SSIM/perf-protocol harness (same spike-depends-on-crate
direction `sniff` already took) but not on any other spike crate.

- **`nef.rs`** — a from-scratch TIFF/EXIF/Nikon-MakerNote reader, narrower than
  `spikes/sniff/src/ifd.rs`'s general embedded-JPEG walker (this one only needs the Nikon
  MakerNote's own PreviewIFD, not DNG SubIFDs or the classic thumbnail-IFD chain) but extended
  with the tags grouping actually needs: `DateTimeOriginal`+`SubSecTimeOriginal` (millisecond
  capture time), Nikon `ShutterCount`/`ShootingMode`/`SerialNumber`, and `Orientation`.
  **Verified against 37 real Z8 NEFs**: `ShutterCount`/`SerialNumber` come through unencrypted on
  this body and match the files' own XMP sidecars exactly; capture time matches to within 1ms
  (Lightroom's own XMP-writing rounds its decimal-fraction expansion slightly differently than a
  literal `"0.<digits>"` interpretation on some files — confirmed against exiftool's independent
  parse, which agrees with this reader, not LRC's XMP; irrelevant at grouping resolution). Whether
  other Nikon bodies (D7500/D3400) encrypt these tags is unverified — a real gap, not silently
  assumed away (see `nef.rs`'s own doc comment).
- **`decode.rs`** — JPEG decode for the T0 preview, adapted from `spikes/sniff/src/decode.rs`.
- **`signals.rs`** — four candidate similarity signals, each a pure function over a pair of
  frames, all measured under the identical grouping algorithm so only the signal changes:
  - `time+dhash` / `time+phash` — via `image_hasher` (the crate RapidRAW's own dependency graph
    already uses, per ADR-0069's audit; MIT OR Apache-2.0, already on `deny.toml`'s allowlist, no
    new entry needed).
  - `time+ssim` — reuses `nicti_prowl::golden::ssim` (no new dependency).
  - `time+dino` — DINOv2 ViT-S/14 global embedding (CLS token, per DINOv2's own `x_norm_clstoken`
    convention), cosine similarity. Runs through `ort` 2.0.0-rc.13 `load-dynamic`, copying
    `spikes/groom/src/ai.rs`'s loader shape. **Unlike groom's MobileSAM/LaMa wrappers, this one
    ran against a real model in this pass**: a real ONNX Runtime 1.19.2 shared library and the
    real `onnx-community/dinov2-small` export (based on `facebook/dinov2-small`, Apache-2.0,
    already an approved standard checkpoint per `docs/licensing.md`'s DINOv2 row) were both
    obtained and used to verify `Dinov2Embedder::embed` end-to-end — a real 384-dim embedding, and
    two near-identical synthetic frames embedding measurably closer than a different one. See
    Measured results for one real caveat found running it.
  - A hard veto (`different_known_camera`) exists independent of the chosen signal: two frames
    from different, known camera serials never link, so an interleaved two-shooter event can't
    cross-link purely because timestamps happen to interleave.
- **`group.rs`** — sequence-constrained two-level segmentation, O(n) over capture order. Links
  frame *i* to a following frame within a level's own `(max_gap_secs, min_similarity,
  max_lookahead)` budget; `max_lookahead` (default 2) tolerates one interleaved/odd frame (a
  second shooter, a test shot) without breaking the chain. **Nesting invariant, structural, not
  just asserted**: `group_sets` operates on whole tight-group boundaries and never splits one —
  proven by a dedicated test that tries to break it under adversarial (position-specific)
  similarity functions.
- **`metrics.rs`** — B-cubed precision/recall/F1 (Bagga & Baldwin 1998) and the Adjusted Rand
  Index (Hubert & Arabie 1985), both standard published formulas, plus this project's own
  over-merge/under-merge pair counts (an over-merge hides a distinct keeper — costly; an
  under-merge is one extra group to review — merely annoying).
- **`label.rs` + CLI `litter draft <nef-dir> --work <dir>`** — extracts T0 previews, runs a
  default candidate (`dhash`, the cheapest signal, since no measured winner exists yet), and
  writes a **local, not published** contact-sheet page (`label.html`) plus `draft.json`. Local by
  design: the page embeds real photos of real people, both a privacy concern (uploading
  third-party photos) and, at con scale, well past a published Artifact's size limit. No server
  needed — thumbnails are plain `<img>` files next to the page (which `file://` origins load
  fine, unlike `fetch()`), and the export button builds a `Blob`/`URL.createObjectURL` download
  entirely client-side. The user clicks gaps to split/merge tight groups, shift-clicks to
  split/merge set groups (only where a tight boundary already exists, enforcing the nesting
  invariant in the UI too), and exports `labels.json`.
- **CLI `litter eval --nef-dir <dir> --labels labels.json [--candidate <name|all>] [--sweep]`** —
  scores one or every candidate against `labels.json`, optionally sweeping a small
  `(max_gap_secs, min_similarity)` grid per level and reporting the best-F1 operating point found.

**Not adopted / deferred:**

- A pure timestamp-threshold heuristic — the gap-histogram finding above rules this out on its
  own; most real duplicates are seconds apart, indistinguishable by gap alone from two genuinely
  different but consecutive shots of the same subject.
- CLIP/OpenCLIP as the embedding candidate — DINOv2 was picked directly per
  `docs/adr/0018-third-party-license-policy.md`'s own flag steering #35 (face/subject grouping)
  toward DINOv2/OpenCLIP over face-recognition models; since #33 and #35 need the same kind of
  general-image embedding and DINOv2 has no OpenAI model-card deployment caveat (unlike CLIP),
  it's the natural shared default. OpenCLIP stays a live alternative for #35 to reconsider, not
  ruled out here.

## Measured results

**Pending the post-con labelling pass** (a real unculled shoot, labelled per the workflow above —
neither can exist in this sandbox, see Context). Not fabricated here.

- **Real-file cross-check (done, not pending)**: `tests/real_nef_cross_check.rs`, gated on
  `NICTI_TEST_REAL_NEF_DIR`, run against 37 real Z8 NEFs
  (`H:\Photos\Furries\Socials\2025\2025-12-27`) — 37/37 pass (capture time to within 1ms, shutter
  count and serial exact, T0 preview found on every file).
- **Real DINOv2 inference (done, not pending)**: `cargo test -p litter -- --ignored
  dinov2_embeds_two_similar_frames_closer_than_a_different_one`, run directly against the real
  ONNX Runtime 1.19.2 shared library and `onnx-community/dinov2-small`. **Passes** — a real
  384-dim embedding, correct similarity ordering. **One real caveat found running it**: the test
  *process* segfaults during exit/teardown, after the test's own assertions already pass and
  print `ok` (confirmed running the compiled test binary directly, isolated from `cargo test`'s
  own harness process) — a known `ort`/`load-dynamic` static-destructor-ordering issue between the
  dynamically loaded ONNX Runtime library and Rust's own exit path, not a defect in this crate's
  own embedding code. Worth knowing before #48/#51 (which already use `ort load-dynamic`) or #36's
  eventual production integration relies on a clean process exit around any `ort` session.
- **Procedure, once a real unculled shoot exists**:
  1. `litter draft <card-dir> --work <scratch-dir>` — extracts previews, runs the default
     candidate, opens `label.html`.
  2. The user corrects tight/set groups by hand in the browser, exports `labels.json`.
  3. Commit `labels.json` (frame identity + group ids only — never image content, per ADR-0018 and
     plain privacy) to a location the eval command can read.
  4. `litter eval --nef-dir <card-dir> --labels labels.json --candidate all --sweep` on the
     Windows reference machine (WSL cross-compile + interop, same pattern `sniff`/`retina` already
     used for their own real-hardware passes) — fills in the table below.
  5. Move this ADR to Accepted if the decision rule's bar is met by at least one candidate, or
     record where every candidate falls short and why.

| Candidate | Tight precision | Tight recall | Tight F1 | Set precision | Set F1 | ms/frame |
|---|---|---|---|---|---|---|
| time+dhash | TBD | TBD | TBD | TBD | TBD | TBD |
| time+phash | TBD | TBD | TBD | TBD | TBD | TBD |
| time+ssim | TBD | TBD | TBD | TBD | TBD | TBD |
| time+dino | TBD | TBD | TBD | TBD | TBD | TBD |

## Consequences

**Feeds [#36](https://github.com/jordanfelle/nicti/issues/36)** (AI culling assist integration)
and **[#35](https://github.com/jordanfelle/nicti/issues/35)** (face/subject grouping, which can
reuse `embed.rs`'s DINOv2 wrapper directly — same embedding, different downstream clustering
question). `crates/nicti-ai::ModelProvider`'s first real method shape is a natural next step once
#36 wires a production embedding path in, but that trait change belongs to #36, not asserted here.

**Follow-up filed**: the post-con labelling + measurement pass is
[#180](https://github.com/jordanfelle/nicti/issues/180) (Part of #6), added to #36's own
`**Blocked by:**` line since #36 shouldn't integrate an unmeasured signal choice.

**A real, flagged risk, not silently assumed away**: Nikon `ShutterCount`/`SerialNumber`
encryption on non-Z-series bodies (D7500/D3400) is unverified — if either body encrypts these
tags, `nef.rs`'s direct reads would return wrong values with no parse error to catch it. Worth
checking against a real D-series file before trusting this reader's output on non-Z8 input.
