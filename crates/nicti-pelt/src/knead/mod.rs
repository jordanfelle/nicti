//! Copy/paste, sync and presets for develop settings (#52, ADR-0052). Named for kneading: working
//! one photo's settings into others.
//!
//! This file is the pure core -- no egui, no catalog: which stages travel (`StageSet`), what was
//! copied (`Clipboard`), and what applying it to a set of photos would change (`plan`). `batch`
//! runs a plan against the catalog and undoes it; `presets` stores named clipboards on disk.
//!
//! Semantics are per-stage and absolute: a checked stage replaces the target's entry wholesale,
//! and a checked stage the source doesn't have is *removed* from the target (a reset). Unchecked
//! stages are never touched. Relative paste (`nicti_pawprint::apply_relative`) is deferred.

pub mod batch;
pub mod presets;
pub mod ui;

use std::collections::{BTreeMap, BTreeSet};

use nicti_pawprint::{EditDocument, StageEntry};
use nicti_tapetum::stages::{
    CROP, EXPOSURE, HEAL, HSL, MASKS, NOISE_REDUCTION, PRESENCE, SHARPEN, TONE, TONE_CURVE,
    VIBRANCE, WB, WORKING_SPACE,
};

/// One row of the checklist: a stage and the label the user sees.
pub struct StageGroup {
    pub id: &'static str,
    pub label: &'static str,
    /// Checked when the checklist first opens. Crop, heal spots and local corrections describe one
    /// specific photo's content, so they start unchecked (LRC does the same). So does the camera
    /// profile: it names a camera-specific `.dcp`, and a target from another camera rejects it
    /// (Develop shows an error, export fails the photo).
    pub default_on: bool,
}

/// Every stage that can travel, in the order the checklist shows them: the "Reset all" list in
/// `develop_panel`, plus the camera profile (`WORKING_SPACE`).
pub const GROUPS: &[StageGroup] = &[
    StageGroup {
        id: WB,
        label: "White balance",
        default_on: true,
    },
    StageGroup {
        id: EXPOSURE,
        label: "Exposure",
        default_on: true,
    },
    StageGroup {
        id: TONE,
        label: "Tone",
        default_on: true,
    },
    StageGroup {
        id: TONE_CURVE,
        label: "Tone curve",
        default_on: true,
    },
    StageGroup {
        id: VIBRANCE,
        label: "Vibrance",
        default_on: true,
    },
    StageGroup {
        id: PRESENCE,
        label: "Texture, clarity, dehaze, saturation",
        default_on: true,
    },
    StageGroup {
        id: HSL,
        label: "Color mixer (HSL)",
        default_on: true,
    },
    StageGroup {
        id: SHARPEN,
        label: "Sharpening",
        default_on: true,
    },
    StageGroup {
        id: NOISE_REDUCTION,
        label: "Noise reduction",
        default_on: true,
    },
    StageGroup {
        id: WORKING_SPACE,
        label: "Camera profile",
        default_on: false,
    },
    StageGroup {
        id: CROP,
        label: "Crop and straighten",
        default_on: false,
    },
    StageGroup {
        id: HEAL,
        label: "Heal and removal spots",
        default_on: false,
    },
    StageGroup {
        id: MASKS,
        label: "Local corrections (masks)",
        default_on: false,
    },
];

/// The stages a copy/sync/preset carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StageSet(BTreeSet<String>);

impl Default for StageSet {
    fn default() -> Self {
        Self(
            GROUPS
                .iter()
                .filter(|g| g.default_on)
                .map(|g| g.id.to_string())
                .collect(),
        )
    }
}

impl StageSet {
    pub fn all() -> Self {
        Self(GROUPS.iter().map(|g| g.id.to_string()).collect())
    }

    pub fn none() -> Self {
        Self(BTreeSet::new())
    }

    pub fn contains(&self, id: &str) -> bool {
        self.0.contains(id)
    }

    pub fn set(&mut self, id: &str, on: bool) {
        if on {
            self.0.insert(id.to_string());
        } else {
            self.0.remove(id);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }
}

/// What a copy captured: for each checked stage, the source's entry, or `None` if the source has
/// none (so pasting it resets that stage on the target).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Clipboard {
    stages: BTreeMap<String, Option<StageEntry>>,
}

impl Clipboard {
    pub fn from_document(doc: &EditDocument, set: &StageSet) -> Self {
        Self {
            stages: set
                .iter()
                .map(|id| (id.to_string(), doc.stages.get(id).cloned()))
                .collect(),
        }
    }

    /// A preset's stages: every one it holds is a stage to set, and nothing else is touched (a
    /// preset never resets a stage it doesn't mention).
    pub fn from_entries(entries: &BTreeMap<String, StageEntry>) -> Self {
        Self {
            stages: entries
                .iter()
                .map(|(id, e)| (id.clone(), Some(e.clone())))
                .collect(),
        }
    }

    /// The stages with an entry, for saving as a preset. Stages the source lacked are dropped:
    /// "reset this stage" is a paste behaviour, not something a preset stores.
    pub fn entries(&self) -> BTreeMap<String, StageEntry> {
        self.stages
            .iter()
            .filter_map(|(id, e)| e.clone().map(|e| (id.clone(), e)))
            .collect()
    }

