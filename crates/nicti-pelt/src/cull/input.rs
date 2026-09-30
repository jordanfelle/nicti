//! Reading the culling keys from egui (#32): one place that turns this frame's key events into
//! [`KeyCommand`]s, so the Library, Loupe, Survey and Compare views all mean the same thing by
//! `3`, `X`, Ctrl+Z, Delete.
//!
//! Two egui details this exists to get right:
//! - `key_pressed` *includes key-repeat*. Holding `X` would flip a reject on and off at the OS
//!   repeat rate, so this reads the raw events and ignores repeats -- one physical press, one
//!   command.
//! - Two presses of a key inside one frame (fast typing on a slow frame) are two commands, not
//!   one, so a burst of `3`, `4` is never collapsed.

use egui::{Event, Key, Modifiers};

use super::keys::{action_for, CullAction};

/// What the user asked for with the keyboard this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCommand {
    /// A marking key. `invert_advance` is set when Shift was held: do the opposite of the
    /// auto-advance toggle for this one press.
    Mark {
        action: CullAction,
        invert_advance: bool,
    },
    Undo,
    Redo,
    /// Delete / Backspace: open the delete prompt for the current targets.
    Delete,
    /// `N`: survey the selected photos.
    Survey,
    /// `C`: compare the selected photos.
    Compare,
}

/// The key a press is bound by. egui reports the *logical* key, and a shifted digit is a symbol
/// (Shift+3 is `#`, Shift+1 is `!`), so binding by it would make Shift+digit -- the "invert
/// auto-advance for this press" chord -- dead on a real keyboard, and unshifted digits dead on
/// layouts (AZERTY) whose number row types symbols. So the number row is bound by *position*:
/// the physical digit key, whatever it types. Every other key stays logical, so letters follow the
/// user's layout.
pub fn binding_key(logical: Key, physical: Option<Key>) -> Key {
    match physical {
        Some(
            p @ (Key::Num0
            | Key::Num1
            | Key::Num2
            | Key::Num3
            | Key::Num4
            | Key::Num5
            | Key::Num6
            | Key::Num7
            | Key::Num8
            | Key::Num9),
        ) => p,
        _ => logical,
    }
}

/// Classifies one non-repeat key press, `None` for a key culling doesn't use.
pub fn classify(key: Key, mods: Modifiers) -> Option<KeyCommand> {
    if mods.command {
        return match key {
            Key::Z if mods.shift => Some(KeyCommand::Redo),
            Key::Z => Some(KeyCommand::Undo),
            Key::Y => Some(KeyCommand::Redo),
            _ => None,
        };
    }
    if mods.alt {
        return None;
    }
    match key {
        Key::Delete | Key::Backspace => Some(KeyCommand::Delete),
        Key::N => Some(KeyCommand::Survey),
        Key::C => Some(KeyCommand::Compare),
        _ => action_for(key).map(|action| KeyCommand::Mark {
            action,
            invert_advance: mods.shift,
        }),
    }
}

