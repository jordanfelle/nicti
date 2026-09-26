//! Parses every `Adobe_imageDevelopSettings.text` payload (a Lua table literal, `s = { Key =
//! Value, ... }`) via the `agprefs` crate and reports a key-frequency histogram classified by
//! owner ticket -- the #62/#53 question this spike exists to answer: which LRC develop-setting
//! keys map to which Nicti build ticket, and how completely `agprefs` actually parses a real
//! catalog's worth of them.
//!
//! The classification below is two-tier: an exact-match table built by running this spike against
//! the user's real 380,300-asset catalog (all 197 distinct keys that catalog contains, by Develop
//! panel, not guessed from Adobe naming conventions in the abstract -- a first draft using
//! prefix-only heuristics missed 83/197 real keys, mostly boolean `Enable*` panel toggles whose
//! name doesn't share a prefix with the panel they gate, and per-channel HSL keys with no common
//! `*Adjustment*` substring at all, e.g. `BlueHue`/`RedSaturation`), plus a suffix/prefix fallback
//! for any key a *different* catalog (older LRC version, different feature set) might contain that
//! this one didn't. `Owner::Unowned` is a real, reviewable finding either way -- either a key this
//! pass confirmed has no current Nicti ticket (`FilterList`/`AllowFilters`/`LensBlur` -- newer
//! Adobe AI features with no existing owner), or one the fallback heuristic genuinely can't place.

use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Owner {
    /// #46: global tone/color adjustments (exposure, contrast, HSL, grain, dehaze, ...).
    Global,
    /// #47: crop/straighten + auto-level horizon (crop rect, manual/auto perspective transform).
    CropGeometry,
    /// #49: masks + local adjustments (brush/gradient/AI subject masks, local correction arrays).
    Masks,
    /// #39: lens corrections (profile-based distortion/vignette, chromatic aberration).
    Lens,
    /// #42: color management (camera/working-space color profile references).
    Color,
    /// #51: healing/removal (spot heal, clone stamp, legacy red-eye).
    Heal,
    /// No confident match -- needs a human to assign it to a ticket before #62 relies on it.
    Unowned,
}

