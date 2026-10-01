//! Crop rectangle. LRC stores `CropLeft/Top/Right/Bottom` as 0..1 fractions of the *oriented*
//! frame (written only while `HasCrop` is true), plus `CropAngle`. nicti's `CropParams` is a pixel
//! rectangle in source-frame space, so the translation is only exact for an upright original with no
//! straighten angle: rotated originals and any non-zero `CropAngle` are left untranslated (counted
//! in `Stats::crops_skipped`, raw values kept in provenance) rather than guessing LRC's rotation
//! sign convention or nicti's orientation handling. A follow-up pins both against real renders.

use nicti_tapetum::coat::CropParams;
use nicti_tapetum::stages::CROP;

use super::{field_bool, Tx};

pub(super) fn apply(tx: &mut Tx) {
    let has_crop = field_bool(tx.root, "HasCrop");
    tx.take("HasCrop");
    let left = tx.num("CropLeft");
    let top = tx.num("CropTop");
    let right = tx.num("CropRight");
    let bottom = tx.num("CropBottom");
    let angle = tx.num("CropAngle").unwrap_or(0.0);
    tx.take("CropConstrainToWarp");
    tx.take("CropConstrainAspectRatio");

    let (Some(left), Some(top), Some(right), Some(bottom)) = (left, top, right, bottom) else {
        return;
    };
    if has_crop == Some(false) {
        return;
    }
    let full_frame = left <= 0.0 && top <= 0.0 && right >= 1.0 && bottom >= 1.0;
    if full_frame && angle == 0.0 {
        return;
    }
    let upright = tx
        .ctx
        .orientation
        .as_deref()
        .is_some_and(|o| o.eq_ignore_ascii_case("AB"));
    let (Some(w), Some(h)) = (tx.ctx.width, tx.ctx.height) else {
        tx.stats.crops_skipped += 1;
        return;
    };
    if !upright || angle != 0.0 || !(right > left && bottom > top) {
        tx.stats.crops_skipped += 1;
        return;
    }
    let (l, t) = (left.clamp(0.0, 1.0) as f32, top.clamp(0.0, 1.0) as f32);
    let (r, b) = (right.clamp(0.0, 1.0) as f32, bottom.clamp(0.0, 1.0) as f32);
    tx.put(
        CROP,
        CropParams {
            x: l * w,
            y: t * h,
            width: (r - l) * w,
            height: (b - t) * h,
            rotation_degrees: 0.0,
        },
    );
    tx.stats.crops += 1;
}

#[cfg(test)]
mod tests {
    use crate::develop::{translate, Context};
    use nicti_tapetum::stages::CROP;

    fn ctx(orientation: &str) -> Context {
        Context {
            width: Some(6000.0),
            height: Some(4000.0),
            orientation: Some(orientation.into()),
            process_version: None,
        }
    }

    const CROP_TEXT: &str = "s = { HasCrop = true, CropLeft = 0.1, CropTop = 0.25, \
        CropRight = 0.9, CropBottom = 0.75, CropAngle = 0 }";

    #[test]
    fn an_upright_unrotated_crop_becomes_a_pixel_rect() {
        let t = translate(CROP_TEXT, &ctx("AB")).unwrap();
        let c = &t.document.stages[CROP].params;
        assert_eq!(c["x"], 600.0);
        assert_eq!(c["y"], 1000.0);
        assert!(crate::develop::close(&c["width"], 4800.0));
        assert!(crate::develop::close(&c["height"], 2000.0));
        assert_eq!(t.stats.crops, 1);
        assert!(t.untranslated.is_empty());
    }

    #[test]
    fn rotated_originals_a_straighten_angle_and_unknown_size_are_skipped_and_counted() {
        let t = translate(CROP_TEXT, &ctx("BC")).unwrap();
        assert!(t.document.stages.is_empty() && t.stats.crops_skipped == 1);
        let t = translate(
            &CROP_TEXT.replace("CropAngle = 0", "CropAngle = 2.5"),
            &ctx("AB"),
        )
        .unwrap();
        assert!(t.document.stages.is_empty() && t.stats.crops_skipped == 1);
        let t = translate(CROP_TEXT, &Context::default()).unwrap();
        assert!(t.document.stages.is_empty() && t.stats.crops_skipped == 1);
    }

    #[test]
    fn no_crop_and_full_frame_crop_write_nothing() {
        let t = translate(
            "s = { HasCrop = false, CropLeft = 0, CropTop = 0, CropRight = 1, CropBottom = 1 }",
            &ctx("AB"),
        )
        .unwrap();
        assert!(t.document.stages.is_empty() && t.stats.crops_skipped == 0);
        let t = translate(
            "s = { CropLeft = 0, CropTop = 0, CropRight = 1, CropBottom = 1 }",
            &ctx("AB"),
        )
        .unwrap();
        assert!(t.document.stages.is_empty());
    }
}
