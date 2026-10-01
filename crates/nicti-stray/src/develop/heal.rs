//! `RetouchAreas` (spot heal / clone / brush heal). Not translated: the real catalog stores each
//! area as `Mask/Ellipse` (`X`/`Y`/`SizeX`/`SizeY`) or `Mask/Paint` (`Dabs` strings) plus
//! `SourceX`/`OffsetY`/`Method`, and neither the source-offset convention nor the dab units are
//! pinned against real LRC renders (only ~0.04% of images use it). The areas are counted for the
//! report and the verbatim text stays in provenance; `RetouchAreas` is left out of `consumed` so it
//! also shows in the untranslated-key histogram. A follow-up maps circle spots to `nicti.heal`
//! once the convention is verified.

use super::{field, items, Tx};

pub(super) fn apply(tx: &mut Tx) {
    let root = tx.root;
    tx.take("EnableRetouch");
    let areas = field(root, "RetouchAreas")
        .map(items)
        .map_or(0, |a| a.len());
    tx.stats.heal_spots_skipped += areas as u64;
}

#[cfg(test)]
mod tests {
    use crate::develop::{translate, Context};

    #[test]
    fn retouch_areas_are_counted_not_translated() {
        let t = translate(
            r#"s = { RetouchAreas = { { SpotType = "heal" }, { SpotType = "clone" } } }"#,
            &Context::default(),
        )
        .unwrap();
        assert_eq!(t.stats.heal_spots_skipped, 2);
        assert!(t.document.stages.is_empty());
        assert_eq!(t.untranslated, vec!["RetouchAreas"]);
    }
}
