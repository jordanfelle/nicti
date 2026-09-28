# #56: export stack (spikes/prey)

## Method

`crates/nicti-preen` was an empty shell before this pass (`pub trait Exporter: Module {}`, no
execution method) -- four components needed a real candidate comparison before #57 could build a
real export pipeline: resize, JPEG/TIFF/PNG encode, EXIF/XMP/ICC metadata write, and watermark
compositing. `spikes/prey` measures each candidate with `nicti-prowl::perf::Protocol` (1 warm-up
+ 5 measured runs, p50/p95/max) via its own CLI (`prey resize|encode|metadata|watermark|
pipeline`), and cross-checks correctness with `nicti-prowl::golden::ssim` and real decode/read-back
round trips (`image::load_from_memory`, `exiftool -validate`).

No real NEF/render pipeline exists yet (#37/#38/#41/#44 are landed or in progress, but nothing
feeds a full-res working-space frame into an exporter today), so every measurement runs against a
synthetic 8-bit sRGB frame -- a gradient with periodic full-black/full-white bands, chosen so a
resize/watermark candidate's gamma-vs-linear behavior actually shows up (a flat or low-contrast
frame wouldn't expose it; see `resize.rs`'s and `watermark.rs`'s own tests of this).

**Environment**: this sandbox has a real RTX 5080 (confirmed via `nvidia-smi`), but WSL exposes no
NVIDIA Vulkan ICD to `wgpu` -- `crouch bench-wgpu`'s own adapter-enumeration log confirms only
`llvmpipe` (software Vulkan) is visible here, the same constraint
`.claude/rules/gpu-gui-and-healing/REFERENCE.md` already documents. All CPU numbers below are real
release-build (`cargo build --release`) timings on this machine's CPU; all GPU numbers are
correctness-only, run against `llvmpipe`, not the real RTX 5080.

## Candidates measured

### Resize

- `resize_fast_linear` (`resize.rs`): `fast_image_resize` v6's Lanczos3 convolution over a
  linear-light `f32x3` buffer (hand-converted to/from sRGB, since `fast_image_resize` has no
  built-in gamma-aware mode).
- `resize_image_crate_srgb` (`resize.rs`): `image::imageops::resize` (Lanczos3), directly over
  gamma-encoded `u8` -- the naive baseline every consumer of the `image` crate gets for free.
- `GpuLanczosResizer` (`gpu_resize.rs`): a two-pass separable Lanczos3 `wgpu` compute kernel
  (`shaders/lanczos_resize.wgsl`) over the same linear-light conversion, weights precomputed on
  the CPU (`compute_taps`, edge-clamped, tap count capped at 96 -- covers downscale ratios up to
  roughly 15x on one axis; an earlier cap of 32, ~5.3x, silently errored on this repo's own
  ordinary export long-edge presets, e.g. 8280px source down to a 1024-1536px long edge -- caught
  by adversarial review, see "Real findings" below).

### JPEG encode

- `encode_jpeg_encoder` (`encode.rs`): the `jpeg-encoder` crate, already used by `spikes/sniff`'s
  T2 preview path. Has a native `add_icc_profile` (auto-splits across multiple APP2 segments).
- `encode_image_crate_jpeg` (`encode.rs`): `image::codecs::jpeg::JpegEncoder`, the naive baseline.
- `native::encode_mozjpeg` (`encode.rs`, `native` Cargo feature only): real mozjpeg via
  `mozjpeg-sys`'s vendored C build. Also has a native `write_icc_profile` (multi-segment-safe).
