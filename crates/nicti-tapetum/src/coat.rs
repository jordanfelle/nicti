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
    /// True when every band is a no-op -- lets a caller skip the HSL pass entirely rather than
    /// paying for an identity transform.
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

/// Parses a stage's raw JSON params into a typed struct, falling back to `T::default()` on any
/// deserialization failure (a schema this build genuinely can't parse) rather than propagating an
/// error -- consistent with `nicti_claw::Registry::get`'s own "no recognized module -> `None`,
/// never a hard failure" convention for an extension point.
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
