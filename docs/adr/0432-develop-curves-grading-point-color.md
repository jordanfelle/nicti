# ADR-0432: Develop UI — point curves, colour mixer, Color Grading and Point Color

**Status:** Accepted
**Ticket:** #432
**Part of:** #424
**Related:** #381, #380, #428, #425, #149, #42, #452, #453, #454

## Context

The Develop panel had a hover-only preview of the four-slider parametric curve and an 8-band HSL
mixer picked through a row of letters. The LightCraft audit (#424, `storytold/lightcraft` at commit
`265248c`, MIT OR Apache-2.0) found the Lightroom-style widgets that were missing: a point-curve
editor, the colour-mixer band picker, 3-way grading wheels and Point Color. Each needs a Tapetum
stage, and none of the stages existed. LightCraft's own roadmap says its checkmarks are unverified
and its colour maths is "tuned by eye", so every constant below is a starting point.

Out of scope, on purpose: a *measured* match against LRC renders. That needs the reference machine and
the #149 harness, so it is #452 (`needs-physical-testing`), not a merge gate. Also out: targeted
adjustment (dragging on the photo to move a curve/mixer value, #453), Point Color's "visualize range"
overlay and LRC `PointColors` import (#454), and the camera Calibration panel (done in #381, ADR-0381).

## Decision

### 1. Three new live stages, each fused into the one live dispatch

`nicti.point_curve`, `nicti.color_grade` and `nicti.point_color` join `spine::LIVE_IDS`
(`tone_curve → point_curve` and `hsl → color_grade → point_color`), so a slider drag is a uniform or
texture write and never rebakes anything (the #428 `nicti.defringe` pattern). All three skip their
shader work when they are no-ops, so an unedited photo is bit-identical to before (pinned by tests that
compare against an untouched run).

`nicti.point_curve` is separate from `nicti.tone_curve` because a `Vec` of points cannot be `Copy` (the
parametric stage and `LiveParams` users rely on it) and because it keeps `ToneCurveParams` and its cache
keys byte-stable.

### 2. Point curves: a CPU-built LUT texture, not uniforms

`PointCurveParams { master, red, green, blue: Vec<[f32; 2]> }` in 0..1; an empty list is the identity and
serializes to `{}`. Points are untrusted, so `sanitized()` drops non-finite ones, clamps, sorts, enforces
strictly increasing x, caps at 32 and turns the identity diagonal into "nothing".

The curve is the same Fritsch-Carlson monotone spline the DCP profile tone curve uses
(`nicti_calico::tonecurve::ToneCurve`), evaluated per channel as `channel(master(x))` into a 256 x 3
`R32Float` texture (one row each for R, G, B) read with `textureLoad`, like `upload_tone_lut`. It is not
packed into `LiveUniforms`, which is already vec4-bound for the 256-entry parametric LUT. It is applied in
the same cube-root perceptual space as the parametric curve, right after it, and re-uploaded only when its
hash changes.

### 3. Color Grading and Point Color: OkLab / OkLCh on ProPhoto pixels

Both need a perceptual space whose chroma and lightness are separable, and HSV (what the HSL stage uses)
shifts hue unevenly and grades poorly. `crates/nicti-tapetum/src/oklab.rs` takes a linear ProPhoto pixel
to OkLab through **one precomputed 3×3** (ProPhoto → XYZ D50 → Bradford → XYZ D65 → LMS), a cube root and
Ottosson's M2, edits it, and returns. The existing HSL stage is deliberately **not** converted.

- **Color Grading** (`ColorGradeParams`): four wheels (shadows, midtones, highlights, global), each a hue
  in the *painted HSV wheel's* degrees, a saturation and a luminance, plus blending and balance. A wheel is
  precomputed into OkLab offsets `(dL, da, db)`; the wheel hue becomes an OkLab direction through the same
  HSV colour the UI paints, so the colour you pick is the colour the grade pushes toward. The tonal weights
  are smoothsteps on OkLab L with a split of `0.5 − balance·0.25` and a half-overlap of
  `0.15 + blending·0.5`; they sum to 1 (a test pins it).
- **Point Color** (`PointColorParams`): up to 8 `PointColorSample`s, a fixed-size array so the params stay
  `Copy`. A sample is a colour in OkLCh plus hue/saturation/luminance shifts, a variance and four ranges;
  its weight is a product of soft boxes in hue, chroma and lightness. Samples are packed three vec4s each.
- The CPU reference is `oklab::OkLabOps::apply`; `live_suffix.wgsl`'s `apply_oklab_ops` is its twin, with a
  GPU-vs-CPU parity test (`stages::tests::oklab_ops_in_the_live_pass_match_the_cpu_twin_...`).

The sample colour comes from the same small as-shot thumbnail the mask colour eyedropper uses
(`catseye.rs`), so it ignores edits already applied. It is close, not exact, on a heavily tone-edited
photo; the ranges are wide enough that it lands in the right colour family. Sampling the rendered frame
is #454.

### 4. The UI

New egui widgets in `nicti-pelt/src/fur/`: `whisker_curve.rs` (drag/add/remove points, per-channel
colour, right-click reset; the point operations are pure and unit-tested; endpoints keep their x),
`iris_wheel.rs` (a painted disc, drag/click to set hue and saturation, double-click to reset) and
`widgets::band_dots` (the colour mixer's coloured band picker). The Develop panel gains a channel picker
in Tone Curve, the Color Grading section (one wheel at a time plus hue/saturation/luminance sliders, so
every value can be set precisely), and the Point Color section. The eyedropper is a fourth `Tool` in
`HealUi`, which the viewport router in `app.rs` already switches on.

### 5. LRC import

`nicti-stray`'s `develop/grade.rs` translates `ToneCurvePV2012{,Red,Green,Blue}` (flat `x, y` lists in
0..255) and Color Grading (Shadows and Highlights hue/saturation live under LRC's older `SplitToning*`
keys, Midtones and Global under `ColorGrade*`, luminance under `ColorGrade<Wheel>Lum`, shared
`SplitToningBalance`/`ColorGradeBlending`; gated by `EnableSplitToning`). A wheel with no saturation and
no luminance is not an edit even if LRC kept a stray hue. **Not translated, still in provenance:**
`PointColors` (its string format needs its own research), calibration and `CameraProfile` (translated since, #381/ADR-0381), and the
legacy Split Toning panel on pre-grading process versions.

## Consequences

- Every numeric constant (wheel scales 0.09/0.12, the Point Color box widths, the tonal split) is
  LightCraft's, untuned on real photos. A graded or point-coloured photo will **not** match LRC until the
  reference-machine parity pass fits them; the import maps values numerically only.
- The new kernels have lavapipe/Vulkan parity tests only. Per the `gpu-gui-and-healing` rule they must be
  run on the reference machine under both Vulkan and Dx12 before they are trusted there.
- Point Color's sample is as-shot, not as-rendered (above).
- Edits made through these controls have no undo yet: Develop has no history (#324), which `SliderOut`
  already reserves the drag signals for.

## Not verified

Visual quality on real photos; LRC parity; the Dx12 backend; the eyedropper against a real decoded frame
(its pure parts are unit-tested, the click path needs a live viewport).
