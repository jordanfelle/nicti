//! The Develop controls an agent may read and write by id (#556, ADR-0434/0440).
//!
//! Each [`Control`] ties the panel's own [`SliderSpec`] (range, default, label: one source of truth,
//! so the registry cannot disagree with the sliders) to a typed accessor on a stage's params
//! struct. Every write goes through the same `stage_params` / `set_stage_params` round trip the
//! panel uses, never a JSON-pointer write, so a registry set and a slider edit leave the same
//! document (the tests compare stage params and the content hash byte for byte).
//!
//! Covered: the Basic panel (white balance incl. the optional `temp_k`, exposure, tone, vibrance,
//! presence). Grading wheels, HSL bands, curves and masks need explicit entries of their own
//! (their params are not one float per field) and are not here yet.

use nicti_tapetum::coat::{
    CalibrationParams, ColorGradeParams, DefringeParams, ExposureParams, HslParams,
    NoiseReductionParams, PresenceParams, SharpenParams, ToneCurveParams, ToneParams,
    VibranceParams, WbParams,
};
use nicti_tapetum::stages::{
    CALIBRATION, COLOR_GRADE, DEFRINGE, EXPOSURE, HSL, NOISE_REDUCTION, PRESENCE, SHARPEN, TONE,
    TONE_CURVE, VIBRANCE, WB,
};

use crate::develop_panel::{
    BLACKS, CAL_BLUE_HUE, CAL_BLUE_SAT, CAL_GREEN_HUE, CAL_GREEN_SAT, CAL_RED_HUE, CAL_RED_SAT,
    CAL_SHADOW_TINT, CLARITY, CONTRAST, CURVE_DARKS, CURVE_HIGHLIGHTS, CURVE_LIGHTS, CURVE_SHADOWS,
    DEFRINGE_GREEN_AMOUNT, DEFRINGE_GREEN_HI, DEFRINGE_GREEN_LO, DEFRINGE_PURPLE_AMOUNT,
    DEFRINGE_PURPLE_HI, DEFRINGE_PURPLE_LO, DEHAZE, EXPOSURE_STOPS, GRADE_BALANCE, GRADE_BLENDING,
    GRADE_HUE, GRADE_LUM, GRADE_SAT, HIGHLIGHTS, HSL_HUE, HSL_LUMINANCE, HSL_SATURATION, NR_COLOR,
    NR_DETAIL, NR_LUMINANCE, SATURATION, SHADOWS, SHARPEN_AMOUNT, SHARPEN_DETAIL, SHARPEN_RADIUS,
    TEMP, TEXTURE, TINT, VIBRANCE_AMOUNT, WHITES,
};
use crate::fur::SliderSpec;
use crate::render::DevelopDoc;

/// The stages' typed params, one variant per stage, generated so a stage appears once. A [`Draft`]
/// holds one stage's params loaded from the document so several fields can change in one typed
/// write (one undo step) and be stored back through `set_stage_params`.
macro_rules! drafts {
    ($($variant:ident($ty:ty) = $stage:expr),+ $(,)?) => {
        pub enum Draft {
            $($variant($ty)),+
        }

        impl Draft {
            pub fn load(dev: &DevelopDoc, stage: &str) -> Option<Draft> {
                $(if stage == $stage {
                    return Some(Draft::$variant(dev.stage_params(stage)));
                })+
                None
            }

            /// Writes the draft back through the undo history (a no-op when nothing changed).
            pub fn store(&self, dev: &mut DevelopDoc) {
                match self {
                    $(Draft::$variant(p) => dev.set_stage_params($stage, p)),+
                }
            }

            /// The params as JSON (`develop.get`).
            pub fn to_json(&self) -> serde_json::Value {
                match self {
                    $(Draft::$variant(p) => {
                        serde_json::to_value(p).unwrap_or(serde_json::Value::Null)
                    }),+
                }
            }

            /// The stage's default params, with no document: lets the registry be checked without
            /// a view.
            #[cfg(test)]
            fn default_for(stage: &str) -> Option<Draft> {
                $(if stage == $stage {
                    return Some(Draft::$variant(<$ty>::default()));
                })+
                None
            }
        }

        /// The stages `develop.get/set` may touch.
        pub const STAGES: &[&str] = &[$($stage),+];
    };
}

