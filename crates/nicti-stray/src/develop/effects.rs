//! Effects panel (#380): the post-crop vignette and film grain, onto `nicti-tapetum`'s
//! `EffectsParams` (stage `nicti.effects`).
//!
//! LRC writes the panel's slider values into every image, including untouched ones (`GrainSeed` is
//! a per-image random number, the midpoint/feather/size/frequency sliders sit at their defaults), so
//! nothing here is taken from a slider whose *amount* is zero: the vignette's shape sliders only
//! count when `PostCropVignetteAmount` is non-zero, the grain's only when `GrainAmount` is -- else an
//! untouched image would import as "edited". `EnableEffects = false` turns both off.
//!
//! Ranges are LRC's (PV2012): amounts -100..100 (grain 0..100), the shape sliders 0..100,
//! roundness -100..100, `PostCropVignetteStyle` 1 = Highlight Priority, 2 = Color Priority,
//! 3 = Paint Overlay. `OverrideLookVignette` (a vignette baked into a Look profile) is not modelled
//! and, when real, stays in the untranslated list.

use nicti_tapetum::coat::{EffectsParams, VignetteStyle};
use nicti_tapetum::stages::EFFECTS;

use super::Tx;

/// An LRC 0..100 slider to 0..1.
fn pct(v: f64) -> f32 {
    (v / 100.0).clamp(0.0, 1.0) as f32
}

/// An LRC -100..100 slider to -1..1.
fn signed(v: f64) -> f32 {
    (v / 100.0).clamp(-1.0, 1.0) as f32
}

pub(super) fn apply(tx: &mut Tx) {
    let enabled = tx.enabled("EnableEffects");
    let default = EffectsParams::default();

    // Every key is read (so each counts as consumed) before anything decides to ignore it.
    let vignette_amount = tx.num("PostCropVignetteAmount").unwrap_or(0.0);
    let midpoint = tx.num("PostCropVignetteMidpoint");
    let feather = tx.num("PostCropVignetteFeather");
    let roundness = tx.num("PostCropVignetteRoundness");
    let style = tx.num("PostCropVignetteStyle");
    let highlights = tx.num("PostCropVignetteHighlightContrast");
    let grain_amount = tx.num("GrainAmount").unwrap_or(0.0);
    let size = tx.num("GrainSize");
    let roughness = tx.num("GrainFrequency");
    let seed = tx.num("GrainSeed");
    if !enabled {
        return;
    }

    let mut effects = EffectsParams::default();
    if vignette_amount != 0.0 {
        effects.vignette_amount = signed(vignette_amount);
        effects.vignette_midpoint = midpoint.map_or(default.vignette_midpoint, pct);
        effects.vignette_feather = feather.map_or(default.vignette_feather, pct);
        effects.vignette_roundness = roundness.map_or(0.0, signed);
        effects.vignette_highlights = highlights.map_or(0.0, pct);
        effects.vignette_style = match style.map(f64::round) {
            Some(2.0) => VignetteStyle::ColorPriority,
            Some(3.0) => VignetteStyle::PaintOverlay,
            _ => VignetteStyle::HighlightPriority,
        };
    }
    if grain_amount != 0.0 {
        effects.grain_amount = pct(grain_amount);
        effects.grain_size = size.map_or(default.grain_size, pct);
        effects.grain_roughness = roughness.map_or(default.grain_roughness, pct);
        // The seed only picks the pattern: wrap an out-of-range value rather than reject it.
        effects.grain_seed = seed.map_or(0, |s| s as i64 as u32);
    }
    tx.put(EFFECTS, effects);
}

#[cfg(test)]
mod tests {
    use crate::develop::{close, translate, Context};
    use nicti_tapetum::stages::EFFECTS;

    fn tr(text: &str) -> crate::develop::Translation {
        translate(text, &Context::default()).unwrap()
    }

