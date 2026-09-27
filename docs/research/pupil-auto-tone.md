# #99 — Classic auto-tone: `spikes/pupil` research notes

See `docs/adr/0099-classic-auto-tone.md` for the decision itself; this is the module-by-module
detail the ADR summarizes.

## Module breakdown

- **`input.rs`** — `LinearMeta`/`load`, a small independent copy of `calico::linear_input`
  (reads `retina dump-linear`'s TIFF + JSON sidecar). Adds `white_balance`: as-shot white balance
  from `cam_mul`, normalized so green is 1.0.
- **`render.rs`** — the fixed default-render color treatment: white-balance → camera RGB →
  XYZ(D50) (via `cam_xyz`) → Bradford-adapted linear sRGB(D65) → sRGB OETF → Rec. 709 luminance.
  Copied from `spikes/rods::display` (established for #40's own "not ADR-0038's real DCP
  pipeline, correct enough to compare on equal footing" purpose), with white balance added since
  #40 compared already-white-balanced (`dump-classic`) inputs.
- **`histogram.rs`** — `Histogram::from_samples` (sorts once), `percentile` (nearest-rank),
  `mean`, `fraction_below`/`fraction_above` (clip-mass fractions). No image histogram utility
  existed anywhere in this repo before this ticket.
- **`sliders.rs`** — `Sliders`: the six PV2012 Basic-panel targets
  (Exposure2012/Contrast2012/Highlights2012/Shadows2012/Whites2012/Blacks2012), their documented
  ranges, and a `clamped()`.
- **`heuristic.rs`** (candidate A) — median → exposure (log2 ratio toward 18% mid-gray,
  `srgb_oetf(0.18) ≈ 0.4849`), interquartile spread (p75-p25) vs. a 0.35 reference spread →
  contrast, `fraction_above(0.98)`/`fraction_below(0.02)` clip mass → Highlights/Shadows, and the
  99.5th/0.5th percentiles' distance from 1.0/0.0 → Whites/Blacks.
- **`fit.rs`** (candidate B) — a 13-element feature vector (bias term, 9 percentiles, mean, both
  clip fractions) and one independent ridge regression per slider:
  `(X^T X + lambda*I) w = X^T y`, solved by Gaussian elimination with partial pivoting
  (hand-rolled — six outputs, a dozen features, not worth a linear-algebra crate dependency and
  its ADR-0018 license-review surface).
- **`truth.rs`** — reads LRC's own Auto slider values back out of a `.lrcat`: opens it read-only +
  immutable (same convention as `shed::open`), joins `Adobe_imageDevelopSettings` → `Adobe_images`
  → `AgLibraryFile` by base name + extension, and parses the develop-settings Lua literal via
  `agprefs` (a small independent copy of `shed::develop`'s parsing, not a dependency on `shed`).
  Missing keys default to `0.0` (LRC omits a key entirely at its default value).
- **`eval.rs`** — per-slider MAE, p95 absolute error (via `Histogram::percentile`, reused
  generically), and signed bias between predicted and ground truth.
- **`bin/pupil.rs`** — `auto <tiff> <json>` (candidate A on one image), `fit --truth <lrcat>
  --inputs <dir>` (fits B on a deterministic 50/50 split by sorted file stem, reports holdout
  error), `eval --truth <lrcat> --inputs <dir> --candidate a|b` (same split for `b`; `a` needs no
  training and reports over every matched sample).

## Why per-slider error, not a golden image

The issue's own wording asked for a golden-image comparison against real LRC auto-tone output.
Nothing in this repo can render PV2012 sliders to pixels yet — that's #46's job (global
adjustments), which hasn't been built. Two options were considered: fake a minimal renderer here
(a crude exposure+tone-curve approximation) to produce an image-level number, or compare the six
slider values directly. The first option adds its own approximation error on top of whatever
error the sliders themselves have, which would make a bad fit and a bad renderer indistinguishable
in the result — exactly the ambiguity a decision rule needs to avoid. Slider-value MAE/p95/bias is
exact, needs no renderer, and is what #46 actually needs to consume as its "Auto" button's
targets. #46's own exit criteria now states it owns the rendered-image comparison instead.

## RapidRAW prior art — not yet done

`docs/research/stalk-prior-art.md` flags #99 among the tickets RapidRAW is "worth studying at the
algorithm level once the deep-dive names the actual entrypoint functions" (see that doc's Cross-
reference section) — but its deep-dive to date only confirmed RapidRAW's SAM-ViT-B masking,
LaMa inpainting, and CLIP-based search path, not an auto-tone/auto-adjust feature specifically.
No RapidRAW auto-tone algorithm has actually been read or cited here. Candidate A's percentile
heuristic instead follows the commonly-documented public approximation of Adobe's own undisclosed
algorithm (per #99's issue body), which doesn't depend on RapidRAW at all. If a future pass finds
RapidRAW does ship a comparable classic auto-adjust, it's a candidate C worth adding to #202's
comparison — not assumed here.

## Deferred to #202

See ADR-0099's Decision section: the real LRC capture, the sample-set selection (needs `ref-10k`
on disk, absent from the sandbox this research was done in), and resolving the decision rule are
all #202's work, not this ticket's.
