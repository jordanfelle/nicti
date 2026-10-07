# ADR-0428: The lens stage (DNG-embedded profile, automatic lateral CA) and global Defringe

**Status:** Accepted
**Ticket:** #428
**Part of:** #424
**Related:** #410, #351, #358, #382, #372, #44, #380

## Context

`nicti.lens` was a passthrough (ADR-0044 reserved its slot; #39 closed without an ADR) and
`nicti-iris`'s `LensCorrection` was an empty trait. The LightCraft audit (#424, `storytold/lightcraft`
at commit `265248c`, MIT OR Apache-2.0) found three pieces of `crates/pipeline/src/optics.rs` worth
taking: an automatic lateral-CA estimator, DNG-embedded `WarpRectilinear`/`FixVignetteRadial`, and a
purple/green defringe. Its own roadmap says its checkmarks are unverified, so every constant here is a
starting point, not a measurement.

Out of scope, on purpose: a lens database for NEF (lensfun, or Nikon's data embedded in the file).
That is #410's decision and plugs in behind the trait below.

## Decision

### 1. The lens stage is its own baked resample pass, in camera RGB

It cannot ride the crop pass: crop runs *after* the live suffix, in ProPhoto, with the channels
already mixed, while per-channel CA correction and the DNG per-plane warp are defined on the camera
channels (the `render-graph` topic's "Lens correction" rule). So `nicti.lens` is a real `BakedExec`
(`nicti-tapetum/src/slit.rs` + `shaders/slit.wgsl`): one bilinear resample of the normalised frame in
which red, green and blue are each sampled at their own source position, then multiplied by the vignette
gain. It stays upstream of heal and of the keying-only `nicti.neutral` node, so a lens change
re-keys an AI mask bake. **That is the key, not the pixels**: the model's neutral image is still built
from the uncorrected decode, so the masks do not yet see the corrected frame (#358; see Consequences).

Per output pixel centre `p` (continuous coordinates):

1. inverse DNG `WarpRectilinear` per colour plane (normalise by the centre-to-farthest-corner
   distance, radial polynomial `kr0..kr3` plus tangential `kt0/kt1`, per the DNG specification);
2. red and blue additionally magnified about the optical centre by `1 + alpha` (automatic CA);
3. bilinear sample of each channel, clamped at the frame edge;
4. the `FixVignetteRadial` gain, evaluated at the **green** source position.

Baked stages run once, full frame, at source extent, and tiling re-runs only the geometry pass, so a
full-frame lens pass needs no halo. The kernel reads a sampled `texture_2d` through `textureLoad` and
writes a write-only storage texture (the Dx12 rule from ADR-0051/#49); its uniform buffer is created
per dispatch.

### 2. `nicti-iris` carries the model; the first provider is the DNG's own

`LensCorrection::model(&LensSource) -> Option<LensModel>`, where `LensModel { warp, vignette }` is
plain data (per-plane coefficients plus centre), so a GPU kernel never knows where it came from.
`dng::DngEmbedded` parses the file's `OpcodeList3`: big-endian, `count` then `{id, version, flags,
size}` per opcode, only ids 1 and 3 read. The blob is untrusted, so the parser bounds the opcode
count, rejects non-finite floats, a plane count outside 1..=4, coefficients beyond 100 and a centre
outside -0.5..1.5, and never reads past the buffer. A lensfun or NEF provider (#410) implements the same
method.

### 3. Automatic lateral CA: LightCraft's estimator, with three changes

Strong, radially oriented green edges are matched in red and blue with 1-D sub-pixel profile
matching; a weighted, residual-trimmed fit of `displacement = alpha * r` gives `[alpha_R, alpha_B]`.
Changes from LightCraft (`nicti-iris/src/lateral_ca.rs`):

- **It runs on a 2x2-box-decimated copy**, so the unchanged ±3 px search covers ±6 px at full
  resolution. LightCraft searched ±3 px at full resolution, which caps the measurable `alpha` near
  `3/r` and *silently discards* every larger match, biasing the result toward zero. A test plants a
  ~7 px corner shift that the old window could not hold.
- **It fits about the optical centre** (the profile's, when there is one), not always the middle.
- **A goodness-of-fit gate.** LightCraft accepts any 12 matches. On pure white noise (and on frames
  with clipped highlights) matches still appear, scattered, and the fit returned a confident
  `alpha` of ~1e-3: about a pixel of red/blue fringe at the corners on an image with no CA. Lateral
  CA makes the displacement proportional to radius, so the radial-scale model must explain at least
  half of the displacement's weighted energy (`MIN_R_SQUARED`) or the plane reads as no estimate.
  Tests cover noise at three levels and clipped highlights.
- **The estimate is memoised** (8 entries, keyed by a content fingerprint of the frame and the
  centre): it is a pure function of the frame, and `LensExec` runs on every baked-cache miss of the
  lens node, on the render path.

**No double correction.** LightCraft estimates CA on the source and *adds* it to a DNG profile's
per-plane warp, correcting a profile-corrected image twice. Here auto-CA is skipped when the embedded
warp's planes differ (`Warp::corrects_lateral_ca`).

Cost, measured: **roughly 0.2-0.4 s for a 45 MP (8256x5504) frame** in release
(`lateral_ca::tests::throughput_at_45mp`: 220-425 ms across runs on a shared WSL machine; one number would
overstate the precision), CPU, only when Remove CA is on and the memo misses. `|alpha|` is clamped to 0.02 (real lateral CA
is well under 0.5 %).

### 4. Defringe is a *live* stage, in the fused dispatch

`nicti.defringe` (`DefringeParams`: purple and green amount plus a hue window each) is not part of the
lens stage. LRC's sliders drag live, and putting them in the baked lens node would re-key every AI mask
bake on every drag. It lives in `live_suffix.wgsl`, right after the camera-to-working matrix, so a slider
drag is a uniform write: **no rebake, no extra pass, no new binding**.

A pixel is desaturated toward its luma when it is (a) saturated (HSV saturation smoothstep 0.05..0.20),
(b) inside a hue window, and (c) next to a strong luminance edge (perceptual-luma range across the
neighbourhood, smoothstep 0.08..0.25). Strength is linear in the normalised amount (LRC 0..20 -> 0..1), so every slider position
does something and an imported LRC amount of 8 and of 20 stay distinguishable. (LightCraft saturated at
8 of 20; copying that left 60 % of the range dead, which review caught.)

Deviations from LightCraft's global defringe, each deliberate:

- **Hue in HSV of the linear working space**, not OkLab: the shader already has `dcp_rgb_to_hsv`, and
  an OkLab conversion would need a ProPhoto-to-XYZ-to-LMS chain here for no demonstrated gain. The
  purple window is 240..360 degrees across the 0..1 slider, green 60..180, with 8 degree shoulders and a
  360/0 seam wrap. LRC's defaults (purple 30/70, green 40/60) land at 276..324 and 108..132.
- **The edge test reads 8 compass taps** `2 * (long edge / 4000)` px away (clamped 1..6) instead of a
  full min/max box, and only when the hue/chroma gate has already passed, so it costs nothing on an
  unfringed pixel. LightCraft's radius grew with the amount, which would have made the edge field
  depend on the slider; here it is fixed, so the amount is purely a strength.
- It runs **before** exposure/tone (scene-linear), as LightCraft does.

`color::defringe_pixel` is the CPU twin; the shader is proven against it on a real adapter and
mutation-checked.

**Known behaviour, inherent to any defringe:** the border pixels of a genuinely purple or green object
sit next to an edge and lose chroma at high amounts; its interior does not (tested). The hue-window
sliders exist to narrow that.

### 5. DNG ingest

Scruff now picks up `.dng` (`RAW_EXTENSIONS`). It is the one format whose files carry their own profile,
so without it the opcode path could never run. The T0 walker already reads DNG SubIFD previews (including
the old-style single-strip JPEG), and XMP sync only ever writes `.xmp` sidecars, never into the RAW
(#372 tracks DNG sidecar semantics beyond that). LibRaw already reads `OpcodeList1/2/3` into
`imgdata.color.dng_levels.rawopcodes` (capped at 4 MB each) but applies none of them: the shim gains
`retina_dng_opcode_list`, the FFI wrapper `LibRawHandle::dng_opcode_list3`, and `LinearFrame` gains
`dng_opcode_list3: Option<Vec<u8>>`. A test builds a tiny uncompressed CFA DNG with a known list and
round-trips it through the real shim, the FFI and `nicti_iris` (no binary fixture in a public repo).

### 6. Cache keys

`LensParams { remove_ca, embedded_profile }` serialises to `{}` at its defaults and `impl_version`
stays **0**, deliberately. A NEF with default params still renders exactly the old passthrough pixels,
so its lens hash, every baked-cache key and (through `nicti.neutral`) every on-disk AI alpha (#353) stay
valid; bumping the version would orphan them all for no pixel change. Bump it when the *algorithm* changes.
The profile and the CA estimate are pure functions of the decoded file, so they are not part of the key.

### 7. LRC import and UI

`nicti-stray`'s `develop/lens.rs` maps `AutoLateralCA` and `DefringePurple/Green{Amount,HueLo,HueHi}`,
gated by `EnableLensCorrections`; as in ADR-0380, a channel's hue sliders count only when its amount is
non-zero, or every untouched image would import as edited. `LensProfile*`, manual distortion and
`Perspective*`/`Upright*` stay untranslated (#382, #427). The Develop panel gains a Lens Corrections
section: Remove chromatic aberration, Use embedded lens profile (offered only for a DNG that has one),
and the six Defringe sliders.

## Consequences

**Not verified, and why it matters:**

- **No real photo has been through any of this.** The one DNG on the dev machine is a lossy-JPEG phone
  file LibRaw cannot decode and has no `OpcodeList3`. The CA constants (gradient 0.2, radial cosine 0.85,
  SSD 0.3, trim 2.5x), the defringe thresholds and hue windows are LightCraft's hand-tuned values,
  re-expressed, and **must be tuned on real Z8 NEFs and a real profile-carrying DNG** (#424's provenance
  rule). `NICTI_TEST_REAL_DNG_DIR` (decode + parse) and the existing `NICTI_TEST_REAL_NEF_DIR` are the hooks.
- **Opcode geometry is assumed, not confirmed.** The opcode centre and the normalisation radius are taken
  over the *decoded frame* (LibRaw's output), ignoring `ActiveArea`/`DefaultCrop` offsets. For a real
  file those differ from the frame by tens of pixels in thousands (a sub-0.5 % centre error), but this is
  unconfirmed on a real profile-carrying DNG. LibRaw exposes `dng_levels.default_crop` should it prove
  to matter.
- **The warp and vignette equations** are the DNG specification's published ones (radial polynomial
  in `r^2`, tangential terms `kt0`/`kt1`, vignette `1 + k0 r^2 + ... + k4 r^10`), implemented from the
  spec text and cross-read against LightCraft's implementation. They have **not** been checked against
  Adobe's `dng_sdk` or a reference render of a real profile-carrying DNG.
- **Only `OpcodeList3` is read.** If a real DNG puts `FixVignetteRadial` in list 1 or 2, its corners
  render darker than in other DNG readers. Unconfirmed without a real profile-carrying file.
- **Hostile profiles.** Coefficients and the centre are bounded at parse time, and the vignette gain
  is clamped to 0..16 in the shader (a negative gain would turn the frame negative and the live
  shader's cube roots into NaN); a profile whose planes differ by float noise (< 1e-6) is not treated
  as already correcting CA.
- **Opcode order.** The vignette gain is evaluated at the green *source* position (vignette-before-warp),
  as LightCraft does; a DNG whose list applies them in the other order would differ slightly.
- **Colour space.** DNG per-plane coefficients are meant for camera RGB, which is what this pass runs on
  (unlike LightCraft, which warps Rec.2020). The profile is applied after LibRaw's demosaic, not on raw
  mosaic values; no reference render exists to compare against.
- **Reference machine.** The new kernel and the live-shader change are proven on lavapipe and against
  their CPU twins; neither has been run on the RTX 5080 under Vulkan *and* Dx12
  (`NICTI_WGPU_BACKEND=vulkan|dx12`, the ADR-0051 rule).
- **AI masks and AI removal still infer from the *uncorrected* decode.** `nicti-siamese`'s neutral
  image and `nicti-groom`'s model frame are built from the `LinearFrame` (`FramePixels`), not from
  the lens-corrected baked frame, so for a DNG whose profile warps, an AI mask's alpha (and an AI
  removal patch) is computed on pre-warp geometry and applied to the corrected frame: misaligned by
  up to the warp's displacement (tens of pixels at a corner for a ~1 % `kr1`). That is #358's scope,
  now partly unblocked. A NEF is barely affected (no profile; auto-CA moves only red and blue by
  well under 0.5 %, green, which dominates the model's luminance, not at all), so this bites only a
  DNG with a warping profile.
- The `docs/licensing.md` entry says ported files carry the copyright line in their header; LightCraft's
  own files have none, so the attribution is the repo-level grant plus our header lines.
- Defringe's extra neighbour reads cost only while a defringe amount is non-zero; not measured at 45 MP.

**Not done:** NEF lens profiles (#410); local Defringe and Moire (#351: the live shader's `defringe`
gate and edge test are the pieces it can reuse); DNG sidecar semantics (#372); `LensProfile*` import
(#382); a preview-extent consistency check for the lens pass (it runs at source extent like every baked
stage).
