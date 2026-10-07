# ADR-0381: Camera Calibration, LRC camera-profile resolution, and a Look's own tone curve

**Status:** Accepted
**Ticket:** #381
**Part of:** #11
**Related:** #62 (ADR-0062), #42 (ADR-0042), #321, #432 (ADR-0432), #452, #454

## Context

ADR-0062 left four LRC develop settings untranslated, and #432 has since done two of them (point
tone curves and Color Grading). What remained:

- **Calibration** (`RedHue`/`RedSaturation`/`GreenHue`/`GreenSaturation`/`BlueHue`/`BlueSaturation`,
  `ShadowTint`): nicti had no stage, no params and no UI for it at all.
- **`CameraProfile`**: LRC stores the profile's *name* ("Camera Landscape", "Adobe Vivid"). nicti's
  profile selection is a file path plus a blake3 of the `.dcp` (`CameraProfileParams`), and nothing
  mapped a name to an installed file.
- **A Look's own tone curve.** Adobe Raw Look profiles (Adobe Color, Vivid, ...) bake a
  `crs:ToneCurvePV2012` in. `xmp_profile.rs` parsed the Look's table but listed the curve as
  unsupported, so a photo imported as "Adobe Vivid" rendered flatter than in LRC.

Out of scope: a *measured* match against LRC (needs the reference machine, tracked as a follow-up like
#452), `PointColors` (#454), a Look's `Clarity2012` and RGBTable-based looks.

## Decision

### 1. Calibration is a live stage whose primaries are folded into the camera matrix

`nicti.calibration` (`coat::CalibrationParams`, seven `-1..=1` fields, all-zero = no-op) joins
`spine::LIVE_IDS` right after `nicti.working_space`.

- **Primaries** change no shader. `color::calibrate_matrix` takes the camera->working matrix, treats
  its three columns as the primaries' images, moves each in xy chromaticity (hue = rotation about the
  D50 white by up to 30 degrees, saturation = scaling the distance from it by up to +-50%, luminance Y
  kept) and then re-scales the columns so they still **sum to the original white**. That last step is
  the point: camera white keeps mapping to the same working-space white, so a neutral stays neutral
  whatever the sliders say (pinned by a test over extreme settings). A primary that would leave the xy
  plane is stopped at `y = 0.02` instead of being dropped; a degenerate matrix (singular, non-finite or
  non-positive gains) is returned unchanged rather than corrupting the render.
  `spine::resolve_inputs` applies it to whichever matrix is in use, the LibRaw one **or** the DCP
  solution's, so with a profile it lands before the HueSatMap, as Adobe's calibration does.
- **Shadows Tint** is a green gain (positive = magenta) weighted by a shadow mask on perceptual luma
  (`SHADOW_TINT_KNEE`), applied right after the matrix and defringe. It lives in `defringe1.z` of
  the existing uniform block (that slot was unused), CPU twin `color::shadow_tint_pixel`, and is skipped
  when 0, so an unedited photo stays bit-identical.
- Every constant (`CALIBRATION_MAX_HUE_DEG`, `CALIBRATION_MAX_SAT`, `SHADOW_TINT_MAX_GAIN`, the knee) is
  a starting point, **untuned against LRC**.

### 2. A Look's tone curve runs through the point-curve stage, ahead of the user's curve

`xmp_profile::parse` now reads `crs:ToneCurvePV2012` into `LookProfile::tone_curve` (0..255 pairs ->
0..1; fewer than two points, a non-finite or out-of-range value, or non-increasing x leaves it empty
and keeps the "unsupported" note). It is **not** composed into the profile's `tone_lut`: that table is
in scene-linear (sqrt-indexed) space, while a Look's curve is display-referred, the same domain as the
Develop panel's point curves. So `spine::resolve_inputs` passes it as `LiveParams::look_curve`
(only when the document selected a DCP *and* that Look) and `color::build_point_curve_luts` composes
`channel(master(look(x)))` into the existing 256x3 texture. No shader change; a document with no Look
curve builds exactly the tables it did before.

Consequence: the Look's curve is applied where the point-curve stage sits (after the parametric curve),
not at the profile stage. For a Look that is the only tone edit the result is the same image; with a
heavy user tone curve the order differs slightly from Adobe's. Accepted, and part of the parity follow-up.

### 3. `CameraProfile` is resolved by name, by the app, at import

- `develop/profile.rs` only extracts the name (`Translation::camera_profile`), keeping `translate`
  pure. **`Adobe Standard` and `Embedded` request nothing**: LRC writes `Adobe Standard` into virtually
  every raw, nicti's own default is the plain camera matrix, and taking it would mark every imported
  photo as edited (the zero-amount-grain precedent, ADR-0380).
- `nicti-stray` cannot read profile folders (that code is in `nicti-pelt`, which depends on stray), so
  `ImportConfig::profile_resolver` carries a `ProfileResolver` trait object. The job collects each
  asset's make/model while matching, asks the resolver once per (make, model, name) per run, and writes
  the answer as the `nicti.working_space` stage **before** the provenance snapshot, so the existing
  "never overwrite a later nicti edit" rule covers it.
- `nicti-pelt`'s `LrcProfileResolver` (`camera_profiles.rs`): a case-insensitive match of the name
  against the camera's installed DCPs wins; otherwise a Look `.xmp` of that name is layered on that
  camera's **Adobe Standard** DCP (a Look needs a DCP underneath, and Adobe's own Raw profiles use it);
  otherwise `None`. Nothing is substituted: an unresolved name is counted in
  `LrcImportReport::profiles_missing` (by name, so the user knows what to install), added to the image's
  untranslated list, and no stage is written.
- Calibration keys are translated in `develop/calibration.rs` (/100, clamped; all-zero writes nothing).

### 4. UI

A "Calibration" section in the Develop panel (Shadows Tint, then Red/Green/Blue Primary Hue and
Saturation), `Reset all` clears it. It edits `DevelopDoc`, so history and autosave come for free.

## Consequences

- The cache key of every photo's live composite changes once (a new node), a one-time re-render, no
  correctness effect.
- Calibration strengths and the Look-curve ordering are untuned: a reference-machine run against LRC
  exports is filed as a follow-up.
- A Look's `Clarity2012` and RGBTable-based Looks are still reported as unsupported.
- A camera model not installed in Adobe's profile folders resolves nothing: the photo imports with the
  default matrix and the profile name in the report.
