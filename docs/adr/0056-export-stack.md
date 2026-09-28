# ADR-0056: Export stack

- **Status:** Proposed -- the JPEG-encoder choice below is now settled by a quality-matched
  comparison (#223); still pending a real full-resolution GPU-resize measurement on the reference
  RTX 5080 (see Consequences)
- **Date:** 2026-09-27
- **Ticket:** [#56](https://github.com/jordanfelle/nicti/issues/56) Research: export stack

## Context

`crates/nicti-preen`'s `Exporter` trait settles identity/versioning only (`pub trait Exporter:
Module {}`) -- its own doc comment defers resize/encode/metadata-write/watermark execution and
the export method signature to this ticket and #57 (Build: export pipeline, currently blocked by
this one). Four component choices were open: a SIMD resizer, a JPEG encoder (mozjpeg vs.
alternatives), a metadata library that can **write** EXIF/XMP/ICC (`kamadak-exif`, already used
for read, is read-only), and a watermark path (SVG/PNG logos, alpha-blended). The benchmark
target is `docs/benchmarks.md`'s "Export >= 2x LRC throughput on the same batch and settings,"
measured by wall clock -- the LRC side of that comparison needs the reference machine and is a
follow-up, not this pass.

## Decision rules (stated before measuring)

- **Resize**: happens in linear light, before the sRGB OETF is reapplied -- averaging
  gamma-encoded values during a downsample is not the same as averaging light, and measurably
  diverges from a linear-light resize on high-contrast content (`resize.rs`'s own test of this).
  A GPU candidate is preferred over CPU if it plus a smaller readback beats CPU plus a full-res
  readback (ADR-0016 measured a full-res GPU->host readback at 0.8-1.5s).
- **JPEG encoder**: at a matched perceptual quality, pick the smallest file whose encode time is
  <=1.5x the fastest. Must support ICC (APP2), arbitrary APPn insertion, and 4:4:4/4:2:0 chroma
  subsampling. Pure Rust wins ties; a C dependency has to earn its place with a real quality-
  matched size advantage, not just a smaller file at the same nominal "quality" parameter (which
  isn't comparable across encoders -- see Consequences).
- **Metadata**: must write the EXIF export subset (Make/Model/Lens, exposure triangle,
  DateTimeOriginal+OffsetTime, Artist/Copyright, Orientation reset to 1, Software, plus the
  mandatory ExifIFD/IFD0 tags a validator checks for), XMP, and ICC into JPEG and PNG, verified by
  `exiftool -validate -warning` with zero warnings. Pure Rust preferred; a native fallback only if
  no pure-Rust option passes.
- **Watermark**: SVG and PNG logos, composited in linear-premultiplied alpha (straight-alpha
  gamma-space blending darkens semi-transparent edges the same way a naive resize does).

## Decision

### Spike: `spikes/prey`

Real, tested (35 unit tests, all against synthetic images -- no real NEF/render pipeline exists
yet, see `docs/research/prey-export-stack.md`'s Method section), not path-gated. A `native` Cargo
feature (off by default, same pattern as `nicti-decode`'s `libraw` feature) gates the one
dependency needing a C compiler + nasm.

### Resize

**`fast_image_resize`'s Lanczos3 convolution over a hand-converted linear-light buffer**
(`resize::resize_fast_linear`), not `image::imageops`'s naive gamma-space resize. Measured
201.33ms p50 at screen-res (3840x2160 -> 1024x576) vs. the naive candidate's 56.12ms -- the naive
path is faster, but only because it skips the eotf/oetf conversion that makes the result
correct on high-contrast content (SSIM < 0.95 between the two candidates on a checkerboard --
`resize.rs::linear_resize_differs_from_naive_srgb_resize_on_high_contrast_edges`). A GPU
candidate (`gpu_resize::GpuLanczosResizer`, a real two-pass separable Lanczos3 `wgpu` compute
kernel, not a stub) was built and cross-verified against the CPU candidate (SSIM > 0.95,
`gpu_resize.rs::gpu_resize_matches_cpu_linear_resize`), but **real hardware timing wasn't
reachable this pass** -- this sandbox's WSL environment exposes no NVIDIA Vulkan ICD to `wgpu`
(only `llvmpipe` software rendering, confirmed via `crouch`'s own adapter log), and `llvmpipe`'s
own 128MiB `max_storage_buffer_binding_size` caps a linear-light full-res source buffer well below
a real 45MP frame's 731MB. **v1 uses the CPU candidate**; the GPU candidate stays available for a
follow-up once real hardware numbers exist (see Consequences).

### JPEG encoder

**`jpeg-encoder` (pure Rust) stays the v1 pick for this pass, per a quality-matched comparison
(#223)**, not mozjpeg. The original nominal-"quality 90" comparison (122KB vs. 327KB, mozjpeg
~2.6x smaller) turned out to have a real confound, not just an uncalibrated-quality-number one:
`jpeg-encoder` 0.6.1's own default (`Encoder::new`) silently switches chroma subsampling from
4:2:0 to 4:4:4 at quality >=90, while mozjpeg's libjpeg default stays fixed at 4:2:0 regardless of
quality -- so the original comparison was partly comparing two different subsampling modes, not
just two encoders' quantization tables. Pinning subsampling to 4:2:0 on both sides
(`encode_jpeg_encoder_with_sampling`/`native::encode_mozjpeg_with_sampling`) and finding each
encoder's smallest quality parameter that reaches a fixed target SSIM against the source
(`find_matched_quality`, a linear ascending scan over quality 1-100 -- not a binary search, since
SSIM isn't guaranteed strictly monotonic in quality) gives a genuine like-for-like result **on one
synthetic test image**: at SSIM 0.98, mozjpeg is real but modest -- ~1.29x smaller (134KB vs.
173KB) -- and at SSIM 0.95, ~1.44x smaller (91KB vs. 131KB), consistent across both targets on
that same image. But mozjpeg is also consistently **~4-4.5x slower at matched quality** (measured
104-116ms p50 vs. jpeg-encoder's 23-29ms p50), which fails this ADR's own decision rule (a
candidate's encode time must stay <=1.5x the fastest to "earn its place"). mozjpeg's size
advantage on this synthetic source is confirmed and no longer speculative, but it's just not large
enough to clear the speed bar this ADR set before measuring. Two things stay open before treating
this as final, not just the usual hardware caveat: a real-NEF re-measurement once #41's render
pipeline exists (this pass's own two synthetic test images gave meaningfully different SSIM
ceilings at 4:2:0, so real photographic chroma content could shift the matched-quality gap), and a
reference-machine re-check of the *speed* ratio specifically. The `native` feature keeps mozjpeg
available (`native::encode_mozjpeg`/`encode_mozjpeg_with_sampling`, also has a native
`write_icc_profile`) for both follow-ups.

### Metadata (EXIF / XMP / ICC)

- **EXIF: `little_exif`**, not `img-parts`' own EXIF support -- `img-parts` only stores/replaces
  a raw EXIF byte blob, it doesn't build the TIFF-structured IFD itself, while `little_exif`
  builds it from typed tags directly and already supports JPEG/PNG/TIFF write paths.
  `metadata::build_exif` writes the full export subset the decision rule requires, plus the
  mandatory `ExifVersion`/`ComponentsConfiguration`/`ColorSpace`/`ExifImageWidth`/
  `ExifImageHeight`/`YCbCrPositioning` tags a real `exiftool -validate` run flagged missing on
  this spike's own first pipeline output (see Measured results below) -- fixed and now a
  regression test, not just fixed ad hoc.
- **ICC: `moxcms`-generated sRGB profile + `img-parts`' `ImageICC`**. `moxcms::ColorProfile::
  new_srgb().encode()` builds a fresh profile at runtime (no vendored `.icc` file, no
  license/provenance question); `img-parts` embeds it (JPEG: auto-split across APP2 segments per
  spec -- unlike `spikes/scent`'s own single-segment XMP splice, an sRGB v2 profile is ~3KB so
  this headroom doesn't matter in practice, but a wider-gamut v4 profile could exceed one segment;
  PNG: `iCCP` chunk, `img-parts` handles the zlib compression itself).
- **XMP: a hand-rolled JPEG APP1 segment insert / PNG `iTXt` chunk insert**
  (`metadata::embed_xmp_jpeg`/`embed_xmp_png`), generalizing `spikes/scent`'s JPEG APP1 approach
  (same signature constant, same single-segment 64KB cap -- an export XMP packet is a few hundred
  bytes, nowhere near that limit) onto `img-parts`' generic segment/chunk primitives, since
  `img-parts` is already a dependency here for ICC/EXIF. `metadata::wrap_xpacket` wraps a raw
  `<x:xmpmeta>` document in the standard xpacket processing-instruction pair -- `exiftool
  -validate` flagged its absence as a minor warning on the first real pipeline output.
- **TIFF ICC/EXIF/XMP write is deferred** -- same class of gap ADR-0059 already flagged for
  DNG/TIFF: no crate in this workspace builds arbitrary TIFF tags, so a naive splice isn't safe
  the way it is for JPEG's flat segment list.

### Watermark

**Linear-premultiplied "over" compositing** (`watermark::composite_sequential`/
`composite_parallel`, straight-alpha in/out, sRGB-encoded), over a `resvg`/`usvg`/`tiny-skia`-
rasterized SVG or a decoded PNG logo. A real half-alpha-white-over-black test proves the linear-
light blend is brighter than a naive gamma-space average (187 vs. 128 on the same inputs --
`watermark.rs::half_alpha_overlay_is_brighter_in_linear_light_than_naive_gamma_average`), the same
class of correctness gap the resize decision rule addresses. **Sequential compositing is the
right default**, not the `rayon`-parallel path: measured 1.18ms p50 sequential vs. 6.10ms p50
parallel for a typical 200x60 logo -- rayon's per-row task overhead exceeds the blend work at this
size. Parallelizing would only pay off for a much larger overlay (not measured this pass). Text
watermarking (a rasterized string) is deferred -- needs a vendored font with a confirmed license,
not picked this pass.

### Proposed `Exporter` execution shape (for #57)

This pass didn't add a method to `crates/nicti-preen`'s `Exporter` trait -- that's #57's
implementation work -- but the pipeline this spike exercises end to end (see
`bin/prey.rs::Pipeline`) sketches the shape #57 should implement: take a rendered
RGBA/RGB working-space frame (Tapetum's eventual output, ADR-0044) plus an export spec (target
long edge, format, quality, ICC profile, metadata, optional watermark), resize in linear light,
encode, write metadata, composite any watermark, and write the result atomically (matching
`spikes/scent`'s sidecar-write pattern). Output-gamut selection (P3/AdobeRGB, soft-proofing)
stays #42's scope -- this spike only settles *how* an ICC profile gets embedded once one exists,
via a fixed sRGB profile as its own placeholder.

## Measured results

See `docs/research/prey-export-stack.md` for the full numbers, environment notes, and three real
bugs found and fixed while building this (a missing full-res `wgpu` buffer-size limit request, a
missing vertical-pass bind-group-layout index, and the 7 `exiftool -validate` warnings above).
Headline figures (release build, this machine):

- Resize (screen-res, 3840x2160->1024x576): naive gamma-space 56ms p50, linear-light CPU 201ms
  p50, GPU (llvmpipe, correctness-only) 284ms p50.
- JPEG encode (2048x1365 @ nominal q90): `jpeg-encoder` 22ms p50/328KB, `image` crate 29ms
  p50/323KB, mozjpeg (native) 70ms p50/122KB.
- Metadata write: all three operations (EXIF/XMP/ICC) sub-millisecond.
- Watermark composite (200x60 logo): sequential 1.18ms p50, rayon-parallel 6.10ms p50 (slower).
- Full pipeline end-to-end (3840x2160->1024x576 @ q90): 229ms p50, real output passes
  `exiftool -validate -warning -a` clean.

## Adversarial review

Run before opening the PR (fresh agent, hostile prompt, full staged diff). Four CONFIRMED
findings, all fixed before merge -- see `docs/research/prey-export-stack.md`'s "Real findings"
section for the full detail, and the PR's own comment for the complete CONFIRMED/SPECULATIVE
split:

1. `compute_taps`'s tap-count cap silently errored on this repo's own ordinary export presets
   (raised 32 -> 96, regression-tested at the real sizes).
2. `read_xmp_png` panicked on a truncated iTXt chunk (bounds-checked, regression-tested).
3. A misleading code comment about `img-parts`' own segment-insert convention (corrected).
4. `docs/licensing.md` undercounted five transitive crates (added, all confirmed permissive).

No SPECULATIVE findings were raised for the mozjpeg color-space handling, resource cleanup,
watermark math, or WGSL tap-index math -- the reviewer traced each and found no issue.

## Options considered

- **mozjpeg as the v1 default JPEG encoder** -- rejected for v1: a real quality-matched comparison
  (#223, see Decision above) confirms a genuine but modest size advantage (~1.3-1.4x smaller) that
  doesn't clear this ADR's own <=1.5x-encode-time bar (mozjpeg measured ~4-4.5x slower at matched
  quality); kept available behind the `native` feature.
- **`turbojpeg`** (libjpeg-turbo bindings) as a fourth JPEG encoder candidate -- not reached this
  pass.
- **A vendored/downloaded ICC profile file** instead of a `moxcms`-generated one -- rejected to
  avoid a license/provenance question over a binary file this repo doesn't need to carry.
- **Depending on `spikes/scent` directly** for its XMP APP1 splice, rather than reimplementing the
  same approach on `img-parts`' primitives -- rejected, spikes don't depend on each other in this
  repo (established convention, e.g. `loaf`'s `refine.rs` copies `siamese`'s guided-filter code
  rather than depending on it).
- **Text watermarking via `ab_glyph`** -- considered, dropped this pass since it needs a vendored
  font with a confirmed license, not picked yet; SVG/PNG logo watermarking covers the decision
  rule's own scope without it.

## What wasn't reachable this pass

See `docs/research/prey-export-stack.md`'s own section -- summarized: real full-res GPU resize
timing on the reference RTX 5080, `turbojpeg`, TIFF metadata write, text watermarking, and GPS
EXIF tags. (The quality-matched JPEG-encoder comparison this section used to list is now done --
see #223 and this ADR's own JPEG encoder section above.)

## Consequences

- **Unblocks #57** (Build: export pipeline) to implement `Exporter`'s real method using this
  pass's component choices and the sketched execution shape above.
- **One follow-up issue filed alongside this ADR** (see the PR): a reference-machine run for the
  real full-res GPU-resize timing and the LRC export-throughput comparison this ADR's own Context
  section defers. (The quality-matched JPEG-encoder follow-up this bullet used to list is done --
  #223 -- and settles jpeg-encoder as the v1 pick *for the synthetic test image measured*;
  mozjpeg's size advantage on that image doesn't clear this ADR's own <=1.5x-encode-time bar. Two
  things stay open, not just the usual hardware note: a reference-machine re-measurement of the
  *speed* ratio on the actual Windows target hardware, and a real-NEF re-measurement of the whole
  comparison once #41's render pipeline exists, since this pass's own experience shows the
  matched-quality result is sensitive to which test image is used.)
- **Requires #42** (color management/soft-proofing) before export can offer a real P3/AdobeRGB
  output gamut -- this pass's ICC embedding path is format-agnostic (any profile bytes work), but
  only ever generates a fixed sRGB profile as its own placeholder.
- **TIFF export's own ICC/EXIF/XMP write stays open** until a dedicated TIFF writer exists (same
  gap ADR-0059 left for DNG) -- #57 should treat TIFF as encode-only (no metadata) until that
  lands, or file a dedicated follow-up if TIFF export with metadata is needed sooner.
