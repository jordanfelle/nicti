//! Typed per-stage develop params (#46's "coat" -- the image's own look), parsed from a
//! `nicti_pawprint::StageEntry`'s untyped `serde_json::Value`. Every struct here derives
//! `Default` and `#[serde(default)]`, so a document with no entry, or an entry missing a field a
//! newer version added, always parses to something sane rather than erroring -- matching
//! `nicti_pawprint::StageEntry`'s own "round-trips untouched" philosophy for fields this build
//! doesn't recognize (`serde_json::from_value` already ignores unrecognized keys, the other half
//! of that same contract).
//!
//! Ranges noted in each field's doc comment match LRC's own PV2012 Basic panel (see
//! `docs/adr/0061-lrc-catalog-import-mapping.md`), so a value round-tripped through XMP import
//! lands on the same slider position a migrating user already expects.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::geometry::CropRect;

/// White balance. `temp_k: None` means "use the frame's own as-shot white balance"
/// (`LinearFrame::cam_mul`) -- the conventional default for a freshly imported photo.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WbParams {
    /// Color temperature in Kelvin, PV2012 range 2000..=50000. `None` = as-shot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temp_k: Option<f32>,
    /// Green-magenta shift, PV2012 range -150.0..=150.0. 0.0 is a no-op, independent of `temp_k`.
    pub tint: f32,
}

/// The selected DCP camera profile (#42), stored on the `nicti.working_space` stage so it flows
/// into that node's hash and therefore Tapetum's live-output cache key. `content_hash` (blake3 hex
/// of the `.dcp` bytes) is the part that matters for invalidation: two different files with the
/// same name must not share cached output. All-`None` (serialized as `{}`, the stage's historical
/// empty params) means no profile -- the plain LibRaw camera matrix.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CameraProfileParams {
    /// Human-readable profile name (the `.dcp`'s `ProfileName`), for the picker.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Where the `.dcp` was loaded from, so a session can reload it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// blake3 hex of the `.dcp` file's bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    /// An Adobe Raw "Look" `.xmp` profile layered on the DCP (#321). Skipped when absent so the
    /// params (and therefore every existing document's stage hash) are unchanged without one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub look: Option<LookRef>,
}

/// Identity of a selected Look `.xmp` profile: like the DCP's, the hash is what invalidates cached
/// output and what a reload is verified against.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LookRef {
    pub name: String,
    pub path: String,
    /// blake3 hex of the `.xmp` file's bytes.
    pub content_hash: String,
}

/// Basic-panel exposure, in stops. PV2012 range -5.0..=5.0. 0.0 is a no-op.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ExposureParams {
    pub stops: f32,
}

/// Basic-panel global tone controls, each PV2012 range -100.0..=100.0 (normalized to -1.0..=1.0
/// here). 0.0 on every field is a no-op. `highlights`/`shadows` are a global (not LRC's own
/// locally-adaptive) approximation -- see `color::apply_tone`'s own doc comment.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ToneParams {
    pub contrast: f32,
    pub highlights: f32,
    pub shadows: f32,
    pub whites: f32,
    pub blacks: f32,
}

/// Vibrance, PV2012 range -100.0..=100.0 (normalized to -1.0..=1.0 here). 0.0 is a no-op.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct VibranceParams {
    pub amount: f32,
}

/// The Basic panel's Presence group beyond Vibrance (#380): global Texture, Clarity, Dehaze and
/// Saturation, each PV2012 range -100.0..=100.0 (normalized to -1.0..=1.0 here). 0.0 on every
/// field is a no-op. They share the per-mask adjustments' kernels and are *summed with* any local
/// delta (`LocalAdjust`'s same-named fields), so a global +0.3 and a local +0.2 act as +0.5 there.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PresenceParams {
    pub texture: f32,
    pub clarity: f32,
    pub dehaze: f32,
    pub saturation: f32,
}

impl PresenceParams {
    pub fn is_noop(&self) -> bool {
        *self == Self::default()
    }

    /// Clamped to -1..1 with non-finite values scrubbed to 0: documents are untrusted, and an
    /// unbounded saturation would overflow the `Rgba16Float` target to Inf.
    pub fn sanitized(&self) -> Self {
        let unit = |v: f32| {
            if v.is_finite() {
                v.clamp(-1.0, 1.0)
            } else {
                0.0
            }
        };
        Self {
            texture: unit(self.texture),
            clarity: unit(self.clarity),
            dehaze: unit(self.dehaze),
            saturation: unit(self.saturation),
        }
    }

    /// Whether the clarity/texture band bases (`mask::bases`) are needed to render this.
    pub fn needs_bands(&self) -> bool {
        self.clarity != 0.0 || self.texture != 0.0
    }

    /// Whether the dehaze transmission/airlight base is needed to render this.
    pub fn needs_haze(&self) -> bool {
        self.dehaze != 0.0
    }

    /// Whether any spatial base is needed -- i.e. whether a render must bake first and build bases
    /// even with no active local correction.
    pub fn needs_bases(&self) -> bool {
        self.needs_bands() || self.needs_haze()
    }
}

/// How the post-crop vignette darkens (LRC's `PostCropVignetteStyle` 1/2/3, #380).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VignetteStyle {
    /// Multiplies the exposure toward the corners, holding highlights back by `vignette_highlights`.
    #[default]
    HighlightPriority,
    /// Multiplies the exposure, then re-saturates so colours stay vivid in the darkened corners.
    ColorPriority,
    /// Blends toward black (negative amount) or white (positive), like painting over the corners.
    PaintOverlay,
}