/// Exact-match table for every key this pass observed in the real catalog, grouped by the
/// Develop-module panel it belongs to (a `-- panel` comment per group, not per line, since the
/// grouping itself is the useful fact). `Enable*` toggles are filed under the panel they gate, not
/// under a separate "toggles" bucket -- e.g. `EnableLensCorrections` is `Lens`, not `Unowned`.
fn exact_owner(key: &str) -> Option<Owner> {
    use Owner::*;
    Some(match key {
        // -- Basic/Tone panel (legacy PV2003 + PV2012+ names coexist in one catalog)
        "Exposure"
        | "Exposure2012"
        | "Contrast"
        | "Contrast2012"
        | "Highlights2012"
        | "Shadows"
        | "Shadows2012"
        | "Whites2012"
        | "Blacks2012"
        | "Clarity2012"
        | "Vibrance"
        | "Saturation"
        | "Brightness"
        | "FillLight"
        | "HighlightRecovery"
        | "Dehaze"
        | "ProcessVersion"
        | "Version"
        | "CompatibleVersion"
        | "AutoTone"
        | "AutoToneDigest"
        | "AutoToneDigestNoSat"
        | "AutoToneDigestPV2"
        | "AutoWhiteVersion"
        | "ParametricDarks"
        | "ParametricHighlights"
        | "ParametricHighlightSplit"
        | "ParametricLights"
        | "ParametricMidtoneSplit"
        | "ParametricShadowSplit"
        | "ParametricShadows"
        | "HDREditMode"
        | "HDRMaxValue"
        | "SDRBlend"
        | "SDRBrightness"
        | "SDRClarity"
        | "SDRContrast"
        | "SDRHighlights"
        | "SDRShadows"
        | "SDRWhites"
        | "EnableCalibration" => Global,
        // -- Detail panel (sharpening + luminance noise reduction) + Effects panel (grain, vignette)
        "SharpenDetail"
        | "SharpenEdgeMasking"
        | "SharpenRadius"
        | "Sharpness"
        | "LuminanceNoiseReductionContrast"
        | "LuminanceNoiseReductionDetail"
        | "LuminanceSmoothing"
        | "GrainSeed"
        | "GrainSize"
        | "PostCropVignetteAmount"
        | "PostCropVignetteFeather"
        | "PostCropVignetteMidpoint"
        | "PostCropVignetteRoundness"
        | "EnableDetail"
        | "EnableEffects" => Global,
        // -- Transform panel: manual crop + straighten + perspective/Upright
        "CropAngle"
        | "CropBottom"
        | "CropConstrainAspectRatio"
        | "CropConstrainToWarp"
        | "CropLeft"
        | "CropRight"
        | "CropTop"
        | "PerspectiveHorizontal"
        | "PerspectiveRotate"
        | "PerspectiveScale"
        | "PerspectiveUpright"
        | "PerspectiveVertical"
        | "PerspectiveX"
        | "PerspectiveY"
        | "UprightCenterMode"
        | "UprightCenterNormX"
        | "UprightCenterNormY"
        | "UprightDependentDigest"
        | "UprightFocalLength35mm"
        | "UprightFocalMode"
        | "UprightFourSegmentsCount"
        | "UprightFourSegments_0"
        | "UprightGuidedDependentDigest"
        | "UprightPreview"
        | "UprightTransformCount"
        | "UprightTransform_0"
        | "UprightTransform_1"
        | "UprightTransform_2"
        | "UprightTransform_3"
        | "UprightTransform_4"
        | "UprightTransform_5"
        | "UprightVersion"
        | "EnableTransform" => CropGeometry,
        // -- Lens Corrections panel: profile-based + manual distortion/vignette + defringe (CA)
        "LensProfileDigest"
        | "LensProfileDistortionScale"
        | "LensProfileEnable"
        | "LensProfileFilename"
        | "LensProfileIsEmbedded"
        | "LensProfileName"
        | "LensProfileSetup"
        | "LensProfileVignettingScale"
        | "LensManualDistortionAmount"
        | "CustomLensProfileDigest"
        | "CustomLensProfileDistortionScale"
        | "CustomLensProfileFilename"
        | "CustomLensProfileIsEmbedded"
        | "CustomLensProfileName"
        | "CustomLensProfileVignettingScale"
        | "DefringeGreenAmount"
        | "DefringeGreenHueHi"
        | "DefringeGreenHueLo"
        | "DefringePurpleAmount"
        | "DefringePurpleHueHi"
        | "DefringePurpleHueLo"
        | "AutoLateralCA"
        | "VignetteAmount"
        | "EnableLensCorrections" => Lens,
        // -- Color/Calibration/Curve panels: profile, white balance, HSL, split toning, color
        // grading, tone curve, grayscale mix, Point Color, and the AI "Look" adjustment
        "CameraProfile"
        | "CameraProfileDigest"
        | "WhiteBalance"
        | "Temperature"
        | "Tint"
        | "CustomTemperature"
        | "CustomTint"
        | "IncrementalTemperature"
        | "IncrementalTint"
        | "ShadowTint"
        | "ConvertToGrayscale"
        | "AutoGrayscaleMix"
        | "GrayMixerAqua"
        | "GrayMixerBlue"
        | "GrayMixerGreen"
        | "GrayMixerMagenta"
        | "GrayMixerOrange"
        | "GrayMixerPurple"
        | "GrayMixerRed"
        | "GrayMixerYellow"
        | "BlueHue"
        | "BlueSaturation"
        | "GreenHue"
        | "GreenSaturation"
        | "RedHue"
        | "RedSaturation"
        | "SaturationAdjustmentAqua"
        | "SaturationAdjustmentBlue"
        | "SaturationAdjustmentMagenta"
        | "SaturationAdjustmentYellow"
        | "LuminanceAdjustmentBlue"
        | "LuminanceAdjustmentGreen"
        | "LuminanceAdjustmentRed"
        | "ColorNoiseReduction"
        | "ColorNoiseReductionDetail"
        | "ColorNoiseReductionSmoothness"
        | "SplitToningBalance"
        | "SplitToningHighlightHue"
        | "SplitToningHighlightSaturation"
        | "SplitToningShadowHue"
        | "SplitToningShadowSaturation"
        | "ColorGradeBlending"
        | "ColorGradeGlobalHue"
        | "ColorGradeGlobalLum"
        | "ColorGradeGlobalSat"
        | "ColorGradeHighlightLum"
        | "ColorGradeMidtoneHue"
        | "ColorGradeMidtoneLum"
        | "ColorGradeMidtoneSat"
        | "ColorGradeShadowLum"
        | "ToneCurve"
        | "ToneCurveBlue"
        | "ToneCurveGreen"
        | "ToneCurveName"
        | "ToneCurveName2012"
        | "ToneCurvePV2012"
        | "ToneCurvePV2012Blue"
        | "ToneCurvePV2012Green"
        | "ToneCurvePV2012Red"
        | "ToneCurveRed"
        | "CurveRefineSaturation"
        | "Look"
        | "AILook"
        | "OverrideLookVignette"
        | "PointColors"
        | "EnableColorAdjustments"
        | "EnableGrayscaleMix"
        | "EnableSplitToning"
        | "EnableToneCurve" => Color,
        // -- Healing/removal (spot heal, clone stamp, legacy red-eye, AI distraction removal)
        "RetouchInfo"
        | "RetouchAreas"
        | "RemoveAreas"
        | "RedEyeInfo"
        | "EnableRetouch"
        | "EnableRedEye"
        | "EnableDistractionRemoval" => Heal,
        // -- Masks + local adjustments (brush/gradient/AI local correction groups, range masks)
        "MaskGroupBasedCorrections" | "RangeMaskMapInfo" => Masks,
        _ => return None,
    })
}

