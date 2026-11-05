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

use nicti_tapetum::coat::{ExposureParams, PresenceParams, ToneParams, VibranceParams, WbParams};
use nicti_tapetum::stages::{EXPOSURE, PRESENCE, TONE, VIBRANCE, WB};

use crate::develop_panel::{
    BLACKS, CLARITY, CONTRAST, DEHAZE, EXPOSURE_STOPS, HIGHLIGHTS, SATURATION, SHADOWS, TEMP,
    TEXTURE, TINT, VIBRANCE_AMOUNT, WHITES,
};
use crate::fur::SliderSpec;
use crate::render::DevelopDoc;

/// One stage's typed params, loaded once so several fields can change in one typed write (one
/// undo step) and stored back through `set_stage_params`.
pub enum Draft {
    Wb(WbParams),
    Exposure(ExposureParams),
    Tone(ToneParams),
    Vibrance(VibranceParams),
    Presence(PresenceParams),
}

impl Draft {
    pub fn load(dev: &DevelopDoc, stage: &str) -> Option<Draft> {
        Some(match stage {
            WB => Draft::Wb(dev.stage_params(WB)),
            EXPOSURE => Draft::Exposure(dev.stage_params(EXPOSURE)),
            TONE => Draft::Tone(dev.stage_params(TONE)),
            VIBRANCE => Draft::Vibrance(dev.stage_params(VIBRANCE)),
            PRESENCE => Draft::Presence(dev.stage_params(PRESENCE)),
            _ => return None,
        })
    }

    /// The stage's default params, with no document: lets the registry be checked without a view.
    #[cfg(test)]
    fn default_for(stage: &str) -> Option<Draft> {
        Some(match stage {
            WB => Draft::Wb(WbParams::default()),
            EXPOSURE => Draft::Exposure(ExposureParams::default()),
            TONE => Draft::Tone(ToneParams::default()),
            VIBRANCE => Draft::Vibrance(VibranceParams::default()),
            PRESENCE => Draft::Presence(PresenceParams::default()),
            _ => return None,
        })
    }

    /// Writes the draft back through the undo history (a no-op when nothing changed).
    pub fn store(&self, dev: &mut DevelopDoc) {
        match self {
            Draft::Wb(p) => dev.set_stage_params(WB, p),
            Draft::Exposure(p) => dev.set_stage_params(EXPOSURE, p),
            Draft::Tone(p) => dev.set_stage_params(TONE, p),
            Draft::Vibrance(p) => dev.set_stage_params(VIBRANCE, p),
            Draft::Presence(p) => dev.set_stage_params(PRESENCE, p),
        }
    }
}

pub struct Control {
    /// The panel slider's id, for example `basic.exposure`.
    pub id: &'static str,
    /// The document stage and the field name inside its params (what `develop.get/set` use).
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

macro_rules! control {
    ($stage:expr, $field:literal, $spec:expr, $variant:ident, $name:ident) => {
        Control {
            id: $spec.id,
            stage: $stage,
            field: $field,
            section: "basic",
            spec: &$spec,
            optional: false,
            read: |d| match d {
                Draft::$variant(p) => Some(p.$name),
                _ => None,
            },
            write: |d, v| {
                if let (Draft::$variant(p), Some(v)) = (d, v) {
                    p.$name = v;
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
    control!(WB, "tint", TINT, Wb, tint),
    control!(EXPOSURE, "stops", EXPOSURE_STOPS, Exposure, stops),
    control!(TONE, "contrast", CONTRAST, Tone, contrast),
    control!(TONE, "highlights", HIGHLIGHTS, Tone, highlights),
    control!(TONE, "shadows", SHADOWS, Tone, shadows),
    control!(TONE, "whites", WHITES, Tone, whites),
    control!(TONE, "blacks", BLACKS, Tone, blacks),
    control!(VIBRANCE, "amount", VIBRANCE_AMOUNT, Vibrance, amount),
    control!(PRESENCE, "texture", TEXTURE, Presence, texture),
    control!(PRESENCE, "clarity", CLARITY, Presence, clarity),
    control!(PRESENCE, "dehaze", DEHAZE, Presence, dehaze),
    control!(PRESENCE, "saturation", SATURATION, Presence, saturation),
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
            assert_eq!(c.id, c.spec.id);
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
        for c in CONTROLS {
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
        // A new field on a params struct must get a control, or `develop.set` could never reach it.
        let fields = |v: serde_json::Value| -> Vec<String> {
            v.as_object().unwrap().keys().cloned().collect()
        };
        let mut serialized = std::collections::BTreeMap::new();
        serialized.insert(
            WB,
            fields(serde_json::to_value(WbParams::default()).unwrap()),
        );
        serialized.insert(
            EXPOSURE,
            fields(serde_json::to_value(ExposureParams::default()).unwrap()),
        );
        serialized.insert(
            TONE,
            fields(serde_json::to_value(ToneParams::default()).unwrap()),
        );
        serialized.insert(
            VIBRANCE,
            fields(serde_json::to_value(VibranceParams::default()).unwrap()),
        );
        serialized.insert(
            PRESENCE,
            fields(serde_json::to_value(PresenceParams::default()).unwrap()),
        );
        for (stage, mut have) in serialized {
            if stage == WB {
                have.push("temp_k".to_owned()); // omitted while as-shot
            }
            for f in &have {
                assert!(find(stage, f).is_some(), "`{stage}`.`{f}` has no control");
            }
            let n = CONTROLS.iter().filter(|c| c.stage == stage).count();
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
