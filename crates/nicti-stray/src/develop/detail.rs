//! Detail panel: sharpening and classic noise reduction. LRC's raw defaults (Sharpness 40,
//! ColorNoiseReduction 25, ...) are imported faithfully, because that is what the photo looks like
//! in LRC. Edge Masking and the separate colour-detail/smoothness/contrast sliders are not modelled
//! by nicti (`SharpenParams`/`NoiseReductionParams` doc comments) and stay untranslated.

use nicti_tapetum::coat::{NoiseReductionParams, SharpenParams};
use nicti_tapetum::stages::{NOISE_REDUCTION, SHARPEN};

use super::Tx;

pub(super) fn apply(tx: &mut Tx) {
    let enabled = tx.enabled("EnableDetail");
    let default = SharpenParams::default();
    let sharpen = SharpenParams {
        amount: (tx.num("Sharpness").unwrap_or(0.0) / 100.0).clamp(0.0, 1.5) as f32,
        radius_px: tx
            .num("SharpenRadius")
            .map_or(default.radius_px, |v| v.clamp(0.5, 3.0) as f32),
        detail: tx
            .num("SharpenDetail")
            .map_or(default.detail, |v| (v / 100.0).clamp(0.0, 1.0) as f32),
    };
    let nr = NoiseReductionParams {
        luminance: (tx.num("LuminanceSmoothing").unwrap_or(0.0) / 100.0).clamp(0.0, 1.0) as f32,
        color: (tx.num("ColorNoiseReduction").unwrap_or(0.0) / 100.0).clamp(0.0, 1.0) as f32,
        detail: (tx.num("LuminanceNoiseReductionDetail").unwrap_or(0.0) / 100.0).clamp(0.0, 1.0)
            as f32,
    };
    if !enabled {
        return;
    }
    // Sharpen radius/detail alone (amount 0) are inert; keep the entry only when it does something.
    if !sharpen.is_noop() {
        tx.put(SHARPEN, sharpen);
    }
    if !nr.is_noop() {
        tx.put(NOISE_REDUCTION, nr);
    }
}

#[cfg(test)]
mod tests {
    use crate::develop::{translate, Context};
    use nicti_tapetum::stages::{NOISE_REDUCTION, SHARPEN};

    #[test]
    fn sharpen_and_noise_reduction_scale_and_keep_radius_in_pixels() {
        let t = translate(
            "s = { Sharpness = 40, SharpenRadius = 1.5, SharpenDetail = 25, \
             LuminanceSmoothing = 30, ColorNoiseReduction = 25, LuminanceNoiseReductionDetail = 50 }",
            &Context::default(),
        )
        .unwrap();
        let s = &t.document.stages[SHARPEN].params;
        assert_eq!((s["amount"].as_f64().unwrap() * 100.0).round(), 40.0);
        assert_eq!(s["radius_px"], 1.5);
        assert_eq!((s["detail"].as_f64().unwrap() * 100.0).round(), 25.0);
        let n = &t.document.stages[NOISE_REDUCTION].params;
        assert_eq!((n["luminance"].as_f64().unwrap() * 100.0).round(), 30.0);
        assert_eq!((n["color"].as_f64().unwrap() * 100.0).round(), 25.0);
    }

    #[test]
    fn zero_amount_writes_no_sharpen_entry_and_disabled_panel_writes_nothing() {
        let t = translate(
            "s = { Sharpness = 0, SharpenRadius = 2 }",
            &Context::default(),
        )
        .unwrap();
        assert!(t.document.stages.is_empty());
        let t = translate(
            "s = { EnableDetail = false, Sharpness = 80, ColorNoiseReduction = 50 }",
            &Context::default(),
        )
        .unwrap();
        assert!(t.document.stages.is_empty());
    }
}
