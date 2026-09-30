//! The marking vocabulary (#32): which key does what, and what each action does to a photo's
//! three markers (rating, pick/reject flag, colour label). Pure -- no egui input, no catalog -- so
//! every transition is unit-tested.
//!
//! All the standard Lightroom Classic keys are live at once and none is "the" workflow: a user who
//! only ever rates with stars, one who only picks/rejects, and one who mixes stars with colour
//! labels all use the same keys and filter on whichever marker they used.
//!
//! Storage follows the catalog schema (ADR-0059/0061): rating `None` = unrated, `0..=5` = stars,
//! `-1` = reject; flag `Some(1)` = pick; label = free text (the five names in [`Label`] here).
//! Reject lives in the rating column, so rejecting a photo replaces its stars, and pick and reject
//! are mutually exclusive (as in Lightroom).

use egui::Key;
use nicti_lair::AssetMeta;

/// The catalog's reject rating.
pub const REJECT: i64 = -1;

/// The colour labels. Names are what the catalog stores (`asset.label`), matching Lightroom's
/// own label names so an LRC import (#62) lands on the same strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Label {
    Red,
    Yellow,
    Green,
    Blue,
    Purple,
}

impl Label {
    pub const ALL: [Label; 5] = [
        Label::Red,
        Label::Yellow,
        Label::Green,
        Label::Blue,
        Label::Purple,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Label::Red => "Red",
            Label::Yellow => "Yellow",
            Label::Green => "Green",
            Label::Blue => "Blue",
            Label::Purple => "Purple",
        }
    }

    pub fn from_name(name: &str) -> Option<Label> {
        Label::ALL
            .into_iter()
            .find(|l| l.name().eq_ignore_ascii_case(name))
    }

    pub fn color(self) -> egui::Color32 {
        match self {
            Label::Red => egui::Color32::from_rgb(0xe0, 0x4a, 0x4a),
            Label::Yellow => egui::Color32::from_rgb(0xe6, 0xc2, 0x2e),
            Label::Green => egui::Color32::from_rgb(0x4c, 0xb0, 0x5c),
            Label::Blue => egui::Color32::from_rgb(0x4a, 0x84, 0xe0),
            Label::Purple => egui::Color32::from_rgb(0x9a, 0x5c, 0xd0),
        }
    }
}

/// One thing a marking key does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CullAction {
    /// `None` clears the rating (unrated); `Some(0..=5)` sets stars.
    SetRating(Option<i64>),
    /// Pick, or un-pick when every target is already picked.
    TogglePick,
    /// Reject, or un-reject when every target is already rejected.
    ToggleReject,
    /// Clear the flag; also clears a reject (Lightroom's "unflag").
    Unflag,
    /// Apply this label, or clear it when every target already has it.
    ToggleLabel(Label),
}

/// The action bound to `key`, if any. Modifiers are handled by the caller (Ctrl+Z is undo, Shift
/// inverts auto-advance); this is only the plain-key table.
pub fn action_for(key: Key) -> Option<CullAction> {
    use CullAction::*;
    Some(match key {
        Key::Num0 => SetRating(None),
        Key::Num1 => SetRating(Some(1)),
        Key::Num2 => SetRating(Some(2)),
        Key::Num3 => SetRating(Some(3)),
        Key::Num4 => SetRating(Some(4)),
        Key::Num5 => SetRating(Some(5)),
        Key::P => TogglePick,
        Key::X => ToggleReject,
        Key::U => Unflag,
        Key::Num6 => ToggleLabel(Label::Red),
        Key::Num7 => ToggleLabel(Label::Yellow),
        Key::Num8 => ToggleLabel(Label::Green),
        Key::Num9 => ToggleLabel(Label::Blue),
        _ => return None,
    })
}

pub fn is_picked(m: &AssetMeta) -> bool {
    m.flag == Some(1)
}

pub fn is_rejected(m: &AssetMeta) -> bool {
    m.rating == Some(REJECT)
}