    #[test]
    fn the_vignette_maps_scale_style_and_shape() {
        let t = tr(
            "s = { PostCropVignetteAmount = -40, PostCropVignetteMidpoint = 30, \
                    PostCropVignetteFeather = 80, PostCropVignetteRoundness = -25, \
                    PostCropVignetteStyle = 3, PostCropVignetteHighlightContrast = 60 }",
        );
        let p = &t.document.stages[EFFECTS].params;
        assert!(close(&p["vignette_amount"], -0.4));
        assert!(close(&p["vignette_midpoint"], 0.3));
        assert!(close(&p["vignette_feather"], 0.8));
        assert!(close(&p["vignette_roundness"], -0.25));
        assert!(close(&p["vignette_highlights"], 0.6));
        assert_eq!(p["vignette_style"], "paint_overlay");
        // No grain asked for: the grain fields stay at their defaults.
        assert_eq!(p["grain_amount"], 0.0);
        assert!(t.untranslated.is_empty(), "{:?}", t.untranslated);
    }

    #[test]
    fn the_styles_map_and_an_unknown_one_falls_back_to_highlight_priority() {
        for (n, want) in [
            (1, "highlight_priority"),
            (2, "color_priority"),
            (3, "paint_overlay"),
            (9, "highlight_priority"),
        ] {
            let t = tr(&format!(
                "s = {{ PostCropVignetteAmount = 10, PostCropVignetteStyle = {n} }}"
            ));
            assert_eq!(t.document.stages[EFFECTS].params["vignette_style"], want);
        }
    }

    #[test]
    fn grain_maps_amount_size_roughness_and_seed() {
        let t =
            tr("s = { GrainAmount = 50, GrainSize = 40, GrainFrequency = 70, GrainSeed = 123456 }");
        let p = &t.document.stages[EFFECTS].params;
        assert!(close(&p["grain_amount"], 0.5));
        assert!(close(&p["grain_size"], 0.4));
        assert!(close(&p["grain_roughness"], 0.7));
        assert_eq!(p["grain_seed"], 123_456);
        assert_eq!(p["vignette_amount"], 0.0);
        assert!(t.untranslated.is_empty(), "{:?}", t.untranslated);
    }

    #[test]
    fn a_huge_or_negative_seed_wraps_instead_of_failing() {
        let t = tr("s = { GrainAmount = 10, GrainSeed = 4294967297 }");
        assert_eq!(t.document.stages[EFFECTS].params["grain_seed"], 1);
        let t = tr("s = { GrainAmount = 10, GrainSeed = -1 }");
        assert_eq!(t.document.stages[EFFECTS].params["grain_seed"], u32::MAX);
    }

    #[test]
    fn an_untouched_image_with_lrcs_default_slider_values_imports_no_effects() {
        // What LRC writes into every image: a random per-image seed and the sliders at their
        // defaults, with both amounts at zero.
        let t = tr(
            "s = { PostCropVignetteAmount = 0, PostCropVignetteMidpoint = 50, \
                    PostCropVignetteFeather = 50, PostCropVignetteRoundness = 0, \
                    PostCropVignetteStyle = 1, GrainAmount = 0, GrainSize = 25, \
                    GrainFrequency = 50, GrainSeed = 987654321 }",
        );
        assert!(t.document.stages.is_empty(), "{:?}", t.document.stages);
        assert!(t.untranslated.is_empty(), "{:?}", t.untranslated);
    }

    #[test]
    fn a_disabled_effects_panel_imports_nothing_but_still_consumes_its_keys() {
        let t = tr("s = { EnableEffects = false, PostCropVignetteAmount = -60, GrainAmount = 40 }");
        assert!(t.document.stages.is_empty());
        assert!(t.untranslated.is_empty(), "{:?}", t.untranslated);
    }

    #[test]
    fn a_lone_roundness_without_an_amount_is_not_an_edit() {
        let t = tr("s = { PostCropVignetteAmount = 0, PostCropVignetteRoundness = 70 }");
        assert!(t.document.stages.is_empty());
    }
}