    /// `target` with every stage in the clipboard replaced (or removed).
    pub fn apply(&self, target: &EditDocument) -> EditDocument {
        let mut out = target.clone();
        for (id, entry) in &self.stages {
            match entry {
                Some(e) => {
                    out.stages.insert(id.clone(), e.clone());
                }
                None => {
                    out.stages.remove(id);
                }
            }
        }
        out
    }
}

/// One photo a plan would change.
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    pub asset_id: i64,
    pub before: EditDocument,
    pub after: EditDocument,
}

/// What pasting a clipboard onto a set of photos would do.
#[derive(Debug, Default, PartialEq)]
pub struct BatchPlan {
    pub changes: Vec<Change>,
    /// Photos whose document the paste would leave as it is -- not written, no undo entry
    /// (ADR-0101 rule 6).
    pub unchanged: usize,
    /// Photos the catalog doesn't have.
    pub missing: usize,
}

/// `targets` is each photo's current document; `None` means the catalog has no such photo.
pub fn plan(clip: &Clipboard, targets: Vec<(i64, Option<EditDocument>)>) -> BatchPlan {
    let mut out = BatchPlan::default();
    for (asset_id, current) in targets {
        let Some(before) = current else {
            out.missing += 1;
            continue;
        };
        let after = clip.apply(&before);
        if after == before {
            out.unchanged += 1;
        } else {
            out.changes.push(Change {
                asset_id,
                before,
                after,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(v: f64) -> StageEntry {
        StageEntry {
            schema_version: 1,
            params: json!({ "v": v }),
        }
    }

    fn doc(stages: &[(&str, f64)]) -> EditDocument {
        let mut d = EditDocument::default();
        for (id, v) in stages {
            d.stages.insert((*id).to_string(), entry(*v));
        }
        d
    }

    #[test]
    fn default_set_leaves_out_per_photo_content_and_all_has_everything() {
        let d = StageSet::default();
        assert!(d.contains(WB) && d.contains(TONE));
        assert!(!d.contains(CROP) && !d.contains(HEAL) && !d.contains(MASKS));
        assert!(
            !d.contains(WORKING_SPACE),
            "a camera profile is camera-specific, so pasting it is opt-in"
        );
        assert!(StageSet::all().contains(MASKS));
        assert_eq!(StageSet::all().iter().count(), GROUPS.len());
    }

    #[test]
    fn a_checked_stage_replaces_the_targets_entry() {
        let clip = Clipboard::from_document(&doc(&[(EXPOSURE, 1.0)]), &StageSet::all());
        let out = clip.apply(&doc(&[(EXPOSURE, -2.0)]));
        assert_eq!(out.stages[EXPOSURE], entry(1.0));
    }

    #[test]
    fn a_checked_stage_missing_from_the_source_is_removed_from_the_target() {
        let mut set = StageSet::none();
        set.set(TONE, true);
        let clip = Clipboard::from_document(&doc(&[(EXPOSURE, 1.0)]), &set);
        let out = clip.apply(&doc(&[(TONE, 0.5), (EXPOSURE, 3.0)]));
        assert!(!out.stages.contains_key(TONE));
        assert_eq!(out.stages[EXPOSURE], entry(3.0));
    }

    #[test]
    fn an_unchecked_stage_is_untouched_even_when_the_source_has_it() {
        let mut set = StageSet::none();
        set.set(WB, true);
        let clip = Clipboard::from_document(&doc(&[(WB, 1.0), (EXPOSURE, 9.0)]), &set);
        let out = clip.apply(&doc(&[(EXPOSURE, 3.0)]));
        assert_eq!(out.stages[EXPOSURE], entry(3.0));
        assert_eq!(out.stages[WB], entry(1.0));
    }

    #[test]
    fn the_masks_stage_travels_as_one_unit() {
        let mut set = StageSet::none();
        set.set(MASKS, true);
        let src = doc(&[(MASKS, 4.0), (EXPOSURE, 1.0)]);
        let out = Clipboard::from_document(&src, &set).apply(&EditDocument::default());
        assert_eq!(out.stages.len(), 1);
        assert_eq!(out.stages[MASKS], src.stages[MASKS]);
    }

    #[test]
    fn a_preset_clipboard_sets_its_stages_and_resets_nothing_else() {
        let entries = Clipboard::from_document(&doc(&[(WB, 1.0)]), &StageSet::all()).entries();
        assert_eq!(entries.len(), 1, "stages the source lacked aren't stored");
        let out = Clipboard::from_entries(&entries).apply(&doc(&[(TONE, 0.5)]));
        assert_eq!(out.stages[WB], entry(1.0));
        assert_eq!(out.stages[TONE], entry(0.5));
    }

    #[test]
    fn plan_separates_changed_unchanged_and_missing_photos() {
        let clip = Clipboard::from_document(&doc(&[(EXPOSURE, 1.0)]), &StageSet::all());
        let plan = plan(
            &clip,
            vec![
                (1, Some(doc(&[(EXPOSURE, 5.0)]))),
                (2, Some(clip.apply(&EditDocument::default()))),
                (3, None),
            ],
        );
        assert_eq!(plan.changes.len(), 1);
        assert_eq!(plan.changes[0].asset_id, 1);
        assert_eq!(plan.changes[0].before, doc(&[(EXPOSURE, 5.0)]));
        assert_eq!((plan.unchanged, plan.missing), (1, 1));
    }
}
