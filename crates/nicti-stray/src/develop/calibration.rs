//! Camera Calibration panel (`RedHue`/`RedSaturation`/`GreenHue`/`GreenSaturation`/`BlueHue`/
//! `BlueSaturation`/`ShadowTint`, all -100..100) (#381), onto `nicti.calibration`.
//!
//! Values map numerically only (/100): nicti folds the primaries into the camera matrix with its
//! own, untuned strength constants (`color::calibrate_matrix`), so a calibrated photo will not match
//! LRC's render until the reference-machine parity pass fits them. All-zero is LRC's untouched state
//! and writes nothing.

use nicti_tapetum::coat::CalibrationParams;
use nicti_tapetum::stages::CALIBRATION;

use super::Tx;

pub(super) fn apply(tx: &mut Tx) {
    // Every key is read (and so consumed) even when the rest is zero.
    let mut unit = |key: &str| (tx.num(key).unwrap_or(0.0) / 100.0).clamp(-1.0, 1.0) as f32;
    let params = CalibrationParams {
        red_hue: unit("RedHue"),
        red_sat: unit("RedSaturation"),
        green_hue: unit("GreenHue"),
        green_sat: unit("GreenSaturation"),
        blue_hue: unit("BlueHue"),
        blue_sat: unit("BlueSaturation"),
        shadow_tint: unit("ShadowTint"),
    };
    tx.put(CALIBRATION, params);
}

#[cfg(test)]
mod tests {
    use super::super::{close, translate, Context};

    fn run(text: &str) -> super::super::Translation {
        translate(text, &Context::default()).unwrap()
    }

    #[test]
    fn calibration_sliders_map_to_unit_range() {
        let t = run(
            "s = { RedHue = 50, RedSaturation = -100, GreenHue = 20, GreenSaturation = 0, \
                     BlueHue = -30, BlueSaturation = 10, ShadowTint = -40 }",
        );
        let p = &t.document.stages["nicti.calibration"].params;
        assert!(close(&p["red_hue"], 0.5));
        assert!(close(&p["red_sat"], -1.0));
        assert!(close(&p["green_hue"], 0.2));
        assert!(close(&p["blue_hue"], -0.3));
        assert!(close(&p["blue_sat"], 0.1));
        assert!(close(&p["shadow_tint"], -0.4));
        assert!(t.untranslated.is_empty(), "{:?}", t.untranslated);
    }

    #[test]
    fn untouched_calibration_writes_no_stage_and_is_not_untranslated() {
        let t = run("s = { RedHue = 0, RedSaturation = 0, ShadowTint = 0, Exposure2012 = 0 }");
        assert!(!t.document.stages.contains_key("nicti.calibration"));
        assert!(t.untranslated.is_empty(), "{:?}", t.untranslated);
    }

    #[test]
    fn out_of_range_values_are_clamped() {
        let t = run("s = { RedHue = 900, ShadowTint = -900 }");
        let p = &t.document.stages["nicti.calibration"].params;
        assert!(close(&p["red_hue"], 1.0));
        assert!(close(&p["shadow_tint"], -1.0));
    }
}