drafts! {
    Wb(WbParams) = WB,
    Exposure(ExposureParams) = EXPOSURE,
    Tone(ToneParams) = TONE,
    Vibrance(VibranceParams) = VIBRANCE,
    Presence(PresenceParams) = PRESENCE,
    ToneCurve(ToneCurveParams) = TONE_CURVE,
    Hsl(HslParams) = HSL,
    ColorGrade(ColorGradeParams) = COLOR_GRADE,
    Calibration(CalibrationParams) = CALIBRATION,
    Defringe(DefringeParams) = DEFRINGE,
    Sharpen(SharpenParams) = SHARPEN,
    NoiseReduction(NoiseReductionParams) = NOISE_REDUCTION,
}

pub struct Control {
    /// A unique id: the panel slider's (`basic.exposure`), or `hsl.<band>.<field>` where the slider
    /// is shared by several bands.
    pub id: &'static str,
    /// The document stage and the field's path inside its params, dotted for nested values
    /// (`temp_k`, `bands.2.hue`, `shadows.sat`): what `develop.get/set` use.
    pub stage: &'static str,
    pub field: &'static str,
    /// The panel section it lives in.
    pub section: &'static str,
    pub spec: &'static SliderSpec,
    /// `None` is a legal value (white balance `temp_k`: as-shot).
    pub optional: bool,
    read: fn(&Draft) -> Option<f32>,
    write: fn(&mut Draft, Option<f32>),
}

impl Control {
    pub fn read(&self, draft: &Draft) -> Option<f32> {
        (self.read)(draft)
    }

    /// Checks `value` against the slider's own range (refused, never clamped, so an agent learns
    /// it was wrong). The bound is checked on the caller's number: `1e300` parses into an `f32` as
    /// infinity, which would serialize back to `null` and poison the document.
    pub fn check(&self, value: Option<f64>) -> Result<Option<f32>, String> {
        match value {
            None if self.optional => Ok(None),
            None => Err(format!("field `{}` must be a number", self.field)),
            Some(n) => {
                let (lo, hi) = (f64::from(self.spec.min), f64::from(self.spec.max));
                if !n.is_finite() || !(lo..=hi).contains(&n) {
                    return Err(format!("field `{}` must be within {lo}..={hi}", self.field));
                }
                Ok(Some(n as f32))
            }
        }
    }

    /// Sets the field on a loaded draft (the value must have passed [`Self::check`]).
    pub fn write(&self, draft: &mut Draft, value: Option<f32>) {
        (self.write)(draft, value);
    }

    #[cfg(test)]
    /// Reads, sets and stores in one typed round trip: the registry's equivalent of a slider edit.
    pub fn set(&self, dev: &mut DevelopDoc, value: Option<f64>) -> Result<(), String> {
        let value = self.check(value)?;
        let mut draft = Draft::load(dev, self.stage).ok_or("not a develop stage")?;
        self.write(&mut draft, value);
        draft.store(dev);
        Ok(())
    }

    pub fn get(&self, dev: &DevelopDoc) -> Option<f32> {
        Draft::load(dev, self.stage).and_then(|d| self.read(&d))
    }
}

/// One control: `(stage, field path, id, section, panel spec, Draft variant, params path)`. The
/// params path is the Rust access path inside the variant's struct (`bands[2].hue`).
macro_rules! control {
    ($stage:expr, $field:literal, $id:expr, $section:literal, $spec:expr, $variant:ident, $($path:tt)+) => {
        Control {
            id: $id,
            stage: $stage,
            field: $field,
            section: $section,
            spec: &$spec,
            optional: false,
            read: |d| match d {
                Draft::$variant(p) => Some(p.$($path)+),
                _ => None,
            },
            write: |d, v| {
                if let (Draft::$variant(p), Some(v)) = (d, v) {
                    p.$($path)+ = v;
                }
            },
        }
    };
}

