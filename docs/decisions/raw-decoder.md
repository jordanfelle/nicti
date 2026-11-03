## RAW decoder

Covers #37's RAW decoder research (ADR-0037): the LibRaw+PR#826 decision, the rawler
cross-check methodology, RapidRAW's HE gap, measured performance, and the vanished `ref-10k`
reference set.

- **RAW decoder (#37)**: `docs/adr/0037-raw-decoder.md` — **Proposed**, pending a decode-latency
  product call (see the ADR's own Decision section; the LGPL question is resolved — LGPL-2.1 §§5-6
  permit combining `rawler`/LibRaw into Nicti's AGPL-3.0-or-later work directly, no "or-later"
  grant needed, see the `licensing` topic). **LibRaw,
  patched with the still-open [LibRaw/LibRaw#826](https://github.com/LibRaw/LibRaw/pull/826)
  (Nikon HE/HE* decoder)**, vendored as a git submodule pinned to `yogthos/LibRaw@nikon-he-decoder`
  — the only candidate that decodes the real library's HE/HE* files (no released LibRaw/rawler/
  rawspeed version handles HE/HE*, which is 88% of the real Z8 files). Measured 100% decode success
  (261/261) across a real 261-file subset pulled from the live source library (HE/HE*/Lossless,
  two camera bodies), zero crashes either decoder side. This table measures decode success only,
  not pixel correctness — for HE/HE*, this research didn't independently re-verify pixel output
  against an oracle, and a successful decode is not itself proof the output is uncorrupted (the
  PR thread's own "336.7M samples bit-exact against Adobe DNG Converter" claim isn't reproduced
  here; treat HE/HE* as production-ready only after that independent oracle check is actually
  done). **rawler stays only as the Lossless-path correctness cross-check** (it structurally can't
  read HE/HE* — confirmed via a clean, typed rejection, never a crash), via a **±1 LSB tolerance
  diff, not hash equality** — the two decoders' Lossless output never hash-matches, a real,
  characterized, one-directional rounding difference in curve-inversion, not a bug. **RapidRAW
  (#69) was checked and doesn't solve HE either** — its own rawler fork still rejects HE/HE*; on
  Windows it silently falls back to the NEF's embedded JPEG instead of a real RAW decode.
  Single-file decode cost is real and high (~1-2.4s, `retina.exe peek`'s full end-to-end process
  time -- launch + read + decode + output, isolated, via WSL interop, not a pure decoder-only
  timer) — 5-12x over
  the 200ms cold-image-switch target and 10-24x over the stricter 100ms 1:1-zoom target, expected
  for PR #826's unoptimized reference code, but a real input to #44's render-graph cache design.
  **The frozen `ref-10k` reference set (both the 393GB NVMe and HDD copies `docs/benchmarks.md`
  describes) vanished from disk mid-research** — unmaintainable per-machine at that size; the
  dataset needs multi-person/multi-machine access going forward, tracked as a separate follow-up
  (#136), not solved by #37. `retina scan` (no manifest required) is the tool for running against
  the live library or any ad hoc subset now.

## #438: LightCraft DNG opcodes, LJPEG, highlight reconstruction, binned preview (ADR-0438)

Research outcome from the LightCraft audit (#424); nothing lifted. The lens opcodes (1, 3) already shipped in #428.
LibRaw as built here applies no DNG opcode (it only reads the lists; the DNG SDK glue is not enabled), so list 2
`GainMap` is a conditional go (apply after demosaic next to the vignette gain, only with a real fixture) and list 1 is a
no-go (it needs the mosaic before LibRaw's demosaic). The LJPEG encoder and DNG writer wait for a DNG-export goal that
no issue states. Highlight reconstruction is the real gap: `shim.cpp` sets `highlight = 0` deferring to a develop stage
that does not exist, and LightCraft's input contract (camera RGB before WB) is what Nicti's linear decode produces;
take `clip_neutral` first, `reconstruct` only after validation on real Z8 frames. Binned preview: measure the demosaic
share of T2 latency first (the 1-2.4 s HE/HE* figure is end-to-end, not a decode/demosaic split). For #137, first compare
the PR fork's and LibRaw's official snapshot output on the same frames; a lossless-vs-HE pair would need a PSNR bound
(lossy), and that a Z8 can record such same-frame pairs is unverified. Also found: no `USE_JPEG`, so lossy/JPEG-compressed
DNGs do not decode.

## JPEG assets decode through `nicti_cornea::jpeg` (#576)

`LibRawDecoder` is RAW-only, so loupe, develop and export on a JPEG asset (landed by #489) fell back to T0 or failed.
`JpegDecoder` (`crates/nicti-cornea/src/jpeg.rs`) implements the same `RawDecoder` trait and returns a `LinearFrame`,
so no caller needs a JPEG branch. `SourceDecoder` (libraw feature) dispatches on extension (`.jpg`/`.jpeg`/`.jpe`/`.jfif` to
`JpegDecoder`, everything else to LibRaw) and replaces `LibRawDecoder` in `app.rs`.

- **Colour management:** the embedded ICC profile is converted to sRGB with calico's `SourceTransforms` (panic-guarded
  moxcms; none or an unusable profile means sRGB), then the sRGB curve is undone into 16-bit linear (256-entry LUT).
- **Frame contract:** `black` 0, `maximum` 65535, `cam_mul`/`pre_mul` neutral (a JPEG is already white balanced),
  `cam_xyz` = Bradford-adapted XYZ(D50) -> linear sRGB (the matrix is XYZ -> "camera", so it is the sRGB matrix itself),
  `model` "JPEG". Pixels are not rotated; `orientation` carries the EXIF value 2-8 (1 and "no tag" both give 0).
- **Limits:** 8-bit source, so no highlight recovery and visible banding when shadows are pushed hard; images over 150 MP
  or files over 512 MB return `DecodeError::Jpeg` (the `image` JPEG decoder has no allocation limit of its own); a gray ICC
  is treated as sRGB. **CMYK/YCCK (#652):** Adobe-marked (APP14) 4-component JPEGs are refused with a clear
  `DecodeError::Jpeg`, because `image` converts them as plain CMYK although Adobe stores inverted ink values (measured:
  stored 255 = "no ink" came out black), so every Photoshop CMYK JPEG would show inverted; proper CMYK (raw samples, the
  CMYK ICC profile) is a follow-up. A CMYK file with no Adobe marker still goes through `image`'s plain conversion.
- **Wide gamut (#652):** a JPEG tagged with an RGB profile that is not sRGB-equivalent (Adobe RGB, Display P3, ...) skips the
  sRGB step: `nicti_calico::source_transform::rgba8_to_linear_prophoto16` widens the 8-bit samples to 16 bits and converts
  source ICC -> **linear ProPhoto (D50)** in one moxcms 16-bit transform (no clip, no second 8-bit rounding), and the frame
  declares `cam_xyz` = XYZ(D50) -> ProPhoto (`space::xyz_d50_to_working_f32`), so the camera-to-working matrix is the
  identity and a neutral stays neutral. Untagged, sRGB-equivalent, gray and unusable profiles keep the sRGB fast path
  above. Tests: P3 green stays outside sRGB (negative sRGB red, >1 green), Adobe RGB in-gamut colours land where sRGB
  puts them, D50 white stays neutral through the declared matrix. **Not validated against a real Adobe RGB / P3 file**
  (none in this sandbox); the synthetic JPEGs carry profiles generated by moxcms itself.
- **Shared JPEG facts (#652):** `nicti_cornea::JPEG_EXTENSIONS`/`is_jpeg` (`jpg`, `jpeg`, `jpe`, `jfif`) is the one list
  decode, Scruff ingest and the T2 path use (they had three copies; `.jpe`/`.jfif` were handled by none, so those files
  are now ingested). The catalog's orientation column (kamadak-exif, 1-8) and the decoder's frame orientation (`image`,
  0 or 2-8) read the same tag; a `nicti-lair` test pins that they agree for tags 1-9 (only 1 <-> 0 differs by design).
- **T2:** already handled before this change (`t2.rs` `is_plain_jpeg` derives T2 from the file itself).

## #552: clip-neutral highlights (ADR-0438 slice 1)

Decode now neutralizes clipped pixels right after normalising: `stages::ClipNeutral::for_frame(frame)` gives a per-channel clip level and a ceiling, and a pixel with *any* channel at or above its clip level (within a 1e-3 margin that catches interpolated edge pixels; untuned) has every channel limited to the ceiling. `normalize.wgsl`, its CPU twin `normalize_pixels` and `nicti-groom`'s `FramePixels` (built once per frame with `FramePixels::new`) all use the same value, so the heal/AI path stays in the space the baked frame is in. The maths: a sample at `maximum` normalises to `clip_c = (range - cblack_c) / range` (exactly 1.0 when `cblack_c` is 0, the "clipped samples land at 1.0" check); the as-shot gains (`color::wb_gains`, green = 1) make the white-balanced clip levels `clip_c * gain_c` differ per channel, so the white-balanced white is the smallest of them and each ceiling is that white divided back by the channel's own gain. A clipped pixel is then exactly neutral after as-shot WB. Metadata that cannot support the maths (non-positive range, `cblack` >= range, non-finite or non-positive gains) means nothing is ever treated as clipped.

Why conditional (a hostile review caught the first version): clamping *every* pixel to the ceiling (dcraw's `highlight=0` clip, what ADR-0438 describes) hard-clips legitimately unclipped saturated reds and blues at white-balanced 1.0 and, with a WB gain below 1 (R 0.5), crushes green image-wide, in a baked stage upstream of exposure, so negative exposure and the later reconstruct slice would lose headroom the sensor recorded. Only pixels that actually clipped are limited now; unclipped pixels and their headroom are untouched (tests pin a saturated red, a blue sky and the gain < 1 case). The cost, confirmed by the second review: the 1e-3 margin is float slack for a sample that really sat at `maximum`; it does not catch demosaic-interpolated edge pixels (a value averaged from clipped and unclipped neighbours lands around 0.975), so the one-pixel ring around a blown area keeps its cast, and there is a real step after WB between a limited pixel and its neighbour (an orange rim around a white disc for a warm highlight; with a gain below 1 a blown core can be darker than its rim). Slice 1 neutralizes fully clipped pixels only; the ring and a feathered transition are slice 2's job (#672).

It is part of the decode node rather than a new graph node: the decode stage's `impl_version` is now 1 (`DecodeStage`), so every key chained from it (baked frames, AI bakes, disk-cached alphas) changes once, as it must when pixels change. The persistent rendered-preview and grid-thumbnail cache is keyed separately (`rendered:v{N}:{identity}:{document}`), so `EYESHINE_VERSION` is bumped to 4 for the same reason. Not verified: no real Z8 frame has been through it (no NEF fixture here). `maximum` is LibRaw's white level: a body that saturates below it never reaches the clip level (no-op, not a wrong correction), one whose `maximum` is above true saturation keeps a cast. The ceiling follows the as-shot gains, so after a manual WB edit a clipped area carries the tint of the difference. Slice 2 (chromaticity-diffusion reconstruct) stays gated on real-frame validation: #672.
