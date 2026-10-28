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

- **Streaming shrink (#336)** — `nicti_preen::resize::StreamResizer` is the same Lanczos3 as `resize_linear_f32` (weights computed like `fast_image_resize`: support widens by the scale when shrinking, normalised per destination pixel), but separable and additive: each source pixel is added into the destination pixels it contributes to, so tiles can arrive in any order and the full-size frame never exists. Memory is the output (`dst_w*dst_h*3` f32; ~35 MB for 2048x1365) plus one row of scratch, instead of ~540 MB at 45 MP. `resize::shrink_target(w, h, orientation, spec)` returns the sensor-orientation target only for a plain shrink whose re-application is a no-op (checked idempotent), else `None`; `export::sink::RenderSink` is `Full(AccumSink)` or `Shrink(ShrinkSink)`, picked in `RenderJob::begin`, and `export_frame` then finds nothing left to resize. Parity with the one-shot resize is pinned to <2e-3 over several tilings and orders. Not done: the GPU readback is still at render resolution (0.8-1.5 s per full frame, ADR-0016), so the issue's "reads back <= output size" needs the GPU resize (`spikes/prey`'s `GpuLanczosResizer`) and the >=2x-LRC throughput measurement needs the reference machine; both stay open as a follow-up. Tiles must cover each source pixel exactly once (the planner's cores do); a gap darkens its destination pixels.

- **TIFF EXIF (#337)** — `nicti-preen/src/tiff_exif.rs`: a TIFF has no APP1/`eXIf` slot, so the EXIF block `metadata::build_exif` makes (`exif_tiff_bytes`, the same bytes a PNG `eXIf` chunk carries) is parsed (bounded, either byte order, native-order values, skip-not-fail on malformed entries) and re-written through the `tiff` encoder: IFD0 tags (Make, Model, Artist, Copyright, Software, Orientation, DateTime, ...) go into the image's own IFD, the Exif and GPS sub-IFDs are written first with `extra_directory()` and linked by tags 34665/34853. Image-owned tags (size, strips, compression, resolution, ICC, XMP) are never copied. exiftool `-validate` rules for TIFF: no ComponentsConfiguration/ExifImageWidth/ExifImageHeight in the Exif IFD (the size lives in the image directory), FlashpixVersion required (added as `0100`), and out-of-line values must start on a word boundary (odd-length ones are padded; the entry count excludes the pad). exiftool reads the same Make/Model/Lens/ISO/ExposureTime/FNumber/FocalLength/DateTimeOriginal/Artist/Copyright/Software/Orientation from a TIFF as from a JPEG (`tests/exiftool_validate.rs`, skips without exiftool); validate is OK for TIFF none/LZW, and Deflate keeps only the two known quirks. Interop and MakerNote sub-structures are not carried.
