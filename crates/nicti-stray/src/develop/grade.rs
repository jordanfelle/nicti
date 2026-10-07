//! Freeform point curves (`ToneCurvePV2012*`) and Color Grading (`ColorGrade*`/`SplitToning*`)
//! (#432), onto `nicti.point_curve` and `nicti.color_grade`.
//!
//! LRC stores a point curve as a flat Lua list of `x, y` pairs in 0..255 (`{ 0, 0, 64, 70, 255, 255 }`,
//! master then the Red/Green/Blue variants). Color Grading keeps its Shadows and Highlights hue and
//! saturation under the older `SplitToning*` keys (shared with the legacy Split Toning panel), the
//! Midtones and Global wheels under `ColorGrade*`, every wheel's luminance under
//! `ColorGrade<Wheel>Lum`, and one shared `SplitToningBalance`/`ColorGradeBlending`.
//!
//! Values are mapped numerically only: nicti's OkLab grading maths is tuned by eye, so a graded
//! photo will not match LRC's render until the reference-machine parity pass (see the follow-up
//! issue filed with #432) fits it.

use agprefs::Value;
use nicti_tapetum::coat::{ColorGradeParams, GradeWheel, PointCurveParams};
use nicti_tapetum::stages::{COLOR_GRADE, POINT_CURVE};

use super::{items, number, Tx};

pub(super) fn apply(tx: &mut Tx) {
    point_curves(tx);
    color_grading(tx);
}

/// A curve's `x, y` pairs, 0..255 -> 0..1. An odd or too-short list yields nothing.
fn pairs(v: &Value<'_>) -> Vec<[f32; 2]> {
    let nums: Vec<f64> = items(v).into_iter().filter_map(number).collect();
    if nums.len() < 4 || !nums.len().is_multiple_of(2) {
        return Vec::new();
    }
    nums.as_chunks::<2>()
        .0
        .iter()
        .map(|p| [(p[0] / 255.0) as f32, (p[1] / 255.0) as f32])
        .collect()
}

fn point_curves(tx: &mut Tx) {
    let read = |tx: &mut Tx, key: &str| tx.take(key).map(pairs).unwrap_or_default();
    let params = PointCurveParams {
        master: read(tx, "ToneCurvePV2012"),
        red: read(tx, "ToneCurvePV2012Red"),
        green: read(tx, "ToneCurvePV2012Green"),
        blue: read(tx, "ToneCurvePV2012Blue"),
    }
    // An identity curve is LRC's untouched state: sanitizing turns it into "nothing".
    .sanitized();
    tx.put(POINT_CURVE, params);
}

fn color_grading(tx: &mut Tx) {
    // Read (and so consume) every key even when the panel is switched off.
    let enabled = tx.enabled("EnableSplitToning");
    let num = |tx: &mut Tx, key: &str| tx.num(key).unwrap_or(0.0);
    let sat = |v: f64| (v / 100.0).clamp(0.0, 1.0) as f32;
    let lum = |v: f64| (v / 100.0).clamp(-1.0, 1.0) as f32;
    let hue = |v: f64| v.rem_euclid(360.0) as f32;
    let shadows = GradeWheel {
        hue: hue(num(tx, "SplitToningShadowHue")),
        sat: sat(num(tx, "SplitToningShadowSaturation")),
        lum: lum(num(tx, "ColorGradeShadowLum")),
    };
    let midtones = GradeWheel {
        hue: hue(num(tx, "ColorGradeMidtoneHue")),
        sat: sat(num(tx, "ColorGradeMidtoneSat")),
        lum: lum(num(tx, "ColorGradeMidtoneLum")),
    };
    let highlights = GradeWheel {
        hue: hue(num(tx, "SplitToningHighlightHue")),
        sat: sat(num(tx, "SplitToningHighlightSaturation")),
        lum: lum(num(tx, "ColorGradeHighlightLum")),
    };
    let global = GradeWheel {
        hue: hue(num(tx, "ColorGradeGlobalHue")),
        sat: sat(num(tx, "ColorGradeGlobalSat")),
        lum: lum(num(tx, "ColorGradeGlobalLum")),
    };
    // Absent blending is LRC's own default of 50, i.e. nicti's 0.5.
    let blending = tx
        .num("ColorGradeBlending")
        .map_or(0.5, |v| (v / 100.0).clamp(0.0, 1.0) as f32);
    let balance = lum(num(tx, "SplitToningBalance"));
    let params = ColorGradeParams {
        shadows,
        midtones,
        highlights,
        global,
        blending,
        balance,
    };
    // A wheel with no saturation and no luminance does nothing whatever its hue, so a catalog that
    // only carries a stray hue (LRC keeps the last hue after the saturation is zeroed) is no edit.
    if enabled && !params.is_noop() {
        tx.put(COLOR_GRADE, params);
    }
}