/// The Effects panel (#380): a post-crop vignette and film grain. Both are defined in
/// *crop-normalized* coordinates (the crop rect is 0..1 on each axis), so a vignette follows the
/// crop and a render at any resolution -- a preview, a full-size export, one tile of it -- puts the
/// same pattern in the same place. Amount 0 on both is a no-op.
///
/// Ranges are LRC's PV2012 ones normalized: amounts -100..100 -> -1..1 (grain 0..100 -> 0..1),
/// midpoint/feather/roundness/size/roughness as 0..100 -> 0..1 (roundness -100..100 -> -1..1).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EffectsParams {
    /// Negative darkens, positive lightens.
    pub vignette_amount: f32,
    /// Where the falloff starts: higher confines it to the edges. LRC default 0.5.
    pub vignette_midpoint: f32,
    /// Softness of the falloff. LRC default 0.5.
    pub vignette_feather: f32,
    /// -1 squarer .. 0 oval following the crop .. +1 a circle.
    pub vignette_roundness: f32,
    /// Highlight Priority only: how much brighter pixels resist the darkening, 0..1.
    pub vignette_highlights: f32,
    pub vignette_style: VignetteStyle,
    /// 0 = none.
    pub grain_amount: f32,
    /// Grain cell size: 0 fine .. 1 coarse. LRC default 0.25.
    pub grain_size: f32,
    /// 0 smooth .. 1 rough (a second, finer octave). LRC default 0.5.
    pub grain_roughness: f32,
    /// Seeds the pattern (LRC's `GrainSeed`), so the same photo always gets the same grain.
    pub grain_seed: u32,
}

impl Default for EffectsParams {
    fn default() -> Self {
        Self {
            vignette_amount: 0.0,
            vignette_midpoint: 0.5,
            vignette_feather: 0.5,
            vignette_roundness: 0.0,
            vignette_highlights: 0.0,
            vignette_style: VignetteStyle::default(),
            grain_amount: 0.0,
            grain_size: 0.25,
            grain_roughness: 0.5,
            grain_seed: 0,
        }
    }
}

impl EffectsParams {
    pub fn vignette_active(&self) -> bool {
        self.vignette_amount != 0.0
    }

    pub fn grain_active(&self) -> bool {
        self.grain_amount != 0.0
    }

    /// Whether either effect changes a pixel -- the geometry pass skips all of it when not.
    pub fn is_noop(&self) -> bool {
        !self.vignette_active() && !self.grain_active()
    }

    /// The params clamped to their documented ranges, NaN scrubbed to the default (documents are
    /// untrusted, and the shader divides/pows by some of these).
    pub fn sanitized(&self) -> Self {
        let d = Self::default();
        let unit = |v: f32, lo: f32, hi: f32, fallback: f32| {
            if v.is_finite() {
                v.clamp(lo, hi)
            } else {
                fallback
            }
        };
        Self {
            vignette_amount: unit(self.vignette_amount, -1.0, 1.0, 0.0),
            vignette_midpoint: unit(self.vignette_midpoint, 0.0, 1.0, d.vignette_midpoint),
            vignette_feather: unit(self.vignette_feather, 0.0, 1.0, d.vignette_feather),
            vignette_roundness: unit(self.vignette_roundness, -1.0, 1.0, 0.0),
            vignette_highlights: unit(self.vignette_highlights, 0.0, 1.0, 0.0),
            vignette_style: self.vignette_style,
            grain_amount: unit(self.grain_amount, 0.0, 1.0, 0.0),
            grain_size: unit(self.grain_size, 0.0, 1.0, d.grain_size),
            grain_roughness: unit(self.grain_roughness, 0.0, 1.0, d.grain_roughness),
            grain_seed: self.grain_seed,
        }
    }
}

/// Tone Curve panel, parametric mode only (LRC's alternate freeform point-curve mode is not
/// modeled -- a documented v1 simplification). Each field is one of the four region sliders
/// (PV2012 `ToneCurvePV2012` range -100.0..=100.0, normalized to -1.0..=1.0 here), moving the
/// curve at a fixed split point: shadows at x=0.0, darks at x=0.25, lights at x=0.75, highlights
/// at x=1.0. LRC's own adjustable split-point sliders are not modeled -- the split points are
/// fixed at 0.25/0.75. 0.0 on every field is a no-op (a straight identity line).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ToneCurveParams {
    pub shadows: f32,
    pub darks: f32,
    pub lights: f32,
    pub highlights: f32,
}

impl ToneCurveParams {
    pub fn is_noop(&self) -> bool {
        self.shadows == 0.0 && self.darks == 0.0 && self.lights == 0.0 && self.highlights == 0.0
    }
}

/// One of the HSL panel's 8 hue bands (Red/Orange/Yellow/Green/Aqua/Blue/Purple/Magenta). Each
/// field is PV2012 range -100.0..=100.0, normalized to -1.0..=1.0 here. 0.0 on every field is a
/// no-op.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HslBand {
    pub hue: f32,
    pub saturation: f32,
    pub luminance: f32,
}

/// The HSL panel's 8 hue bands, in LRC's own fixed order: Red, Orange, Yellow, Green, Aqua, Blue,
/// Purple, Magenta -- each band's hue center is spaced 45 degrees apart starting at red (0 deg).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HslParams {
    pub bands: [HslBand; 8],
}