pub static CONTROLS: &[Control] = &[
    Control {
        id: TEMP.id,
        stage: WB,
        field: "temp_k",
        section: "basic",
        spec: &TEMP,
        optional: true,
        read: |d| match d {
            Draft::Wb(p) => p.temp_k,
            _ => None,
        },
        write: |d, v| {
            if let Draft::Wb(p) = d {
                p.temp_k = v;
            }
        },
    },
    control!(WB, "tint", TINT.id, "basic", TINT, Wb, tint),
    control!(
        EXPOSURE,
        "stops",
        EXPOSURE_STOPS.id,
        "basic",
        EXPOSURE_STOPS,
        Exposure,
        stops
    ),
    control!(
        TONE,
        "contrast",
        CONTRAST.id,
        "basic",
        CONTRAST,
        Tone,
        contrast
    ),
    control!(
        TONE,
        "highlights",
        HIGHLIGHTS.id,
        "basic",
        HIGHLIGHTS,
        Tone,
        highlights
    ),
    control!(TONE, "shadows", SHADOWS.id, "basic", SHADOWS, Tone, shadows),
    control!(TONE, "whites", WHITES.id, "basic", WHITES, Tone, whites),
    control!(TONE, "blacks", BLACKS.id, "basic", BLACKS, Tone, blacks),
    control!(
        VIBRANCE,
        "amount",
        VIBRANCE_AMOUNT.id,
        "basic",
        VIBRANCE_AMOUNT,
        Vibrance,
        amount
    ),
    control!(PRESENCE, "texture", TEXTURE.id, "basic", TEXTURE, Presence, texture),
    control!(PRESENCE, "clarity", CLARITY.id, "basic", CLARITY, Presence, clarity),
    control!(PRESENCE, "dehaze", DEHAZE.id, "basic", DEHAZE, Presence, dehaze),
    control!(
        PRESENCE,
        "saturation",
        SATURATION.id,
        "basic",
        SATURATION,
        Presence,
        saturation
    ),
    control!(
        TONE_CURVE,
        "shadows",
        CURVE_SHADOWS.id,
        "tone_curve",
        CURVE_SHADOWS,
        ToneCurve,
        shadows
    ),
    control!(
        TONE_CURVE,
        "darks",
        CURVE_DARKS.id,
        "tone_curve",
        CURVE_DARKS,
        ToneCurve,
        darks
    ),
    control!(
        TONE_CURVE,
        "lights",
        CURVE_LIGHTS.id,
        "tone_curve",
        CURVE_LIGHTS,
        ToneCurve,
        lights
    ),
    control!(
        TONE_CURVE,
        "highlights",
        CURVE_HIGHLIGHTS.id,
        "tone_curve",
        CURVE_HIGHLIGHTS,
        ToneCurve,
        highlights
    ),
    control!(
        HSL,
        "bands.0.hue",
        "hsl.0.hue",
        "hsl",
        HSL_HUE,
        Hsl,
        bands[0].hue
    ),
    control!(
        HSL,
        "bands.0.saturation",
        "hsl.0.saturation",
        "hsl",
        HSL_SATURATION,
        Hsl,
        bands[0].saturation
    ),
    control!(
        HSL,
        "bands.0.luminance",
        "hsl.0.luminance",
        "hsl",
        HSL_LUMINANCE,
        Hsl,
        bands[0].luminance
    ),
    control!(
        HSL,
        "bands.1.hue",
        "hsl.1.hue",
        "hsl",
        HSL_HUE,
        Hsl,
        bands[1].hue
    ),
    control!(
        HSL,
        "bands.1.saturation",
        "hsl.1.saturation",
        "hsl",
        HSL_SATURATION,
        Hsl,
        bands[1].saturation
    ),
    control!(
        HSL,
        "bands.1.luminance",
        "hsl.1.luminance",
        "hsl",
        HSL_LUMINANCE,
        Hsl,
        bands[1].luminance
    ),
    control!(
        HSL,
        "bands.2.hue",
        "hsl.2.hue",
        "hsl",
        HSL_HUE,
        Hsl,
        bands[2].hue
    ),
    control!(
        HSL,
        "bands.2.saturation",
        "hsl.2.saturation",
        "hsl",
        HSL_SATURATION,
        Hsl,
        bands[2].saturation
    ),
    control!(
        HSL,
        "bands.2.luminance",
        "hsl.2.luminance",
        "hsl",
        HSL_LUMINANCE,
        Hsl,
        bands[2].luminance
    ),
    control!(
        HSL,
        "bands.3.hue",
        "hsl.3.hue",
        "hsl",
        HSL_HUE,
        Hsl,
        bands[3].hue
    ),
    control!(
        HSL,
        "bands.3.saturation",
        "hsl.3.saturation",
        "hsl",
        HSL_SATURATION,
        Hsl,
        bands[3].saturation
    ),
    control!(
        HSL,
        "bands.3.luminance",
        "hsl.3.luminance",
        "hsl",
        HSL_LUMINANCE,
        Hsl,
        bands[3].luminance
    ),
    control!(
        HSL,
        "bands.4.hue",
        "hsl.4.hue",
        "hsl",
        HSL_HUE,
        Hsl,
        bands[4].hue
    ),
    control!(
        HSL,
        "bands.4.saturation",
        "hsl.4.saturation",
        "hsl",
        HSL_SATURATION,
        Hsl,
        bands[4].saturation
    ),
    control!(
        HSL,
        "bands.4.luminance",
        "hsl.4.luminance",
        "hsl",
        HSL_LUMINANCE,
        Hsl,
        bands[4].luminance
    ),
    control!(
        HSL,
        "bands.5.hue",
        "hsl.5.hue",
        "hsl",
        HSL_HUE,
        Hsl,
        bands[5].hue
    ),
    control!(
        HSL,
        "bands.5.saturation",
        "hsl.5.saturation",
        "hsl",
        HSL_SATURATION,
        Hsl,
        bands[5].saturation
    ),
    control!(
        HSL,
        "bands.5.luminance",
        "hsl.5.luminance",
        "hsl",
        HSL_LUMINANCE,
        Hsl,
        bands[5].luminance
    ),
    control!(
        HSL,
        "bands.6.hue",
        "hsl.6.hue",
        "hsl",
        HSL_HUE,
        Hsl,
        bands[6].hue
    ),
    control!(
        HSL,
        "bands.6.saturation",
        "hsl.6.saturation",
        "hsl",
        HSL_SATURATION,
        Hsl,
        bands[6].saturation
    ),
    control!(
        HSL,
        "bands.6.luminance",
        "hsl.6.luminance",
        "hsl",
        HSL_LUMINANCE,
        Hsl,
        bands[6].luminance
    ),
    control!(
        HSL,
        "bands.7.hue",
        "hsl.7.hue",
        "hsl",
        HSL_HUE,
        Hsl,
        bands[7].hue
    ),
    control!(
        HSL,
        "bands.7.saturation",
        "hsl.7.saturation",
        "hsl",
        HSL_SATURATION,
        Hsl,
        bands[7].saturation
    ),
    control!(
        HSL,
        "bands.7.luminance",
        "hsl.7.luminance",
        "hsl",
        HSL_LUMINANCE,
        Hsl,
        bands[7].luminance
    ),
    control!(
        COLOR_GRADE,
        "shadows.hue",
        "grade.shadows.hue",
        "color_grading",
        GRADE_HUE,
        ColorGrade,
        shadows.hue
    ),
    control!(
        COLOR_GRADE,
        "shadows.sat",
        "grade.shadows.sat",
        "color_grading",
        GRADE_SAT,
        ColorGrade,
        shadows.sat
    ),
    control!(
        COLOR_GRADE,
        "shadows.lum",
        "grade.shadows.lum",
        "color_grading",
        GRADE_LUM,
        ColorGrade,
        shadows.lum
    ),
    control!(
        COLOR_GRADE,
        "midtones.hue",
        "grade.midtones.hue",
        "color_grading",
        GRADE_HUE,
        ColorGrade,
        midtones.hue
    ),
    control!(
        COLOR_GRADE,
        "midtones.sat",
        "grade.midtones.sat",
        "color_grading",
        GRADE_SAT,
        ColorGrade,
        midtones.sat
    ),
    control!(
        COLOR_GRADE,
        "midtones.lum",
        "grade.midtones.lum",
        "color_grading",
        GRADE_LUM,
        ColorGrade,
        midtones.lum
    ),
    control!(
        COLOR_GRADE,
        "highlights.hue",
        "grade.highlights.hue",
        "color_grading",
        GRADE_HUE,
        ColorGrade,
        highlights.hue
    ),
    control!(
        COLOR_GRADE,
        "highlights.sat",
        "grade.highlights.sat",
        "color_grading",
        GRADE_SAT,
        ColorGrade,
        highlights.sat
    ),
    control!(
        COLOR_GRADE,
        "highlights.lum",
        "grade.highlights.lum",
        "color_grading",
        GRADE_LUM,
        ColorGrade,
        highlights.lum
    ),
    control!(
        COLOR_GRADE,
        "global.hue",
        "grade.global.hue",
        "color_grading",
        GRADE_HUE,
        ColorGrade,
        global.hue
    ),
    control!(
        COLOR_GRADE,
        "global.sat",
        "grade.global.sat",
        "color_grading",
        GRADE_SAT,
        ColorGrade,
        global.sat
    ),
    control!(
        COLOR_GRADE,
        "global.lum",
        "grade.global.lum",
        "color_grading",
        GRADE_LUM,
        ColorGrade,
        global.lum
    ),
    control!(
        COLOR_GRADE,
        "blending",
        GRADE_BLENDING.id,
        "color_grading",
        GRADE_BLENDING,
        ColorGrade,
        blending
    ),
    control!(
        COLOR_GRADE,
        "balance",
        GRADE_BALANCE.id,
        "color_grading",
        GRADE_BALANCE,
        ColorGrade,
        balance
    ),
    control!(
        CALIBRATION,
        "red_hue",
        CAL_RED_HUE.id,
        "calibration",
        CAL_RED_HUE,
        Calibration,
        red_hue
    ),
    control!(
        CALIBRATION,
        "red_sat",
        CAL_RED_SAT.id,
        "calibration",
        CAL_RED_SAT,
        Calibration,
        red_sat
    ),
    control!(
        CALIBRATION,
        "green_hue",
        CAL_GREEN_HUE.id,
        "calibration",
        CAL_GREEN_HUE,
        Calibration,
        green_hue
    ),
    control!(
        CALIBRATION,
        "green_sat",
        CAL_GREEN_SAT.id,
        "calibration",
        CAL_GREEN_SAT,
        Calibration,
        green_sat
    ),
    control!(
        CALIBRATION,
        "blue_hue",
        CAL_BLUE_HUE.id,
        "calibration",
        CAL_BLUE_HUE,
        Calibration,
        blue_hue
    ),
    control!(
        CALIBRATION,
        "blue_sat",
        CAL_BLUE_SAT.id,
        "calibration",
        CAL_BLUE_SAT,
        Calibration,
        blue_sat
    ),
    control!(
        CALIBRATION,
        "shadow_tint",
        CAL_SHADOW_TINT.id,
        "calibration",
        CAL_SHADOW_TINT,
        Calibration,
        shadow_tint
    ),
    control!(
        SHARPEN,
        "amount",
        SHARPEN_AMOUNT.id,
        "detail",
        SHARPEN_AMOUNT,
        Sharpen,
        amount
    ),
    control!(
        SHARPEN,
        "radius_px",
        SHARPEN_RADIUS.id,
        "detail",
        SHARPEN_RADIUS,
        Sharpen,
        radius_px
    ),
    control!(
        SHARPEN,
        "detail",
        SHARPEN_DETAIL.id,
        "detail",
        SHARPEN_DETAIL,
        Sharpen,
        detail
    ),
    control!(
        NOISE_REDUCTION,
        "luminance",
        NR_LUMINANCE.id,
        "detail",
        NR_LUMINANCE,
        NoiseReduction,
        luminance
    ),
    control!(
        NOISE_REDUCTION,
        "color",
        NR_COLOR.id,
        "detail",
        NR_COLOR,
        NoiseReduction,
        color
    ),
    control!(
        NOISE_REDUCTION,
        "detail",
        NR_DETAIL.id,
        "detail",
        NR_DETAIL,
        NoiseReduction,
        detail
    ),
    control!(
        DEFRINGE,
        "purple_amount",
        DEFRINGE_PURPLE_AMOUNT.id,
        "lens",
        DEFRINGE_PURPLE_AMOUNT,
        Defringe,
        purple_amount
    ),
    control!(
        DEFRINGE,
        "purple_hue_lo",
        DEFRINGE_PURPLE_LO.id,
        "lens",
        DEFRINGE_PURPLE_LO,
        Defringe,
        purple_hue_lo
    ),
    control!(
        DEFRINGE,
        "purple_hue_hi",
        DEFRINGE_PURPLE_HI.id,
        "lens",
        DEFRINGE_PURPLE_HI,
        Defringe,
        purple_hue_hi
    ),
    control!(
        DEFRINGE,
        "green_amount",
        DEFRINGE_GREEN_AMOUNT.id,
        "lens",
        DEFRINGE_GREEN_AMOUNT,
        Defringe,
        green_amount
    ),
    control!(
        DEFRINGE,
        "green_hue_lo",
        DEFRINGE_GREEN_LO.id,
        "lens",
        DEFRINGE_GREEN_LO,
        Defringe,
        green_hue_lo
    ),
    control!(
        DEFRINGE,
        "green_hue_hi",
        DEFRINGE_GREEN_HI.id,
        "lens",
        DEFRINGE_GREEN_HI,
        Defringe,
        green_hue_hi
    ),
];

