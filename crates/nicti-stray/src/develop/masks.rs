//! `MaskGroupBasedCorrections` -> `nicti.masks` (ADR-0048/0049, the #49 mapping note on #62).
//! Everything below was checked against the real v13 catalog's own data, not only the issue note:
//!
//! - A correction is `{ What = "Correction", CorrectionMasks = { ... }, Local* = ..., }`; a
//!   component is `{ What = "Mask/Gradient" | "Mask/CircularGradient" | "Mask/Image" |
//!   "Mask/Aggregate" (a brush wrapper) | "Mask/Paint", MaskBlendMode, MaskInverted, ... }`.
//! - `Local*` sliders are -1..1 (like nicti's), **except** `LocalExposure2012`, which is stored as
//!   stops/4 (every real value is a multiple of 0.05 only after x4: 0.0625, 0.075, -0.175 ...), so
//!   nicti's `exposure` (stops) = value x 4.
//! - AI rasters (`Mask/Image`, `FullMaskSize`) have the *uncropped* frame's aspect on cropped
//!   photos, so mask space is the uncropped frame -- the same space `MaskParams` documents.
//! - A radial's `MaskInverted = true` pairs with a darkening (negative) exposure in 1,804 of 1,863
//!   real cases, i.e. "effect applied outside" -- nicti's own `invert`.
//!
//! Translated: linear/radial gradients (upright originals only -- whether LRC's coordinates follow
//! the sensor or the displayed orientation is not pinned for rotated files), Subject/Sky/Background
//! AI masks (a recipe, no geometry, so any orientation). Not translated, and the *whole correction*
//! is then skipped rather than applied with the wrong selection: brushes (`Mask/Aggregate`/
//! `Mask/Paint`, radius units unverified), People/object categories, range masks, and any unknown
//! kind. Skips are counted; the verbatim text stays in provenance.

use nicti_siamese::providers::recipe_for;
use nicti_stalk::SegmentTarget;
use nicti_tapetum::mask::params::{
    LocalAdjust, LocalCorrection, MaskComponent, MaskGroup, MaskParams, MaskSource, Op, TintColor,
    MAX_CORRECTIONS,
};
use nicti_tapetum::stages::MASKS;

use super::{field, field_bool, field_num, field_str, items, Tx};
use agprefs::Value;

pub(super) fn apply(tx: &mut Tx) {
    let root = tx.root;
    let enabled = tx.enabled("EnableMaskGroupBasedCorrections");
    tx.take("MaskGroupBasedCorrections");
    let Some(list) = field(root, "MaskGroupBasedCorrections") else {
        return;
    };
    let upright = tx
        .ctx
        .orientation
        .as_deref()
        .is_some_and(|o| o.eq_ignore_ascii_case("AB"));
    let dims = tx.ctx.width.zip(tx.ctx.height);

    let mut params = MaskParams::default();
    for (index, correction) in items(list).into_iter().enumerate() {
        if params.corrections.len() >= MAX_CORRECTIONS {
            tx.stats.mask_corrections_skipped += 1;
            continue;
        }
        match correction_for(correction, index, upright, dims, &mut tx.stats.ai_masks) {
            Outcome::Translated(c) => {
                tx.stats.mask_corrections += 1;
                params.corrections.push(c);
            }
            Outcome::Noop => {}
            Outcome::Skipped { components } => {
                tx.stats.mask_corrections_skipped += 1;
                tx.stats.mask_components_skipped += components;
            }
        }
    }
    if enabled && !params.corrections.is_empty() {
        tx.put(MASKS, params.sanitized());
    }
}

enum Outcome {
    Translated(LocalCorrection),
    /// Inactive, or every adjustment is zero: nothing to apply (and no AI bake to trigger).
    Noop,
    Skipped {
        components: u64,
    },
}