impl HslParams {
    /// True when every band is a no-op. Unlike [`SharpenParams::is_noop`]/
    /// [`NoiseReductionParams::is_noop`] (which gate a real fast path in
    /// `stages::LiveSuffixKernel::encode`), nothing currently calls this to skip work -- HSL is
    /// fused into the same per-pixel dispatch every other live stage shares, so there's no
    /// separate pass to skip. Kept as a public predicate for a future caller (e.g. a "this panel
    /// has edits" UI indicator).
    pub fn is_noop(&self) -> bool {
        self.bands
            .iter()
            .all(|b| b.hue == 0.0 && b.saturation == 0.0 && b.luminance == 0.0)
    }
}

/// Basic-panel Sharpening (LRC's Detail panel, but grouped with the other global stages here
/// since it's still a `serde(default)` no-op-at-zero live param like every other stage in this
/// module). PV2012 ranges: `amount` 0..=150 (normalized 0.0..=1.5), `radius_px` 0.5..=3.0 (kept in
/// pixels, not normalized -- a radius has no natural -1..1 center), `detail` 0..=100 (normalized
/// 0.0..=1.0). LRC's Masking slider (edge-only sharpening via a mask) is not modeled -- a
/// documented v1 simplification; `detail` alone already damps sharpening in flat regions via the
/// edge weight in [`crate::detail::apply_detail`]. `amount` of 0.0 is a no-op regardless of the
/// other two fields.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SharpenParams {
    pub amount: f32,
    pub radius_px: f32,
    pub detail: f32,
}

impl Default for SharpenParams {
    fn default() -> Self {
        Self {
            amount: 0.0,
            radius_px: 1.0,
            detail: 0.5,
        }
    }
}

impl SharpenParams {
    pub fn is_noop(&self) -> bool {
        self.amount == 0.0
    }
}

/// Detail panel's classic (non-AI) manual Noise Reduction -- distinct from the AI/SCUNet baked
/// denoise stage (#40), which this ticket doesn't touch. PV2012 ranges: `luminance`/`color`
/// 0..=100 (normalized 0.0..=1.0, amount of blur-blend), `detail` 0..=100 (normalized 0.0..=1.0,
/// edge-preservation strength shared between the luminance and color passes -- LRC's own
/// Luminance Detail and Color Detail sliders are collapsed into this one field, a documented v1
/// simplification). Every field at 0.0 is a no-op.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NoiseReductionParams {
    pub luminance: f32,
    pub color: f32,
    pub detail: f32,
}

impl NoiseReductionParams {
    pub fn is_noop(&self) -> bool {
        self.luminance == 0.0 && self.color == 0.0
    }
}

/// Crop + straighten (#47): `x`/`y`/`width`/`height` are the crop rectangle in *source-image*
/// pixel space (top-left origin). `width`/`height` of `0.0` (the default, via `Default`) is a
/// sentinel meaning "no crop yet -- use the full source frame": [`Self::effective_rect`] resolves
/// that sentinel against the actual source extent, so `Default` stays a true no-op like every
/// other coat params struct, rather than a degenerate zero-size rect. `rotation_degrees` is the
/// manual straighten angle (see `geometry::Affine2D::crop_and_rotate`'s own doc comment for the
/// clockwise-positive/y-down sign convention), populated either by the Ctrl-drag-a-reference-line
/// gesture or the Canny/Hough auto-level button -- both write into this same field, since they're
/// complementary entry points to the same underlying value, not alternates with separate storage.
/// Always clamped to `geometry::MAX_STRAIGHTEN_DEGREES` before being stored (`set_rotation`).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CropParams {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub rotation_degrees: f32,
}

impl CropParams {
    /// True when this is exactly the "no crop, no rotation" no-op state.
    pub fn is_noop(&self) -> bool {
        self.x == 0.0
            && self.y == 0.0
            && self.width <= 0.0
            && self.height <= 0.0
            && self.rotation_degrees == 0.0
    }

    /// Resolves this crop's rectangle against `source_extent` (in pixels): `width`/`height` <= 0.0
    /// (the `Default` sentinel) means "the full source frame," ignoring `x`/`y` too (a zero-size
    /// rect with a nonzero offset is not a meaningful crop, so the whole rect resets together, not
    /// just the size half of it).
    pub fn effective_rect(&self, source_extent: (f32, f32)) -> CropRect {
        if self.width <= 0.0 || self.height <= 0.0 {
            return CropRect {
                x: 0.0,
                y: 0.0,
                width: source_extent.0,
                height: source_extent.1,
            };
        }
        CropRect {
            x: self.x,
            y: self.y,
            width: self.width,
            height: self.height,
        }
    }

    /// Sets `rotation_degrees`, clamped to `geometry::MAX_STRAIGHTEN_DEGREES` -- the single
    /// writer both the straighten gesture and the auto-level button should call, so neither entry
    /// point can bypass the clamp by writing the field directly.
    pub fn set_rotation(&mut self, degrees: f32) {
        self.rotation_degrees = crate::geometry::clamp_rotation_degrees(degrees);
    }
}

/// What a heal [`Spot`] does to its destination circle (#51, ADR-0050).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpotKind {
    /// Feathered patch copy from `source_offset` (`heal::clone_stamp`).
    Clone,
    /// Gradient-domain (Poisson) blend from `source_offset` (`heal::spot_heal`).
    Heal,
    /// AI object removal: no `source_offset`; filled from a pre-inpainted patch produced from
    /// `mask_recipe` (a MobileSAM prompt, then LaMa) -- see [`HealParams`] and `heal::RemovalPatch`.
    Remove,
}

