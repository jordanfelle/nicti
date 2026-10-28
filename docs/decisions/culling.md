## Culling

Covers burst/duplicate grouping (#33), blur/misfocus/eye detection (#34), subject grouping (#35),
Ultralytics YOLO as a culling/detection candidate (#108), and the culling UX build (#32).

- **Burst/duplicate grouping (#33)**: `docs/adr/0033-burst-duplicate-grouping.md` — con-day
  duplicates are pose sets 2-30s apart, not sub-second bursts (measured on a real con day: most
  gaps land at 2-10s, only 152/1,368 are <=1s), so a pure timestamp threshold misses most real
  duplicates. `spikes/litter` implements sequence-constrained two-level (tight/set) grouping,
  where time acts as a constraint and a visual-similarity signal (dHash/pHash/SSIM/a real DINOv2
  embedding, all four measured under the identical grouping algorithm) makes the actual link
  decision. Nesting invariant (every set group is a union of whole tight groups) is structural,
  not just asserted — a dedicated test tries to break it under adversarial similarity functions.
  A from-scratch EXIF/Nikon-MakerNote reader (capture time, shutter count, serial, PreviewIFD)
  cross-checks byte-exact against 37 real Z8 NEFs' own XMP sidecars. A real ONNX Runtime library
  and a real DINOv2 export were obtained and run end-to-end (not left as an untested `#[ignore]`)
  — found one real caveat along the way: a segfault-on-exit that's an `ort`/`load-dynamic`
  teardown-ordering quirk, not a bug in this crate. **Proposed, not Accepted** — the actual
  accuracy numbers wait on a real unculled con shoot (none exists on disk yet; every large con
  folder found is already culled), tracked in the labelling/measurement follow-up (#180).
- **Blur/misfocus/eye detection (#34)**: `docs/adr/0034-blur-misfocus-eye-detection.md` — every
  sharpness candidate (Laplacian variance, Tenengrad, a real 2D FFT high-frequency ratio,
  structure-tensor motion/defocus discriminator) scores a frame's *sharpest tile*, not a whole-
  frame average, so a shallow-depth-of-field shot isn't penalized for its own bokeh. Same
  ground-truth gap as #33 (no unculled con shoot on disk), substituted this pass with synthetic
  degradation of a synthetic keeper image set — which also enables a real (not fabricated)
  measurement on that set: the keeper false-flag rate (all three candidates: 0.0% on this pass's
  8-image synthetic set, not yet measured on real photos). A real finding: a single threshold
  calibrated on defocus severity didn't transfer to detecting motion blur on this synthetic set,
  even though motion blur's own relative score drop was severe — worth per-degradation calibration
  once real labels exist. A Nikon `AFInfo2` AF-area reader (`af.rs`) targets the real Z8/Z9
  `"0400"` version (an adversarial review + primary-source lookup caught an earlier draft reading
  the wrong `"0100"`/`"0101"` offsets) and composes with any sharpness candidate for a misfocus
  signal, but is unverified against a real NEF. Eye detection (human closed-eye, fursuit "eyes
  obscured") is research-only this pass — real candidates were license-checked but none was run
  against real weights, so no bundled candidate ships rather than fabricating a result. **Proposed,
  not Accepted** — real numbers wait on the next unculled con card (#238), same as #33's own #180.
- **Subject grouping (#35)**: `docs/adr/0035-subject-grouping.md` — clusters a shoot's photos by
  subject/person for review-per-subject. Human face-recognition models don't apply: most subjects
  are fursuiters, and InsightFace/RetinaFace are license-excluded anyway (non-commercial only).
  `spikes/rosette` uses general image embeddings instead (DINOv2, reused from litter's own
  embedder; OpenCLIP; DINOv3), clustered with DBSCAN over cosine distance and a
  silhouette-coefficient-guided `eps` sweep — not litter's own sequence-constrained grouping,
  which only links nearby frames in capture order and would miss a subject reappearing elsewhere
  in the shoot. A full-frame-vs-subject-crop ablation is included (crop source: a precomputed
  mask, same shape as `spikes/siamese`'s BiRefNet output). **DINOv3 is license-gated** (Meta's
  DINOv3 License requires a "Built with DINOv3" attribution if adopted) and its ONNX export sits
  behind a Hugging Face manual-accept gate; **no clean-license OpenCLIP ONNX export was found**
  (the one community export, Apple's MobileCLIP2, is research-only) — its embedder is implemented
  against a documented self-export recipe, untested against a real model this pass. **Proposed,
  measurement pending** — same "no real unculled/labelled shoot exists yet" constraint as #33,
  plus no DINOv3/OpenCLIP model file obtained in this pass.
- **YOLO as a culling/detection candidate (#108)**:
  `docs/adr/0108-yolo-culling-detection-candidate.md` — Ultralytics YOLO was excluded on license
  grounds until ADR-0066 made Nicti's own outbound license AGPL-3.0-or-later. Re-verified the
  license against the primary source
  (`ultralytics/ultralytics`'s `LICENSE` file, fetched fresh rather than re-trusting the existing
  `docs/licensing.md` row): genuinely AGPL-3.0, no drift; current model family is YOLO26
  (n/s/m/l/x), ONNX-exportable, fitting the existing `ort`-based inference pattern litter/rosette
  already use. **Complementary, not redundant**, with #34 (which scores sharpness *within* a
  region, a different primitive from detection) and #35 (whose cited prior art, the Fursee paper,
  runs YOLO as a head-crop pre-processing stage feeding a DINOv3 embedder — detection and
  embedding are sequential, not competing). The Fursee paper's own hybrid pipeline reaches 93.33%
  retrieval hit rate / 0.8755 clustering F1 on fursuit identity, but that's a downstream-clustering
  number, not a standalone YOLO detection benchmark — no ticket has measured fursuit-head detection
  accuracy in isolation. **Accepted, deferred**: no dedicated integration this pass — #35's own
  full-frame-vs-crop ablation (pending #243) is the concrete mechanism to decide whether a
  detection stage is worth adding, so this ticket doesn't duplicate that pending experiment.
- **Culling UX (#32)**: `docs/adr/0032-culling-ux.md` — the build that gives the catalog's
  rating/flag/label columns a UI. The owner's own flow is stars only (1 = discard, 4 = "edit this",
  re-rate the 4s to 5 or 1); others use pick/reject or colour labels; the resulting requirement was
  that *no* marker is privileged and discarding is just "filter on whatever you marked with,
  select all, Delete" -- so there are no profiles, only the standard Lightroom Classic keys all live
  at once (`0-5`, `P`/`X`/`U`, `6-9`, Ctrl+Z/Y), #242's filter bar (extended with unrated / exactly-N / unflagged / no-label), and a Delete
  that always asks Recycle Bin vs. catalog-only. Reject is `rating = -1` and replaces the stars;
  pick/reject are exclusive; a toggle decides once for a whole multi-selection. Writes never block a
  keypress: a writer thread owns all marker I/O and the undo ring while the UI updates a cache first
  (the catalog is one mutex an import holds in bursts). Marking never reloads the grid, so the
  cursor never jumps. Delete follows Carry's discipline (journal before write, steppable, startup
  recovery) and adds one rule of its own: only a *positive* "absent from a reachable folder" ever
  removes a catalog row -- every disk question is present/absent/unknown (an errored stat or an
  unreachable folder is unknown, and keeps the row), which an adversarial review showed the first
  version got wrong in three ways (`Path::exists` swallowing errors, no reachability check after
  the trash call, a stale journal path at recovery). A sidecar shared with an unselected sibling
  is left for it. egui gotchas found on the way: `key_pressed` includes OS auto-repeat and egui derives
  `repeat` from its own held-key set, so holding `X` would flip a reject repeatedly unless raw
  events are read and repeats ignored. Survey/Compare work on the embedded previews only. **Not
  verified in this sandbox**: the real Windows Recycle Bin, drawn pixels, real-hardware latency.

- **Culling at 1M photos (#335, measured 2026-10-11, release, Linux x86_64, 1M synthetic assets)**:
  select-all then mark: `CullState::mark` ~90-115 ms on the UI thread (the 1M-entry `pending` map +
  id vec; above the < 50 ms budget), the `Done` reply folded in `poll` ~370 ms in one frame (settling
  1M `pending` entries -- *not* the cache insert/trim, skipping that changed nothing), the worker's
  `get_meta` over 1M ids 6 s and `set_meta` over 1M rows ~80 s in ONE transaction holding the catalog
  mutex (even a no-op rewrite costs ~23 us/row; a rating change ~70 us/row: the three rating indexes
  and `trg_facet_asset_update`, not the statement). `Selection::remap` over a select-all ~190 ms per
  snapshot change. **Fixed (#497)**: a mark over `OPTIMISTIC_MAX` ids is tracked as one sorted id list
  (`cull/mod.rs` `Pending`) instead of a per-id map, and a bulk `Done` reply only refreshes already-cached
  photos: `mark` ~4.5 ms (sorted ids; unsorted adds a sort), worst `poll` frame ~20 ms for one bulk in flight
  (each further bulk in flight adds one binary search per reply item). **Fixed**: the worker reads/writes in 20k-row chunks (`worker.rs` `CHUNK`), so the
  mutex is held ~3 s at a stretch instead of ~80 s for other catalog users (grid snapshot/thumbnail
  reads; the marker reads queued on the same writer thread still wait for the whole run, and a
  non-worker writer can now interleave between chunks) (a mid-way failure records the written part as an
  undo entry); `remap` returns at once when the id snapshot is unchanged (190 ms -> 0.7 ms). **Fixed (#498)**: `Selection::remap` merge-walks old/new ids when the new snapshot is the old one plus/minus
  photos (sets sized by the difference; reorder or a large diff falls back to the `HashSet`): select-all at 1M
  ~200 ms -> ~2 ms. **Fixed (#499)**: `set_meta` guards each column (`... AND col IS NOT ?`) so a no-op rewrite
  skips the index/trigger work (23 -> ~2.5 us/row) and borrows a 256 MB page cache for batches of 1000+ rows
  (the default ~2 MB thrashed on the five rating-keyed indexes): a select-all rating change at 1M 50 s -> 32 s
  (the facet trigger is ~9 s of that, the rest is index maintenance). **Fixed (#580)**: a bulk rating change
  (`set_meta` / `set_rating` of 1000+ rows) drops `trg_facet_asset_update` inside its transaction and applies the
  exact facet delta once (`with_facet_delta`): the per-(volume, model, rating) counts of the touched ids are read
  before and after (read-only, chunked `IN` lists) and the difference is added to `facet_counts` (empty buckets swept),
  (ids are sorted and deduplicated first: `IN` counts a row once only within one chunk, and a duplicate id in two chunks doubled its delta, caught by the duplicate-id tests), then the trigger is recreated from its own `sqlite_master` definition (no second copy of its SQL to drift). SQLite
  DDL is transactional, so a failed batch rolls the trigger back; small batches keep the trigger. Measured (1M rows,
  one reference run, `tests/scale.rs`, niced): select-all rating change 22.4 s -> 11.1 s, longest single 20k
  transaction (the mutex hold) 759 ms -> 255 ms; a no-op rewrite 1.75 s -> 2.11 s (the read-only count queries).
  The rating-keyed sort indexes were kept (decision on #580); pruning them is a separate, schema-changing ticket.
  **Not fixed (follow-ups)**: the marker
  filters' 1.5-2 s sort-index scan, and the ~128 MB undo entry (deliberate, ADR-0032). Harnesses:
  `nicti-lair/tests/scale.rs::select_all_mark_at_one_million_assets`,
  `cull/tests.rs::mark_select_all_at_one_million`, `grid/selection.rs::scale::remap_at_one_million_photos`
  (all `#[ignore]`, run with `--release --ignored --nocapture`).

## #439: LightCraft cull heuristics and red-eye / pet-eye (ADR-0439)

Research outcome from the LightCraft audit (#424); nothing lifted. `squint` and `litter` already measure more than
`cull.rs` (Laplacian, Tenengrad, FFT ratio, structure-tensor motion/defocus, AF-region misfocus; dHash/pHash/SSIM/DINOv2
with sequence constraints). The one new aggregator is a block-Laplacian p90 (a hot-pixel-robust cousin of `max_over_tiles`; it does not
solve localised misfocus, which only the AF-region ratio does), to be added as a candidate
in the #238 real-card sweep (judged per degradation class like the rest; LightCraft's calibration constant is not
taken). Its 8x8 signature is the dHash/pHash family; clipped fraction has no ground truth (no exposure tag in the
label set). Red-eye and pet-eye are no-go with no build ticket: Nicti's work is convention and fursuit photography,
masks and heal cover the rare case, and the feature is a detector, a GPU kernel and an equivalence test. Start from
LightCraft's ellipse + normalised-grid design if it is ever asked for.