/// Fallback for a key this pass never observed (a different LRC version, or a feature this
/// catalog's photos never triggered): per-channel HSL keys share no common prefix in either
/// naming generation (`BlueHue`/`SaturationAdjustmentBlue`), so a suffix check catches a key this
/// exact-match table doesn't yet know about, on the same reasoning as the keys it does know about.
fn heuristic_owner(key: &str) -> Owner {
    if key.ends_with("Hue") || key.ends_with("Saturation") || key.ends_with("Luminance") {
        Owner::Color
    } else if key.starts_with("Crop")
        || key.starts_with("Perspective")
        || key.starts_with("Upright")
    {
        Owner::CropGeometry
    } else if key.starts_with("Mask") {
        Owner::Masks
    } else {
        Owner::Unowned
    }
}

pub fn classify_key(key: &str) -> Owner {
    exact_owner(key).unwrap_or_else(|| heuristic_owner(key))
}

#[derive(Debug, Serialize)]
pub struct DevelopReport {
    pub rows_seen: i64,
    pub parse_failures: i64,
    /// key -> (raw occurrence count, owner classification)
    pub key_frequency: BTreeMap<String, (i64, Owner)>,
    pub process_version_counts: Vec<(String, i64)>,
    pub has_masks_count: i64,
    pub has_ai_masks_count: i64,
    pub has_big_data_count: i64,
}