/// A model-based mask recipe (never derived pixels) for a [`SpotKind::Remove`] spot -- ADR-0021's
/// "the recipe, not the pixels" rule for AI masks. `params` carries the MobileSAM prompt (see
/// `nicti_groom::sam::Prompt`'s JSON shape); `model_version` pins the checkpoint so a model upgrade
/// is an explicit re-run, never a silent change to an old edit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MaskRecipe {
    pub model_id: String,
    pub model_version: String,
    pub params: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
}

/// One clone/heal/remove operation, in *source-image* pixel space (like [`CropParams`]).
/// `center`/`radius` describe the destination circle; `source_offset` (Clone/Heal only) is the
/// vector from `center` to the source patch's own center. Every `Option` field uses
/// `skip_serializing_if`, because `nicti_pawprint::hash_value` refuses JSON `null`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Spot {
    pub kind: SpotKind,
    pub center: (f32, f32),
    pub radius: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_offset: Option<(f32, f32)>,
    #[serde(default)]
    pub feather: f32,
    #[serde(default = "default_opacity")]
    pub opacity: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mask_recipe: Option<MaskRecipe>,
}

fn default_opacity() -> f32 {
    1.0
}

impl Spot {
    pub fn clone_spot(
        center: (f32, f32),
        radius: f32,
        source_offset: (f32, f32),
        feather: f32,
    ) -> Self {
        Self {
            kind: SpotKind::Clone,
            center,
            radius,
            source_offset: Some(source_offset),
            feather,
            opacity: 1.0,
            mask_recipe: None,
        }
    }

    pub fn heal_spot(
        center: (f32, f32),
        radius: f32,
        source_offset: (f32, f32),
        feather: f32,
    ) -> Self {
        Self {
            kind: SpotKind::Heal,
            center,
            radius,
            source_offset: Some(source_offset),
            feather,
            opacity: 1.0,
            mask_recipe: None,
        }
    }

    pub fn remove_spot(
        center: (f32, f32),
        radius: f32,
        feather: f32,
        mask_recipe: MaskRecipe,
    ) -> Self {
        Self {
            kind: SpotKind::Remove,
            center,
            radius,
            source_offset: None,
            feather,
            opacity: 1.0,
            mask_recipe: Some(mask_recipe),
        }
    }
}

/// Heal/remove stage (#51): an ordered list of spots, applied in list order -- order matters here
/// (two overlapping spots give a different result depending on which is applied last), unlike
/// ADR-0021's order-free `stages` map.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HealParams {
    pub spots: Vec<Spot>,
}

impl HealParams {
    pub fn is_noop(&self) -> bool {
        self.spots.is_empty()
    }
}

/// Parses a stage's raw JSON params into a typed struct, falling back to `T::default()` on any
/// deserialization failure (a schema this build genuinely can't parse) rather than propagating an
/// error -- consistent with `nicti_claw::Registry::get`'s own "no recognized module -> `None`,
/// never a hard failure" convention for an extension point.
/// Lens corrections (#428), the baked `nicti.lens` stage.
///
/// Both switches are *serialized only when they differ from the default*, so the default params
/// are `{}` -- exactly what the passthrough stage hashed before this stage was real. A photo whose
/// lens stage does nothing (a NEF with `remove_ca` off) therefore keeps every baked-cache key and,
/// through the keying-only `nicti.neutral` node, every on-disk AI alpha (#353) it had.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LensParams {
    /// Automatic lateral chromatic aberration removal (LRC's `AutoLateralCA`). The scale is
    /// estimated from the photo at bake time; skipped when the embedded profile already corrects
    /// per-channel warp.
    #[serde(skip_serializing_if = "is_false")]
    pub remove_ca: bool,
    /// Apply the DNG's own lens profile (`OpcodeList3` WarpRectilinear/FixVignetteRadial) when it
    /// carries one. On by default: that is how every DNG reader renders the file. A no-op for
    /// files without a profile.
    #[serde(skip_serializing_if = "is_true")]
    pub embedded_profile: bool,
}

fn is_false(v: &bool) -> bool {
    !*v
}

fn is_true(v: &bool) -> bool {
    *v
}

impl Default for LensParams {
    fn default() -> Self {
        Self {
            remove_ca: false,
            embedded_profile: true,
        }
    }
}

impl LensParams {
    /// True when the stage can do nothing *for any file*: used only to pick the cheap texture copy
    /// when the frame has no profile either (see `slit::LensExec`).
    pub fn is_noop(&self) -> bool {
        !self.remove_ca && !self.embedded_profile
    }
}

/// Global Defringe (#428): desaturates purple and green colour fringes at high-contrast edges.
///
/// Two independent channels, each with an amount and a hue window. Ranges are LRC's normalized:
/// amounts 0..20 -> 0..1, hue lo/hi 0..100 -> 0..1 (the hue window's ends within that fringe
/// colour's range, see `color::defringe_pixel`). Both amounts 0 is a no-op.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DefringeParams {
    pub purple_amount: f32,
    /// LRC default 30/100.
    pub purple_hue_lo: f32,
    /// LRC default 70/100.
    pub purple_hue_hi: f32,
    pub green_amount: f32,
    /// LRC default 40/100.
    pub green_hue_lo: f32,
    /// LRC default 60/100.
    pub green_hue_hi: f32,
}

impl Default for DefringeParams {
    fn default() -> Self {
        Self {
            purple_amount: 0.0,
            purple_hue_lo: 0.30,
            purple_hue_hi: 0.70,
            green_amount: 0.0,
            green_hue_lo: 0.40,
            green_hue_hi: 0.60,
        }
    }
}

