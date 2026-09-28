---
paths:
  - "spikes/prey/**"
  - "docs/adr/0056-export-stack.md"
  - "crates/nicti-preen/**"
---
# Export Stack — Quick Reference

Full reasoning/history: `docs/decisions/export.md`.

- **Export stack (#56)** — `docs/adr/0056`: **Proposed**, JPEG-encoder pick now confirmed
  (#223), still pending a real full-resolution GPU-resize measurement on the reference RTX 5080;
  wiring into a real export pipeline is #57's build.
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
