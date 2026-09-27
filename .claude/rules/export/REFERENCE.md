---
paths:
  - "spikes/prey/**"
  - "docs/adr/0056-export-stack.md"
  - "crates/nicti-preen/**"
---
# Export Stack — Quick Reference

Full reasoning/history: `docs/decisions/export.md`.

- **Export stack (#56)** — `docs/adr/0056`: **Proposed**, pending a quality-matched JPEG-encoder
  comparison and a real full-resolution GPU-resize measurement on the reference RTX 5080; wiring
  into a real export pipeline is #57's build.
- **Resize**: `fast_image_resize` Lanczos3 over a hand-converted linear-light `f32x3` buffer
  (`resize::resize_fast_linear`) is v1's pick, not `image::imageops`'s naive gamma-space resize —
  the naive path is faster but measurably wrong on high-contrast content. A `wgpu` compute
  candidate (`gpu_resize::GpuLanczosResizer`, real two-pass separable Lanczos3, not a stub) exists
  and is correctness-verified, but has no real-hardware timing yet (WSL sandbox exposes only
  `llvmpipe` to `wgpu`, and its 128MiB `max_storage_buffer_binding_size` also caps full-res GPU
  work below a real 45MP source buffer's 731MB).
- **JPEG encoder**: `jpeg-encoder` (pure Rust) for v1. mozjpeg (`native` Cargo feature,
  off by default like `nicti-decode`'s `libraw` feature) produced ~2.6x smaller files at the same
  nominal "quality 90" but nominal quality isn't comparable across encoders — not yet a
  like-for-like result, follow-up needed before promoting mozjpeg.
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
  throughput comparison for `docs/benchmarks.md`'s >=2x target), quality-matched JPEG-encoder
  re-measurement.

## Package contents

- **`spikes/prey`** (#56/ADR-0056's export-stack research) — `resize.rs`/`gpu_resize.rs` (CPU/GPU
  linear-light Lanczos3 resize), `encode.rs` (`jpeg-encoder`/`image`-crate/mozjpeg JPEG candidates
  plus 16-bit TIFF/PNG encode), `icc.rs` (`moxcms` sRGB profile generation + `img-parts` JPEG/PNG
  embed), `metadata.rs` (`little_exif` EXIF write, hand-rolled XMP JPEG-APP1/PNG-iTXt insert,
  `wrap_xpacket`), `watermark.rs` (`resvg` SVG rasterize + linear-premultiplied composite,
  sequential and `rayon`-parallel). `src/bin/prey.rs` exposes `resize`/`encode`/`metadata`/
  `watermark`/`pipeline` subcommands, each measured via `nicti-prowl::perf::Protocol`. 35 unit
  tests (all synthetic images — no real NEF/render pipeline exists yet), not path-gated. See
  `docs/research/prey-export-stack.md`.