impl DefringeParams {
    /// Whether the shader has anything to do -- it skips every neighbour read otherwise.
    pub fn is_noop(&self) -> bool {
        self.purple_amount <= 0.0 && self.green_amount <= 0.0
    }

    /// Clamped to the documented ranges with NaN scrubbed (documents are untrusted), and each
    /// window's high end kept at or above its low end.
    pub fn sanitized(&self) -> Self {
        let d = Self::default();
        let unit = |v: f32, fallback: f32| {
            if v.is_finite() {
                v.clamp(0.0, 1.0)
            } else {
                fallback
            }
        };
        let purple_hue_lo = unit(self.purple_hue_lo, d.purple_hue_lo);
        let green_hue_lo = unit(self.green_hue_lo, d.green_hue_lo);
        Self {
            purple_amount: unit(self.purple_amount, 0.0),
            purple_hue_lo,
            purple_hue_hi: unit(self.purple_hue_hi, d.purple_hue_hi).max(purple_hue_lo),
            green_amount: unit(self.green_amount, 0.0),
            green_hue_lo,
            green_hue_hi: unit(self.green_hue_hi, d.green_hue_hi).max(green_hue_lo),
        }
    }
}

/// One Color Grading wheel (#432). `hue` is degrees on the same HSV-style wheel the UI paints
/// (`0..360`), `sat` is `0..1` (LRC 0..100), `lum` is `-1..1` (LRC -100..100). Zero `sat` and `lum`
/// is a no-op whatever the hue.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GradeWheel {
    pub hue: f32,
    pub sat: f32,
    pub lum: f32,
}

/// Color Grading (#432): three tonal wheels plus a global one, evaluated in OkLab (see
/// `oklab.rs`). `blending` is `0..1` (LRC 0..100, default 50) and widens the overlap between the
/// tonal ranges; `balance` is `-1..1`: negative grows the shadow range, positive the highlight
/// range (LRC's Balance direction).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ColorGradeParams {
    pub shadows: GradeWheel,
    pub midtones: GradeWheel,
    pub highlights: GradeWheel,
    pub global: GradeWheel,
    pub blending: f32,
    pub balance: f32,
}

impl Default for ColorGradeParams {
    fn default() -> Self {
        Self {
            shadows: GradeWheel::default(),
            midtones: GradeWheel::default(),
            highlights: GradeWheel::default(),
            global: GradeWheel::default(),
            blending: 0.5,
            balance: 0.0,
        }
    }
}

impl ColorGradeParams {
    pub fn is_noop(&self) -> bool {
        [self.shadows, self.midtones, self.highlights, self.global]
            .iter()
            .all(|w| w.sat == 0.0 && w.lum == 0.0)
    }

    /// Clamped to the documented ranges with NaN scrubbed (documents are untrusted).
    pub fn sanitized(&self) -> Self {
        let clamp = |v: f32, lo: f32, hi: f32, fallback: f32| {
            if v.is_finite() {
                v.clamp(lo, hi)
            } else {
                fallback
            }
        };
        let wheel = |w: GradeWheel| GradeWheel {
            hue: clamp(w.hue, 0.0, 360.0, 0.0),
            sat: clamp(w.sat, 0.0, 1.0, 0.0),
            lum: clamp(w.lum, -1.0, 1.0, 0.0),
        };
        Self {
            shadows: wheel(self.shadows),
            midtones: wheel(self.midtones),
            highlights: wheel(self.highlights),
            global: wheel(self.global),
            blending: clamp(self.blending, 0.0, 1.0, 0.5),
            balance: clamp(self.balance, -1.0, 1.0, 0.0),
        }
    }
}

/// Most Point Color samples one photo can carry.
pub const MAX_POINT_COLORS: usize = 8;

/// One Point Color sample (#432): a colour picked from the photo (in OkLCh: `lum` is OkLab L
/// `0..1`, `chroma` `0..~0.37`, `hue` degrees) plus how far to move colours near it and how wide
/// "near" is. Shifts and `variance` are `-1..1`; the four ranges are `0..1` with 0.5 the default
/// (LRC's 50).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PointColorSample {
    pub lum: f32,
    pub chroma: f32,
    pub hue: f32,
    pub hue_shift: f32,
    pub sat_shift: f32,
    pub lum_shift: f32,
    pub variance: f32,
    pub range: f32,
    pub hue_range: f32,
    pub sat_range: f32,
    pub lum_range: f32,
}

impl Default for PointColorSample {
    fn default() -> Self {
        Self {
            lum: 0.5,
            chroma: 0.0,
            hue: 0.0,
            hue_shift: 0.0,
            sat_shift: 0.0,
            lum_shift: 0.0,
            variance: 0.0,
            range: 0.5,
            hue_range: 0.5,
            sat_range: 0.5,
            lum_range: 0.5,
        }
    }
}

impl PointColorSample {
    /// True when moving colours near this sample changes nothing.
    pub fn is_noop(&self) -> bool {
        self.hue_shift == 0.0
            && self.sat_shift == 0.0
            && self.lum_shift == 0.0
            && self.variance == 0.0
    }