/// Streams every row of `Adobe_imageDevelopSettings` and parses its `text` column. A single
/// unparseable row is recorded as a failure and skipped, not fatal to the whole pass -- one
/// malformed history-step entry (LRC has been known to leave a truncated one behind after a
/// crash) must not silently zero out every other row's key-frequency contribution.
pub fn analyze(conn: &Connection) -> Result<DevelopReport> {
    let process_version_counts = group_counts(
        conn,
        "SELECT COALESCE(processVersion, '(none)'), COUNT(*) \
         FROM Adobe_imageDevelopSettings GROUP BY processVersion",
    )?;
    let has_masks_count: i64 = conn.query_row(
        "SELECT COALESCE(SUM(hasMasks), 0) FROM Adobe_imageDevelopSettings",
        [],
        |r| r.get(0),
    )?;
    let has_ai_masks_count: i64 = conn.query_row(
        "SELECT COALESCE(SUM(hasAIMasks), 0) FROM Adobe_imageDevelopSettings",
        [],
        |r| r.get(0),
    )?;
    let has_big_data_count: i64 = conn.query_row(
        "SELECT COALESCE(SUM(hasBigData), 0) FROM Adobe_imageDevelopSettings",
        [],
        |r| r.get(0),
    )?;

    let mut stmt = conn.prepare("SELECT text FROM Adobe_imageDevelopSettings")?;
    let texts: Vec<Option<String>> = stmt
        .query_map([], |r| r.get(0))?
        .collect::<Result<Vec<_>, _>>()?;

    let mut rows_seen = 0i64;
    let mut parse_failures = 0i64;
    let mut counts: BTreeMap<String, i64> = BTreeMap::new();
    for text in texts.into_iter().flatten() {
        rows_seen += 1;
        match agprefs::Agpref::parse(&text) {
            Ok(pref) => {
                let Some(fields) = pref.get_struct() else {
                    parse_failures += 1;
                    continue;
                };
                for key in fields.keys() {
                    *counts.entry(key.to_string()).or_insert(0) += 1;
                }
            }
            Err(_) => parse_failures += 1,
        }
    }

    let key_frequency = counts
        .into_iter()
        .map(|(k, n)| {
            let owner = classify_key(&k);
            (k, (n, owner))
        })
        .collect();

    Ok(DevelopReport {
        rows_seen,
        parse_failures,
        key_frequency,
        process_version_counts,
        has_masks_count,
        has_ai_masks_count,
        has_big_data_count,
    })
}