#[cfg(test)]
mod tests {
    use crate::develop::{translate, Context};
    use nicti_tapetum::coat::{ColorGradeParams, PointCurveParams};
    use nicti_tapetum::stages::{COLOR_GRADE, POINT_CURVE};

    fn tr(text: &str) -> crate::develop::Translation {
        translate(text, &Context::default()).unwrap()
    }

    #[test]
    fn point_curves_map_per_channel_from_0_255_pairs() {
        let t = tr(
            "s = { ToneCurvePV2012 = { 0, 0, 64, 128, 255, 255, }, \
             ToneCurvePV2012Red = { 0, 25, 255, 230, }, ToneCurvePV2012Blue = { 0, 0, 255, 255, } }",
        );
        let curves: PointCurveParams =
            serde_json::from_value(t.document.stages[POINT_CURVE].params.clone()).unwrap();
        assert_eq!(curves.master.len(), 3);
        assert!((curves.master[1][0] - 64.0 / 255.0).abs() < 1e-6);
        assert!((curves.master[1][1] - 128.0 / 255.0).abs() < 1e-6);
        assert_eq!(curves.red.len(), 2);
        assert!(curves.green.is_empty());
        assert!(curves.blue.is_empty(), "the blue curve is the identity");
        assert!(t.untranslated.is_empty(), "{:?}", t.untranslated);
    }

    #[test]
    fn identity_and_malformed_curves_write_nothing() {
        let t =
            tr("s = { ToneCurvePV2012 = { 0, 0, 255, 255, }, ToneCurvePV2012Red = { 1, 2, 3, } }");
        assert!(!t.document.stages.contains_key(POINT_CURVE));
    }

    #[test]
    fn color_grading_reads_split_toning_for_shadows_and_highlights() {
        let t = tr(
            "s = { SplitToningShadowHue = 220, SplitToningShadowSaturation = 40, \
             ColorGradeShadowLum = -10, ColorGradeMidtoneHue = 30, ColorGradeMidtoneSat = 20, \
             SplitToningHighlightHue = 50, SplitToningHighlightSaturation = 30, \
             ColorGradeGlobalHue = 300, ColorGradeGlobalSat = 5, ColorGradeBlending = 70, \
             SplitToningBalance = -25 }",
        );
        let g: ColorGradeParams =
            serde_json::from_value(t.document.stages[COLOR_GRADE].params.clone()).unwrap();
        assert_eq!(g.shadows.hue, 220.0);
        assert!((g.shadows.sat - 0.4).abs() < 1e-6 && (g.shadows.lum + 0.1).abs() < 1e-6);
        assert_eq!((g.midtones.hue, g.midtones.sat), (30.0, 0.2));
        assert!((g.highlights.sat - 0.3).abs() < 1e-6);
        assert!((g.global.sat - 0.05).abs() < 1e-6);
        assert!((g.blending - 0.7).abs() < 1e-6 && (g.balance + 0.25).abs() < 1e-6);
        assert!(t.untranslated.is_empty(), "{:?}", t.untranslated);
    }

    #[test]
    fn a_stray_hue_with_no_saturation_or_luminance_is_not_an_edit() {
        let t = tr("s = { SplitToningShadowHue = 220, ColorGradeBlending = 50 }");
        assert!(!t.document.stages.contains_key(COLOR_GRADE));
        // Both keys were read, so neither shows up as untranslated.
        assert!(t.untranslated.is_empty(), "{:?}", t.untranslated);
    }

    #[test]
    fn a_disabled_grading_panel_writes_nothing_but_consumes_its_keys() {
        let t = tr("s = { EnableSplitToning = false, ColorGradeGlobalSat = 40 }");
        assert!(!t.document.stages.contains_key(COLOR_GRADE));
        assert!(t.untranslated.is_empty(), "{:?}", t.untranslated);
    }
}
