# ADR-0438: LightCraft DNG opcodes, LJPEG, highlight reconstruction and binned preview

**Status:** Proposed (go: highlight reconstruction; conditional go: GainMap; no-go: OpcodeList1, LJPEG codec, binned preview; one test pattern for #137)
**Ticket:** #438
**Part of:** #424
**Related:** #137, #410, #428, ADR-0037, ADR-0428, ADR-0029

## Context

The LightCraft audit (#424) listed four pieces of `crates/raw/src/` in `storytold/lightcraft` (MIT OR Apache-2.0): `opcodes.rs`
(713 lines, parse and apply DNG opcode lists), `ljpeg.rs` (790, lossless JPEG decode and encode) with `dngwrite.rs`
(353), `highlight.rs` (397, clip-to-neutral then chromaticity diffusion) and `binned.rs` (194, superpixel
preview), plus the compressed-vs-uncompressed NEF oracle in `tests/corpus.rs`. Read at `265248c`; nothing is copied
here, so there is no `docs/licensing.md` entry (ADR-0428 already recorded the one adaptation, below).

## Findings

### 1. Opcodes: the lens ones already shipped; list 1 and 2 are the open question

ADR-0428 (#428) adapted `WarpRectilinear` (id 1) and `FixVignetteRadial` (id 3) from `opcodes.rs` into
`nicti-iris/src/dng.rs`, with a hardened parser, and wired `OpcodeList3` through the shim. What remains in
LightCraft's applier: `FixBadPixelsConstant/List`, `MapTable`, `MapPolynomial`, `GainMap`, `Delta/ScalePerRow/Column`,
`WarpFisheye` and `TrimBounds` (parsed, not applied).

LibRaw, as Nicti builds it, applies none of them. It reads `OpcodeList1/2/3` into `dng_levels.rawopcodes` (4 MB cap
each, `metadata/tiff.cpp`) and applies them only through its Adobe DNG SDK glue, which `build.rs` does not enable (it
vendors no external libraries). So a DNG that depends on `GainMap` (lens shading in many phone, Fuji and
Adobe-converted DNGs) or on bad-pixel lists renders without them.

- **OpcodeList1 (raw sample values, before demosaic): no-go.** Nicti lets LibRaw demosaic, so applying list 1 means
  owning the mosaic between unpack and demosaic, a seam the shim does not expose. The opcodes in it (bad pixels,
  per-row/column deltas) matter for specific sensors, not the Nikon NEF target (v1, ADR-0015).
- **GainMap (opcode 9, list 2): conditional go.** It is a smooth per-CFA-plane gain, so applying it after demosaic on
  camera RGB, next to the vignette gain in the lens pass (ADR-0428 section 1), is a close approximation. Do it
  **only when a real DNG that carries one is in hand** as a fixture and the result is compared against Adobe's
  rendering; LightCraft's own note that list 2 runs on normalised values while list 1 runs on 16-bit values must be
  re-checked against the DNG 1.7 spec, not trusted. No such file is in the corpus today.
- The other value-mapping opcodes are no-go until a file needs them.

### 2. LJPEG codec and DNG writer: no-go until DNG export is a product goal

Decoding of *lossless* JPEG is already covered by LibRaw (DNG, CR2, ARW). Caveat found while checking: `build.rs` defines no `USE_JPEG`, so `internal/defines.h` sets `NO_JPEG` and `lossy_dng_load_raw()` is an empty stub; lossy and JPEG-compressed DNGs (Smart Previews, some phone DNGs) do not decode at all, and LibRaw's own opcode-8 handling inside that function is not compiled either. That is a separate decoder question, not an LJPEG-codec one. LightCraft's value is the
**encoder** (bit-exact round-trip tests, DNG export). A title search of the tracker for "DNG" finds no issue asking for DNG export beyond #438 itself (#337 is EXIF into TIFF, a
different writer; the search was not exhaustive), and the agent-written 790-line codec would need its own fuzzing before it ever touched user files.
If DNG export is wanted, file it as a product ticket first; ADR-0428's test fixture builder already writes a tiny
uncompressed DNG without it.

### 3. Highlight reconstruction: go, it is a real gap

`shim.cpp` sets `p.highlight = 0` with the comment "highlight reconstruction is a develop-stage concern", but no
such stage exists: a repo search finds no highlight recovery in the decode or render path. The "clip" matches are
gamut clipping, lateral CA, the auto-tone clipped-fraction histogram (`perk.rs`) and a vignette style; none rebuilds
clipped channels, and LRC's Highlights/Whites map to plain tone sliders (`nicti-stray/src/develop/basic.rs`). So on a blown-highlight frame the channels clip at different levels after white
balance and render magenta or cyan at the edge of the highlight, which Lightroom hides by reconstructing.

LightCraft's `highlight.rs` input contract is demosaiced **camera RGB, before white
balance, white level 1.0**. Nicti's linear decode is the same kind of image (`output_color = 0`, `user_mul = 1`, WB applied
downstream by calico), though whether a clipped sample lands at exactly 1.0 after `(px - black - cblack)/(maximum - black)`
depends on LibRaw's scaling and was not traced. Two pieces, both worth taking as **ideas to re-derive**, not code to copy:

1. `clip_neutral`: limit each white-balanced channel at the smallest white-balanced clip level, then divide the
   multipliers back out. Cheap, per pixel, no neighbourhood. This alone removes the colour cast on clipped areas.
2. `reconstruct`: where only some channels clip, rebuild them from the local chromaticity of unclipped neighbours via a
   coarse-to-fine normalised-convolution fill. It is a reference for the existing `rods`-style research, not a
   replacement for a tuned method; its constants are unvalidated on real photos (the repo is days old and its own
   roadmap says so).

Open points a build ticket must settle: the clip level per channel from `maximum`, `black`/`cblack` (`LinearFrame`), and
a bit-depth-correct "white level 1.0"; where the node sits in the render graph (a baked node before WB, keyed by the
decode, so it must join the cache keys of ADR-0428 section 6); interaction with LRC import (LRC's Highlights/Whites are
tone sliders, not reconstruction); and validation on real Z8 frames with blown skies, not synthetic ramps.

### 4. Binned preview: no-go until measured

`binned.rs` skips demosaic for a reduced-size image. Nicti's tiers already use the embedded JPEG for grid and first-pass
cull (ADR-0029, T0/T1), and LibRaw's own `half_size` exists (the shim sets it to 0). A binned path only helps where a
full decode is needed for a small result. For HE/HE* NEFs the wavelet unpack probably dominates the demosaic, but the
only figure on record (1-2.4 s per file on the PR-fork reference, ADR-0037) is end-to-end `retina peek` time, not a
split, so this is a hypothesis. **Measure the demosaic share of T2 latency on ref-10k first**; only
if it exceeds roughly a third is a binned T2 worth a ticket. Note it cannot feed the AI mask or heal paths, which want
full resolution.

### 5. Test pattern for #137: take it

LightCraft's `corpus_nef_compressed_matches_uncompressed` checks a lossless NEF against an uncompressed NEF of the same
scene by block correlation (its own comment concedes the pair is two separate exposures). The pattern carries to #137:
when LibRaw's official HE/HE* snapshot replaces the PR fork, compare its output with the lossless decode of the *same
frame*. HE/HE* is lossy, so the oracle is a **PSNR/block-correlation bound**, not bit equality, and it is only as
good as the pair. **Where same-frame pairs come from is unverified:** I could not confirm that the Z8 can record one
frame in two RAW qualities (its second-slot modes may only duplicate the same file), so do not assume it. Safer oracles
for #137, in order: the same HE/HE* frame decoded by the PR fork and by LibRaw's official snapshot (bit-exact or tight
bound), then a same-scene pair if the operator has one. Add as a note on #137.

## Do not take (confirmed)

AHD/X-Trans demosaic (CPU-only, narrower than LibRaw and `rods`), vendor decoders (empirical Huffman tables, no
Z8/Z9), camera-preview colour fitting (Nicti has DCP).

## Consequences

- **Build ticket (go):** highlight reconstruction, slice 1 `clip_neutral` as a baked pre-WB node, slice 2 `reconstruct`
  after validation on real frames. **M**
- **Conditional ticket:** GainMap after-demosaic, gated on a real fixture. **S**
- **Comment on #137:** the HE-vs-lossless pair oracle. **XS**
- **No change** to the shim, the build or `docs/licensing.md` from this ADR.