fn group_counts(conn: &Connection, sql: &str) -> Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_known_keys_by_prefix() {
        assert_eq!(classify_key("Exposure2012"), Owner::Global);
        assert_eq!(classify_key("CropTop"), Owner::CropGeometry);
        assert_eq!(classify_key("MaskGroupBasedCorrections"), Owner::Masks);
        assert_eq!(classify_key("LensProfileEnable"), Owner::Lens);
        assert_eq!(classify_key("CameraProfile"), Owner::Color);
        assert_eq!(classify_key("RetouchInfo"), Owner::Heal);
        assert_eq!(classify_key("SomeFutureAdobeKey"), Owner::Unowned);
    }

    /// The exact 197 distinct keys this pass observed across the real, 380,307-row catalog
    /// (`shed develop` against the user's own backup) -- pinned here so `exact_owner`'s claimed
    /// 191-classified/6-unowned split can never silently drift again the way it did once already:
    /// a first version of this table included `GrainAmount`, a plausible-sounding LR key name
    /// added from general knowledge rather than checked against this real list, making
    /// `exact_owner` classify 192 keys (one of them never actually observed) while the ADR/research
    /// docs all claimed 191 -- a real contradiction between the code and its own documentation,
    /// caught by an adversarial review, not by any test. This test is the regression guard: run it
    /// against a real catalog's own key dump before ever adding or removing an `exact_owner` arm.
    const REAL_KEYS: &[&str] = &[
        "AILook",
        "AllowFilters",
        "AutoGrayscaleMix",
        "AutoLateralCA",
        "AutoTone",
        "AutoToneDigest",
        "AutoToneDigestNoSat",
        "AutoToneDigestPV2",
        "AutoWhiteVersion",
        "Blacks2012",
        "BlueHue",
        "BlueSaturation",
        "Brightness",
        "CameraProfile",
        "CameraProfileDigest",
        "Clarity2012",
        "ColorGradeBlending",
        "ColorGradeGlobalHue",
        "ColorGradeGlobalLum",
        "ColorGradeGlobalSat",
        "ColorGradeHighlightLum",
        "ColorGradeMidtoneHue",
        "ColorGradeMidtoneLum",
        "ColorGradeMidtoneSat",
        "ColorGradeShadowLum",
        "ColorNoiseReduction",
        "ColorNoiseReductionDetail",
        "ColorNoiseReductionSmoothness",
        "CompatibleVersion",
        "Contrast",
        "Contrast2012",
        "ConvertToGrayscale",
        "CropAngle",
        "CropBottom",
        "CropConstrainAspectRatio",
        "CropConstrainToWarp",
        "CropLeft",
        "CropRight",
        "CropTop",
        "CurveRefineSaturation",
        "CustomLensProfileDigest",
        "CustomLensProfileDistortionScale",
        "CustomLensProfileFilename",
        "CustomLensProfileIsEmbedded",
        "CustomLensProfileName",
        "CustomLensProfileVignettingScale",
        "CustomTemperature",
        "CustomTint",
        "DefringeGreenAmount",
        "DefringeGreenHueHi",
        "DefringeGreenHueLo",
        "DefringePurpleAmount",
        "DefringePurpleHueHi",
        "DefringePurpleHueLo",
        "Dehaze",
        "EnableCalibration",
        "EnableColorAdjustments",
        "EnableDetail",
        "EnableDistractionRemoval",
        "EnableEffects",
        "EnableGrayscaleMix",
        "EnableLensCorrections",
        "EnableRedEye",
        "EnableRetouch",
        "EnableSplitToning",
        "EnableToneCurve",
        "EnableTransform",
        "Exposure",
        "Exposure2012",
        "FillLight",
        "FilterList",
        "GrainSeed",
        "GrainSize",
        "GrayMixerAqua",
        "GrayMixerBlue",
        "GrayMixerGreen",
        "GrayMixerMagenta",
        "GrayMixerOrange",
        "GrayMixerPurple",
        "GrayMixerRed",
        "GrayMixerYellow",
        "GreenHue",
        "GreenSaturation",
        "HDREditMode",
        "HDRMaxValue",
        "HighlightRecovery",
        "Highlights2012",
        "IncrementalTemperature",
        "IncrementalTint",
        "LensBlur",
        "LensManualDistortionAmount",
        "LensProfileDigest",
        "LensProfileDistortionScale",
        "LensProfileEnable",
        "LensProfileFilename",
        "LensProfileIsEmbedded",
        "LensProfileName",
        "LensProfileSetup",
        "LensProfileVignettingScale",
        "Look",
        "LuminanceAdjustmentBlue",
        "LuminanceAdjustmentGreen",
        "LuminanceAdjustmentRed",
        "LuminanceNoiseReductionContrast",
        "LuminanceNoiseReductionDetail",
        "LuminanceSmoothing",
        "MaskGroupBasedCorrections",
        "OverrideLookVignette",
        "ParametricDarks",
        "ParametricHighlightSplit",
        "ParametricHighlights",
        "ParametricLights",
        "ParametricMidtoneSplit",
        "ParametricShadowSplit",
        "ParametricShadows",
        "PerspectiveHorizontal",
        "PerspectiveRotate",
        "PerspectiveScale",
        "PerspectiveUpright",
        "PerspectiveVertical",
        "PerspectiveX",
        "PerspectiveY",
        "PointColors",
        "PostCropVignetteAmount",
        "PostCropVignetteFeather",
        "PostCropVignetteMidpoint",
        "PostCropVignetteRoundness",
        "Preset",
        "ProcessVersion",
        "RangeMaskMapInfo",
        "RedEyeInfo",
        "RedHue",
        "RedSaturation",
        "RemoveAreas",
        "RetouchAreas",
        "RetouchInfo",
        "SDRBlend",
        "SDRBrightness",
        "SDRClarity",
        "SDRContrast",
        "SDRHighlights",
        "SDRShadows",
        "SDRWhites",
        "Saturation",
        "SaturationAdjustmentAqua",
        "SaturationAdjustmentBlue",
        "SaturationAdjustmentMagenta",
        "SaturationAdjustmentYellow",
        "ShadowTint",
        "Shadows",
        "Shadows2012",
        "SharpenDetail",
        "SharpenEdgeMasking",
        "SharpenRadius",
        "Sharpness",
        "SplitToningBalance",
        "SplitToningHighlightHue",
        "SplitToningHighlightSaturation",
        "SplitToningShadowHue",
        "SplitToningShadowSaturation",
        "Temperature",
        "Tint",
        "ToggleStyleAmount",
        "ToggleStyleDigest",
        "ToneCurve",
        "ToneCurveBlue",
        "ToneCurveGreen",
        "ToneCurveName",
        "ToneCurveName2012",
        "ToneCurvePV2012",
        "ToneCurvePV2012Blue",
        "ToneCurvePV2012Green",
        "ToneCurvePV2012Red",
        "ToneCurveRed",
        "UprightCenterMode",
        "UprightCenterNormX",
        "UprightCenterNormY",
        "UprightDependentDigest",
        "UprightFocalLength35mm",
        "UprightFocalMode",
        "UprightFourSegmentsCount",
        "UprightFourSegments_0",
        "UprightGuidedDependentDigest",
        "UprightPreview",
        "UprightTransformCount",
        "UprightTransform_0",
        "UprightTransform_1",
        "UprightTransform_2",
        "UprightTransform_3",
        "UprightTransform_4",
        "UprightTransform_5",
        "UprightVersion",
        "Version",
        "Vibrance",
        "VignetteAmount",
        "WhiteBalance",
        "Whites2012",
    ];

    const REAL_UNOWNED: &[&str] = &[
        "AllowFilters",
        "FilterList",
        "LensBlur",
        "Preset",
        "ToggleStyleAmount",
        "ToggleStyleDigest",
    ];

    #[test]
    fn classifies_the_real_catalogs_197_keys_as_191_owned_plus_6_unowned() {
        assert_eq!(
            REAL_KEYS.len(),
            197,
            "the pinned ground-truth key list itself drifted"
        );
        let mut unowned = Vec::new();
        for key in REAL_KEYS {
            if classify_key(key) == Owner::Unowned {
                unowned.push(*key);
            }
        }
        assert_eq!(
            unowned, REAL_UNOWNED,
            "exact_owner's classified/unowned split no longer matches the real catalog's key set \
             -- did an arm get added that was never actually observed, or a real key stop being \
             classified?"
        );
    }

    #[test]
    fn analyzes_a_synthetic_develop_settings_row() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE Adobe_imageDevelopSettings (id_local INTEGER PRIMARY KEY, text TEXT,
                processVersion TEXT, hasMasks INTEGER, hasAIMasks INTEGER, hasBigData INTEGER);
            "#,
        )
        .unwrap();
        conn.execute(
            "INSERT INTO Adobe_imageDevelopSettings (text, processVersion, hasMasks, hasAIMasks, hasBigData) \
             VALUES (?1, '15.4', 1, 0, 1)",
            [r#"s = { Exposure2012 = 0.5, CropTop = 0.1, ProcessVersion = "15.4" }"#],
        )
        .unwrap();
        // A deliberately malformed row -- must not abort the whole pass.
        conn.execute(
            "INSERT INTO Adobe_imageDevelopSettings (text, processVersion, hasMasks, hasAIMasks, hasBigData) \
             VALUES (?1, '15.4', 0, 0, 0)",
            ["s = { this is not valid lua"],
        )
        .unwrap();

        let report = analyze(&conn).unwrap();
        assert_eq!(report.rows_seen, 2);
        assert_eq!(report.parse_failures, 1);
        assert_eq!(
            report.key_frequency.get("Exposure2012"),
            Some(&(1, Owner::Global))
        );
        assert_eq!(
            report.key_frequency.get("CropTop"),
            Some(&(1, Owner::CropGeometry))
        );
        assert_eq!(report.has_masks_count, 1);
    }
}
