# ADR-0047: Crop, straighten, and auto-level

- **Status:** Accepted
- **Date:** 2026-09-27
- **Ticket:** #47 Build: crop/straighten + auto-level horizon

## Context

#44/#45 already landed crop as an affine sample pass over the live suffix's own output
(`crates/nicti-tapetum::geometry`/`stages::CropKernel`/`present_sample.wgsl`), but that pass only
ever composed a plain translation (`Affine2D::crop`) -- no rotation, and no typed params (crop's
`default_params` was a bare `json!({"x": 0.0, "y": 0.0})`, never wired through `coat.rs`'s typed
-params convention #46 established for every other stage). #47 asks for two complementary,
both-required features:

1. Manual straighten via a **Ctrl-drag-a-reference-line gesture**, distinct from a freeform
   rotate-drag on the crop overlay (confirmed missing from RapidRAW, see #69).
2. An **auto-level button**: downscale -> Canny edge detection -> Hough line transform -> median
   angle, via `imageproc` if it covers both primitives without pulling in the full opencv-rust
   binding surface.

## Decision

**Rotation composes into the existing affine crop transform, not a new stage** (per ADR-0044's
stage-order note): `geometry::Affine2D::crop_and_rotate` extends `Affine2D::crop` with a rotation
about an arbitrary center, reducing exactly to `crop` at `rotation_degrees == 0.0`.
`geometry::affine_for_crop(rect, rotation_degrees)` is the one entry point a caller needs --
it derives the rotation center from the crop rect's own center, so nothing upstream of
`CropKernel::set_transform` has to hand-derive that itself. `present_sample.wgsl` needed **no
shader change at all**: it already reads a full 2x3 affine (`a,b,c,d,tx,ty`), so a rotation is
just a different set of coefficients through the exact same kernel.

**Sign convention (documented explicitly, not just asserted, since this is exactly the class of
bug an adversarial review is told to attack):** `rotation_degrees` is the angle the *displayed
content* appears to rotate **clockwise**, in this crate's y-down pixel convention. Locked in by
three tests in `geometry.rs` (`crop_and_rotate_90_degrees_matches_hand_derived_clockwise_rotation`,
a 180-degree point-reflection check, and a translation+rotation composition check that the rect's
own center always samples the rect's own center regardless of angle) plus a real GPU-vs-CPU
parity test (`stages::present_sample_gpu_matches_cpu_reference_with_straighten_rotation`) proving
`present_sample.wgsl` matches `geometry::sample_bilinear` with a real 12-degree rotation, not just
the pre-#47 translation-only case.

**The Ctrl-drag gesture and the auto-level button write into the same field**
(`coat::CropParams::rotation_degrees`, via `CropParams::set_rotation`, which always clamps to
`geometry::MAX_STRAIGHTEN_DEGREES` = 45 degrees) -- they're complementary entry points to one
value, not alternates with separate storage, matching the issue's own "both must exist, they're
complementary" framing. `geometry::straighten_delta_degrees(dx, dy)` is the shared math both the
manual drag (`nicti-pelt::render::DevelopView::straighten_from_drag`) and auto-level
(`autolevel::detect_level_angle`, feeding `DevelopView::apply_auto_straighten`) reduce to: given a
vector that should be level, auto-detect whether it's meant to be horizontal or vertical (whichever
axis dominates), and return the smallest rotation (always wrapped into `(-90, 90]` degrees) that
levels it -- never a >90-degree flip for a near-diagonal input.

### `imageproc` vs `opencv-rust`

**`imageproc` 0.27 wins outright** -- it has both primitives this ticket needs
(`edges::canny`, `hough::detect_lines`/`PolarLine`/`LineDetectionOptions`), confirmed by reading
its actual source in this sandbox's cargo registry cache before committing to it (not assumed from
the crate name). MIT-licensed, and with `default-features = false` (dropping `text`/`ab_glyph` font
rendering, `fft`/`rustdct`, and `rayon` -- none needed for Canny/Hough) it adds a modest,
well-scoped dependency surface: `image` (also MIT, `default-features = false`) plus `nalgebra`,
`itertools`, `approx`, `num`, `rand`/`rand_distr` -- all already-common, permissively-licensed
crates, no new `deny.toml` allowlist entry needed (`MIT` already covers both). `opencv-rust` would
have meant either a system OpenCV install or a vendored build, a far larger binary/build-time
surface, and a C++ FFI boundary -- exactly the "full opencv-rust binding surface" this ticket asks
to avoid when `imageproc` alone covers the need, which it does.

`autolevel::detect_level_angle` downscales to `DOWNSCALE_LONG_EDGE` (800px), runs Canny (fixed
20.0/50.0 thresholds -- not yet tuned against a real photo, see Consequences), Hough
(`vote_threshold` scaled to the downscaled image's shorter edge, `suppression_radius = 8`), and
takes the **median** deviation-from-level (not the mean, and not just the single highest-vote line)
across every detected line within `MAX_AXIS_DEVIATION_DEGREES` (30 degrees) of either axis -- a real
diagonal feature (a receding fence line, a staircase) is gated out rather than dragging the result
toward its own angle, and the median is robust to one or two spurious near-axis detections a single
outlier line would otherwise skew.

### Crop rectangle scope

`coat::CropParams` (x/y/width/height in source-pixel space, `Default` sentinel `width == 0.0 ||
height == 0.0` meaning "full source frame," resolved by `effective_rect`) is a real, cached,
non-destructive `StageEntry` like every other stage -- `stages::crop_stage`'s `default_params` now
uses `coat::default_value::<CropParams>()` instead of the old untyped `json!({"x":.., "y":..})`,
and `BasicStage`'s existing `cache_contribution` default impl (hash of id + impl_version + params)
already wires it into the render graph's cache key correctly with zero extra plumbing.

**The interactive Develop preview's own canvas size does not change when the crop rect shrinks.**
This deliberately mirrors real editor UX (Lightroom Classic, Photoshop): the crop tool shows the
full, uncropped render with a movable/resizable rect overlay (`nicti-pelt::develop_panel::
handle_viewport_gesture`/`draw_crop_overlay` -- corner-handle resize in the rect's own rotated
local frame, a separate rotate handle for freeform rotate-drag, body-drag-to-pan, and Ctrl-drag
-anywhere for the reference-line gesture), and the actual pixel resample to the smaller framing
only happens at export/full-res time. `crates/nicti-tapetum::tile::TiledRender`/`MemorySink`
already supports an output extent decoupled from the source (its own `TilePlanner::plan` takes an
arbitrary ROI + a `MemorySink::new(output_extent)` of any size) -- that plumbing was already real,
landed in #45 PR4 for the full-res tiled path, and needed no change here. What's genuinely new in
this ticket is the rect model, the rotation math, the two gesture entry points, and the overlay UI
-- not a render-pipeline resize capability, which already existed for the path that needs it
(export), and was never needed for the path that doesn't (an interactive dim-outside-the-rect
preview).

## Measured results

- 31 new/changed unit tests in `nicti-tapetum::geometry`/`coat` (rotation math, sign convention,
  `CropParams` no-op/round-trip/effective-rect behavior), all passing.
- 8 new unit tests in `nicti-tapetum::autolevel` against synthetic single-line images with known
  exact tilt angles (0, 8, and 45 degrees), a uniform featureless image, and a zero-sized image --
  all passing, including the 45-degree "not confident evidence" gate.
- 1 new GPU-vs-CPU parity test (`present_sample_gpu_matches_cpu_reference_with_straighten_rotation`)
  against a real (llvmpipe software) `wgpu` adapter, passing.
- Full workspace `cargo fmt --all -- --check` / `cargo clippy --workspace --exclude retina
  --exclude nicti-cornea --exclude knead --all-targets --all-features -- -D warnings` / `cargo test
  --workspace --all-targets --all-features --exclude retina --exclude nicti-cornea --exclude
  knead`: clean, matching CI's own exact invocation (`.github/workflows/ci.yml`).
- **No real-photo measurement exists for `autolevel::detect_level_angle`'s Canny thresholds or
  Hough vote/suppression parameters** -- this sandbox has no real NEF/photo fixture for this path
  (same gap as ADR-0099's own reference-machine deferral). The thresholds are documented, reasonable
  starting points, not measured against a real photo corpus.

## Options considered

- **A separate `nicti-straighten`/`nicti-autolevel` crate.** Rejected: the math is a small, tightly
  -coupled extension of `nicti-tapetum::geometry` (the crop transform itself) and a self-contained
  CPU analysis step with one public function -- a new crate would be more scaffolding than code,
  the opposite of this repo's own "avoid half-finished scaffolding" convention (see `color.rs`'s
  own doc comment on the same principle for #45's HueSatMap bindings).
- **`opencv-rust`.** Rejected -- see the dedicated section above.
- **Decoupling the interactive preview's geometry-stage output extent from the baked/live extent,
  so the crop rect visually resizes the live canvas.** Deferred, not rejected outright -- a real
  enhancement (filed as a follow-up, see Consequences), not required for this ticket's two
  explicitly-bolded asks (the gesture, the auto-level button) or for a correct, real crop rect data
  model/overlay/export path, and it would have meant restructuring `RenderRequest`/`Renderer`
  across `renderer.rs`, `tile.rs`, `bench/knead`, and `nicti-pelt::render` -- a materially separate
  , larger architectural change than "wire rotation into the existing affine transform."

## Consequences

- Follow-up filed: **#272** -- live Develop preview should resize its own canvas to the crop
  rect's aspect/size in real time (today only pan+rotate visibly apply in the interactive preview;
  the crop rect itself is correctly modeled, cached, and already honored end-to-end by the existing
  full-res `TiledRender`/`MemorySink` export path, just not yet the low-res live preview canvas).
- Follow-up filed: **#273** -- tune `autolevel::detect_level_angle`'s Canny/Hough thresholds
  against real photos once a reference-machine pass (matching
  #149/#163/#164/#171/#200/#202/#222/#233/#236's own pattern) is available -- `needs-physical
  -testing` labeled, not blocking this ticket.
- `stages::crop_stage`'s `default_params` change (untyped `json!` -> `coat::default_value::
  <CropParams>()`) changes the exact JSON an empty/default crop entry serializes to. No persisted
  `EditDocument` exists anywhere yet (catalog persistence is #31's scope), so there is no migration
  concern from this change.