/// What `action` turns each of `metas` into. Toggles decide once for the whole batch (all targets
/// already in the state -> clear it, otherwise set it on all), so a mixed multi-selection ends up
/// uniform instead of every photo flipping the other way.
pub fn apply_all(action: CullAction, metas: &[AssetMeta]) -> Vec<AssetMeta> {
    match action {
        CullAction::SetRating(r) => metas
            .iter()
            .map(|m| AssetMeta {
                rating: r,
                ..m.clone()
            })
            .collect(),
        CullAction::TogglePick => {
            let clear = !metas.is_empty() && metas.iter().all(is_picked);
            metas
                .iter()
                .map(|m| {
                    if clear {
                        unflag(m)
                    } else {
                        // Picking a rejected photo un-rejects it: the two are exclusive.
                        AssetMeta {
                            rating: if is_rejected(m) { None } else { m.rating },
                            flag: Some(1),
                            label: m.label.clone(),
                        }
                    }
                })
                .collect()
        }
        CullAction::ToggleReject => {
            let clear = !metas.is_empty() && metas.iter().all(is_rejected);
            metas
                .iter()
                .map(|m| {
                    if clear {
                        AssetMeta {
                            rating: None,
                            ..m.clone()
                        }
                    } else {
                        // Rejecting replaces the stars and drops the pick.
                        AssetMeta {
                            rating: Some(REJECT),
                            flag: None,
                            label: m.label.clone(),
                        }
                    }
                })
                .collect()
        }
        CullAction::Unflag => metas.iter().map(unflag).collect(),
        CullAction::ToggleLabel(label) => {
            let name = label.name();
            let clear = !metas.is_empty() && metas.iter().all(|m| m.label.as_deref() == Some(name));
            metas
                .iter()
                .map(|m| AssetMeta {
                    label: if clear { None } else { Some(name.to_string()) },
                    ..m.clone()
                })
                .collect()
        }
    }
}

