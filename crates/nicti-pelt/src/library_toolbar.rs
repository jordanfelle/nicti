//! Which Library toolbar actions are available, and -- when one is not -- why (#776). The grid's
//! action row is mostly disabled until there is a selection, so every disabled button says what
//! it is waiting for. Pure, so the wording and the conditions are unit-tested.

/// What the toolbar's enable logic looks at.
#[derive(Debug, Clone, Copy, Default)]
pub struct ToolbarState {
    /// Photos matching the current filter.
    pub total: usize,
    /// Photos an action would act on (the selection, or the cursor photo).
    pub targets: usize,
    pub has_selection: bool,
    /// An import, sync, move, delete or export is running.
    pub busy: bool,
    pub delete_running: bool,
    pub export_running: bool,
    /// A Copy/Sync/Save-preset/Straighten prompt is open.
    pub asking: bool,
    pub has_clipboard: bool,
}

/// `None` = the action is available; `Some(reason)` = disabled, with the hover text to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Why {
    pub select_all: Option<&'static str>,
    pub delete: Option<&'static str>,
    pub survey: Option<&'static str>,
    pub compare: Option<&'static str>,
    pub export: Option<&'static str>,
    pub copy: Option<&'static str>,
    pub paste: Option<&'static str>,
    pub sync: Option<&'static str>,
    pub straighten: Option<&'static str>,
    pub apply_preset: Option<&'static str>,
}

const NEED_ONE: &str = "Select at least one photo.";
const NEED_TWO: &str = "Select at least two photos; the cursor photo is the reference.";
const BUSY: &str = "Wait for the running import, sync, move, delete or export to finish.";
const ASKING: &str = "Finish the open settings prompt first.";

pub fn why_disabled(s: &ToolbarState) -> Why {
    // The batch-edit actions share these two blockers after their own precondition.
    let edit_blocker = if s.busy {
        Some(BUSY)
    } else if s.asking {
        Some(ASKING)
    } else {
        None
    };
    let need_one = (s.targets == 0).then_some(NEED_ONE);
    Why {
        select_all: (s.total == 0).then_some("No photos match the current filter."),
        delete: need_one.or_else(|| (s.busy || s.delete_running).then_some(BUSY)),
        survey: (s.targets < 2 || !s.has_selection).then_some("Select at least two photos."),
        compare: (s.total < 2).then_some("Compare needs at least two photos in the grid."),
        export: need_one.or_else(|| {
            if s.busy {
                Some(BUSY)
            } else {
                s.export_running.then_some("An export is already running.")
            }
        }),
        copy: (s.total == 0).then_some("No photos in the grid."),
        paste: (!s.has_clipboard)
            .then_some("Copy settings first.")
            .or(need_one)
            .or(edit_blocker),
        sync: (s.targets < 2).then_some(NEED_TWO).or(edit_blocker),
        straighten: (s.targets < 2).then_some(NEED_TWO).or(edit_blocker),
        apply_preset: need_one.or(edit_blocker),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> ToolbarState {
        ToolbarState {
            total: 10,
            targets: 3,
            has_selection: true,
            has_clipboard: true,
            ..ToolbarState::default()
        }
    }

    #[test]
    fn with_a_selection_and_nothing_running_everything_is_available() {
        let w = why_disabled(&state());
        assert_eq!(
            w,
            Why {
                select_all: None,
                delete: None,
                survey: None,
                compare: None,
                export: None,
                copy: None,
                paste: None,
                sync: None,
                straighten: None,
                apply_preset: None,
            }
        );
    }

    #[test]
    fn nothing_selected_names_what_each_button_waits_for() {
        let w = why_disabled(&ToolbarState {
            total: 10,
            targets: 0,
            has_clipboard: true,
            ..ToolbarState::default()
        });
        assert_eq!(w.delete, Some(NEED_ONE));
        assert_eq!(w.export, Some(NEED_ONE));
        assert_eq!(w.paste, Some(NEED_ONE));
        assert_eq!(w.apply_preset, Some(NEED_ONE));
        assert_eq!(w.survey, Some("Select at least two photos."));
        assert_eq!(w.sync, Some(NEED_TWO));
        assert_eq!(w.straighten, Some(NEED_TWO));
        // These need photos in the grid, not a selection.
        assert_eq!(w.select_all, None);
        assert_eq!(w.compare, None);
        assert_eq!(w.copy, None);
    }

    #[test]
    fn one_photo_enables_single_photo_actions_but_not_two_photo_ones() {
        let w = why_disabled(&ToolbarState {
            targets: 1,
            ..state()
        });
        assert_eq!((w.delete, w.export, w.paste), (None, None, None));
        assert_eq!(w.survey, Some("Select at least two photos."));
        assert_eq!(w.sync, Some(NEED_TWO));
    }

    #[test]
    fn a_running_job_blocks_the_mutating_actions_with_its_own_reason() {
        let busy = why_disabled(&ToolbarState {
            busy: true,
            ..state()
        });
        for reason in [
            busy.delete,
            busy.export,
            busy.paste,
            busy.sync,
            busy.straighten,
        ] {
            assert_eq!(reason, Some(BUSY));
        }
        assert_eq!(busy.apply_preset, Some(BUSY));
        assert_eq!(busy.survey, None, "looking is never blocked");

        let exporting = why_disabled(&ToolbarState {
            export_running: true,
            ..state()
        });
        assert_eq!(exporting.export, Some("An export is already running."));
        assert_eq!(exporting.delete, None);
    }

    #[test]
    fn paste_needs_a_clipboard_first_and_an_open_prompt_blocks_editing() {
        let w = why_disabled(&ToolbarState {
            has_clipboard: false,
            ..state()
        });
        assert_eq!(w.paste, Some("Copy settings first."));
        let w = why_disabled(&ToolbarState {
            asking: true,
            ..state()
        });
        assert_eq!(w.paste, Some(ASKING));
        assert_eq!(w.sync, Some(ASKING));
    }

    #[test]
    fn an_empty_grid_disables_select_all_compare_and_copy() {
        let w = why_disabled(&ToolbarState::default());
        assert!(w.select_all.is_some() && w.compare.is_some() && w.copy.is_some());
    }
}