    pub fn sanitized(&self) -> Self {
        let clamp = |v: f32, lo: f32, hi: f32, fallback: f32| {
            if v.is_finite() {
                v.clamp(lo, hi)
            } else {
                fallback
            }
        };
        Self {
            lum: clamp(self.lum, 0.0, 1.0, 0.5),
            chroma: clamp(self.chroma, 0.0, 0.5, 0.0),
            hue: clamp(self.hue, 0.0, 360.0, 0.0),
            hue_shift: clamp(self.hue_shift, -1.0, 1.0, 0.0),
            sat_shift: clamp(self.sat_shift, -1.0, 1.0, 0.0),
            lum_shift: clamp(self.lum_shift, -1.0, 1.0, 0.0),
            variance: clamp(self.variance, -1.0, 1.0, 0.0),
            range: clamp(self.range, 0.0, 1.0, 0.5),
            hue_range: clamp(self.hue_range, 0.0, 1.0, 0.5),
            sat_range: clamp(self.sat_range, 0.0, 1.0, 0.5),
            lum_range: clamp(self.lum_range, 0.0, 1.0, 0.5),
        }
    }
}

/// Point Color (#432): up to [`MAX_POINT_COLORS`] samples, applied in OkLCh after Color Grading.
/// Fixed-size so the params stay `Copy`; only the first `count` samples are live.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PointColorParams {
    pub count: u8,
    pub samples: [PointColorSample; MAX_POINT_COLORS],
}

impl PointColorParams {
    pub fn is_noop(&self) -> bool {
        self.live().all(|s| s.is_noop())
    }

    /// The live samples (the first `count`, capped).
    pub fn live(&self) -> impl Iterator<Item = &PointColorSample> {
        self.samples
            .iter()
            .take(usize::from(self.count).min(MAX_POINT_COLORS))
    }

    pub fn sanitized(&self) -> Self {
        let count = self.count.min(MAX_POINT_COLORS as u8);
        let mut samples = [PointColorSample::default(); MAX_POINT_COLORS];
        for (dst, src) in samples
            .iter_mut()
            .zip(&self.samples)
            .take(usize::from(count))
        {
            *dst = src.sanitized();
        }
        Self { count, samples }
    }
}

/// Maximum control points per point-curve channel; extra points in an untrusted document are
/// dropped by [`PointCurveParams::sanitized`].
pub const MAX_CURVE_POINTS: usize = 32;

/// Freeform point curves (#432): a master RGB curve plus one per channel, each a list of (x, y)
/// control points in 0..1 (LRC's 0..255 `ToneCurvePV2012*` pairs divided by 255). An empty list is
/// the identity. Applied right after the parametric [`ToneCurveParams`] in the same cube-root
/// perceptual space; the master curve runs first, then the channel's own curve.
///
/// A separate stage from `nicti.tone_curve` so that stage stays `Copy` and its cache keys are
/// unchanged. Serializes to `{}` at its defaults (every list empty).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PointCurveParams {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub master: Vec<[f32; 2]>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub red: Vec<[f32; 2]>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub green: Vec<[f32; 2]>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub blue: Vec<[f32; 2]>,
}

/// One channel's points, cleaned: finite, clamped to 0..1, sorted by x, strictly increasing in x
/// (a later point within 1e-4 of the previous x is dropped), capped at [`MAX_CURVE_POINTS`]. Fewer
/// than two survivors, or exactly the identity diagonal, becomes empty (no-op).
fn sanitize_curve(points: &[[f32; 2]]) -> Vec<[f32; 2]> {
    let mut pts: Vec<[f32; 2]> = points
        .iter()
        .filter(|p| p[0].is_finite() && p[1].is_finite())
        .map(|p| [p[0].clamp(0.0, 1.0), p[1].clamp(0.0, 1.0)])
        .collect();
    pts.sort_by(|a, b| a[0].total_cmp(&b[0]));
    let mut out: Vec<[f32; 2]> = Vec::with_capacity(pts.len());
    for p in pts {
        if out.last().is_none_or(|l| p[0] - l[0] > 1e-4) {
            out.push(p);
        }
    }
    if out.len() > MAX_CURVE_POINTS {
        // Keep the right-hand endpoint: dropping it would flat-extend the curve from mid-graph.
        let last = out[out.len() - 1];
        out.truncate(MAX_CURVE_POINTS - 1);
        out.push(last);
    }
    if out.len() < 2 || out.iter().all(|p| (p[0] - p[1]).abs() < 1e-6) {
        out.clear();
    }
    out
}

impl PointCurveParams {
    pub fn is_noop(&self) -> bool {
        let s = self.sanitized();
        s.master.is_empty() && s.red.is_empty() && s.green.is_empty() && s.blue.is_empty()
    }

    /// Every channel cleaned with [`sanitize_curve`] (documents are untrusted).
    pub fn sanitized(&self) -> Self {
        Self {
            master: sanitize_curve(&self.master),
            red: sanitize_curve(&self.red),
            green: sanitize_curve(&self.green),
            blue: sanitize_curve(&self.blue),
        }
    }
}

pub fn parse<T: serde::de::DeserializeOwned + Default>(params: &Value) -> T {
    serde_json::from_value(params.clone()).unwrap_or_default()
}