fn correction_for(
    c: &Value<'_>,
    index: usize,
    upright: bool,
    dims: Option<(f32, f32)>,
    ai_masks: &mut u64,
) -> Outcome {
    if field_bool(c, "CorrectionActive") == Some(false) {
        return Outcome::Noop;
    }
    // A range mask narrows the selection; applying the correction without it would be wrong.
    if let Some(range) = field(c, "CorrectionRangeMask") {
        if field_num(range, "Type").is_some_and(|t| t != 0.0) {
            return Outcome::Skipped { components: 0 };
        }
    }
    let masks = field(c, "CorrectionMasks").map(items).unwrap_or_default();
    let mut components = Vec::new();
    let mut ai_in_this = 0;
    for m in masks {
        if field_bool(m, "MaskActive") == Some(false) {
            continue;
        }
        match component_for(m, upright, dims) {
            Some((component, is_ai)) => {
                ai_in_this += u64::from(is_ai);
                components.push(component);
            }
            // One untranslatable component changes the selection: skip the whole correction.
            None => return Outcome::Skipped { components: 1 },
        }
    }
    if components.is_empty() {
        return Outcome::Noop;
    }
    let adjust = adjust_for(c);
    if adjust.is_noop() {
        return Outcome::Noop;
    }
    *ai_masks += ai_in_this;
    let id = field_str(c, "CorrectionSyncID")
        .or_else(|| field_str(c, "CorrectionID"))
        .map_or_else(|| format!("lrc-{index}"), str::to_string);
    Outcome::Translated(LocalCorrection {
        id,
        name: field_str(c, "CorrectionName").unwrap_or("").to_string(),
        enabled: true,
        amount: field_num(c, "CorrectionAmount").map_or(1.0, |a| a.clamp(0.0, 1.0) as f32),
        mask: MaskGroup { components },
        adjust,
    })
}

/// `(component, is_ai)`, or `None` when this component kind/geometry can't be translated.
fn component_for(
    m: &Value<'_>,
    upright: bool,
    dims: Option<(f32, f32)>,
) -> Option<(MaskComponent, bool)> {
    let inverted = field_bool(m, "MaskInverted").unwrap_or(false);
    let op = match field_num(m, "MaskBlendMode").unwrap_or(0.0) as i64 {
        0 => Op::Add,
        1 => Op::Subtract,
        2 => Op::Intersect,
        _ => return None,
    };
    let num = |k: &str| field_num(m, k).map(|v| v as f32);
    let (source, invert) = match field_str(m, "What")? {
        "Mask/Image" => {
            let sub_type = field_num(m, "MaskSubType")? as i64;
            let sub_category = field_num(m, "MaskSubCategoryID").map(|v| v as i64);
            match (sub_type, sub_category) {
                (1, _) => (MaskSource::Ai(recipe_for(SegmentTarget::Subject)), inverted),
                (2, _) => (MaskSource::Ai(recipe_for(SegmentTarget::Sky)), inverted),
                // "Background" is the subject's inverse: one model run serves both.
                (0, Some(22)) => (
                    MaskSource::Ai(recipe_for(SegmentTarget::Subject)),
                    !inverted,
                ),
                _ => return None,
            }
        }
        "Mask/Gradient" if upright => (
            MaskSource::LinearGradient {
                p0: [num("FullX")?, num("FullY")?],
                p1: [num("ZeroX")?, num("ZeroY")?],
            },
            inverted,
        ),
        "Mask/CircularGradient" if upright => {
            let (w, h) = dims?;
            let long = w.max(h);
            let (left, top, right, bottom) =
                (num("Left")?, num("Top")?, num("Right")?, num("Bottom")?);
            if !(right > left && bottom > top) {
                return None;
            }
            // Semi-axes as a fraction of the long edge (nicti's length unit).
            let radii = [
                (right - left) / 2.0 * w / long,
                (bottom - top) / 2.0 * h / long,
            ];
            let feather =
                num("Feather").unwrap_or(0.0).clamp(0.0, 100.0) / 100.0 * radii[0].min(radii[1]);
            (
                MaskSource::RadialGradient {
                    center: [(left + right) / 2.0, (top + bottom) / 2.0],
                    radii,
                    angle_deg: 0.0,
                    feather,
                },
                inverted,
            )
        }
        _ => return None,
    };
    let is_ai = matches!(source, MaskSource::Ai(_));
    Some((
        MaskComponent {
            source,
            op,
            invert,
            opacity: field_num(m, "MaskValue").map_or(1.0, |v| v.clamp(0.0, 1.0) as f32),
        },
        is_ai,
    ))
}

