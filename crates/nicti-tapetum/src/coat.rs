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
}