/// The culling commands pressed this frame, in order. Empty while a text field has keyboard focus
/// (typing "3" into the folder box must not rate a photo). The keys are consumed, so no other
/// handler also acts on them.
pub fn poll(ctx: &egui::Context) -> Vec<KeyCommand> {
    if ctx.egui_wants_keyboard_input() {
        return Vec::new();
    }
    ctx.input_mut(|i| {
        // (logical key as egui reports it, key to classify by, modifiers)
        let presses: Vec<(Key, Key, Modifiers)> = i
            .events
            .iter()
            .filter_map(|e| match e {
                Event::Key {
                    key,
                    physical_key,
                    pressed: true,
                    repeat: false,
                    modifiers,
                } => Some((*key, binding_key(*key, *physical_key), *modifiers)),
                _ => None,
            })
            .collect();

        let mut commands = Vec::new();
        let mut to_consume: Vec<(Key, Modifiers)> = Vec::new();
        for (logical, key, mods) in presses {
            if let Some(cmd) = classify(key, mods) {
                commands.push(cmd);
                if !to_consume.contains(&(logical, mods)) {
                    to_consume.push((logical, mods));
                }
            }
        }
        // Swallow every event of a claimed key -- including its repeats, which were ignored above
        // but must not leak to another handler as if they were fresh presses.
        for (key, mods) in to_consume {
            i.consume_key(mods, key);
        }
        commands
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{vec2, Pos2, Rect};

    fn key_event(key: Key, mods: Modifiers, pressed: bool, repeat: bool) -> Event {
        Event::Key {
            key,
            physical_key: Some(key),
            pressed,
            repeat,
            modifiers: mods,
        }
    }

    /// One physical tap: press then release. egui recomputes `repeat` from its own held-key
    /// tracking (a press of a key it still believes is down is a repeat whatever the platform
    /// said), so a test that never releases would see its second press swallowed -- real
    /// keyboards always send the release.
    fn tap(key: Key, mods: Modifiers) -> Vec<Event> {
        vec![
            key_event(key, mods, true, false),
            key_event(key, mods, false, false),
        ]
    }

    fn taps(keys: &[(Key, Modifiers)]) -> Vec<Event> {
        keys.iter().flat_map(|(k, m)| tap(*k, *m)).collect()
    }

    fn plain(keys: &[Key]) -> Vec<Event> {
        taps(
            &keys
                .iter()
                .map(|k| (*k, Modifiers::NONE))
                .collect::<Vec<_>>(),
        )
    }

    /// Runs one headless frame with `events` and returns what `poll` saw.
    fn frame(ctx: &egui::Context, events: Vec<Event>) -> Vec<KeyCommand> {
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(400.0, 300.0))),
            events,
            ..Default::default()
        };
        let mut out = Vec::new();
        let output = ctx.run_ui(input, |_ui| {
            out = poll(ctx);
        });
        output.drop_without_applying_deltas();
        out
    }

    #[test]
    fn a_number_key_is_a_rating_mark_that_does_not_invert_advance() {
        let ctx = egui::Context::default();
        assert_eq!(
            frame(&ctx, plain(&[Key::Num3])),
            vec![KeyCommand::Mark {
                action: CullAction::SetRating(Some(3)),
                invert_advance: false
            }]
        );
    }

    #[test]
    fn shift_inverts_auto_advance_for_that_press_only() {
        let ctx = egui::Context::default();
        let shifted = frame(&ctx, tap(Key::X, Modifiers::SHIFT));
        assert_eq!(
            shifted,
            vec![KeyCommand::Mark {
                action: CullAction::ToggleReject,
                invert_advance: true
            }]
        );
        let unshifted = frame(&ctx, plain(&[Key::X]));
        assert!(matches!(
            unshifted[0],
            KeyCommand::Mark {
                invert_advance: false,
                ..
            }
        ));
    }

    #[test]
    fn holding_a_key_does_not_re_fire_it() {
        let ctx = egui::Context::default();
        // Press, then the OS auto-repeat events while it stays held.
        let cmds = frame(
            &ctx,
            vec![
                key_event(Key::X, Modifiers::NONE, true, false),
                key_event(Key::X, Modifiers::NONE, true, true),
                key_event(Key::X, Modifiers::NONE, true, true),
            ],
        );
        assert_eq!(cmds.len(), 1, "OS key-repeat must not toggle reject again");
        // Later frames while it is still held carry only repeats: nothing at all.
        assert!(frame(&ctx, vec![key_event(Key::X, Modifiers::NONE, true, true)]).is_empty());
        assert!(frame(&ctx, vec![key_event(Key::X, Modifiers::NONE, true, true)]).is_empty());
        // Releasing and tapping again is a new press.
        frame(&ctx, vec![key_event(Key::X, Modifiers::NONE, false, false)]);
        assert_eq!(frame(&ctx, plain(&[Key::X])).len(), 1);
    }

    #[test]
    fn a_key_the_platform_reports_as_a_fresh_press_while_still_held_is_a_repeat() {
        // egui, not the platform, decides what counts as a repeat: a "press" of a key it still
        // believes is down is one. Guards the assumption `poll` relies on.
        let ctx = egui::Context::default();
        assert_eq!(
            frame(&ctx, vec![key_event(Key::P, Modifiers::NONE, true, false)]).len(),
            1
        );
        assert!(frame(&ctx, vec![key_event(Key::P, Modifiers::NONE, true, false)]).is_empty());
    }

    #[test]
    fn two_presses_in_one_frame_are_two_commands_in_order() {
        let ctx = egui::Context::default();
        let cmds = frame(&ctx, plain(&[Key::Num3, Key::Num4]));
        assert_eq!(cmds.len(), 2);
        assert!(matches!(
            cmds[0],
            KeyCommand::Mark {
                action: CullAction::SetRating(Some(3)),
                ..
            }
        ));
        assert!(matches!(
            cmds[1],
            KeyCommand::Mark {
                action: CullAction::SetRating(Some(4)),
                ..
            }
        ));
        // The same key tapped twice inside one frame counts twice too.
        assert_eq!(frame(&ctx, plain(&[Key::P, Key::P])).len(), 2);
    }

    #[test]
    fn undo_and_redo_chords() {
        let ctx = egui::Context::default();
        assert_eq!(
            frame(&ctx, tap(Key::Z, Modifiers::COMMAND)),
            vec![KeyCommand::Undo]
        );
        assert_eq!(
            frame(&ctx, tap(Key::Z, Modifiers::COMMAND | Modifiers::SHIFT)),
            vec![KeyCommand::Redo],
            "Ctrl+Shift+Z is redo, not undo"
        );
        assert_eq!(
            frame(&ctx, tap(Key::Y, Modifiers::COMMAND)),
            vec![KeyCommand::Redo]
        );
    }

    #[test]
    fn a_control_chord_never_marks() {
        let ctx = egui::Context::default();
        // Ctrl+3, Ctrl+X (cut), Alt+P: none of these are marks.
        for (k, m) in [
            (Key::Num3, Modifiers::COMMAND),
            (Key::X, Modifiers::COMMAND),
            (Key::P, Modifiers::ALT),
        ] {
            assert!(frame(&ctx, tap(k, m)).is_empty(), "{k:?}");
        }
    }

    #[test]
    fn delete_survey_and_compare_keys() {
        let ctx = egui::Context::default();
        assert_eq!(frame(&ctx, plain(&[Key::Delete])), vec![KeyCommand::Delete]);
        assert_eq!(
            frame(&ctx, plain(&[Key::Backspace])),
            vec![KeyCommand::Delete]
        );
        assert_eq!(frame(&ctx, plain(&[Key::N])), vec![KeyCommand::Survey]);
        assert_eq!(frame(&ctx, plain(&[Key::C])), vec![KeyCommand::Compare]);
    }

    #[test]
    fn navigation_and_unrelated_keys_are_left_alone() {
        let ctx = egui::Context::default();
        assert!(frame(
            &ctx,
            plain(&[Key::ArrowLeft, Key::Space, Key::A, Key::Enter])
        )
        .is_empty());
    }

    #[test]
    fn claimed_keys_are_consumed_so_no_other_handler_sees_them() {
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(400.0, 300.0))),
            events: plain(&[Key::Num3, Key::ArrowLeft]),
            ..Default::default()
        };
        let mut seen = (true, false);
        let output = ctx.run_ui(input, |_ui| {
            poll(&ctx);
            seen = ctx.input(|i| (i.key_pressed(Key::Num3), i.key_pressed(Key::ArrowLeft)));
        });
        output.drop_without_applying_deltas();
        assert_eq!(seen, (false, true), "3 was consumed, the arrow was not");
    }

    #[test]
    fn typing_in_a_text_field_never_marks_a_photo() {
        let ctx = egui::Context::default();
        let mut text = String::new();
        let run = |events: Vec<Event>, text: &mut String| {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(400.0, 300.0))),
                events,
                ..Default::default()
            };
            let mut cmds = Vec::new();
            let output = ctx.run_ui(input, |ui| {
                let edit = ui.text_edit_singleline(text);
                edit.request_focus();
                cmds = poll(&ctx);
            });
            output.drop_without_applying_deltas();
            cmds
        };
        // Frame 1 focuses the field; frame 2 has it focused when the key arrives.
        run(vec![], &mut text);
        let cmds = run(plain(&[Key::Num3]), &mut text);
        assert!(
            cmds.is_empty(),
            "the folder box owns the keyboard: {cmds:?}"
        );
    }

    #[test]
    fn shift_plus_a_digit_still_marks_even_though_egui_reports_a_symbol() {
        // What a real keyboard sends: Shift+1 arrives as the logical key `!`, with the physical
        // key still the number-row `1`.
        let ctx = egui::Context::default();
        let events = vec![
            Event::Key {
                key: Key::Exclamationmark,
                physical_key: Some(Key::Num1),
                pressed: true,
                repeat: false,
                modifiers: Modifiers::SHIFT,
            },
            Event::Key {
                key: Key::Exclamationmark,
                physical_key: Some(Key::Num1),
                pressed: false,
                repeat: false,
                modifiers: Modifiers::SHIFT,
            },
        ];
        assert_eq!(
            frame(&ctx, events),
            vec![KeyCommand::Mark {
                action: CullAction::SetRating(Some(1)),
                invert_advance: true
            }],
            "Shift+1 is a one-star mark that inverts auto-advance"
        );
    }

    #[test]
    fn the_number_row_binds_by_position_and_letters_stay_logical() {
        // AZERTY-style: the number-row 1 key types a symbol rather than a digit.
        assert_eq!(
            binding_key(Key::Exclamationmark, Some(Key::Num1)),
            Key::Num1
        );
        // A letter follows the user's layout: logical wins over the physical position.
        assert_eq!(binding_key(Key::X, Some(Key::B)), Key::X);
        assert_eq!(binding_key(Key::X, None), Key::X);
        // No physical key reported: the logical digit still works.
        assert_eq!(binding_key(Key::Num3, None), Key::Num3);
    }
}