pub fn find(stage: &str, field: &str) -> Option<&'static Control> {
    CONTROLS
        .iter()
        .find(|c| c.stage == stage && c.field == field)
}

/// The registry as JSON (for `develop.controls`): everything an agent needs to pick a control and
/// a legal value, from the same specs the sliders use.
pub fn listing() -> serde_json::Value {
    serde_json::Value::Array(
        CONTROLS
            .iter()
            .map(|c| {
                serde_json::json!({
                    "id": c.id,
                    "label": c.spec.label,
                    "section": c.section,
                    "stage": c.stage,
                    "field": c.field,
                    "min": c.spec.min,
                    "max": c.spec.max,
                    // An optional control's real default is "unset" (as-shot), not the slider's resting value.
                    "default": if c.optional { serde_json::Value::Null } else { serde_json::json!(c.spec.default) },
                    "optional": c.optional,
                })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::DEVELOP_STAGES;
    use crate::render::DevelopView;

    fn develop() -> Option<DevelopView> {
        Some(DevelopView::new(crate::test_gpu::shared()?))
    }

    /// What the Basic panel does for one slider: load the stage, change the field, store it.
    fn panel_edit(dev: &mut DevelopView, c: &Control, v: f32) {
        match c.stage {
            WB => {
                let mut p: WbParams = dev.stage_params(WB);
                match c.field {
                    "temp_k" => p.temp_k = Some(v),
                    _ => p.tint = v,
                }
                dev.set_stage_params(WB, &p);
            }
            EXPOSURE => {
                let mut p: ExposureParams = dev.stage_params(EXPOSURE);
                p.stops = v;
                dev.set_stage_params(EXPOSURE, &p);
            }
            TONE => {
                let mut p: ToneParams = dev.stage_params(TONE);
                match c.field {
                    "contrast" => p.contrast = v,
                    "highlights" => p.highlights = v,
                    "shadows" => p.shadows = v,
                    "whites" => p.whites = v,
                    _ => p.blacks = v,
                }
                dev.set_stage_params(TONE, &p);
            }
            VIBRANCE => {
                let mut p: VibranceParams = dev.stage_params(VIBRANCE);
                p.amount = v;
                dev.set_stage_params(VIBRANCE, &p);
            }
            _ => {
                let mut p: PresenceParams = dev.stage_params(PRESENCE);
                match c.field {
                    "texture" => p.texture = v,
                    "clarity" => p.clarity = v,
                    "dehaze" => p.dehaze = v,
                    _ => p.saturation = v,
                }
                dev.set_stage_params(PRESENCE, &p);
            }
        }
    }

    #[test]
    fn the_registry_covers_every_develop_stage_with_unique_ids_and_the_slider_ranges() {
        let mut ids: Vec<&str> = CONTROLS.iter().map(|c| c.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), CONTROLS.len(), "ids are unique");
        let mut fields: Vec<(&str, &str)> = CONTROLS.iter().map(|c| (c.stage, c.field)).collect();
        fields.sort_unstable();
        fields.dedup();
        assert_eq!(fields.len(), CONTROLS.len(), "(stage, field) is unique");
        for stage in DEVELOP_STAGES {
            assert!(
                CONTROLS.iter().any(|c| c.stage == *stage),
                "develop.get/set offers `{stage}` but no control writes it"
            );
        }
        for c in CONTROLS {
            assert!(DEVELOP_STAGES.contains(&c.stage), "{}", c.id);
            assert!(c.spec.min < c.spec.max && (c.spec.min..=c.spec.max).contains(&c.spec.default));
            assert!(!c.id.is_empty() && !c.section.is_empty());
        }
        assert!(find(WB, "temp_k").is_some_and(|c| c.optional));
        assert!(find(WB, "tint").is_some_and(|c| !c.optional));
        assert_eq!(listing().as_array().map(Vec::len), Some(CONTROLS.len()));
    }

    #[test]
    fn a_registry_set_equals_a_panel_edit_byte_for_byte() {
        let (Some(mut by_registry), Some(mut by_panel)) = (develop(), develop()) else {
            return;
        };
        for c in CONTROLS.iter().filter(|c| c.section == "basic") {
            // A value inside the range that is not the default, so it really changes the document.
            let v = c.spec.min + (c.spec.max - c.spec.min) * 0.75;
            c.set(&mut by_registry, Some(f64::from(v))).unwrap();
            panel_edit(&mut by_panel, c, v);
            let stage = c.stage;
            assert_eq!(
                serde_json::to_string(&by_registry.document().stages.get(stage)).unwrap(),
                serde_json::to_string(&by_panel.document().stages.get(stage)).unwrap(),
                "stage params of {}",
                c.id
            );
            assert_eq!(
                by_registry.document().stage_hash(stage).map(|h| h.ok()),
                by_panel.document().stage_hash(stage).map(|h| h.ok()),
                "stage hash of {}",
                c.id
            );
            assert_eq!(c.get(&by_registry), Some(v), "{}", c.id);
        }
        assert_eq!(
            by_registry.document().content_hash().ok(),
            by_panel.document().content_hash().ok(),
            "whole-document content hash"
        );
        assert!(by_registry.document().content_hash().is_ok());
    }

    #[test]
    fn every_control_reads_back_what_it_writes_and_every_params_field_has_a_control() {
        // No GPU needed: a control whose stage and Draft variant disagree would write nothing.
        for c in CONTROLS {
            let mut d = Draft::default_for(c.stage).unwrap();
            let v = c.spec.min + (c.spec.max - c.spec.min) * 0.25;
            c.write(&mut d, Some(v));
            assert_eq!(c.read(&d), Some(v), "{} does not round-trip", c.id);
        }
        // A new numeric field on any stage's params must get a control, or `develop.set` could
        // never reach it: walk every stage's default params for numeric leaves (dotted paths, array
        // indices as numbers), plus `temp_k`, which is omitted from the document while as-shot.
        fn leaves(prefix: &str, v: &serde_json::Value, out: &mut Vec<String>) {
            match v {
                serde_json::Value::Number(_) => out.push(prefix.to_owned()),
                serde_json::Value::Object(o) => {
                    for (k, v) in o {
                        let p = if prefix.is_empty() {
                            k.clone()
                        } else {
                            format!("{prefix}.{k}")
                        };
                        leaves(&p, v, out);
                    }
                }
                serde_json::Value::Array(a) => {
                    for (i, v) in a.iter().enumerate() {
                        leaves(&format!("{prefix}.{i}"), v, out);
                    }
                }
                _ => {}
            }
        }
        for stage in DEVELOP_STAGES {
            let mut have = Vec::new();
            leaves("", &Draft::default_for(stage).unwrap().to_json(), &mut have);
            if *stage == WB {
                have.push("temp_k".to_owned());
            }
            for f in &have {
                assert!(find(stage, f).is_some(), "`{stage}`.`{f}` has no control");
            }
            let n = CONTROLS.iter().filter(|c| c.stage == *stage).count();
            assert_eq!(
                n,
                have.len(),
                "`{stage}` has a control for a field it does not have"
            );
        }
        // The listing says "unset" for the optional control, not the slider's resting value.
        let temp = listing()
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["field"] == "temp_k")
            .cloned()
            .unwrap();
        assert!(temp["default"].is_null() && temp["optional"] == true);
    }

    /// An edit that does not go through the registry's accessors: the stage's params as JSON, one
    /// value set by its path, back through `set_stage_params`. A control wired to the wrong field
    /// (or named differently from the serialized field) disagrees with it.
    fn json_path_edit(dev: &mut DevelopView, c: &Control, v: f32) {
        fn go<T>(dev: &mut DevelopView, c: &Control, v: f32)
        where
            T: serde::Serialize + serde::de::DeserializeOwned + Default + 'static,
        {
            let mut val = serde_json::to_value(dev.stage_params::<T>(c.stage)).unwrap();
            let ptr = format!("/{}", c.field.replace('.', "/"));
            *val.pointer_mut(&ptr)
                .unwrap_or_else(|| panic!("no {ptr} in {}", c.stage)) = serde_json::json!(v);
            let p: T = serde_json::from_value(val).unwrap();
            dev.set_stage_params(c.stage, &p);
        }
        match c.stage {
            TONE_CURVE => go::<ToneCurveParams>(dev, c, v),
            HSL => go::<HslParams>(dev, c, v),
            COLOR_GRADE => go::<ColorGradeParams>(dev, c, v),
            CALIBRATION => go::<CalibrationParams>(dev, c, v),
            DEFRINGE => go::<DefringeParams>(dev, c, v),
            SHARPEN => go::<SharpenParams>(dev, c, v),
            NOISE_REDUCTION => go::<NoiseReductionParams>(dev, c, v),
            other => panic!("no JSON oracle for {other}"),
        }
    }

    #[test]
    fn the_other_controls_match_an_independent_json_path_edit_in_params_and_hash() {
        let (Some(mut by_registry), Some(mut by_json)) = (develop(), develop()) else {
            return;
        };
        let others: Vec<&Control> = CONTROLS.iter().filter(|c| c.section != "basic").collect();
        assert!(others.len() > 50, "the non-Basic controls are present");
        for c in others {
            let v = c.spec.min + (c.spec.max - c.spec.min) * 0.75;
            c.set(&mut by_registry, Some(f64::from(v))).unwrap();
            json_path_edit(&mut by_json, c, v);
            assert_eq!(
                serde_json::to_string(&by_registry.document().stages.get(c.stage)).unwrap(),
                serde_json::to_string(&by_json.document().stages.get(c.stage)).unwrap(),
                "stage params of {}",
                c.id
            );
            assert_eq!(c.get(&by_registry), Some(v), "{}", c.id);
        }
        assert_eq!(
            by_registry.document().content_hash().ok(),
            by_json.document().content_hash().ok(),
            "whole-document content hash"
        );
        assert!(by_registry.document().content_hash().is_ok());
    }

    #[test]
    fn nested_fields_are_set_by_dotted_path_in_one_undo_step() {
        use crate::cmd::{develop_stage_get, develop_stage_set};
        let Some(mut dev) = develop() else { return };
        let out = develop_stage_set(
            &mut dev,
            HSL,
            &serde_json::json!({"bands.2.hue": 0.3, "bands.2.saturation": -0.2}),
        )
        .unwrap();
        assert_eq!(out["bands"][2]["hue"], f64::from(0.3f32));
        let g = develop_stage_get(&dev, HSL).unwrap();
        assert!((g["bands"][2]["saturation"].as_f64().unwrap() + 0.2).abs() < 1e-6);
        assert_eq!(g["bands"][3]["hue"], 0.0, "other bands are untouched");
        // Two fields, one undo step.
        assert!(dev.undo());
        assert_eq!(
            develop_stage_get(&dev, HSL).unwrap()["bands"][2]["hue"],
            0.0
        );
        // Out-of-range and unknown paths are refused before anything is written.
        assert!(develop_stage_set(
            &mut dev,
            COLOR_GRADE,
            &serde_json::json!({"shadows.hue": 361.0})
        )
        .is_err());
        assert!(develop_stage_set(
            &mut dev,
            COLOR_GRADE,
            &serde_json::json!({"shadows.hue": 200.0})
        )
        .is_ok());
        assert!(
            develop_stage_set(&mut dev, HSL, &serde_json::json!({"bands.8.hue": 0.1})).is_err()
        );
        assert!(develop_stage_set(&mut dev, HSL, &serde_json::json!({"bands": 1})).is_err());
    }

    #[test]
    fn a_multi_field_patch_through_develop_set_equals_the_panel_and_is_one_undo_step() {
        use crate::cmd::develop_stage_set;
        let (Some(mut by_set), Some(mut by_panel)) = (develop(), develop()) else {
            return;
        };
        // Make the later undo step unambiguous: every write is its own step.
        by_panel.disable_gesture_merging();
        develop_stage_set(
            &mut by_set,
            TONE,
            &serde_json::json!({"contrast": 0.5, "shadows": -0.25}),
        )
        .unwrap();
        let mut tone: ToneParams = by_panel.stage_params(TONE);
        tone.contrast = 0.5;
        tone.shadows = -0.25;
        by_panel.set_stage_params(TONE, &tone);
        assert_eq!(
            by_set.document().content_hash().ok(),
            by_panel.document().content_hash().ok()
        );
        assert!(by_set.document().content_hash().is_ok());
        // One step: a single undo reverts both fields.
        assert!(by_set.undo());
        let after: ToneParams = by_set.stage_params(TONE);
        assert_eq!(after, ToneParams::default());
        // Setting a value back to the default leaves the document exactly as untouched (the entry
        // is removed, as the panel's drag-back-to-start does).
        let untouched = develop().unwrap().document().content_hash().ok();
        develop_stage_set(&mut by_set, TONE, &serde_json::json!({"contrast": 0.5})).unwrap();
        develop_stage_set(&mut by_set, TONE, &serde_json::json!({"contrast": 0.0})).unwrap();
        assert_eq!(by_set.document().content_hash().ok(), untouched);
    }

    #[test]
    fn the_optional_temperature_clears_back_to_as_shot_and_others_refuse_null() {
        let Some(mut dev) = develop() else { return };
        let temp = find(WB, "temp_k").unwrap();
        temp.set(&mut dev, Some(6500.0)).unwrap();
        assert_eq!(temp.get(&dev), Some(6500.0));
        temp.set(&mut dev, None).unwrap();
        assert_eq!(temp.get(&dev), None);
        assert!(find(WB, "tint").unwrap().set(&mut dev, None).is_err());
        assert!(temp.set(&mut dev, Some(1999.0)).is_err());
        assert!(temp.set(&mut dev, Some(f64::NAN)).is_err());
    }
}