/// The JSON `Value` a fresh `T::default()` serializes to -- what a `RenderStage::default_params`
/// closure returns for a stage backed by one of this module's typed structs.
pub fn default_value<T: Serialize + Default>() -> Value {
    serde_json::to_value(T::default()).expect("a coat params struct always serializes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn point_curve_defaults_serialize_to_nothing_and_sanitize_cleans_untrusted_points() {
        assert_eq!(
            serde_json::to_value(PointCurveParams::default()).unwrap(),
            serde_json::json!({})
        );
        assert!(PointCurveParams::default().is_noop());
        // The identity diagonal, a single point and an all-NaN list are all no-ops.
        for pts in [
            vec![[0.0, 0.0], [1.0, 1.0]],
            vec![[0.5, 0.7]],
            vec![[f32::NAN, 0.2], [0.4, f32::INFINITY]],
        ] {
            let p = PointCurveParams {
                master: pts,
                ..Default::default()
            };
            assert!(p.is_noop());
        }
        let s = PointCurveParams {
            red: vec![[0.9, 2.0], [-1.0, -0.5], [0.9, 0.1], [0.5, 0.6]],
            ..Default::default()
        }
        .sanitized();
        // Sorted, clamped, the duplicate x at 0.9 dropped.
        assert_eq!(s.red, vec![[0.0, 0.0], [0.5, 0.6], [0.9, 1.0]]);
        let many: Vec<[f32; 2]> = (0..100)
            .map(|i| [i as f32 / 99.0, (i as f32 / 99.0).powi(2)])
            .collect();
        let capped = PointCurveParams {
            blue: many,
            ..Default::default()
        }
        .sanitized();
        assert_eq!(capped.blue.len(), MAX_CURVE_POINTS);
        assert_eq!(
            capped.blue.last(),
            Some(&[1.0, 1.0]),
            "the x = 1 endpoint survives"
        );
    }

    #[test]
    fn defringe_sanitize_clamps_scrubs_nan_and_orders_the_windows() {
        let s = DefringeParams {
            purple_amount: f32::NAN,
            purple_hue_lo: 0.9,
            purple_hue_hi: 0.2,
            green_amount: 7.0,
            green_hue_lo: -3.0,
            green_hue_hi: f32::INFINITY,
        }
        .sanitized();
        assert_eq!(s.purple_amount, 0.0);
        assert!(s.purple_hue_hi >= s.purple_hue_lo);
        assert_eq!(s.green_amount, 1.0);
        assert_eq!(s.green_hue_lo, 0.0);
        assert_eq!(s.green_hue_hi, DefringeParams::default().green_hue_hi);
        assert!(DefringeParams::default().is_noop());
        assert!(!DefringeParams {
            green_amount: 0.1,
            ..Default::default()
        }
        .is_noop());
    }

    #[test]
    fn default_lens_params_are_the_historical_empty_params() {
        // Keeps every existing document's lens hash (and so its baked-cache and AI-alpha keys).
        assert_eq!(default_value::<LensParams>(), serde_json::json!({}));
        assert_eq!(
            parse::<LensParams>(&serde_json::json!({})),
            LensParams::default()
        );
        assert!(LensParams::default().embedded_profile && !LensParams::default().remove_ca);
    }

    #[test]
    fn lens_params_round_trip_each_non_default_switch() {
        for p in [
            LensParams {
                remove_ca: true,
                embedded_profile: true,
            },
            LensParams {
                remove_ca: false,
                embedded_profile: false,
            },
            LensParams {
                remove_ca: true,
                embedded_profile: false,
            },
        ] {
            let v = serde_json::to_value(p).unwrap();
            assert_ne!(
                v,
                serde_json::json!({}),
                "a non-default must change the hash input"
            );
            assert_eq!(parse::<LensParams>(&v), p);
        }
        // Garbage degrades to the default instead of erroring.
        assert_eq!(
            parse::<LensParams>(&serde_json::json!({ "remove_ca": "yes" })),
            LensParams::default()
        );
    }

    #[test]
    fn no_camera_profile_serializes_to_the_historical_empty_params() {
        // Keeps existing documents' `nicti.working_space` hash (and so every cache key) unchanged.
        assert_eq!(
            default_value::<CameraProfileParams>(),
            serde_json::json!({})
        );
    }

    #[test]
    fn parse_falls_back_to_default_on_missing_entry() {
        let parsed: ToneParams = parse(&serde_json::json!({}));
        assert_eq!(parsed, ToneParams::default());
    }

    #[test]
    fn parse_ignores_unrecognized_fields() {
        let parsed: VibranceParams = parse(&serde_json::json!({"amount": 0.4, "future_field": 1}));
        assert_eq!(parsed, VibranceParams { amount: 0.4 });
    }

    #[test]
    fn effects_params_default_is_noop_and_round_trips_through_parse() {
        let e = EffectsParams::default();
        assert!(e.is_noop() && !e.vignette_active() && !e.grain_active());
        let value = serde_json::to_value(e).unwrap();
        assert_eq!(parse::<EffectsParams>(&value), e);
        // An older document with no entry, or a newer one with extra fields, parses to the default.
        assert_eq!(parse::<EffectsParams>(&serde_json::json!({})), e);
        let styled: EffectsParams = parse(&serde_json::json!({
            "vignette_amount": -0.4, "vignette_style": "paint_overlay", "future": 1
        }));
        assert_eq!(styled.vignette_style, VignetteStyle::PaintOverlay);
        assert!(styled.vignette_active() && !styled.is_noop());
    }

    #[test]
    fn effects_params_sanitize_clamps_and_scrubs_non_finite_values() {
        let hostile = EffectsParams {
            vignette_amount: f32::NAN,
            vignette_midpoint: 7.0,
            vignette_feather: -3.0,
            vignette_roundness: f32::INFINITY,
            grain_amount: 9.0,
            grain_size: f32::NAN,
            ..EffectsParams::default()
        }
        .sanitized();
        assert_eq!(hostile.vignette_amount, 0.0);
        assert_eq!(hostile.vignette_midpoint, 1.0);
        assert_eq!(hostile.vignette_feather, 0.0);
        assert_eq!(hostile.vignette_roundness, 0.0);
        assert_eq!(hostile.grain_amount, 1.0);
        assert_eq!(hostile.grain_size, EffectsParams::default().grain_size);
    }

    #[test]
    fn presence_params_default_is_noop_and_needs_no_bases() {
        let p = PresenceParams::default();
        assert!(p.is_noop() && !p.needs_bases());
        // Saturation is per-pixel: it never needs a spatial base.
        let sat = PresenceParams {
            saturation: 0.5,
            ..Default::default()
        };
        assert!(!sat.is_noop() && !sat.needs_bases());
        let clarity = PresenceParams {
            clarity: 0.1,
            ..Default::default()
        };
        assert!(clarity.needs_bands() && !clarity.needs_haze());
        let dehaze = PresenceParams {
            dehaze: -0.1,
            ..Default::default()
        };
        assert!(dehaze.needs_haze() && !dehaze.needs_bands());
    }

    #[test]
    fn presence_params_sanitize_clamps_and_scrubs() {
        let p = PresenceParams {
            texture: f32::NAN,
            clarity: 1e30,
            dehaze: -5.0,
            saturation: f32::INFINITY,
        }
        .sanitized();
        assert_eq!(
            (p.texture, p.clarity, p.dehaze, p.saturation),
            (0.0, 1.0, -1.0, 0.0)
        );
    }

    #[test]
    fn parse_falls_back_to_default_on_wrong_shape() {
        let parsed: ExposureParams = parse(&serde_json::json!({"stops": "not a number"}));
        assert_eq!(parsed, ExposureParams::default());
    }

    #[test]
    fn wb_params_default_is_as_shot_with_no_tint() {
        let parsed: WbParams = parse(&serde_json::json!({}));
        assert_eq!(
            parsed,
            WbParams {
                temp_k: None,
                tint: 0.0
            }
        );
    }

    #[test]
    fn wb_params_round_trips_an_explicit_temp() {
        let value = serde_json::json!({"temp_k": 4200.0, "tint": -12.5});
        let parsed: WbParams = parse(&value);
        assert_eq!(
            parsed,
            WbParams {
                temp_k: Some(4200.0),
                tint: -12.5
            }
        );
    }

    #[test]
    fn default_value_round_trips_through_parse() {
        let value = default_value::<ToneParams>();
        let parsed: ToneParams = parse(&value);
        assert_eq!(parsed, ToneParams::default());
    }

    #[test]
    fn tone_curve_params_default_is_noop() {
        assert!(ToneCurveParams::default().is_noop());
        let nonzero = ToneCurveParams {
            darks: 0.1,
            ..Default::default()
        };
        assert!(!nonzero.is_noop());
    }

    #[test]
    fn hsl_params_default_is_eight_noop_bands() {
        let params = HslParams::default();
        assert_eq!(params.bands.len(), 8);
        assert!(params.is_noop());
        let mut nonzero = params;
        nonzero.bands[3].saturation = 0.2;
        assert!(!nonzero.is_noop());
    }

    #[test]
    fn sharpen_params_default_has_zero_amount_but_nonzero_radius_and_detail() {
        let params = SharpenParams::default();
        assert!(params.is_noop());
        assert_eq!(params.amount, 0.0);
        assert!(params.radius_px > 0.0);
        assert!(params.detail > 0.0);
    }

    #[test]
    fn noise_reduction_params_default_is_noop() {
        assert!(NoiseReductionParams::default().is_noop());
        let nonzero = NoiseReductionParams {
            luminance: 0.3,
            ..Default::default()
        };
        assert!(!nonzero.is_noop());
    }

    #[test]
    fn crop_params_default_is_noop() {
        assert!(CropParams::default().is_noop());
        let nonzero = CropParams {
            rotation_degrees: 1.0,
            ..Default::default()
        };
        assert!(!nonzero.is_noop());
    }

    #[test]
    fn crop_params_default_effective_rect_is_the_full_source_frame() {
        let rect = CropParams::default().effective_rect((100.0, 50.0));
        assert_eq!(
            rect,
            crate::geometry::CropRect {
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 50.0
            }
        );
    }

    #[test]
    fn crop_params_effective_rect_uses_the_explicit_rect_when_set() {
        let params = CropParams {
            x: 5.0,
            y: 6.0,
            width: 20.0,
            height: 10.0,
            rotation_degrees: 0.0,
        };
        let rect = params.effective_rect((100.0, 50.0));
        assert_eq!(
            rect,
            crate::geometry::CropRect {
                x: 5.0,
                y: 6.0,
                width: 20.0,
                height: 10.0
            }
        );
    }

    #[test]
    fn crop_params_set_rotation_clamps_to_max_straighten_range() {
        let mut params = CropParams::default();
        params.set_rotation(9000.0);
        assert_eq!(
            params.rotation_degrees,
            crate::geometry::MAX_STRAIGHTEN_DEGREES
        );
    }

    #[test]
    fn crop_params_round_trips_through_parse() {
        let params = CropParams {
            x: 1.0,
            y: 2.0,
            width: 30.0,
            height: 40.0,
            rotation_degrees: 3.5,
        };
        let value = serde_json::to_value(params).unwrap();
        let parsed: CropParams = parse(&value);
        assert_eq!(parsed, params);
    }

    #[test]
    fn hsl_params_round_trips_through_parse() {
        let mut params = HslParams::default();
        params.bands[0] = HslBand {
            hue: 10.0,
            saturation: -20.0,
            luminance: 5.0,
        };
        let value = serde_json::to_value(params).unwrap();
        let parsed: HslParams = parse(&value);
        assert_eq!(parsed, params);
    }
}
