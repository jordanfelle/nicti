//! Lens Corrections panel (#428): the two parts nicti models -- `AutoLateralCA` onto
//! `LensParams::remove_ca` (stage `nicti.lens`) and the Defringe sliders onto `DefringeParams`
//! (stage `nicti.defringe`).
//!
//! LRC writes the panel's slider values into every image, so, as in `effects.rs`, a Defringe
//! channel's hue sliders only count when its *amount* is non-zero (else an untouched image would
//! import as "edited"), and `EnableLensCorrections = false` turns the whole panel off.
//!
//! Ranges are LRC's (PV2012): amounts 0..20, hue sliders 0..100. Left untranslated on purpose:
//! `LensProfileEnable`/`LensProfileSetup` and the other profile and manual-distortion keys (#382:
//! nicti has no lens database yet; a DNG's own embedded profile is applied regardless), and
//! `Perspective*`/`Upright*` (#427).

use nicti_tapetum::coat::{DefringeParams, LensParams};
use nicti_tapetum::stages::{DEFRINGE, LENS};

use super::Tx;

/// An LRC 0..100 slider to 0..1.
fn pct(v: f64) -> f32 {
    (v / 100.0).clamp(0.0, 1.0) as f32
}

/// An LRC 0..20 Defringe amount to 0..1.
fn amount(v: f64) -> f32 {
    (v / 20.0).clamp(0.0, 1.0) as f32
}

pub(super) fn apply(tx: &mut Tx) {
    let enabled = tx.enabled("EnableLensCorrections");
    let default = DefringeParams::default();

    // Every key is read (so each counts as consumed) before anything decides to ignore it.
    let auto_ca = tx.num("AutoLateralCA").unwrap_or(0.0);
    let purple_amount = tx.num("DefringePurpleAmount").unwrap_or(0.0);
    let purple_lo = tx.num("DefringePurpleHueLo");
    let purple_hi = tx.num("DefringePurpleHueHi");
    let green_amount = tx.num("DefringeGreenAmount").unwrap_or(0.0);
    let green_lo = tx.num("DefringeGreenHueLo");
    let green_hi = tx.num("DefringeGreenHueHi");
    if !enabled {
        return;
    }

    tx.put(
        LENS,
        LensParams {
            remove_ca: auto_ca != 0.0,
            ..LensParams::default()
        },
    );

    let mut defringe = DefringeParams::default();
    if purple_amount > 0.0 {
        defringe.purple_amount = amount(purple_amount);
        defringe.purple_hue_lo = purple_lo.map_or(default.purple_hue_lo, pct);
        defringe.purple_hue_hi = purple_hi.map_or(default.purple_hue_hi, pct);
    }
    if green_amount > 0.0 {
        defringe.green_amount = amount(green_amount);
        defringe.green_hue_lo = green_lo.map_or(default.green_hue_lo, pct);
        defringe.green_hue_hi = green_hi.map_or(default.green_hue_hi, pct);
    }
    tx.put(DEFRINGE, defringe.sanitized());
}

#[cfg(test)]
mod tests {
    use crate::develop::{close, translate, Context};
    use nicti_tapetum::stages::{DEFRINGE, LENS};

    fn tr(text: &str) -> crate::develop::Translation {
        translate(text, &Context::default()).unwrap()
    }

    #[test]
    fn auto_lateral_ca_maps_to_remove_ca() {
        let t = tr("s = { AutoLateralCA = 1 }");
        assert_eq!(t.document.stages[LENS].params["remove_ca"], true);
        assert!(t.untranslated.is_empty(), "{:?}", t.untranslated);
        // Off is the default: no entry at all.
        let t = tr("s = { AutoLateralCA = 0 }");
        assert!(!t.document.stages.contains_key(LENS));
    }

    #[test]
    fn defringe_maps_amounts_and_hue_windows() {
        let t = tr("s = { DefringePurpleAmount = 8, DefringePurpleHueLo = 20, \
                    DefringePurpleHueHi = 90, DefringeGreenAmount = 20, \
                    DefringeGreenHueLo = 10, DefringeGreenHueHi = 80 }");
        let p = &t.document.stages[DEFRINGE].params;
        assert!(close(&p["purple_amount"], 0.4));
        assert!(close(&p["purple_hue_lo"], 0.2));
        assert!(close(&p["purple_hue_hi"], 0.9));
        assert!(close(&p["green_amount"], 1.0));
        assert!(close(&p["green_hue_lo"], 0.1));
        assert!(close(&p["green_hue_hi"], 0.8));
        assert!(t.untranslated.is_empty(), "{:?}", t.untranslated);
    }

    #[test]
    fn one_channel_leaves_the_other_at_its_defaults() {
        let t = tr("s = { DefringeGreenAmount = 10, DefringePurpleHueLo = 5, DefringePurpleHueHi = 95 }");
        let p = &t.document.stages[DEFRINGE].params;
        assert!(close(&p["green_amount"], 0.5));
        assert_eq!(p["purple_amount"], 0.0);
        // The purple hue sliders only count when the purple amount is non-zero.
        assert!(close(&p["purple_hue_lo"], 0.3));
        assert!(close(&p["purple_hue_hi"], 0.7));
    }

    #[test]
    fn an_untouched_image_with_lrcs_default_slider_values_imports_nothing() {
        // What LRC writes into every image: the hue sliders at their defaults, amounts at zero.
        let t = tr("s = { DefringePurpleAmount = 0, DefringePurpleHueLo = 30, \
                    DefringePurpleHueHi = 70, DefringeGreenAmount = 0, \
                    DefringeGreenHueLo = 40, DefringeGreenHueHi = 60, AutoLateralCA = 0 }");
        assert!(!t.document.stages.contains_key(DEFRINGE));
        assert!(!t.document.stages.contains_key(LENS));
        assert!(t.untranslated.is_empty(), "{:?}", t.untranslated);
    }

    #[test]
    fn a_disabled_panel_imports_nothing_but_still_consumes_its_keys() {
        let t = tr("s = { EnableLensCorrections = false, AutoLateralCA = 1, \
                    DefringePurpleAmount = 10 }");
        assert!(!t.document.stages.contains_key(DEFRINGE));
        assert!(!t.document.stages.contains_key(LENS));
        assert!(t.untranslated.is_empty(), "{:?}", t.untranslated);
    }

    #[test]
    fn out_of_range_values_are_clamped_not_rejected() {
        let t = tr("s = { DefringePurpleAmount = 500, DefringePurpleHueLo = -40, \
                    DefringePurpleHueHi = 900 }");
        let p = &t.document.stages[DEFRINGE].params;
        assert!(close(&p["purple_amount"], 1.0));
        assert!(close(&p["purple_hue_lo"], 0.0));
        assert!(close(&p["purple_hue_hi"], 1.0));
    }
}
