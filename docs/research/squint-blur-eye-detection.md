# #34: blur/misfocus/eye detection (`spikes/squint`)

Full write-up backing `docs/adr/0034-blur-misfocus-eye-detection.md`. See that ADR for the
decision rule and Consequences; this doc is the method and reproduction detail.

## Method

Real culled folders don't contain real rejects (blurry/misfocused/eyes-closed frames a
photographer already threw away), the same gap ADR-0033 hit for burst/duplicate grouping. This
pass substitutes synthetic degradation of a synthetic keeper image set (8 generated images, not
real photographs -- see the real run below), which also enables a measurement that isn't itself
fabricated: the **keeper false-flag rate** -- how often a candidate wrongly flags an undegraded
image from that set. The false-flag *methodology* is real; its numbers this pass are a
synthetic-set result, not yet a real-photo one -- that's the real cost this project cares about
most (a hidden keeper behind a false reject), measured on a stand-in until #238's real con-card
pass replaces it with real photos.

## Candidates measured (synthetic, this pass)

Every candidate scores a grayscale frame's *sharpest tile* (default 64px), not a whole-frame
average, so a shallow-depth-of-field shot isn't penalized for its own intentional background blur:

- **`laplacian_variance`** -- variance of a discrete 3x3 Laplacian response (Pech-Canul et al.'s
  standard "variance of Laplacian" blur metric).
- **`tenengrad`** -- mean squared Sobel gradient magnitude (Krotkov 1988).
- **`fft_high_freq_ratio`** -- a real 2D FFT (`rustfft`, row-then-column decomposition) split into
  a low-frequency disc and everything outside it; the high-frequency energy fraction.
- **`structure_tensor_anisotropy`** -- motion-vs-defocus discriminator, not itself a pass/fail
  candidate: eigenvalue anisotropy of the averaged structure tensor over the sharpest tile.
  Directional energy loss indicates motion blur; isotropic loss indicates defocus.
- **`af_region_misfocus_ratio`** -- composes with any global candidate: sharpness inside the AF
  area (from `af::AfArea::rescale_to`) divided by the frame's own sharpest-tile score.

## Synthetic degradation (`synth.rs`)

- **Defocus**: a disk (pillbox) kernel at radii 1/3/6/10px -- the textbook circular-aperture PSF.
- **Motion blur**: a linear-PSF kernel at lengths 3/8/16px and angles 0°/45°/90°.
- **Misfocus**: defocus confined to a centered region sized relative to the image's own dimensions
  (`convolve_region`, ~47%/120-of-256 of each axis), background left untouched -- simulates a real
  back-/front-focus shot where the AF area is soft but the rest of the frame isn't. Included in
  `default_sweep()` (radius 6), so `eval::run` actually measures it, not just `Defocus`/`Motion`.

## Measurement methodology (`eval.rs`)

Stated before any numbers exist, same discipline ADR-0033 followed:

1. Calibrate a per-candidate threshold: the median score across every keeper degraded at one
   concrete reference severity (`defocus_r6`, chosen to look unambiguously out-of-focus in a manual
   spot-check).
2. **Detection rate** at every other severity: fraction of degraded images below that threshold.
3. **Keeper false-flag rate**: fraction of *undegraded* keepers below that same threshold.

## Real run, 8 synthetic "keeper" images

```
cargo run -p squint --release -- synthetic --keepers-dir <dir-of-images>
```

Images: 256x256, smooth low-frequency background gradient with a 120x120 textured center region
(simulating in-focus subject detail against a defocused/plain background) -- not real photographs;
real numbers are the follow-up con-card pass's job (#238).

| Candidate | Keeper false-flag | Detect @ r1 | @ r3 | @ r6 (ref) | @ r10 | @ localized misfocus r6 | @ any motion blur | ms/frame p50 |
|---|---|---|---|---|---|---|---|---|
| `laplacian_variance` | 0.0% | 0% | 0% | 50% | 100% | 0% | 0% | 0.15 |
| `tenengrad` | 0.0% | 0% | 0% | 50% | 100% | 0% | 0% | 0.18 |
| `fft_high_freq_ratio` | 0.0% | 0% | 0% | 50% | 100% | 0% | 0% | 0.17 |

**Real finding**: every candidate's mean score *ratio* (degraded/original) collapses under motion
blur too (down to 0.3% of baseline at the most severe length/angle), but none of them cross the
defocus-calibrated absolute threshold for any motion-blur variant tested. A single reference-
severity threshold doesn't automatically generalize across degradation types on this synthetic set
-- worth calibrating per-degradation-class (or training a multi-class model) once real labelled
data exists, rather than assuming one number covers both blur types.

**Second real finding**: localized misfocus (blurring only the centered ~22%-of-area subject
region) is detected 0% of the time by every global candidate, despite a score-ratio drop as severe
as `defocus_r6`/`r10`'s own. This is structural, not a calibration gap: `max_over_tiles` reports
the frame's sharpest tile, and most of the frame stays sharp when only the subject region is
blurred -- exactly the case `af_region_misfocus_ratio` exists to catch (AF-region sharpness
relative to the frame's own max), not a redundant candidate alongside the global ones.

## AF-area reader (`af.rs`)

Nikon `AFInfo2` (tag `0x00B7`) parsed from published third-party documentation of the format (no
Nikon spec, no code copied from any specific tool) -- version `"0100"`/`"0101"` fixed layout only.
Unit-tested against a synthetic fixture built to the real Z8's declared dimensions (8256x5504,
per `docs/research/sniff-embedded-jpeg.md`), **not yet cross-checked against a real NEF** -- no
such file exists in this sandbox. `#238` tracks that cross-check (exiftool-independent-parse,
same method `spikes/litter`'s `nef.rs` used for its own MakerNote fields) alongside the real
con-card labelling pass.

## Eye detection: research only, no bundled candidate

Every real candidate needs a downloaded model this pass didn't get far enough to run end-to-end
(unlike `spikes/litter`'s real DINOv2 run). Shipping a stub that looks like a working detector
without ever running one would be exactly the kind of fabricated-looking-real result this
project's ADRs avoid -- see `eyes.rs`'s own module doc comment for the full license research
(MediaPipe Face Landmarker, YuNet, OWLv2/Grounding DINO, and a DINOv2-patch-probe reuse of #33's
own embedding) and why none of them were run this pass.

## Reproducing

```
cargo test -p squint
cargo run -p squint --release -- synthetic --keepers-dir <dir-of-images>
cargo run -p squint --release -- draft --nef-dir <card-dir> --work <scratch-dir>
cargo run -p squint --release -- labels --work <scratch-dir> --labels labels.json
```

`--nef-dir`/`--labels` paths above are placeholders, not real ones -- a real unculled con card
doesn't exist in this sandbox (see Method); #238 tracks the pass once one does.
