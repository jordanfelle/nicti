## Export stack

Covers #56's export-stack research — full reasoning and every measured number in
`docs/adr/0056-export-stack.md`. This file is the per-topic summary; that ADR is the full research
trail.

- **Handoff from ADR-0019**: `crates/nicti-export`'s `Exporter` trait was left as identity/
  versioning only, deferring resize/encode/metadata-write/watermark execution and the export
  method signature to this ticket and #57.
- **Resize**: `fast_image_resize`'s Lanczos3 over a hand-converted linear-light buffer, not
  `image::imageops`'s naive gamma-space resize — the naive path is faster (56ms vs. 201ms p50 at
  screen-res) but measurably diverges from the correct result on high-contrast content. A GPU
  candidate (`wgpu` compute) was built and cross-verified against the CPU one, but real hardware
  timing wasn't reachable this pass (this sandbox's WSL exposes no NVIDIA Vulkan ICD; only
  `llvmpipe`, whose own buffer-binding limit also caps full-res GPU work below a real 45MP frame).
- **JPEG encoder**: `jpeg-encoder` (pure Rust) for v1, not mozjpeg — mozjpeg's file size was
  ~2.6x smaller at the same nominal "quality 90" parameter, but the two encoders' quality scales
  aren't calibrated the same way, so this isn't yet a like-for-like comparison. mozjpeg stays
  available behind a `native` Cargo feature for a follow-up quality-matched re-measurement.
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
