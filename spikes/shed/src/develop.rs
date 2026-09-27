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
//! this one didn't. `Owner::Unowned` remains for a key the fallback heuristic genuinely can't
//! place -- #157 resolved the 6 keys that had no owner ticket after this pass's first sweep by
//! measuring their real presence/active usage (`analyze_unowned_keys`, `UNOWNED_KEYS`) rather than
//! guessing: `FilterList`/`AllowFilters` turned out to gate 4 distinct LRC AI filters, not one,
//! with real but very unevenly distributed usage across `FilterList.Filters[].Title` entries:
//! 20,303 Denoise / 47 People Removal / 6 Super Resolution / 1 Reflection Removal, summing to
//! 20,357 *entries* across 20,356 *active rows* (one row has 2 `Filters[]` entries -- entry count
//! and row count are different things, both real) -- Denoise -> `AiDenoise` (#40), People/
//! Reflection Removal -> `Heal` (#51, already scoped for "AI distraction removal"), Super
//! Resolution has no existing owner and got a new ticket (#174). `LensBlur` is present in nearly
//! every row (380,300/380,307) but always as an empty bookkeeping table -- 0 rows had real
//! content, i.e. the feature has never actually been used in this catalog -- so it's `ProvenanceOnly`:
//! not worth a render-owning ticket for a feature with zero real usage, but #62's importer still
//! keeps it verbatim in the provenance blob rather than silently dropping it. `Preset`/
//! `ToggleStyleAmount`/`ToggleStyleDigest` are style-preset apply/toggle bookkeeping, not develop
//! parameters -> `Presets` (#52).

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
    /// #51: healing/removal (spot heal, clone stamp, legacy red-eye, AI People/Reflection Removal).
    Heal,
    /// #40: demosaic + noise reduction, including the AI Denoise entry inside `FilterList`.
    AiDenoise,
    /// #52: style-preset apply/toggle bookkeeping (which preset, toggle state/digest) -- not a
    /// develop parameter itself.
    Presets,
    /// Confirmed real (not a classifier miss) but with zero active usage in the measured
    /// catalog (#157) -- kept verbatim in #62's provenance blob, no render-owning ticket yet.
    ProvenanceOnly,
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
        // -- AI filter-panel container (#157): key-level classification only -- `FilterList`
        // holds several distinct filter types (Denoise dominant at 20,303 of 20,357 real filter
        // entries across 20,356 active rows -- one row has 2 entries, see this module's doc
        // comment; People Removal/Reflection Removal -> Heal; Super Resolution -> #174), but
        // `classify_key` operates per-key, not per-filter-entry. `AiDenoise` here is the
        // majority-case default; #62's importer must still inspect `FilterList.Filters[].Title`
        // to route People/Reflection Removal entries to #51 and Super Resolution entries to
        // #174 (see this module's doc comment and `analyze_unowned_keys`'s `filter_titles`).
        "FilterList" | "AllowFilters" => AiDenoise,
        // -- Present in nearly every row but always an empty bookkeeping table in the measured
        // catalog (0/380,300 rows had real content) -- see this module's doc comment.
        "LensBlur" => ProvenanceOnly,
        // -- Style-preset apply/toggle bookkeeping (#52), not a develop parameter.
        "Preset" | "ToggleStyleAmount" | "ToggleStyleDigest" => Presets,
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

    // Parsed row-by-row rather than collected into a `Vec<Option<String>>` first -- the real
    // catalog has ~380,300 rows, each a several-KB Lua-literal payload, so buffering every one
    // before parsing any of them costs a peak allocation of hundreds of MB for no benefit.
    let mut stmt = conn.prepare("SELECT text FROM Adobe_imageDevelopSettings")?;
    let mut rows = stmt.query([])?;

    let mut rows_seen = 0i64;
    let mut parse_failures = 0i64;
    let mut counts: BTreeMap<String, i64> = BTreeMap::new();
    while let Some(row) = rows.next()? {
        let Some(text): Option<String> = row.get(0)? else {
            continue;
        };
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

/// The 6 keys #61 found with no owner ticket (see this module's doc comment). Order matches the
/// report table in `docs/adr/0061-lrc-catalog-import-mapping.md`.
pub const UNOWNED_KEYS: &[&str] = &[
    "FilterList",
    "AllowFilters",
    "LensBlur",
    "Preset",
    "ToggleStyleAmount",
    "ToggleStyleDigest",
];

#[derive(Debug, Serialize)]
pub struct UnownedKeyUsage {
    pub key: &'static str,
    /// Rows where the key exists in the parsed struct at all, present or not truthy.
    pub present_count: i64,
    /// Rows where the value is also non-empty/non-zero/true -- see `is_active`. Distinguishes
    /// "the panel wrote its usual bookkeeping key" (e.g. `LensBlur = {  }`, an empty table LRC
    /// writes on every image regardless of whether the AI Lens Blur filter was ever opened) from
    /// "the feature actually holds real data."
    pub active_count: i64,
    /// `FilterList` only: each entry's `Filters[].Title` value, canonicalized by
    /// `canonicalize_filter_title` before use as a report key -- see that function's doc comment.
    /// Confirmed `FilterList` is not single-purpose: it holds Denoise, People Removal, Reflection
    /// Removal, and Super Resolution entries, which don't share one owner ticket.
    pub filter_titles: Option<BTreeMap<String, i64>>,
}

/// A LRC develop-setting value counts as "active" when it isn't the trivial/default form for its
/// type: `false`/`0`/empty string/empty list/empty struct. LRC writes several of the 6 unowned
/// keys as an always-present empty container (`{  }`) regardless of whether the user ever touched
/// the corresponding panel -- `present_count` alone would conflate "feature never used" with
/// "feature used," this tells them apart.
fn is_active(value: &agprefs::Value) -> bool {
    use agprefs::Value;
    match value {
        Value::Unit => false,
        Value::Bool(b) => *b,
        Value::Int(i) => *i != 0,
        Value::Float(f) => *f != 0.0,
        Value::String(s) => !s.is_empty(),
        Value::Values(vals) => !vals.is_empty(),
        Value::Struct(fields) => !fields.is_empty(),
    }
}

/// A sentinel bucket for a `FilterList.Filters[].Title` value that doesn't match LRC's own known
/// localization-key shape -- see `canonicalize_filter_title`.
const UNRECOGNIZED_FILTER_TITLE: &str = "<unrecognized filter title>";

/// Every real `Title` value #157 observed looks like a Lightroom localization key:
/// `"$$$/<path>/<Name>=<display label>"` (e.g. `"$$$/CRaw/Filter/Title/Denoise=Denoise"`,
/// `"$$$/CRaw/Filter/PeopleRemoval/FilterPanelTitle=People Removal"`) -- Adobe's own fixed
/// AI-filter-type identifiers, not user data. `analyze_unowned_keys` uses this value as a report
/// key, though, so it must not blindly trust an arbitrary parsed string: a corrupt catalog, a
/// future LRC version, or a hand-edited `.lrcat` could contain a `Title` that isn't one of these
/// known identifiers at all. Only a value matching this exact shape (`$$$/` prefix, exactly one
/// `=` splitting a key path from a short label) is reported verbatim; anything else collapses
/// into `UNRECOGNIZED_FILTER_TITLE`, a fixed bucket with no raw content -- this keeps the tool's
/// actual purpose (discovering which real, *known-shape* AI filters a catalog uses) while never
/// serializing an unvalidated string as a JSON key.
fn canonicalize_filter_title(title: &str) -> String {
    let is_known_shape = title.starts_with("$$$/")
        && title.matches('=').count() == 1
        && title.split('=').nth(1).is_some_and(|label| {
            !label.is_empty()
                && label.len() <= 64
                && label.chars().all(|c| c.is_ascii_graphic() || c == ' ')
        });
    if is_known_shape {
        title.to_string()
    } else {
        UNRECOGNIZED_FILTER_TITLE.to_string()
    }
}

/// Aggregate presence/active counts for `UNOWNED_KEYS` only, across every row of
/// `Adobe_imageDevelopSettings` -- the measurement #157 needs before deciding each key's owner.
/// Reports counts only, no value contents: `Preset`/`ToggleStyleDigest` can carry
/// user-created-preset identifiers, which ADR-0061's privacy policy keeps out of anything
/// committed to this repo.
pub fn analyze_unowned_keys(conn: &Connection) -> Result<Vec<UnownedKeyUsage>> {
    let mut stmt = conn.prepare("SELECT text FROM Adobe_imageDevelopSettings")?;
    let mut rows = stmt.query([])?;

    let mut present: BTreeMap<&'static str, i64> = UNOWNED_KEYS.iter().map(|k| (*k, 0)).collect();
    let mut active: BTreeMap<&'static str, i64> = UNOWNED_KEYS.iter().map(|k| (*k, 0)).collect();
    let mut filter_titles: BTreeMap<String, i64> = BTreeMap::new();

    while let Some(row) = rows.next()? {
        let Some(text): Option<String> = row.get(0)? else {
            continue;
        };
        let Ok(pref) = agprefs::Agpref::parse(&text) else {
            continue;
        };
        let Some(fields) = pref.get_struct() else {
            continue;
        };
        for key in UNOWNED_KEYS {
            if let Some(value) = fields.get(*key) {
                *present.get_mut(key).unwrap() += 1;
                if is_active(value) {
                    *active.get_mut(key).unwrap() += 1;
                }
            }
        }
        if let Some(agprefs::Value::Struct(filter_list)) = fields.get("FilterList") {
            if let Some(agprefs::Value::Values(filters)) = filter_list.get("Filters") {
                for filter in filters {
                    if let agprefs::Value::Struct(filter_fields) = filter {
                        if let Some(agprefs::Value::String(title)) = filter_fields.get("Title") {
                            *filter_titles
                                .entry(canonicalize_filter_title(title))
                                .or_insert(0) += 1;
                        }
                    }
                }
            }
        }
    }

    Ok(UNOWNED_KEYS
        .iter()
        .map(|key| UnownedKeyUsage {
            key,
            present_count: present[key],
            active_count: active[key],
            filter_titles: (*key == "FilterList").then(|| filter_titles.clone()),
        })
        .collect())
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
        assert_eq!(classify_key("FilterList"), Owner::AiDenoise);
        assert_eq!(classify_key("AllowFilters"), Owner::AiDenoise);
        assert_eq!(classify_key("LensBlur"), Owner::ProvenanceOnly);
        assert_eq!(classify_key("Preset"), Owner::Presets);
        assert_eq!(classify_key("ToggleStyleAmount"), Owner::Presets);
        assert_eq!(classify_key("ToggleStyleDigest"), Owner::Presets);
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

    #[test]
    fn classifies_the_real_catalogs_197_keys_with_zero_unowned() {
        assert_eq!(
            REAL_KEYS.len(),
            197,
            "the pinned ground-truth key list itself drifted"
        );
        let mut unowned = Vec::new();
        let mut counts: BTreeMap<Owner, i64> = BTreeMap::new();
        for key in REAL_KEYS {
            let owner = classify_key(key);
            if owner == Owner::Unowned {
                unowned.push(*key);
            }
            *counts.entry(owner).or_insert(0) += 1;
        }
        assert!(
            unowned.is_empty(),
            "#157 assigned an owner (or ProvenanceOnly) to every previously-unowned key -- a new \
             Unowned hit here ({unowned:?}) means a real key stopped being classified"
        );
        // Per-owner counts, not just "not Unowned" -- a match-arm-ordering slip that silently
        // routes a key to the wrong (but still real) Owner would pass an is_empty()-only check;
        // these totals must match ADR-0061's own owner table exactly. #157 added AiDenoise(2),
        // Presets(3), ProvenanceOnly(1); the rest predate it.
        let expected: BTreeMap<Owner, i64> = [
            (Owner::Color, 69),
            (Owner::Global, 56),
            (Owner::CropGeometry, 33),
            (Owner::Lens, 24),
            (Owner::Heal, 7),
            (Owner::AiDenoise, 2),
            (Owner::Masks, 2),
            (Owner::Presets, 3),
            (Owner::ProvenanceOnly, 1),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            counts, expected,
            "per-owner key counts drifted from ADR-0061's table -- a key was reclassified to a \
             different (but still real) owner than the one this pass measured"
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

    #[test]
    fn is_active_treats_empty_containers_and_falsy_scalars_as_inactive() {
        use agprefs::Value;
        assert!(!is_active(&Value::Unit));
        assert!(!is_active(&Value::Bool(false)));
        assert!(!is_active(&Value::Int(0)));
        assert!(!is_active(&Value::Float(0.0)));
        assert!(!is_active(&Value::String("".into())));
        assert!(!is_active(&Value::Values(vec![])));
        assert!(!is_active(&Value::Struct(Default::default())));

        assert!(is_active(&Value::Bool(true)));
        assert!(is_active(&Value::Int(1)));
        assert!(is_active(&Value::String("x".into())));
        assert!(is_active(&Value::Values(vec![Value::Int(1)])));
    }

    #[test]
    fn canonicalize_filter_title_passes_through_known_shapes_and_buckets_the_rest() {
        assert_eq!(
            canonicalize_filter_title("$$$/CRaw/Filter/Title/Denoise=Denoise"),
            "$$$/CRaw/Filter/Title/Denoise=Denoise"
        );
        assert_eq!(
            canonicalize_filter_title(
                "$$$/CRaw/Filter/PeopleRemoval/FilterPanelTitle=People Removal"
            ),
            "$$$/CRaw/Filter/PeopleRemoval/FilterPanelTitle=People Removal"
        );
        // No `$$$/` prefix at all.
        assert_eq!(
            canonicalize_filter_title("arbitrary user string"),
            UNRECOGNIZED_FILTER_TITLE
        );
        // Right prefix, but no `=` splitting a key path from a label.
        assert_eq!(
            canonicalize_filter_title("$$$/CRaw/Filter/Title/Denoise"),
            UNRECOGNIZED_FILTER_TITLE
        );
        // Two `=` signs -- not the expected one-split shape.
        assert_eq!(
            canonicalize_filter_title("$$$/a=b=c"),
            UNRECOGNIZED_FILTER_TITLE
        );
        // Empty label after the `=`.
        assert_eq!(
            canonicalize_filter_title("$$$/a="),
            UNRECOGNIZED_FILTER_TITLE
        );
        // Implausibly long label.
        let long_label = "x".repeat(65);
        assert_eq!(
            canonicalize_filter_title(&format!("$$$/a={long_label}")),
            UNRECOGNIZED_FILTER_TITLE
        );
    }

    #[test]
    fn analyze_unowned_keys_distinguishes_present_from_active_and_breaks_down_filter_titles() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE Adobe_imageDevelopSettings (id_local INTEGER PRIMARY KEY, text TEXT);",
        )
        .unwrap();
        // LensBlur present but empty (LRC's real always-there-but-unused bookkeeping shape);
        // FilterList present with one real Denoise entry; AllowFilters true to match.
        conn.execute(
            "INSERT INTO Adobe_imageDevelopSettings (text) VALUES (?1)",
            [r#"s = {
                LensBlur = {  },
                AllowFilters = true,
                FilterList = { Filters = { { Title = "$$$/CRaw/Filter/Title/Denoise=Denoise" } } },
            }"#],
        )
        .unwrap();
        // A second row: LensBlur present-but-empty again, nothing else present.
        conn.execute(
            "INSERT INTO Adobe_imageDevelopSettings (text) VALUES (?1)",
            [r#"s = { LensBlur = {  } }"#],
        )
        .unwrap();
        // A third row: a FilterList entry whose Title doesn't match LRC's known localization-key
        // shape -- must collapse into the fixed sentinel bucket, not appear verbatim.
        conn.execute(
            "INSERT INTO Adobe_imageDevelopSettings (text) VALUES (?1)",
            [r#"s = { FilterList = { Filters = { { Title = "not a real lrc title" } } } }"#],
        )
        .unwrap();

        let usage = analyze_unowned_keys(&conn).unwrap();
        let by_key: BTreeMap<&str, &UnownedKeyUsage> = usage.iter().map(|u| (u.key, u)).collect();

        assert_eq!(by_key["LensBlur"].present_count, 2);
        assert_eq!(by_key["LensBlur"].active_count, 0);
        assert_eq!(by_key["FilterList"].present_count, 2);
        assert_eq!(by_key["FilterList"].active_count, 2);
        assert_eq!(by_key["AllowFilters"].present_count, 1);
        assert_eq!(by_key["AllowFilters"].active_count, 1);
        assert_eq!(by_key["Preset"].present_count, 0);

        let titles = by_key["FilterList"].filter_titles.as_ref().unwrap();
        assert_eq!(
            titles.get("$$$/CRaw/Filter/Title/Denoise=Denoise"),
            Some(&1)
        );
        assert_eq!(titles.get(UNRECOGNIZED_FILTER_TITLE), Some(&1));
        assert!(!titles.contains_key("not a real lrc title"));
        assert!(by_key["AllowFilters"].filter_titles.is_none());
    }
}
