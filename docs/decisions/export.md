## Export stack

Covers #56's export-stack research — full reasoning and every measured number in
`docs/adr/0056-export-stack.md`. This file is the per-topic summary; that ADR is the full research
trail.

- **Handoff from ADR-0019**: `crates/nicti-preen`'s `Exporter` trait was left as identity/
  versioning only, deferring resize/encode/metadata-write/watermark execution and the export
  method signature to this ticket and #57.
- **Resize**: `fast_image_resize`'s Lanczos3 over a hand-converted linear-light buffer, not
  `image::imageops`'s naive gamma-space resize — the naive path is faster (56ms vs. 201ms p50 at
  screen-res) but measurably diverges from the correct result on high-contrast content. A GPU
  candidate (`wgpu` compute) was built and cross-verified against the CPU one, but real hardware
  timing wasn't reachable this pass (this sandbox's WSL exposes no NVIDIA Vulkan ICD; only
  `llvmpipe`, whose own buffer-binding limit also caps full-res GPU work below a real 45MP frame).
- **JPEG encoder**: `jpeg-encoder` (pure Rust) for v1, not mozjpeg — **settled by a
  quality-matched comparison (#223) on one synthetic test image**. The original nominal-quality-90
  gap (~2.6x smaller for mozjpeg) turned out to be partly a chroma-subsampling mismatch, not just
  an uncalibrated quality scale: `jpeg-encoder`'s own default silently switches 4:2:0→4:4:4 at
  quality ≥90, while mozjpeg's stays fixed at 4:2:0. Pinning subsampling to 4:2:0 on both sides and
  matching quality to a target SSIM shows mozjpeg is genuinely ~1.3-1.4x smaller on that image, but
  also ~4-4.5x slower — real, but past this ADR's own ≤1.5x-encode-time bar for a candidate to earn
  its place. Not yet real-photo-confirmed: this pass's own two synthetic fixtures gave meaningfully
  different SSIM ceilings at 4:2:0, so real photographic chroma content could shift the result.
  mozjpeg stays available behind a `native` Cargo feature for a future reference-machine/real-NEF
  re-check.
- **Metadata**: `little_exif` for EXIF write (builds the TIFF-structured IFD itself, unlike
  `img-parts`' raw-blob EXIF support), a runtime-generated `moxcms` sRGB ICC profile embedded via
  `img-parts` (JPEG APP2 multi-segment, PNG `iCCP`), and a hand-rolled XMP JPEG-APP1/PNG-iTXt
  insert generalizing `spikes/scent`'s approach onto `img-parts`' primitives. A real `exiftool
  -validate` run on the first pipeline output flagged 7 real warnings (missing mandatory EXIF/TIFF
  tags, missing XMP xpacket wrapper) — all fixed, now regression-tested.
- **Watermark**: SVG (`resvg`/`usvg`/`tiny-skia`, real rasterization) or PNG logos, composited in
  linear-premultiplied alpha — straight-alpha gamma-space blending darkens semi-transparent edges
  the same way a naive resize does. Sequential compositing beat `rayon`-parallel at typical logo
  sizes (1.18ms vs. 6.10ms p50) — parallelism overhead exceeds the blend work below some size.
  Text watermarking is deferred (no vendored font with a confirmed license picked yet).
- **TIFF export's metadata write (ICC/EXIF/XMP) stays deferred** — same class of gap ADR-0059 left
  for DNG: no crate in this workspace builds arbitrary TIFF tags yet.

## #57: the export pipeline

Full record: `docs/adr/0057-export-pipeline.md` (Accepted). The decisions, in the order they were
made while building it:

- **Where the code goes.** The engine went into `crates/nicti-preen` and stays GPU-free and
  catalog-free (it depends on neither `nicti-tapetum` nor `nicti-lair`), so every pixel/format/
  naming/collision behavior is unit-testable without an adapter. The render and the Pounce wiring
  live in `nicti-pelt/src/export/`. The graph/registry/"document → kernel inputs" step moved from
  `nicti-pelt::render` into `nicti_tapetum::spine` so Develop, export and `bench/knead` share it.
- **Chained jobs, one lane each.** decode (CPU) → render (GPU, one tile per step) → encode+write
  (CPU). A single job would run a ~1.7 s decode inside a GPU-lane step and starve `RemoveJob`.
  Stages submit the next through a weak `Pounce::submitter()`, because a job holding a `Pounce`
  clone could become the last handle on a worker thread, where `Drop for Pounce` would join itself.
  The render job declares `vram_bytes: 0` because Pounce silently drops a background job that
  declares more than the *total* budget and a 45 MP frame's textures exceed the placeholder budget.
- **The photo-identity bug.** `apply_document` recomputes every node's hash from the document, so
  the identity Develop/Loupe set with `set_own_hash(DECODE, ..)` was reset on the first render and
  two same-size photos could be served each other's cached pixels. Found because export would have
  hit it immediately (mutation-checked: removing the stamp makes the tests fail). Fix: stamp the
  identity into the render-time document copy.
- **Edits come from the catalog.** `edit_variant` was always written empty and never read. The
  get/put pair on `CatalogStore` plus Develop autosave (pointer release when dirty, before a photo
  switch, before an export, on exit) is the minimum export needs; History/undo and AI-removal
  recompute stay with #324. Export skips AI Remove spots and fails a photo whose DCP profile is gone.
- **Orientation** is applied after resize, from the file's EXIF, because LibRaw's decode is
  unrotated. **PNG EXIF** is a raw `eXIf` chunk lifted from a 1×1 JPEG `little_exif` writes: its own
  PNG writer emits a non-standard zTXt "Raw profile type exif" and rewrites any XMP it finds.
- **TIFF** turned out to be writable with ICC, XMP and resolution through the `tiff` crate (the ICC
  tag must be UNDEFINED, not the crate's `[u8]` BYTE). Two exiftool TIFF complaints are validator/
  crate quirks, not defects: it flags every Adobe-Deflate file (libtiff's own `tiffcp -c zip`
  too), and the `tiff` crate leaves IFD values at an odd offset after an odd compressed strip.
- **Collisions.** `create_new` claims a name atomically (threads, processes, FAT/exFAT), a unique
  temp file is written and renamed over the claim, and the plan de-duplicates within the batch on a
  case-folded path under every policy. `spikes/scent::atomic_write` (one shared temp name, silent
  overwrite) is unsafe for parallel writers.