fn unflag(m: &AssetMeta) -> AssetMeta {
    AssetMeta {
        rating: if is_rejected(m) { None } else { m.rating },
        flag: None,
        label: m.label.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(rating: Option<i64>, flag: Option<i64>, label: Option<&str>) -> AssetMeta {
        AssetMeta {
            rating,
            flag,
            label: label.map(str::to_string),
        }
    }

    fn one(action: CullAction, m: AssetMeta) -> AssetMeta {
        apply_all(action, &[m]).pop().unwrap()
    }

    #[test]
    fn number_keys_set_stars_and_zero_clears() {
        let base = meta(Some(2), Some(1), Some("Red"));
        assert_eq!(
            one(CullAction::SetRating(Some(4)), base.clone()),
            meta(Some(4), Some(1), Some("Red")),
            "only the rating changes"
        );
        assert_eq!(
            one(CullAction::SetRating(None), base),
            meta(None, Some(1), Some("Red"))
        );
    }

    #[test]
    fn rating_a_rejected_photo_un_rejects_it() {
        let rejected = meta(Some(REJECT), None, None);
        assert_eq!(
            one(CullAction::SetRating(Some(3)), rejected).rating,
            Some(3)
        );
    }

    #[test]
    fn pick_toggles_and_replaces_a_reject() {
        let plain = meta(Some(3), None, None);
        let picked = one(CullAction::TogglePick, plain.clone());
        assert_eq!(picked, meta(Some(3), Some(1), None), "stars are kept");
        assert_eq!(
            one(CullAction::TogglePick, picked),
            plain,
            "second press clears"
        );

        let rejected = meta(Some(REJECT), None, None);
        assert_eq!(
            one(CullAction::TogglePick, rejected),
            meta(None, Some(1), None),
            "pick and reject are exclusive"
        );
    }

    #[test]
    fn reject_toggles_replaces_stars_and_drops_the_pick() {
        let starred_picked = meta(Some(4), Some(1), Some("Blue"));
        let rejected = one(CullAction::ToggleReject, starred_picked);
        assert_eq!(rejected, meta(Some(REJECT), None, Some("Blue")));
        assert_eq!(
            one(CullAction::ToggleReject, rejected),
            meta(None, None, Some("Blue")),
            "second press un-rejects back to unrated, not to the lost stars"
        );
    }

    #[test]
    fn unflag_clears_pick_and_reject_but_keeps_stars_and_label() {
        assert_eq!(
            one(CullAction::Unflag, meta(Some(2), Some(1), Some("Red"))),
            meta(Some(2), None, Some("Red"))
        );
        assert_eq!(
            one(CullAction::Unflag, meta(Some(REJECT), None, None)),
            meta(None, None, None)
        );
    }

    #[test]
    fn label_toggles_and_replaces_another_label() {
        let plain = meta(None, None, None);
        let red = one(CullAction::ToggleLabel(Label::Red), plain.clone());
        assert_eq!(red.label.as_deref(), Some("Red"));
        assert_eq!(one(CullAction::ToggleLabel(Label::Red), red.clone()), plain);
        assert_eq!(
            one(CullAction::ToggleLabel(Label::Blue), red)
                .label
                .as_deref(),
            Some("Blue")
        );
    }

    #[test]
    fn toggles_decide_once_for_a_mixed_batch() {
        let picked = meta(None, Some(1), None);
        let plain = meta(None, None, None);
        // Mixed: not all picked -> everyone ends up picked (never a flip-flop mix).
        let out = apply_all(CullAction::TogglePick, &[picked.clone(), plain.clone()]);
        assert!(out.iter().all(is_picked));
        // All picked -> everyone is cleared.
        let out = apply_all(CullAction::TogglePick, &[picked.clone(), picked]);
        assert!(out.iter().all(|m| !is_picked(m)));

        let rej = meta(Some(REJECT), None, None);
        let out = apply_all(CullAction::ToggleReject, &[rej.clone(), plain.clone()]);
        assert!(out.iter().all(is_rejected));
        let out = apply_all(CullAction::ToggleReject, &[rej.clone(), rej]);
        assert!(out.iter().all(|m| !is_rejected(m)));

        let red = meta(None, None, Some("Red"));
        let out = apply_all(CullAction::ToggleLabel(Label::Red), &[red.clone(), plain]);
        assert!(out.iter().all(|m| m.label.as_deref() == Some("Red")));
        let out = apply_all(CullAction::ToggleLabel(Label::Red), &[red.clone(), red]);
        assert!(out.iter().all(|m| m.label.is_none()));
    }

    #[test]
    fn an_empty_batch_is_a_no_op() {
        for action in [
            CullAction::TogglePick,
            CullAction::ToggleReject,
            CullAction::Unflag,
            CullAction::SetRating(Some(1)),
            CullAction::ToggleLabel(Label::Green),
        ] {
            assert!(apply_all(action, &[]).is_empty());
        }
    }

    #[test]
    fn the_standard_lightroom_keys_are_bound() {
        assert_eq!(action_for(Key::Num0), Some(CullAction::SetRating(None)));
        assert_eq!(action_for(Key::Num5), Some(CullAction::SetRating(Some(5))));
        assert_eq!(action_for(Key::P), Some(CullAction::TogglePick));
        assert_eq!(action_for(Key::X), Some(CullAction::ToggleReject));
        assert_eq!(action_for(Key::U), Some(CullAction::Unflag));
        assert_eq!(
            action_for(Key::Num6),
            Some(CullAction::ToggleLabel(Label::Red))
        );
        assert_eq!(
            action_for(Key::Num9),
            Some(CullAction::ToggleLabel(Label::Blue))
        );
        assert_eq!(action_for(Key::A), None);
        assert_eq!(action_for(Key::ArrowLeft), None, "navigation is not a mark");
    }

    #[test]
    fn label_names_round_trip_through_the_catalog_strings() {
        for l in Label::ALL {
            assert_eq!(Label::from_name(l.name()), Some(l));
        }
        assert_eq!(Label::from_name("red"), Some(Label::Red));
        assert_eq!(Label::from_name("Teal"), None);
    }
}
