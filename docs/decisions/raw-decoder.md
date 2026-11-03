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
so no caller needs a JPEG branch. `SourceDecoder` (libraw feature) dispatches on extension (`.jpg`/`.jpeg` to
`JpegDecoder`, everything else to LibRaw) and replaces `LibRawDecoder` in `app.rs`.

- **Colour management:** the embedded ICC profile is converted to sRGB with calico's `SourceTransforms` (panic-guarded
  moxcms; none or an unusable profile means sRGB), then the sRGB curve is undone into 16-bit linear (256-entry LUT).
- **Frame contract:** `black` 0, `maximum` 65535, `cam_mul`/`pre_mul` neutral (a JPEG is already white balanced),
  `cam_xyz` = Bradford-adapted XYZ(D50) -> linear sRGB (the matrix is XYZ -> "camera", so it is the sRGB matrix itself),
  `model` "JPEG". Pixels are not rotated; `orientation` carries the EXIF value 2-8 (1 and "no tag" both give 0).
- **Limits:** 8-bit source, so no highlight recovery and visible banding when shadows are pushed hard; images over 150 MP
  or files over 512 MB return `DecodeError::Jpeg` (the `image` JPEG decoder has no allocation limit of its own); CMYK/YCCK
  JPEGs go through the `image` crate's naive conversion (unverified, a CMYK ICC profile is ignored); a gray ICC is
  treated as sRGB.
- **Known limitation (follow-up):** the ICC step is 8-bit sRGB, so Adobe RGB / P3 colours outside sRGB clip and get a
  second 8-bit rounding before the curve is undone, although the working space is ProPhoto. Not validated against a wide-gamut
  (Adobe RGB / Display P3) real file in this sandbox; the unit tests cover linearisation, neutral stays neutral,
  routing and errors.
- **T2:** already handled before this change (`t2.rs` `is_plain_jpeg` derives T2 from the file itself).
