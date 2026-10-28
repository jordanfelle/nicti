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
- ~~TIFF export's metadata write (ICC/EXIF/XMP) stays deferred~~ — ICC/XMP landed in #57, EXIF in #337 (`tiff_exif.rs`, via the `tiff` encoder's `extra_directory`); DNG metadata write is still open (ADR-0059).

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

- **Text watermark (#338)** — `WatermarkSpec { text, text_color }` (text wins over the logo path; validated 1-120 chars, no control characters; `WatermarkSource::for_spec` picks). Set in the bundled **Inter Regular** (SIL OFL 1.1; the one copy is `crates/nicti-pelt/assets/fonts/Inter-Regular.ttf`, `include_bytes!`'d by `nicti-preen`, so no new asset row), outlined by `skrifa` 0.44 (MIT OR Apache-2.0, already in the lockfile through egui's font stack: no new crate) into one `tiny-skia` path, filled and then composited exactly like a logo (linear light, anchor/scale/opacity/inset unchanged). The line box is the *ink* plus 3% em padding (not the font's line metrics), so leading/trailing spaces cannot shift the text inside its box and tall accents/descenders are not clipped; whitespace-only text and zero-width/bidi-control characters are rejected by `validate`. No shaping: Latin/Greek/Cyrillic, no kerning, no complex scripts or bidi; a glyph the font lacks draws `.notdef` rather than failing the export. The export dialog gained a Text field and a color button. Not done: choosing a font, bold/italic, multi-line text, a stroke/shadow for legibility on mixed backgrounds, and `{Filename}`-style tokens in the text.

- **Output sharpening (#339)** — `ExportSpec.sharpen: Option<SharpenSpec { media: Screen|Matte|Glossy, amount: Low|Standard|High }>` (None = off; old presets load with it off), `nicti-preen/src/sharpen.rs`, applied in `export_frame` after the linear-light resize and orientation, in output space, **before** the watermark (a logo is never sharpened). An unsharp mask on perceptual (sRGB-OETF) luma only: Gaussian blur of the luma, difference x amount, 0.0025 noise threshold, correction capped at 0.25, then the pixel's linear RGB is scaled by the luma ratio so hue is preserved (near-black pixels get an additive change); pixels over 1.0, out-of-gamut darks and non-finite ones are not modified but feed the blur at their clamped luma (a neighbour of a blown highlight sees a true edge; found in review: mid-grey fill had inverted the response there). Black cannot get darker and is skipped. Radius and strength table: screen 0.6 px / .35 .6 .9; matte 1.0 px x dpi/300 / .5 .85 1.3; glossy 0.8 px x dpi/300 / .4 .7 1.1 (paper radii scale with the export DPI, clamped 0.5-2x; screen ignores it). **Untuned**: reasoned starting values, not matched to LRC's unpublished output; need a visual comparison and real prints on the reference machine. Costs two extra luma planes (w*h f32 each) while it runs (blurred in place with one scratch plane), so a no-resize 45 MP export adds ~360 MB transiently; ~0.4-0.65 s per 24 MP single-threaded in release. A channel already over 1.0 shifts colour when its pixel is scaled (the usual clipped-colour behaviour of luma scaling). The export dialog's Size section has the media and amount combos. Not done: per-media threshold/halo tuning, sharpening in a wider-gamut-aware luma (the luma weights are Rec.709 for every export space), and a before/after preview.

- **GPS EXIF (#340)** — `SourceExif.gps: Option<Gps>` is read from the source file's EXIF (`exif` crate, same read as the rest of `SourceExif`; nothing is stored in the catalog) and validated all-or-nothing: hemisphere letters N/S/E/W, degrees within range, no zero denominators, and the (0,0) "no fix" some cameras write is treated as absent. `MetadataSpec.include_location` (default **false**: privacy; old presets load with it off) makes `build_exif` write the GPS sub-IFD (GPSVersionID, Lat/Lon + refs, Altitude + ref) through `little_exif`; it only applies under `MetadataPolicy::All`, and `CopyrightOnly`/`None` never carry a position. The XMP never carries one. All three EXIF formats read it back in exiftool (JPEG, PNG `eXIf`, TIFF via #337's `tiff_exif.rs`) with the same coordinates, and `-validate` is clean; a TIFF GPS IFD needs `GPSProcessingMethod` (TIFF/EP), which `tiff_exif.rs` adds as `GPS` when absent (the other formats don't require it, so none is invented for them). The dialog's Metadata row has an "Include location (GPS)" checkbox (enabled only under All metadata). Not done: GPS date/time stamp, speed/bearing/direction tags, reading GPS from XMP sidecars, writing the position into the XMP, and WebP (lossless WebP's EXIF path was not checked against exiftool for GPS).

- **Export planning is a Pounce job (#343)** — `ExportRun::start` now only validates (settings, exporter, watermark file: a mistake still comes straight back as `StartError`) and rejects an empty id list; reading each photo from the catalog (asset, root, keywords, saved edit) and planning the file names (`plan_batch`, one `stat` per photo) run as the first CPU-lane job, `PlanJob`, `PLAN_CHUNK` = 64 photos per chunk so a 10k-photo batch never holds the UI thread and cancel is honoured between chunks. `ExportRun` is a small phase machine (`Planning` -> `Running(Shared)` | `Ended(report)`); `preparing()` gives `(prepared, total)` for the dialog's "Preparing N of M…" line, `progress()`/`poll()`/`cancel()` behave as before once running, and a planning failure (catalog error, nothing left to export, a planning conflict) or a cancel during planning ends the run with a report (`total` = ids given, the reason in `warnings`, or all `cancelled`) instead of an up-front error. A `PlanJob` dropped un-run (runtime shut down) also ends the run rather than leaving it hanging. The job is `Priority::Foreground` (user-initiated and brief), so it does not queue behind a library scan; the heavy stages stay Background. A cancel racing the Planning->Running flip is forwarded after the flip (found in review with an injected delay: the cancel stored its flag, saw `Planning`, and the export ran anyway); a Pounce-level cancel of the planning job (the activity panel's button) ends the run via `Drop`. `PLAN_CHUNK` is 2 under `cfg(test)` so multi-chunk batches are exercised with a handful of photos. Not done: `plan_batch` itself is still one call inside the last chunk (its `stat`s are the remaining O(n) cost, now off the UI thread), and a planning failure no longer reopens the dialog with the settings in place.
