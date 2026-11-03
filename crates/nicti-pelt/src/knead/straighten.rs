//! Rotation-only batch action (#614): take the straighten angle of one photo and set it on, or add
//! it to, every selected photo's own, leaving each target's crop rectangle and every other stage
//! alone. It reuses the paste machinery's `Change`/`BatchPlan`, so `batch` runs and undoes it
//! like a paste (one undo for the whole batch).
//!
//! The crop is **not fitted here**: the true source extent of a RAW isn't in the catalog (width and
//! height are the preview's declared size, #550/#604), and decoding every target just to read it
//! would defeat a batch action. Fitting happens where the extent is known: `spine::resolve_inputs`
//! fits every crop it resolves, so Develop, export, the grid's rendered tier and prebake all see
//! the same fitted rectangle (#589's bug class: a fit applied by only one consumer).
//!
//! Until #341 applies EXIF orientation to Develop's overlays, an angle measured on a sideways photo
//! is not the angle of an upright one, so a target is only changed when its EXIF Orientation is
//! known and equals the reference's; the rest are counted, not silently changed.

use nicti_pawprint::{EditDocument, StageEntry};
use nicti_tapetum::coat::{self, CropParams};
use nicti_tapetum::stages::CROP;

use super::{BatchPlan, Change};

/// A rotation closer than this to the target's own counts as already there: below any visible
/// change, and absorbs f32 noise between two runs.
const UNCHANGED_EPSILON_DEGREES: f32 = 0.01;

/// How the reference photo's angle is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Every target gets exactly the reference's angle.
    Set,
    /// The reference's angle is added to each target's own.
    Add,
}

/// One photo a rotation would act on.
pub struct Target {
    pub asset_id: i64,
    /// `None`: the catalog has no such photo.
    pub doc: Option<EditDocument>,
    /// EXIF Orientation 1-8; `None` = never read.
    pub orientation: Option<u8>,
}

#[derive(Debug, Default, PartialEq)]
pub struct RotationPlan {
    pub plan: BatchPlan,
    /// Skipped: orientation unknown, or different from the reference's.
    pub orientation_skipped: usize,
}

/// Plans `mode`/`degrees` over `targets`. A non-finite `degrees` changes nothing.
pub fn plan_rotation(
    targets: Vec<Target>,
    reference_orientation: Option<u8>,
    mode: Mode,
    degrees: f32,
) -> RotationPlan {
    let mut out = RotationPlan::default();
    for t in targets {
        let Some(before) = t.doc else {
            out.plan.missing += 1;
            continue;
        };
        if reference_orientation.is_none() || t.orientation != reference_orientation {
            out.orientation_skipped += 1;
            continue;
        }
        let Some(after) = rotated(&before, mode, degrees) else {
            out.plan.unchanged += 1;
            continue;
        };
        out.plan.changes.push(Change {
            asset_id: t.asset_id,
            before,
            after,
        });
    }
    out
}

