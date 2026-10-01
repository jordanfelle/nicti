//! Basic panel, white balance, parametric tone curve (the keys `nicti-tapetum`'s `coat.rs` has a
//! stage for). Anything else on those panels (Clarity/Texture/Dehaze/global Saturation, point
//! curves, the curve split points when non-default) has no nicti global stage yet and stays
//! untranslated -- see the follow-up issues filed with #62.

use nicti_tapetum::coat::{ExposureParams, ToneCurveParams, ToneParams, VibranceParams, WbParams};
use nicti_tapetum::stages::{EXPOSURE, TONE, TONE_CURVE, VIBRANCE, WB};

use super::Tx;

/// PV2012 slider (-100..100) to nicti's -1..1.
fn unit(v: f64) -> f32 {
    (v / 100.0).clamp(-1.0, 1.0) as f32
}

pub(super) fn apply(tx: &mut Tx) {
    let exposure = tx.num("Exposure2012").unwrap_or(0.0);
    tx.put(
        EXPOSURE,
        ExposureParams {
            stops: exposure.clamp(-5.0, 5.0) as f32,
        },
    );

    let tone = ToneParams {
        contrast: unit(tx.num("Contrast2012").unwrap_or(0.0)),
        highlights: unit(tx.num("Highlights2012").unwrap_or(0.0)),
        shadows: unit(tx.num("Shadows2012").unwrap_or(0.0)),
        whites: unit(tx.num("Whites2012").unwrap_or(0.0)),
        blacks: unit(tx.num("Blacks2012").unwrap_or(0.0)),
    };
    tx.put(TONE, tone);

    let vibrance = VibranceParams {
        amount: unit(tx.num("Vibrance").unwrap_or(0.0)),
    };
    tx.put(VIBRANCE, vibrance);

    white_balance(tx);
    tone_curve(tx);
}

/// `WhiteBalance = "As Shot"` means nicti's own as-shot default (`temp_k: None`): LRC's
/// `Temperature`/`Tint` then hold the *as-shot* values, which nicti derives itself -- importing them
/// as an override would freeze a different estimate. Any other `WhiteBalance` (Custom, Daylight,
/// ...) is an explicit override.
fn white_balance(tx: &mut Tx) {
    let balance = tx.text("WhiteBalance");
    let temperature = tx.num("Temperature");
    let tint = tx.num("Tint");
    let as_shot = balance
        .as_deref()
        .is_none_or(|b| b.eq_ignore_ascii_case("as shot"));
    if as_shot {
        return;
    }
    if let Some(k) = temperature {
        tx.put(
            WB,
            WbParams {
                temp_k: Some(k.clamp(2000.0, 50000.0) as f32),
                tint: tint.unwrap_or(0.0).clamp(-150.0, 150.0) as f32,
            },
        );
    }
}

/// Parametric mode only: the four region sliders. nicti's split points are fixed at 25/50/75
/// (`ToneCurveParams`), so a catalog that moved them is counted, not silently flattened.
fn tone_curve(tx: &mut Tx) {
    let curve = ToneCurveParams {
        shadows: unit(tx.num("ParametricShadows").unwrap_or(0.0)),
        darks: unit(tx.num("ParametricDarks").unwrap_or(0.0)),
        lights: unit(tx.num("ParametricLights").unwrap_or(0.0)),
        highlights: unit(tx.num("ParametricHighlights").unwrap_or(0.0)),
    };
    let shadow_split = tx.num("ParametricShadowSplit");
    let mid_split = tx.num("ParametricMidtoneSplit");
    let high_split = tx.num("ParametricHighlightSplit");
    let moved = [(shadow_split, 25.0), (mid_split, 50.0), (high_split, 75.0)]
        .iter()
        .any(|(v, default)| v.is_some_and(|v| (v - default).abs() > 0.5));
    if moved && !curve.is_noop() {
        tx.stats.tone_curve_splits_ignored += 1;
    }
    tx.put(TONE_CURVE, curve);
}

#[cfg(test)]
mod tests {
    use crate::develop::{translate, Context};
    use nicti_tapetum::stages::*;

    fn tr(text: &str) -> crate::develop::Translation {
        translate(text, &Context::default()).unwrap()
    }

    #[test]
    fn basic_sliders_map_with_the_right_scale() {
        let t = tr(
            "s = { Exposure2012 = 0.75, Contrast2012 = 25, Highlights2012 = -50, \
                    Shadows2012 = 100, Whites2012 = -10, Blacks2012 = 5, Vibrance = 40 }",
        );
        let stages = &t.document.stages;
        assert_eq!(stages[EXPOSURE].params["stops"], 0.75);
        assert_eq!(stages[TONE].params["contrast"], 0.25);
        assert_eq!(stages[TONE].params["highlights"], -0.5);
        assert_eq!(stages[TONE].params["shadows"], 1.0);
        assert!(crate::develop::close(
            &stages[VIBRANCE].params["amount"],
            0.4
        ));
        assert!(t.untranslated.is_empty());
    }

    #[test]
    fn all_default_values_write_no_stages() {
        let t = tr("s = { Exposure2012 = 0, Contrast2012 = 0, Vibrance = 0, Saturation = 0 }");
        assert!(t.document.stages.is_empty());
        assert!(t.untranslated.is_empty());
    }

    #[test]
    fn as_shot_white_balance_is_left_to_nicti_but_custom_is_imported() {
        let t = tr(r#"s = { WhiteBalance = "As Shot", Temperature = 5200, Tint = 12 }"#);
        assert!(!t.document.stages.contains_key(WB));
        let t = tr(r#"s = { WhiteBalance = "Custom", Temperature = 4300, Tint = -8 }"#);
        assert_eq!(t.document.stages[WB].params["temp_k"], 4300.0);
        assert_eq!(t.document.stages[WB].params["tint"], -8.0);
    }

    #[test]
    fn parametric_curve_maps_and_flags_moved_split_points() {
        let t = tr("s = { ParametricShadows = -20, ParametricHighlights = 30, \
                    ParametricShadowSplit = 25, ParametricMidtoneSplit = 50, \
                    ParametricHighlightSplit = 75 }");
        assert!(crate::develop::close(
            &t.document.stages[TONE_CURVE].params["shadows"],
            -0.2
        ));
        assert_eq!(t.stats.tone_curve_splits_ignored, 0);
        let t = tr("s = { ParametricShadows = -20, ParametricMidtoneSplit = 40 }");
        assert_eq!(t.stats.tone_curve_splits_ignored, 1);
    }

    #[test]
    fn untranslated_lists_only_real_values_and_not_consumed_keys() {
        let t = tr(
            r#"s = { Clarity2012 = 12, Texture = 0, Dehaze = 5, Saturation = 0,
                    ToneCurvePV2012 = { 0, 0, 64, 70, 255, 255, }, Exposure2012 = 1 }"#,
        );
        assert_eq!(
            t.untranslated,
            vec!["Clarity2012", "Dehaze", "ToneCurvePV2012"]
        );
    }
}