- `encode_jpeg_encoder_with_sampling` / `native::encode_mozjpeg_with_sampling` (`encode.rs`, #223):
  same two encoders, but with chroma subsampling pinned explicitly instead of left at each
  library's own default -- see "Quality-matched JPEG-encoder comparison (#223)" below for why.
- `find_matched_quality` (`encode.rs`, #223): given an encode closure and a target SSIM, scans
  quality 1-100 ascending and returns the first quality whose decoded output scores at least the
  target -- the quality-matching step this ADR's own decision rule requires before comparing
  encoder candidates' size or speed (nominal "quality 90" isn't comparable across encoders).

`turbojpeg` (libjpeg-turbo bindings) was not reached this pass.

### Metadata (EXIF / XMP / ICC)

- EXIF: `little_exif::metadata::Metadata`, chosen over `img-parts`' own EXIF support because
  `img-parts` only stores/replaces a raw EXIF byte blob -- it doesn't build the TIFF-structured
  IFD itself. `little_exif` builds the IFD from typed tags directly (`write_exif_jpeg`/
  `write_exif_png`, using its own end-to-end `write_to_vec`).
- ICC: `moxcms::ColorProfile::new_srgb().encode()` generates a fresh sRGB IEC61966-2.1 profile at
  runtime (no vendored `.icc` file, no license/provenance question), embedded via `img-parts`'
  `ImageICC` (JPEG: auto-splits across APP2 segments per spec; PNG: `iCCP` chunk, zlib-compressed
  by `img-parts` itself).
- XMP: a hand-rolled JPEG APP1 segment insert / PNG `iTXt` chunk insert
  (`embed_xmp_jpeg`/`embed_xmp_png`), generalizing `spikes/scent`'s JPEG APP1 approach (same
  signature constant, same single-segment 64KB cap -- copied, not depended on, since spikes don't
  depend on each other) onto `img-parts`' generic segment/chunk primitives instead of a
  hand-rolled byte walker, since `img-parts` is already a dependency here for ICC/EXIF.
  `wrap_xpacket` wraps a raw `<x:xmpmeta>` document in the standard xpacket processing-instruction
  pair (`exiftool -validate` flags a packet missing this as a minor warning -- found on this
  spike's own first real pipeline output, fixed, and now a regression test).

### Watermark

- `rasterize_svg` (`watermark.rs`): `resvg`/`usvg`/`tiny-skia`, real SVG rasterization (not a
  stub) into a straight-alpha RGBA buffer.
- `composite_sequential`/`composite_parallel` (`watermark.rs`): Porter-Duff "over" compositing in
  **linear-premultiplied** light (straight-alpha in/out, sRGB-encoded) -- the naive gamma-space
  average would darken semi-transparent edges the same way a naive gamma-space resize does.
- Text watermarking (a rasterized string, not a pre-made logo asset) was not reached this pass --
  it needs a vendored font with a confirmed license, not picked this pass.

## Raw numbers (this machine, release build)

**Resize, screen-res (3840x2160 -> 1024x576, 8.3M source pixels -- see the GPU note below for why
this size, not full-res, was used for the three-way comparison):**

| Candidate | p50 | p95 |
|---|---|---|
| `resize_image_crate_srgb` (naive, gamma-space) | 56.12ms | 56.26ms |
| `resize_fast_linear` (CPU, linear-light) | 201.33ms | 255.66ms |
| `GpuLanczosResizer` (llvmpipe software Vulkan, linear-light) | 283.51ms | 288.51ms |

**Resize, full-res (8280x5520 -> 2048x1365, CPU only -- GPU not run, see below):**

| Candidate | p50 | p95 |
|---|---|---|
| `resize_image_crate_srgb` | 389.58ms | 392.14ms |
| `resize_fast_linear` | 1100.40ms | 1139.00ms |

The naive gamma-space resize is consistently faster than the linear-light candidate (no
eotf/oetf conversion pass) -- expected, and the whole point of the linear-light decision rule is
that this speed comes at a real correctness cost on high-contrast content (see
`resize.rs::linear_resize_differs_from_naive_srgb_resize_on_high_contrast_edges`, SSIM < 0.95
between the two on a checkerboard).

**GPU resize was not run at full resolution**: `llvmpipe`'s own advertised
`max_storage_buffer_binding_size` (128MiB) caps a single linear-light `f32x4` source buffer at
~8.4M pixels -- a real 45MP frame needs 731MB, well over that. This is a software-adapter limit,
not something `wgpu`'s `required_limits: adapter.limits()` can raise past what the adapter itself
reports (a real validation error was hit and fixed by requesting the adapter's own limits instead
of `wgpu`'s conservative defaults -- see `gpu_resize.rs`'s doc comment -- but the *ceiling* itself
is the software adapter's, not a config bug). Real full-res GPU timing needs the
cross-compile-to-Windows-and-run-via-WSL-interop path ADR-0044/0054 used against the real RTX
5080 -- **not reached this pass**.

**JPEG encode (2048x1365 @ nominal quality 90):**

| Candidate | Output size | p50 | p95 |
|---|---|---|---|
| `jpeg-encoder` | 327,592 bytes | 22.19ms | 34.70ms |
| `image` crate | 323,351 bytes | 28.80ms | 29.59ms |
| mozjpeg (native) | 122,338 bytes | 70.35ms | 71.63ms |

mozjpeg's output is ~2.6x smaller than either pure-Rust encoder's at the same nominal "quality 90"
parameter, but nominal quality numbers aren't comparable across encoders -- mozjpeg's own default
quantization tables and (by default) optimized Huffman coding are more perceptually tuned per
quality unit than either pure-Rust encoder's. mozjpeg is also ~2.5-3x slower to encode than either
pure-Rust option. **This pass's own "is mozjpeg's 2.6x-smaller file actually the same perceptual
quality" question is now closed -- see the next section.**

**Quality-matched JPEG-encoder comparison (#223), chroma subsampling pinned to 4:2:0 on both
sides:**

Following up on the open question above turned up a second, more consequential confound than
"nominal quality numbers aren't calibrated the same way": `jpeg-encoder` 0.6.1's own default
(`Encoder::new`) silently switches chroma subsampling from 4:2:0 to 4:4:4 at quality 90 and above,
while mozjpeg's libjpeg default (`jpeg_set_defaults`) stays fixed at 4:2:0 regardless of quality.
The original quality-90 comparison above was therefore partly comparing 4:4:4 output (jpeg-encoder)
against 4:2:0 output (mozjpeg) -- a bigger deal than an uncalibrated quantizer, since chroma
subsampling roughly halves chroma-plane data outright. Confirmed with a regression test
(`jpeg_encoder_default_sampling_switches_at_quality_90`): the size jump crossing quality 90 shrinks
once subsampling is pinned instead of left to switch.

With subsampling pinned to 4:2:0 on both candidates (`encode_jpeg_encoder_with_sampling`/
`native::encode_mozjpeg_with_sampling`) and each encoder's quality matched to reach a fixed target
SSIM against the source (`find_matched_quality`, `prey quality-sweep`), on a smoother
"photo_like_frame" test source (large-scale tonal gradient plus moderate-frequency texture --
`synthetic_frame`'s per-pixel-varying blue channel turned out to be adversarially high-frequency
for chroma subsampling, capping SSIM around 0.91 even at quality 100 and making it useless for
finding *any* quality-matched threshold above that):

| Target SSIM | `jpeg-encoder` matched q | size | mozjpeg matched q | size | size ratio | encode time (jpeg-encoder / mozjpeg) | time ratio |
|---|---|---|---|---|---|---|---|
| 0.98 | 30 | 173,118 bytes | 44 | 133,967 bytes | 1.29x | 29.07ms / 116.36ms | 4.00x |
| 0.95 | 17 | 130,870 bytes | 26 | 90,584 bytes | 1.44x | 23.41ms / 104.69ms | 4.47x |

mozjpeg's smaller-file result is now **real and confirmed on this synthetic source**, not an
artifact of mismatched subsampling or an uncalibrated quality parameter -- ~1.3-1.4x smaller at
genuinely matched perceptual quality, consistent across two different SSIM targets on the same
image. But it is also consistently **~4-4.5x slower** at that same matched quality, which fails
this ADR's own decision rule (a candidate's encode time must stay <=1.5x the fastest to "earn its
place" over the pure-Rust default). **Conclusion for this pass: `jpeg-encoder` stays the v1
default.** mozjpeg's size advantage doesn't clear the speed bar this ADR set before measuring --
see ADR-0056's own JPEG encoder section for the same conclusion. Two caveats on how far this
generalizes, not just the usual hardware one:

- **Hardware**: these p50 numbers are this sandbox's Linux CPU, not the Windows reference machine.
  The *ratio* between the two encoders is what this measurement is really about, and isn't
  expected to flip on different hardware, but a reference-machine re-confirmation would still be
  worth doing before fully closing this out.
- **Image content**: measured against one synthetic test image (`photo_like_frame`), not a real
  NEF-derived render. This pass's own experience shows the result is content-sensitive --
  `synthetic_frame`'s per-pixel chroma noise capped SSIM around 0.91 regardless of quality and had
  to be swapped out for a smoother fixture before either encoder's quality-vs-SSIM curve was even
  usable. A real photo's noise/texture profile (sensor noise, foliage/fur detail, sky gradients)
  could plausibly shift the matched-quality size/speed gap in either direction. Treat "jpeg-encoder
  wins" as this pass's result on synthetic content, not yet a real-photo-confirmed conclusion --
  the real-NEF re-measurement this doc's "What wasn't reachable" section already tracks (blocked on
  #41 landing a full render pipeline) should re-run this same `quality-sweep` comparison once real
  renders exist, not just re-confirm timing on different hardware.

**Metadata write (2048x1365 JPEG, 420,176-byte base):**

| Operation | p50 | p95 |
|---|---|---|
| EXIF write (`little_exif`) | 0.43ms | 0.54ms |
| XMP embed (`embed_xmp_jpeg`) | 0.36ms | 0.38ms |
| ICC embed (`img-parts`) | 0.36ms | 0.38ms |

All three are sub-millisecond -- metadata write is not a meaningful cost next to resize/encode.

**Watermark composite (2048x1365 frame, a 200x60 logo):**

| Candidate | p50 | p95 |
|---|---|---|
| `composite_sequential` | 1.18ms | 4.28ms |
| `composite_parallel` (rayon) | 6.10ms | 24.26ms |

The parallel path is consistently *slower* at this overlay size -- rayon's per-row task overhead
exceeds the blend work for a 200x60 (12,000-pixel) overlay. Sequential is the right default for a
typical logo-sized watermark; parallelizing would only pay off for a much larger overlay (e.g. a
full-frame tiled watermark), not measured this pass.

**Pipeline end-to-end** (resize -> watermark -> encode -> EXIF -> XMP, 3840x2160 -> 1024x576 @
q90): 228.85ms p50 / 264.15ms p95 (measured before the CodeRabbit-caught pipeline-ordering fix
below; re-measurement wasn't repeated for that fix alone since it changes correctness, not the
cost of any individual step). The real output JPEG passes `exiftool -validate -warning -a` clean
(`Validate: OK`) after fixing two real findings caught by that same command on the first real
pipeline output (see below).

## Real findings from building this, not just measuring it

- **`exiftool -validate` on the first real pipeline output flagged 7 warnings**: missing
  mandatory `ExifIFD` tags (`ExifVersion`, `ComponentsConfiguration`, `ColorSpace`,
  `ExifImageWidth`, `ExifImageHeight`), a missing mandatory `IFD0` tag (`YCbCrPositioning`), and a
  missing XMP xpacket wrapper. All seven are fixed (`metadata.rs::build_exif` now always sets the
  six EXIF/TIFF tags; `metadata::wrap_xpacket` wraps a raw XMP document) and covered by
  regression tests, not just fixed ad hoc.
- **A real wgpu validation error, twice**: the default device limits cap a single buffer at
  256MiB (too small for a full-res linear-light source buffer), and separately
  `max_storage_buffer_binding_size` defaults to 128MiB even after requesting the adapter's own
  (larger) `max_buffer_size` -- these are two different limits. Fixed the first by requesting
  `adapter.limits()` on device creation; the second is a real software-adapter ceiling, not a
  config bug (see the full-res GPU note above).
- **The vertical-pass `wgpu` bind group layout came back empty** (`Number of bindings ... does
  not match ... (0)`) on the first run: the vertical compute entry point's bindings live at
  `@group(1)` in the shared WGSL module (a second bind group, distinct from the horizontal pass's
  `@group(0)`), so `get_bind_group_layout(0)` returned an empty auto-derived layout for that entry
  point. Fixed by requesting layout index 1 and calling `set_bind_group(1, ...)` for the vertical
  pass.
- **Adversarial review (before this PR opened) caught four real issues, all fixed**:
  1. `compute_taps`'s `MAX_TAPS=32` cap (~5.3x downscale ceiling) silently errored on this repo's
     own ordinary export long-edge presets from the real 45MP source (1024-2048px long edge is a
     4.0-8.1x ratio) -- neither existing test came close to that boundary (both used 4x/0.25x).
     Raised to `MAX_TAPS=96` (~15x ceiling), added a regression test at the real preset sizes plus
     a test confirming a genuinely extreme ratio still errors loudly rather than silently
     producing wrong (non-1.0-summing) weights.
  2. `read_xmp_png` panicked (out-of-bounds slice) on an iTXt chunk whose contents were exactly
     the XMP keyword bytes with no null terminator -- a plausible truncated/malformed PNG, not
     just adversarial input. Fixed with explicit bounds checks at every byte-offset step, plus a
     regression test constructing exactly that malformed chunk via `img-parts`.
  3. `embed_xmp_jpeg`'s comment claimed its insert position matched `img-parts`' own EXIF/ICC
     insert convention; reading `img-parts` 0.4.0's actual source showed it inserts at index 3,
     not 1. Not a behavior bug (JPEG readers tolerate any APPn order), but a wrong rationale in
     the code -- comment corrected.
  4. `docs/licensing.md`'s update paragraph undercounted -- five transitive crates
     (`brotli`/`crc`/`crc-catalog`/`dunce`/a second `quick-xml` version) landed in `Cargo.lock` via
     `resvg`/`usvg`/`moxcms` without being individually disclosed. All five confirmed permissively
     licensed and added to the paragraph.
- **CodeRabbit's review on PR #221 caught two more real issues, both fixed**:
  1. `bin/prey.rs`'s `Pipeline` command had two copies of the same chain that had drifted apart:
     the timed loop decoded the JPEG back out, composited a watermark onto it, and then discarded
     the result without re-encoding (`let _ = rgba;`) -- so the recorded timing didn't include a
     real re-encode step -- while the real-output-file write path never watermarked at all,
     despite this doc's own "resize -> encode -> EXIF -> XMP -> watermark" claim. Fixed by
     factoring both into one `build_pipeline_jpeg` function (resize -> watermark the resized RGB
     frame directly -> encode -> EXIF -> XMP), used by both the timed loop and the real write, so
     the two can't drift again.
  2. `encode.rs::encode_jpeg_encoder` cast `width`/`height` to `u16` with a bare `as` -- a
     dimension past 65535px would silently wrap around and pass the wrong size to the encoder
     while the pixel buffer stayed full-size, producing a corrupted encode rather than a clean
     error. Fixed with `u16::try_from`, regression-tested.

## What wasn't reachable this pass

- Real full-resolution GPU resize timing against the reference RTX 5080 (needs the
  cross-compile-to-Windows-via-WSL-interop path, not attempted this pass).
- A reference-machine (real Windows target hardware) re-confirmation of the quality-matched
  JPEG-encoder speed ratio (#223 closed the SSIM-matched comparison itself on this sandbox's
  Linux CPU).
- A real-NEF re-measurement of #223's quality-matched JPEG-encoder comparison: the SSIM/size/speed
  numbers above are against one synthetic test image (`photo_like_frame`), and this pass's own
  experience shows the result is content-sensitive (a different synthetic fixture,
  `synthetic_frame`, couldn't even reach a usable SSIM target at 4:2:0). Blocked on #41's full
  render pipeline existing so a real decoded/rendered photo is available to sweep against.
- `turbojpeg` (libjpeg-turbo bindings) as a fourth JPEG encoder candidate.
- ICC/EXIF/XMP write into a TIFF export target -- no crate in this workspace builds arbitrary
  TIFF tags (same class of gap ADR-0059 already flagged for DNG).
- Text watermarking (a rasterized string) -- needs a vendored font with a confirmed license, not
  picked this pass. SVG and PNG logo watermarks are real and tested.
- GPS EXIF tags (deferred -- needs a GPSInfo sub-IFD `little_exif` exposes via a separate tag
  group not exercised here).

## Reproducing

```bash
cargo test -p prey                                    # unit tests, synthetic images only
cargo test -p prey --features native                   # + mozjpeg-backed tests
cargo build -p prey --release
target/release/prey resize --src-width 3840 --src-height 2160 --dst-long-edge 1024
target/release/prey encode --width 2048 --height 1365 --quality 90
target/release/prey metadata --width 2048 --height 1365
target/release/prey watermark --width 2048 --height 1365
target/release/prey pipeline --src-width 3840 --src-height 2160 --dst-long-edge 1024 \
    --out-file /tmp/prey-pipeline.jpg
exiftool -validate -warning -a /tmp/prey-pipeline.jpg   # expect "Validate: OK"
target/release/prey quality-sweep --width 2048 --height 1365 --target-ssim 0.98  # #223
```