/// `doc` with its crop rotation set/added, or `None` when that leaves it where it is.
fn rotated(doc: &EditDocument, mode: Mode, degrees: f32) -> Option<EditDocument> {
    if !degrees.is_finite() {
        return None;
    }
    let entry = doc.stages.get(CROP);
    let current: CropParams = entry.map_or_else(CropParams::default, |e| coat::parse(&e.params));
    let mut crop = current;
    crop.set_rotation(match mode {
        Mode::Set => degrees,
        Mode::Add => current.rotation_degrees + degrees,
    });
    if (crop.rotation_degrees - current.rotation_degrees).abs() < UNCHANGED_EPSILON_DEGREES {
        return None;
    }
    let mut after = doc.clone();
    if crop.is_noop() {
        // Back to "no crop, no rotation": drop the entry rather than store a default over it.
        after.stages.remove(CROP);
    } else {
        after.stages.insert(
            CROP.to_string(),
            StageEntry {
                schema_version: entry.map_or(1, |e| e.schema_version),
                params: serde_json::to_value(crop).expect("a coat params struct always serializes"),
            },
        );
    }
    Some(after)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_tapetum::geometry::MAX_STRAIGHTEN_DEGREES;

    fn doc_with(crop: Option<CropParams>) -> EditDocument {
        let mut d = EditDocument::default();
        if let Some(c) = crop {
            d.stages.insert(
                CROP.to_string(),
                StageEntry {
                    schema_version: 1,
                    params: serde_json::to_value(c).unwrap(),
                },
            );
        }
        d
    }

    fn rotation_of(d: &EditDocument) -> f32 {
        d.stages.get(CROP).map_or(0.0, |e| {
            coat::parse::<CropParams>(&e.params).rotation_degrees
        })
    }

    fn target(id: i64, crop: Option<CropParams>, orientation: Option<u8>) -> Target {
        Target {
            asset_id: id,
            doc: Some(doc_with(crop)),
            orientation,
        }
    }

    #[test]
    fn set_gives_every_target_the_reference_angle_and_keeps_its_crop_rect() {
        let rect = CropParams {
            x: 10.0,
            y: 20.0,
            width: 300.0,
            height: 200.0,
            rotation_degrees: 1.0,
            upright: None,
        };
        let p = plan_rotation(
            vec![target(1, None, Some(1)), target(2, Some(rect), Some(1))],
            Some(1),
            Mode::Set,
            2.5,
        );
        assert_eq!(p.plan.changes.len(), 2);
        assert!((rotation_of(&p.plan.changes[0].after) - 2.5).abs() < 1e-6);
        let kept: CropParams =
            coat::parse(&p.plan.changes[1].after.stages.get(CROP).unwrap().params);
        assert_eq!(
            (kept.x, kept.y, kept.width, kept.height),
            (10.0, 20.0, 300.0, 200.0)
        );
        assert!((kept.rotation_degrees - 2.5).abs() < 1e-6);
    }

    #[test]
    fn add_adds_to_each_targets_own_angle_and_clamps() {
        let at = |d: f32| CropParams {
            rotation_degrees: d,
            ..Default::default()
        };
        let p = plan_rotation(
            vec![
                target(1, Some(at(1.0)), Some(1)),
                target(2, Some(at(44.0)), Some(1)),
                target(3, None, Some(1)),
            ],
            Some(1),
            Mode::Add,
            3.0,
        );
        let rot: Vec<f32> = p
            .plan
            .changes
            .iter()
            .map(|c| rotation_of(&c.after))
            .collect();
        assert!((rot[0] - 4.0).abs() < 1e-6);
        assert_eq!(rot[1], MAX_STRAIGHTEN_DEGREES, "clamped, not 47");
        assert!((rot[2] - 3.0).abs() < 1e-6);
    }

    #[test]
    fn a_target_already_at_the_angle_is_unchanged_and_zero_removes_the_entry() {
        let at = |d: f32| CropParams {
            rotation_degrees: d,
            ..Default::default()
        };
        let p = plan_rotation(
            vec![
                target(1, Some(at(2.0)), Some(1)),
                target(2, Some(at(2.0)), Some(1)),
            ],
            Some(1),
            Mode::Set,
            2.0,
        );
        assert_eq!((p.plan.changes.len(), p.plan.unchanged), (0, 2));
        let p = plan_rotation(
            vec![target(1, Some(at(2.0)), Some(1))],
            Some(1),
            Mode::Set,
            0.0,
        );
        assert!(
            !p.plan.changes[0].after.stages.contains_key(CROP),
            "back to no crop and no rotation drops the entry"
        );
    }

    #[test]
    fn only_photos_with_the_references_orientation_are_changed() {
        let p = plan_rotation(
            vec![
                target(1, None, Some(1)),
                target(2, None, Some(6)),
                target(3, None, None),
            ],
            Some(1),
            Mode::Set,
            1.0,
        );
        assert_eq!(p.plan.changes.len(), 1);
        assert_eq!(p.orientation_skipped, 2);
        // An unread reference orientation proves nothing, so nothing changes.
        let p = plan_rotation(vec![target(1, None, Some(1))], None, Mode::Set, 1.0);
        assert_eq!((p.plan.changes.len(), p.orientation_skipped), (0, 1));
    }

    #[test]
    fn missing_photos_are_counted_and_a_non_finite_angle_changes_nothing() {
        let mut t = target(1, None, Some(1));
        t.doc = None;
        let p = plan_rotation(
            vec![t, target(2, None, Some(1))],
            Some(1),
            Mode::Set,
            f32::NAN,
        );
        assert_eq!(
            (p.plan.missing, p.plan.unchanged, p.plan.changes.len()),
            (1, 1, 0)
        );
        let p = plan_rotation(
            vec![target(2, None, Some(1))],
            Some(1),
            Mode::Add,
            f32::INFINITY,
        );
        assert_eq!(p.plan.unchanged, 1);
    }

    #[test]
    fn every_other_stage_is_left_alone() {
        let mut d = doc_with(None);
        d.stages.insert(
            "nicti.exposure".to_string(),
            StageEntry {
                schema_version: 1,
                params: serde_json::json!({ "ev": 1.5 }),
            },
        );
        let p = plan_rotation(
            vec![Target {
                asset_id: 1,
                doc: Some(d.clone()),
                orientation: Some(1),
            }],
            Some(1),
            Mode::Set,
            1.0,
        );
        assert_eq!(
            p.plan.changes[0].after.stages.get("nicti.exposure"),
            d.stages.get("nicti.exposure")
        );
    }
}