fn adjust_for(c: &Value<'_>) -> LocalAdjust {
    let unit = |k: &str| field_num(c, k).unwrap_or(0.0).clamp(-1.0, 1.0) as f32;
    let tint_saturation = field_num(c, "LocalToningSaturation")
        .unwrap_or(0.0)
        .clamp(0.0, 1.0) as f32;
    LocalAdjust {
        // Stored as stops/4 (see the module doc), nicti keeps stops.
        exposure: (field_num(c, "LocalExposure2012").unwrap_or(0.0) * 4.0).clamp(-5.0, 5.0) as f32,
        contrast: unit("LocalContrast2012"),
        highlights: unit("LocalHighlights2012"),
        shadows: unit("LocalShadows2012"),
        whites: unit("LocalWhites2012"),
        blacks: unit("LocalBlacks2012"),
        temp: unit("LocalTemperature"),
        tint: unit("LocalTint"),
        saturation: unit("LocalSaturation"),
        hue: unit("LocalHue"),
        clarity: unit("LocalClarity2012"),
        texture: unit("LocalTexture"),
        dehaze: unit("LocalDehaze"),
        sharpness: unit("LocalSharpness"),
        noise: unit("LocalLuminanceNoise"),
        color: (tint_saturation > 0.0).then(|| TintColor {
            hue_deg: field_num(c, "LocalToningHue")
                .unwrap_or(0.0)
                .rem_euclid(360.0) as f32,
            saturation: tint_saturation,
        }),
    }
}

#[cfg(test)]
mod tests {
    use crate::develop::{translate, Context};
    use nicti_tapetum::mask::params::{MaskParams, MaskSource, Op};
    use nicti_tapetum::stages::MASKS;

    fn upright() -> Context {
        Context {
            width: Some(6000.0),
            height: Some(4000.0),
            orientation: Some("AB".into()),
            process_version: None,
        }
    }

    fn masks(t: &crate::develop::Translation) -> MaskParams {
        serde_json::from_value(t.document.stages[MASKS].params.clone()).unwrap()
    }

