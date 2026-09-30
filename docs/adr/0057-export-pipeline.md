# ADR-0057: Export pipeline

- **Status:** Accepted
- **Date:** 2026-09-30
- **Ticket:** [#57](https://github.com/jordanfelle/nicti/issues/57) Build: export pipeline
- **Builds on:** [ADR-0056](0056-export-stack.md) (component choices), [ADR-0054](0054-job-scheduler-pounce.md)
  (Pounce), [ADR-0044](0044-stage-cached-render-graph.md) (Tapetum), [ADR-0042](0042-color-management.md)

## Context

ADR-0056 picked the export components and prototyped them in `spikes/prey`. #57 turns that into a
feature: select photos, render each with its edits at full resolution, resize/convert/watermark,
encode to JPEG/PNG/TIFF with metadata, and write named files -- in the background, without
blocking the UI or the GPU lane, and without ever overwriting a file by accident. Four things the
spike did not settle: where the engine lives, how a batch moves through Pounce, how names and
collisions are decided, and where a photo's edits come from (nothing persisted them yet).

## Decisions

### 1. Engine in `nicti-preen`; render + jobs in `nicti-pelt`

`nicti-preen` (the `Exporter` extension-point crate) now holds everything format-independent that
turns a host-memory linear frame into a file: the settings model (`spec`), filename tokens
(`naming`), planning (`plan`), collision-safe writes (`write`), linear-light resize, orientation,
output color conversion, watermark, metadata, and the built-in JPEG/PNG/TIFF exporters. It is
GPU-free and catalog-free by construction -- it depends on neither `nicti-tapetum` nor
`nicti-lair` -- so all of it is unit-testable without an adapter. The `Exporter` trait gained the
format-specific `encode(image, format_spec, embed)`; resize/color/orientation/watermark are shared
and happen in `export_frame` before it. The GPU render and Pounce wiring live in
`nicti-pelt/src/export/`.

### 2. A photo's render pipeline is one shared definition (`nicti_tapetum::spine`)

The graph, the stage registry and "document -> kernel inputs" (`resolve_inputs`) moved out of
`nicti-pelt::render` into `spine.rs`, used by Develop, export and `bench/knead`, so a Develop
preview and an export of the same document cannot drift. `Renderer::render_live` stops after the
live suffix so a tiled full-res export never allocates a full-size geometry target.

### 3. Chained Pounce jobs, one lane each

Per photo: **decode** (CPU lane) -> **render** (GPU lane, one `TiledRender` tile per `step()`,
ADR-0054's ~16 ms chunks) -> **encode + write** (CPU lane). One job doing all three would run a
~1.7 s decode inside a GPU-lane step and starve `RemoveJob`. Stages chain through a weak
`Pounce::submitter()` (a job must never hold a `Pounce` clone: `Drop for Pounce` joins the workers
when it sees the last handle, and a queued job could become that last handle on a worker thread).
`advance()` is the single place that decides what may start, bounding memory: at most one photo
decoded/decoding ahead, one rendering, and `rendered + encoding <= 2` (roughly 2.4 GB worst case at
45 MP full size; downsized exports need far less). The render job declares `vram_bytes: 0`:
Pounce silently drops a background job that declares more than the *total* VRAM budget, which a
45 MP frame's textures exceed under the placeholder budget (same as `RemoveJob`).

Each photo has one `Ticket` that settles exactly once (exported / skipped / failed / cancelled); a
dropped ticket settles as cancelled, so the report always accounts for every photo and is
published once. A per-photo failure returns `Ok(Done)` and lands in the report -- a `step()` error
would make Pounce drop the job without telling anyone. Drops never submit (they can run inside
the scheduler's lock); only `step()` and the UI's per-frame `poll` call `advance`.

### 4. Photo identity goes in the *document*, not on the graph node

`RenderGraph::apply_document` recomputes every node's `own_hash` from the document, so identity
set with `set_own_hash(DECODE, ..)` is silently reset on the next render. Two photos of the same
pixel size then share baked/live cache keys and serve each other's pixels. This was already true of
the Develop/Loupe path (found while building export, with a regression test); both now call
`spine::stamp_source_identity` on the render-time copy of the document. `DecodeExec` ignores the
params, and the stored document is never stamped.

### 5. Edits are read from the catalog: `get_master_edit` / `put_master_edit`

`edit_variant` always existed but was always written empty and never read. `CatalogStore` gained
the get/put pair (canonical JSON, non-finite floats refused), and Develop now saves: on pointer
release when dirty, before switching photos, before an export, and on exit; it loads the stored
document (and reloads/verifies a named DCP profile) when a photo opens. History/undo and
persisting or re-running AI removals stay with #324: export renders clone/heal spots (they are
document-driven) and skips AI Remove spots (their patches live only in memory). A camera profile
that is missing or changed on disk **fails that photo** at export instead of silently rendering
different colors.

### 6. Filenames: tokens, planning, collisions

Tokens: `{Filename} {Sequence[:N]} {Date[:YYYYMMDD]} {Rating} {Make} {Model} {Folder}`, `{{`/`}}`
literals; a filename template may not contain a path separator, the optional subfolder template
may. Every component is sanitized on every OS (invalid characters, trailing dots/spaces, reserved
device names case-insensitively including `nul.jpg` and `COM¹`, 255 UTF-16 units, total path kept
under `MAX_PATH` with room for a suffix; `.`/`..` sanitize to nothing so a template cannot climb
out of the destination). Sequence numbers are `start + position in the selection`, fixed at plan
time, so parallelism and failures never change a name (a failed photo leaves a gap). The plan
de-duplicates within the batch on a case-folded path under every policy.

`write_output` claims a name with `create_new` (atomic across threads and processes, works on
FAT/exFAT), writes a unique temp file (`.{pid}-{n}.nicti-tmp`), `sync_all`s, and renames over
the claim, retrying transient Windows access-denied errors; on any error the temp file and the claim
are removed. This replaces `spikes/scent::atomic_write`, whose single shared temp name and silently
overwriting rename are unsafe for parallel writers. A crash can leave a 0-byte claim or a temp
file; that is documented, not swept, because destinations are not tracked.

### 7. Format details

- **JPEG**: `jpeg-encoder`, chroma subsampling always pinned, JFIF density = DPI, ICC natively,
  XMP as APP1, EXIF via `little_exif`.
- **PNG**: `image` (8/16-bit), then ICC, `pHYs`, XMP (`iTXt`) and EXIF as a standard `eXIf` chunk in
  one `img-parts` pass. `little_exif` can only write PNG EXIF as a non-standard ImageMagick-style
  `zTXt` profile (exiftool flags it) and rewrites any XMP `iTXt` it finds, so the raw TIFF EXIF
  block is lifted out of a 1x1 JPEG it writes instead.
- **TIFF**: the `tiff` crate directly with resolution, ICC (tag 34675, as UNDEFINED -- the crate's
  `[u8]` is BYTE, which exiftool flags) and XMP (tag 700). EXIF in TIFF stays deferred. This amends
  ADR-0056's "TIFF is encode-only".
- **EXIF `ColorSpace`** is 1 only for sRGB and 0xFFFF otherwise (the spike hardcoded 1).
- **Orientation**: LibRaw's decode is unrotated and nothing in the render applies IFD0 Orientation,
  so export rotates the finished, resized buffer and writes Orientation = 1. Crop coordinates stay
  in sensor orientation, as in Develop.
- `exiftool -validate` is clean on JPEG and PNG output and on TIFF apart from two known
  validator/crate quirks: it flags *any* Adobe-Deflate TIFF (libtiff's own `tiffcp -c zip` output
  too), and the `tiff` crate does not pad an odd-length compressed strip, which can leave IFD
  values at an odd offset ("[minor] Odd offset"). Both are ignored by the integration test.

## Options considered

- **One job per photo doing decode+render+encode** -- rejected: blocks the GPU lane for the decode.
- **Rayon inside the resizer / a private encode pool** -- rejected: bypasses Pounce's CPU-lane
  throttle. `fast_image_resize` is built without its `rayon` feature.
- **A streaming file-encoder tile sink** (never hold the whole frame) -- deferred; the accumulate-
  then-encode design is simpler and bounded by the pipeline limits above. Filed as a follow-up.
- **`set_own_hash` for photo identity, re-applied after `apply_document`** -- rejected: it
  invalidates the graph's hash memo every frame; stamping the document is idempotent.
- **A native folder picker (`rfd`)** -- deferred: the destination is a typed path like Import/Move.

## Consequences

- Closes #57; the `Exporter` extension point has its first real implementations.
- Develop edits now persist; #324 layers History/undo and AI-removal recompute on top.
- Follow-ups filed: streaming/GPU-downscale sink (memory and readback cost), TIFF EXIF, text
  watermarks, output sharpening, GPS, RAW orientation in the loupe/Develop view (with #309),
  black-point compensation for export (#320), and the still-open reference-machine measurements
  from ADR-0056.
