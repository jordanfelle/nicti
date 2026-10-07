# ADR-0410: Nikon's embedded lens-correction data as the NEF lens source

**Status:** Accepted (opt-in until verified, see Decision 4)
**Date:** 2026-10-07
**Ticket:** #410
**Part of:** #7
**Related:** #428 (ADR-0428), #358, #382, #39, #37, ADR-0018, ADR-0044

## Context

ADR-0428 made `nicti.lens` a real stage and gave `nicti_iris::LensCorrection` a real method
(`model(&LensSource) -> Option<LensModel>`), with `dng::DngEmbedded` as the only provider. A NEF still
gets nothing: #39 closed without choosing a source, and #410 asks for the decision (lensfun vs the
data inside the NEF; Nikon Z / NIKKOR Z coverage) plus the implementation.

What the research found:

- **Nikon Z bodies write their own correction data into the file**, in TIFF tag `0xC7D5` of a SubIFD
  (ExifTool: `NikonNEFInfo`, "SubIFD1 tag 0xc7d5 of NEF images from cameras such as the Z6 and Z7").
  It is **not encrypted** (only MakerNote LensData 0x0098 and ShotInfo 0x0091 are). The payload is a
  `Nikon\0` header, an inner TIFF header and an IFD whose entry 0x05 is the distortion block, 0x06 the
  vignette block (and 0x07 a lateral-CA block). It is whatever the body knows for the mounted lens, so
  it covers every Z lens including future ones, and it is the data Nikon's own tools apply.
- **Public decoders are rare.** ART 1.26.9 reads it (GPL, so used for semantics only); rawproc reads
  distortion/vignette; darktable, RawTherapee, LibRaw (0.22.2), rawspeed and rawler do not. The vendored
  LibRaw parses no distortion or vignette tags, so this is our own reader either way.
- **The coefficient semantics are reverse-engineered and the public sources disagree.** ART reads each
  block as a Horner polynomial `1 + c[0] r^n + ... + c[n-1] r` with `n = 4` (distortion) and `n = 8`
  (vignette), `r = 1` at the recorded image's corner, vignette gain `1 / sqrt(poly)`. The pixls.us
  threads read the distortion block as even powers only (DNG `WarpRectilinear`). Nobody has confirmed
  either on a Z8, Z9 or Zf.
- **lensfun** covers 35 NIKKOR Z lenses (gaps: Z 24/1.8 S, 35/1.2 S, 17-28, 400 TC, 800 PF; several
  lack vignette or CA data) and the Z bodies; its last release is 2023 (the database lives on master).
  The library is LGPL-3.0 (DLL-only per `docs/licensing.md`; `lensfun-rs` statically links, adding a
  relink obligation) and the database is CC BY-SA 3.0.

## Decision

1. **The NEF lens source is Nikon's embedded data**, read by our own clean-room decoder. No new
   dependency, no licence surface. The layout is taken from ExifTool's public tag tables and the
   pixls.us threads; no code was copied (ART's is GPL).
2. **Split like `dng`:** `nicti_cornea::embedded::Walker::find_nikon_lens_info` locates and reads the
   raw blob (it reuses the preview walker's IFD reader, so it builds without the `libraw` feature) and
   `LinearFrame.nikon_lens_info` carries the bytes. `nicti_iris::nikon` decodes them (`parse`) and maps
   them onto the shared `LensModel` (`NikonEmbedded`).
3. **Refit onto the existing model, don't extend it.** `Warp` is an even polynomial in `r^2`
   (`kr0..kr3`) and `Vignette` is `1 + k0 r^2 + ... + k4 r^10`. Nikon's polynomials include odd powers
   and the vignette is `1 / sqrt(poly)`, so each is least-squares fitted over `r` in 0..1 onto those
   forms (`fit_even`, 64 samples). An even-only distortion polynomial refits exactly; a general one to
   about 2e-3 (tested). This keeps the GPU kernel, the CPU twin and the cache key untouched.
4. **Opt-in until verified.** Because decision-relevant semantics are unconfirmed, the provider sits
   behind `LensParams::nikon_profile`, **default off** (serialised only when on). A default NEF
   therefore renders exactly as before, `LensStage::IMPL_VERSION` stays 0, and no baked key or on-disk
   AI alpha changes. The Develop panel shows "Use Nikon lens profile (unverified)" only for files that
   carry the data. The parity follow-up turns it on by default and bumps `IMPL_VERSION` at that point.
5. **Lateral CA is not decoded.** Block 0x07's layout is not documented anywhere citable; the existing
   auto-CA estimator (ADR-0428) still covers it. Revisit with the parity follow-up.
6. **lensfun is deferred, not rejected.** It is the right fallback for adapted (FTZ/F-mount) lenses and
   files without the tag, and it needs its own licensing decision (LGPL DLL vs `lensfun-rs`). Filed as
   a follow-up.
7. **Hardening.** The blob comes from an untrusted file: the locate step caps its size (64 KiB), the
   decoder bounds the entry and coefficient counts, rejects zero denominators and non-finite values,
   drops a block on its own, and the provider rejects `|c| > 100` and a non-positive vignette falloff.
   The camera's own flag is honoured (`Off`/`NoLens` produce no model).

## Consequences

- No NEF user sees a change unless they tick the new switch.
- **Unverified:** nothing here has run on a real Z-series file. This sandbox has none, so the
  real-file test (`nicti-tapetum/tests/real_nef_lens.rs`, `NICTI_TEST_REAL_NEF_DIR`) skips. The
  parity follow-up converts the same NEFs with Adobe DNG Converter (which turns Nikon's data into
  `WarpRectilinear`/`FixVignetteRadial`, which `dng.rs` already reads), compares the models, settles
  which reading of the polynomial is right, and flips the default.
- #358 (masks see the post-lens frame) is no longer blocked on a missing source, only on that parity
  result.
- #382 (LRC `LensProfile*` import) can map onto `nikon_profile`.