    const LINEAR: &str = r#"s = { MaskGroupBasedCorrections = { { CorrectionActive = true,
        CorrectionAmount = 0.8, CorrectionName = "m1", CorrectionSyncID = "SYNC1",
        CorrectionMasks = { { FullX = 0.1, FullY = 0.2, MaskActive = true, MaskBlendMode = 0,
          MaskInverted = false, MaskValue = 1, What = "Mask/Gradient", ZeroX = 0.1, ZeroY = 0.6 } },
        LocalExposure2012 = -0.175, LocalShadows2012 = 0.2, What = "Correction" } } }"#;

    #[test]
    fn a_linear_gradient_correction_maps_geometry_amount_and_scales_exposure_by_four() {
        let t = translate(LINEAR, &upright()).unwrap();
        let p = masks(&t);
        assert_eq!(p.corrections.len(), 1);
        let c = &p.corrections[0];
        assert_eq!(
            (c.id.as_str(), c.name.as_str(), c.amount),
            ("SYNC1", "m1", 0.8)
        );
        assert!((c.adjust.exposure - -0.7).abs() < 1e-6);
        assert_eq!(c.adjust.shadows, 0.2);
        match &c.mask.components[0].source {
            MaskSource::LinearGradient { p0, p1 } => {
                assert_eq!((*p0, *p1), ([0.1, 0.2], [0.1, 0.6]));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(t.stats.mask_corrections, 1);
        assert!(t.untranslated.is_empty());
    }

    #[test]
    fn geometric_masks_need_an_upright_original_but_ai_masks_do_not() {
        let rotated = Context {
            orientation: Some("DA".into()),
            ..upright()
        };
        let t = translate(LINEAR, &rotated).unwrap();
        assert!(t.document.stages.is_empty());
        assert_eq!(
            (
                t.stats.mask_corrections_skipped,
                t.stats.mask_components_skipped
            ),
            (1, 1)
        );

        let subject = r#"s = { MaskGroupBasedCorrections = { { CorrectionActive = true,
            CorrectionAmount = 1, CorrectionMasks = { { MaskActive = true, MaskBlendMode = 0,
            MaskInverted = false, MaskSubType = 1, MaskValue = 1, What = "Mask/Image" } },
            LocalShadows2012 = 0.2, What = "Correction" } } }"#;
        let t = translate(subject, &rotated).unwrap();
        assert_eq!(masks(&t).corrections.len(), 1);
        assert_eq!(t.stats.ai_masks, 1);
    }

    #[test]
    fn sky_and_background_become_recipes_and_background_inverts_the_subject() {
        let text = |sub: &str, inv: &str| {
            format!(
                r#"s = {{ MaskGroupBasedCorrections = {{ {{ CorrectionAmount = 1,
                CorrectionMasks = {{ {{ MaskBlendMode = 0, MaskInverted = {inv}, {sub}
                MaskValue = 1, What = "Mask/Image" }} }}, LocalShadows2012 = 0.2,
                What = "Correction" }} }} }}"#
            )
        };
        let sky = masks(&translate(&text("MaskSubType = 2,", "true"), &upright()).unwrap());
        let c = &sky.corrections[0].mask.components[0];
        assert!(c.invert);
        assert!(matches!(&c.source, MaskSource::Ai(r) if r.params["target"] == "sky"));
        let bg = masks(
            &translate(
                &text("MaskSubCategoryID = 22, MaskSubType = 0,", "false"),
                &upright(),
            )
            .unwrap(),
        );
        let c = &bg.corrections[0].mask.components[0];
        assert!(c.invert, "background is the inverse of the subject");
        assert!(matches!(&c.source, MaskSource::Ai(r) if r.params["target"] == "subject"));
    }

    #[test]
    fn a_radial_maps_center_radii_in_long_edge_units_and_keeps_inversion() {
        let t = translate(
            r#"s = { MaskGroupBasedCorrections = { { CorrectionAmount = 1,
            CorrectionMasks = { { Bottom = 0.75, Feather = 50, Left = 0.25, MaskBlendMode = 0,
              MaskInverted = true, MaskValue = 1, Right = 0.75, Top = 0.25,
              What = "Mask/CircularGradient" } },
            LocalExposure2012 = -0.25, What = "Correction" } } }"#,
            &upright(),
        )
        .unwrap();
        let c = &masks(&t).corrections[0].mask.components[0];
        assert!(c.invert);
        match &c.source {
            MaskSource::RadialGradient {
                center,
                radii,
                feather,
                ..
            } => {
                assert_eq!(*center, [0.5, 0.5]);
                // 0.25 of 6000 / 6000 and 0.25 of 4000 / 6000.
                assert!((radii[0] - 0.25).abs() < 1e-6 && (radii[1] - 1.0 / 6.0).abs() < 1e-6);
                assert!((feather - 0.5 * radii[1]).abs() < 1e-6);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn subtract_blend_is_kept_but_a_brush_or_people_component_skips_the_correction() {
        let t = translate(
            r#"s = { MaskGroupBasedCorrections = { { CorrectionAmount = 1, CorrectionMasks = {
              { MaskBlendMode = 0, MaskSubType = 1, What = "Mask/Image" },
              { MaskBlendMode = 1, FullX = 0, FullY = 0, ZeroX = 0, ZeroY = 1, What = "Mask/Gradient" } },
              LocalShadows2012 = 0.5, What = "Correction" },
              { CorrectionAmount = 1, CorrectionMasks = { { MaskBlendMode = 0, What = "Mask/Aggregate" } },
              LocalShadows2012 = 0.5, What = "Correction" },
              { CorrectionAmount = 1, CorrectionMasks = { { MaskBlendMode = 0, MaskSubType = 3,
              What = "Mask/Image" } }, LocalShadows2012 = 0.5, What = "Correction" } } }"#,
            &upright(),
        )
        .unwrap();
        let p = masks(&t);
        assert_eq!(p.corrections.len(), 1);
        assert_eq!(p.corrections[0].mask.components[1].op, Op::Subtract);
        assert_eq!(t.stats.mask_corrections_skipped, 2);
    }

    #[test]
    fn inactive_noop_range_masked_and_excess_corrections_are_handled() {
        let none = translate(
            r#"s = { MaskGroupBasedCorrections = {
              { CorrectionActive = false, CorrectionMasks = { { MaskSubType = 1, What = "Mask/Image" } },
                LocalShadows2012 = 0.5, What = "Correction" },
              { CorrectionMasks = { { MaskSubType = 1, What = "Mask/Image" } }, What = "Correction" },
              { CorrectionRangeMask = { Type = 2 }, CorrectionMasks = { { MaskSubType = 1,
                What = "Mask/Image" } }, LocalShadows2012 = 0.5, What = "Correction" } } }"#,
            &upright(),
        )
        .unwrap();
        assert!(none.document.stages.is_empty());
        assert_eq!(none.stats.mask_corrections_skipped, 1);

        let one = r#"{ CorrectionMasks = { { MaskSubType = 1, What = "Mask/Image" } },
            LocalShadows2012 = 0.5, What = "Correction" }"#;
        let many = format!(
            "s = {{ MaskGroupBasedCorrections = {{ {} }} }}",
            vec![one; 20].join(",")
        );
        let t = translate(&many, &upright()).unwrap();
        assert_eq!(masks(&t).corrections.len(), 16);
        assert_eq!(t.stats.mask_corrections_skipped, 4);
    }
}
