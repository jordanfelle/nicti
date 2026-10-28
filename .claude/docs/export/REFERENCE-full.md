<!-- Full text of .claude/rules/export/REFERENCE.md before it was split into an index. NOT auto-loaded. -->
# Export Stack — Quick Reference

Full reasoning/history: `docs/decisions/export.md`.

- **Export stack (#56)** — `docs/adr/0056`: **Proposed**, JPEG-encoder pick now confirmed
  (#223), still pending a real full-resolution GPU-resize measurement on the reference RTX 5080;
  the pipeline built on it landed in #57 (see below).
- **Resize**: `fast_image_resize` Lanczos3 over a hand-converted linear-light `f32x3` buffer
  (`resize::resize_fast_linear`) is v1's pick, not `image::imageops`'s naive gamma-space resize —
  the naive path is faster but measurably wrong on high-contrast content. A `wgpu` compute
  candidate (`gpu_resize::GpuLanczosResizer`, real two-pass separable Lanczos3, not a stub) exists
  and is correctness-verified, but has no real-hardware timing yet (WSL sandbox exposes only
  `llvmpipe` to `wgpu`, and its 128MiB `max_storage_buffer_binding_size` also caps full-res GPU
  work below a real 45MP source buffer's 731MB).
- **JPEG encoder**: `jpeg-encoder` (pure Rust) for v1, **settled by a quality-matched comparison
  (#223) on one synthetic test image** — not yet real-photo-confirmed (see below). Real confound
  found in the original nominal-quality-90 comparison: `jpeg-encoder`'s own default silently
  switches chroma subsampling 4:2:0→4:4:4 at quality ≥90 while mozjpeg's stays fixed at 4:2:0 —
  not just an uncalibrated-quality-number issue. With subsampling pinned to 4:2:0 on both sides
  and quality matched to a target SSIM (`find_matched_quality`), mozjpeg is genuinely ~1.3-1.4x
  smaller but ~4-4.5x slower — real on that image, but fails this ADR's own ≤1.5x-encode-time bar.
  mozjpeg (`native` Cargo feature, off by default like `nicti-decode`'s `libraw` feature) stays
  available for a future reference-machine/real-NEF re-check, not promoted to v1 default. This
  pass's own two synthetic fixtures gave meaningfully different SSIM ceilings at 4:2:0 (one capped
  around 0.91 regardless of quality) — real photographic chroma content could shift the result.
- **Metadata**: `little_exif` (EXIF, builds the IFD itself), `moxcms`-generated sRGB ICC + `img-
  parts` (embed, both JPEG/PNG), hand-rolled XMP JPEG-APP1/PNG-iTXt insert on `img-parts`
  primitives (adapted from `spikes/scent`'s approach — copied, not depended-on). `metadata::
  wrap_xpacket` wraps a raw XMP document in the standard xpacket PI pair; a real pipeline output
  needs this or `exiftool -validate` flags it. TIFF metadata write deferred (no TIFF-tag-writer
  crate in this workspace, same gap ADR-0059 left for DNG).
- **Watermark**: `resvg`/`usvg`/`tiny-skia` SVG rasterization + linear-premultiplied "over"
  compositing (`watermark::composite_sequential`). Sequential beats `rayon`-parallel at typical
  logo sizes — don't reach for `composite_parallel` without measuring first. Text watermarking
  deferred (no vendored font license picked).
- **Gotchas hit building this**: `wgpu`'s default buffer-size limits are too small for a full-res
  linear-light source buffer (request `adapter.limits()` on device creation); a shader module with
  entry points at different `@group` indices needs `get_bind_group_layout`/`set_bind_group` called
  with *that entry point's* group index, not always 0.
- **Open follow-ups**: reference-machine run (real full-res GPU resize timing + LRC export-
  throughput comparison for `docs/benchmarks.md`'s >=2x target, plus a reference-hardware
  re-confirmation of #223's JPEG-encoder speed ratio — this sandbox's numbers are Linux CPU, not
  the Windows target), and a real-NEF re-measurement of #223's whole comparison once #41's render
  pipeline exists (synthetic-image-only so far).

- **Export pipeline (#57, landed)** — `docs/adr/0057`: **Accepted**. Engine = `crates/nicti-preen`
  (GPU-free, catalog-free: `export_frame(WorkingFrame, ExportContext, ExporterRegistry)` does
  size → linear-light resize → orient → output matrix → watermark → quantize → encode); render +
  jobs = `crates/nicti-pelt/src/export/` (`jobs.rs` `ExportRun`, `dialog.rs` `ExportUi`,
  `presets.rs`, `sink.rs`). Per photo: decode (CPU lane) → render (GPU lane, one tile/step, lane-clamped `vram_bytes`, #255)
  → encode+write (CPU lane), chained through `Pounce::submitter()`; one `Ticket` per photo settles
  exactly once, so the report always adds up. Memory bounded by `advance()` (~2.4 GB worst case at
  45 MP full size).
- **Gotcha (#57)**: `RenderGraph::apply_document` recomputes *every* node's hash from the document,
  so `set_own_hash(DECODE, identity)` is undone on the next render and two same-size photos share
  cache keys. Always `spine::stamp_source_identity` on the render-time document copy (Develop and
  export both do; regression tests in `nicti-pelt` `render.rs` and `export/jobs.rs`).
- **Names/collisions (#57)**: tokens `{Filename} {Sequence[:N]} {Date[:fmt]} {Rating} {Make} {Model}
  {Folder}`; sequence = start + selection position, fixed at plan time (failures leave gaps);
  batch de-duplicated on a case-folded path under every policy; `write_output` claims the name with
  `create_new`, writes a unique temp, renames. Every component sanitized on every OS.
- **Formats (#57)**: JPEG = `jpeg-encoder` (subsampling pinned, JFIF density, ICC native, XMP APP1,
  EXIF via `little_exif`). PNG = `image` + one `img-parts` pass (iCCP, pHYs, iTXt, `eXIf` from the
  raw TIFF block — `little_exif`'s own PNG writer emits a non-standard zTXt and rewrites XMP).
  TIFF = `tiff` crate with resolution/ICC (UNDEFINED type)/XMP tags and EXIF (#337, IFD0 + Exif/GPS sub-IFDs). EXIF `ColorSpace` is
  0xFFFF for non-sRGB. exiftool `-validate` clean, except two known TIFF quirks (Adobe-Deflate
  flag; odd IFD offsets after an odd compressed strip).
- **WebP/AVIF (#429)**: lossless WebP = `image-webp` (ICC/EXIF/XMP chunks, 8-bit only; lossy #361 still open, no JXL encoder #359). AVIF = `ravif`/rav1e behind `nicti-preen` feature `avif` (pelt `avif-export`, off by default, #286); BT.601 full-range 4:4:4, 8 or 10-bit (`Sixteen`=10), EXIF only; **sRGB only** (no ICC/XMP in ravif, `colr` box omitted for the implied BT.709/sRGB; wide gamut needs a hand-written colr/ICC box). Encode-speed benchmark not yet run.
- **Edits (#57)**: read from `CatalogStore::get_master_edit`; Develop autosaves. AI Remove spots
  are skipped at export (patches aren't persisted, #324); local adjustments (masks) are applied at the photo's own extent (#354, see `masking`); a missing/changed DCP profile fails that
  photo rather than exporting different colors. Export rotates by the file's EXIF Orientation.

## Package contents

- **`spikes/prey`** (#56/ADR-0056's export-stack research) — `resize.rs`/`gpu_resize.rs` (CPU/GPU
  linear-light Lanczos3 resize), `encode.rs` (`jpeg-encoder`/`image`-crate/mozjpeg JPEG candidates
  plus 16-bit TIFF/PNG encode; `find_matched_quality` + the `_with_sampling` encoder variants for
  #223's quality-matched, subsampling-pinned comparison), `icc.rs` (`moxcms` sRGB profile
  generation + `img-parts` JPEG/PNG embed), `metadata.rs` (`little_exif` EXIF write, hand-rolled
  XMP JPEG-APP1/PNG-iTXt insert, `wrap_xpacket`), `watermark.rs` (`resvg` SVG rasterize +
  linear-premultiplied composite, sequential and `rayon`-parallel). `src/bin/prey.rs` exposes
  `resize`/`encode`/`metadata`/`watermark`/`pipeline`/`quality-sweep` subcommands, each measured
  via `nicti-prowl::perf::Protocol`. Unit tests all use synthetic images (no real NEF/render
  pipeline exists yet), not path-gated. See `docs/research/prey-export-stack.md`.
- **`crates/nicti-preen`** (#57) — `spec.rs` (`ExportSpec`/`ExportPreset`, `validate`), `naming.rs`
  (`Template`, `sanitize_component`), `plan.rs` (`plan_batch`), `write.rs` (`write_output`),
  `resize.rs`, `orient.rs`, `color.rs`, `watermark.rs` (SVG/PNG logos), `metadata.rs` (source EXIF
  read, EXIF/XMP build + embed), `exporters.rs` (`JpegExporter`/`PngExporter`/`TiffExporter`,
  `builtin_registry`), and `lib.rs` (`Exporter`, `export_frame`). `tests/exiftool_validate.rs`
  skips when `exiftool` isn't on PATH.
- **`crates/nicti-pelt/src/export/`** (#57) — see the Export pipeline bullet above. Presets are
  `<catalog>.export-presets.json` (temp+rename), three built-ins never written. Ctrl+Shift+E or the
  Library toolbar's "Export N…" opens the dialog.

- **Streaming shrink (#336)** — `nicti_preen::resize::StreamResizer` is the same Lanczos3 as `resize_linear_f32` (weights computed like `fast_image_resize`: support widens by the scale when shrinking, normalised per destination pixel), but separable and additive: each source pixel is added into the destination pixels it contributes to, so tiles can arrive in any order and the full-size frame never exists. Memory is the output (`dst_w*dst_h*3` f32; ~35 MB for 2048x1365) plus one row of scratch, instead of ~540 MB at 45 MP. `resize::shrink_target(w, h, orientation, spec)` returns the sensor-orientation target only for a plain shrink whose re-application is a no-op (checked idempotent), else `None`; `export::sink::RenderSink` is `Full(AccumSink)` or `Shrink(ShrinkSink)`, picked in `RenderJob::begin`, and `export_frame` then finds nothing left to resize. Parity with the one-shot resize is pinned to <2e-3 over several tilings and orders. Not done: the GPU readback is still at render resolution (0.8-1.5 s per full frame, ADR-0016), so the issue's "reads back <= output size" needs the GPU resize (`spikes/prey`'s `GpuLanczosResizer`) and the >=2x-LRC throughput measurement needs the reference machine; both stay open as a follow-up. Tiles must cover each source pixel exactly once (the planner's cores do); a gap darkens its destination pixels.

- **TIFF EXIF (#337)** — `nicti-preen/src/tiff_exif.rs`: a TIFF has no APP1/`eXIf` slot, so the EXIF block `metadata::build_exif` makes (`exif_tiff_bytes`, the same bytes a PNG `eXIf` chunk carries) is parsed (bounded, either byte order, native-order values, skip-not-fail on malformed entries) and re-written through the `tiff` encoder: IFD0 tags (Make, Model, Artist, Copyright, Software, Orientation, DateTime, ...) go into the image's own IFD, the Exif and GPS sub-IFDs are written first with `extra_directory()` and linked by tags 34665/34853. Image-owned tags (size, strips, compression, resolution, ICC, XMP) are never copied. exiftool `-validate` rules for TIFF: no ComponentsConfiguration/ExifImageWidth/ExifImageHeight in the Exif IFD (the size lives in the image directory), FlashpixVersion required (added as `0100`), and out-of-line values must start on a word boundary (odd-length ones are padded; the entry count excludes the pad). exiftool reads the same Make/Model/Lens/ISO/ExposureTime/FNumber/FocalLength/DateTimeOriginal/Artist/Copyright/Software/Orientation from a TIFF as from a JPEG (`tests/exiftool_validate.rs`, skips without exiftool); validate is OK for TIFF none/LZW, and Deflate keeps only the two known quirks. Interop and MakerNote sub-structures are not carried.

- **Text watermark (#338)** — `WatermarkSpec { text, text_color }` (text wins over the logo path; validated 1-120 chars, no control characters; `WatermarkSource::for_spec` picks). Set in the bundled **Inter Regular** (SIL OFL 1.1; the one copy is `crates/nicti-pelt/assets/fonts/Inter-Regular.ttf`, `include_bytes!`'d by `nicti-preen`, so no new asset row), outlined by `skrifa` 0.44 (MIT OR Apache-2.0, already in the lockfile through egui's font stack: no new crate) into one `tiny-skia` path, filled and then composited exactly like a logo (linear light, anchor/scale/opacity/inset unchanged). The line box is the *ink* plus 3% em padding (not the font's line metrics), so leading/trailing spaces cannot shift the text inside its box and tall accents/descenders are not clipped; whitespace-only text and zero-width/bidi-control characters are rejected by `validate`. No shaping: Latin/Greek/Cyrillic, no kerning, no complex scripts or bidi; a glyph the font lacks draws `.notdef` rather than failing the export. The export dialog gained a Text field and a color button. Not done: choosing a font, bold/italic, multi-line text, a stroke/shadow for legibility on mixed backgrounds, and `{Filename}`-style tokens in the text.

- **Output sharpening (#339)** — `ExportSpec.sharpen: Option<SharpenSpec { media: Screen|Matte|Glossy, amount: Low|Standard|High }>` (None = off; old presets load with it off), `nicti-preen/src/sharpen.rs`, applied in `export_frame` after the linear-light resize and orientation, in output space, **before** the watermark (a logo is never sharpened). An unsharp mask on perceptual (sRGB-OETF) luma only: Gaussian blur of the luma, difference x amount, 0.0025 noise threshold, correction capped at 0.25, then the pixel's linear RGB is scaled by the luma ratio so hue is preserved (near-black pixels get an additive change); pixels over 1.0 / non-finite are skipped. Radius and strength table: screen 0.6 px / .35 .6 .9; matte 1.0 px x dpi/300 / .5 .85 1.3; glossy 0.8 px x dpi/300 / .4 .7 1.1 (paper radii scale with the export DPI, clamped 0.5-2x; screen ignores it). **Untuned**: reasoned starting values, not matched to LRC's unpublished output; need a visual comparison and real prints on the reference machine. Costs two extra luma planes (w*h f32 each) while it runs, so a no-resize 45 MP export adds ~360 MB transiently. The export dialog's Size section has the media and amount combos. Not done: per-media threshold/halo tuning, sharpening in a wider-gamut-aware luma (the luma weights are Rec.709 for every export space), and a before/after preview.
