//! HSL panel: LRC's eight hue bands, in the same order as `HslParams::bands`.

use nicti_tapetum::coat::{HslBand, HslParams};
use nicti_tapetum::stages::HSL;

use super::Tx;

const BANDS: [&str; 8] = [
    "Red", "Orange", "Yellow", "Green", "Aqua", "Blue", "Purple", "Magenta",
];

pub(super) fn apply(tx: &mut Tx) {
    // Read (and so consume) every key even when the panel is switched off.
    let enabled = tx.enabled("EnableColorAdjustments");
    let mut params = HslParams::default();
    for (i, band) in BANDS.iter().enumerate() {
        let get = |tx: &mut Tx, prefix: &str| {
            (tx.num(&format!("{prefix}{band}")).unwrap_or(0.0) / 100.0).clamp(-1.0, 1.0) as f32
        };
        params.bands[i] = HslBand {
            hue: get(tx, "HueAdjustment"),
            saturation: get(tx, "SaturationAdjustment"),
            luminance: get(tx, "LuminanceAdjustment"),
        };
    }
    if enabled {
        tx.put(HSL, params);
    }
}

#[cfg(test)]
mod tests {
    use crate::develop::{translate, Context};
    use nicti_tapetum::stages::HSL;

    #[test]
    fn bands_map_by_name_in_nictis_band_order() {
        let t = translate(
            "s = { HueAdjustmentRed = 10, SaturationAdjustmentAqua = -30, \
             LuminanceAdjustmentMagenta = 50 }",
            &Context::default(),
        )
        .unwrap();
        let bands = &t.document.stages[HSL].params["bands"];
        assert!(crate::develop::close(&bands[0]["hue"], 0.1));
        assert!(crate::develop::close(&bands[4]["saturation"], -0.3));
        assert_eq!(bands[7]["luminance"], 0.5);
        assert!(t.untranslated.is_empty());
    }

    #[test]
    fn a_disabled_panel_writes_nothing_but_consumes_its_keys() {
        let t = translate(
            "s = { EnableColorAdjustments = false, HueAdjustmentRed = 10 }",
            &Context::default(),
        )
        .unwrap();
        assert!(t.document.stages.is_empty());
        assert!(t.untranslated.is_empty());
    }
}
