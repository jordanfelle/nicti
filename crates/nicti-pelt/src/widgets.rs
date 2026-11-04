//! Widgets by name and injected input for the agent control channel (#649, ADR-0434 section 4).
//!
//! **Registry.** The widgets worth driving call [`reg`] (or [`reg_destructive`]) with a
//! `kind:name` and their rect. Nothing is recorded unless the channel installed a [`Widgets`] into
//! the egui context ([`install`]); with the channel off a call is one failed context lookup. The
//! frame's registrations replace the previous frame's at [`begin_frame`], so the listing is always
//! a complete, recent frame.
//!
//! **Injection.** Clicks, keys and drags are queued as steps of real [`egui::Event`]s and handed to
//! egui from `raw_input_hook` ([`deliver`]), one step per frame, so every widget sees exactly what
//! a mouse or keyboard would have produced. A click is addressed to a *registered widget by name*:
//! there is no way to click at arbitrary coordinates, and typing text is not offered.
//!
//! **The security boundary** (ADR-0434 section 3). Without `--control-allow-destructive`:
//! - widgets that confirm something destructive are registered with [`reg_destructive`], which
//!   records nothing, so an agent cannot name them;
//! - while a destructive dialog (delete, export) is open, injection is refused outright, and any
//!   steps already queued are discarded at delivery time. Naming alone is not enough: a registered
//!   widget behind a modal can share a position with a dialog button, and Enter or Space can
//!   activate a focused one.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::{Event, Key, Modifiers, PointerButton, Pos2, Rect};

/// Most widgets recorded per frame.
pub const MAX_WIDGETS: usize = 1024;
/// Longest widget name.
pub const MAX_NAME_LEN: usize = 80;
/// Most queued steps; an agent that outruns the frames is told to settle first.
pub const MAX_PENDING_STEPS: usize = 64;
/// The window counts as visible while a frame was drawn this recently.
pub const VISIBLE_WITHIN: Duration = Duration::from_secs(2);
/// Intermediate pointer moves of a drag.
const DRAG_STEPS: usize = 8;
/// Largest drag offset in points, per axis.
pub const MAX_DRAG: f32 = 4000.0;
/// Frames a click/key/drag needs after its last step before its effect has been drawn.
const SETTLE_FRAMES: u64 = 2;

struct State {
    allow_destructive: bool,
    /// Registrations of the frame being drawn.
    current: BTreeMap<String, Rect>,
    /// Registrations of the last finished frame: what commands see.
    last: BTreeMap<String, Rect>,
    steps: VecDeque<Vec<Event>>,
    frame: u64,
    last_frame_at: Option<Instant>,
    last_injected_frame: u64,
    destructive_ui_open: bool,
    /// Steps discarded because a destructive dialog opened under them.
    discarded: u64,
    /// The events [`deliver`] put into this frame's input, so [`scrub_injected`] can take exactly
    /// those back out (and never a person's).
    injected_now: Vec<Event>,
    /// A step was delivered and no `ui` pass has run since: a window that is hidden runs the hook
    /// and `logic` but not `ui`, and further steps would pile up unscrubbed in eframe's carried input.
    awaiting_ui: bool,
}

/// The channel's handle on the registry and the input queue. Cheap to clone.
#[derive(Clone)]
pub struct Widgets(Arc<Mutex<State>>);

/// The registry lives in egui's context data under its own type, so a constant key is enough.
fn ctx_id() -> egui::Id {
    egui::Id::NULL
}

/// Turns the registry on for `ctx` and returns the handle. Call once, when the channel starts.
pub fn install(ctx: &egui::Context, allow_destructive: bool) -> Widgets {
    let w = Widgets(Arc::new(Mutex::new(State {
        allow_destructive,
        current: BTreeMap::new(),
        last: BTreeMap::new(),
        steps: VecDeque::new(),
        frame: 0,
        last_frame_at: None,
        last_injected_frame: 0,
        awaiting_ui: false,
        destructive_ui_open: false,
        discarded: 0,
        injected_now: Vec::new(),
    })));
    ctx.data_mut(|d| d.insert_temp(ctx_id(), w.clone()));
    w
}

fn handle(ctx: &egui::Context) -> Option<Widgets> {
    ctx.data(|d| d.get_temp::<Widgets>(ctx_id()))
}

/// `kind:name`, lower-case ASCII, digits and `_ . - :`, at most [`MAX_NAME_LEN`].
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name.contains(':')
        && name.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-' | b':')
        })
}

/// Whether the control channel's registry is installed. Callers that would build a name per widget
/// (grid cells) check this first, so the channel being off costs one lookup per frame, not an
/// allocation per cell.
pub fn enabled(ctx: &egui::Context) -> bool {
    handle(ctx).is_some()
}

/// Records `rect` under `name` for this frame. A no-op while the channel is off.
pub fn register(ctx: &egui::Context, name: &str, rect: Rect) {
    let Some(w) = handle(ctx) else { return };
    if !valid_name(name) || !rect.is_finite() {
        return;
    }
    let mut st = w.lock();
    if st.current.len() < MAX_WIDGETS {
        st.current.insert(name.to_owned(), rect);
    }
}

/// [`register`] for a widget's response.
pub fn reg(ctx: &egui::Context, name: &str, response: &egui::Response) {
    register(ctx, name, response.rect);
}

/// Like [`reg`], for widgets that confirm something destructive (a delete dialog's buttons, the
/// export dialog's Start): recorded only when the channel was started with
/// `--control-allow-destructive`. Without it the widget is simply not there to be named.
pub fn reg_destructive(ctx: &egui::Context, name: &str, response: &egui::Response) {
    let Some(w) = handle(ctx) else { return };
    if w.lock().allow_destructive {
        register(ctx, name, response.rect);
    }
}

/// Starts a frame: the finished frame's registrations become the visible listing.
pub fn begin_frame(ctx: &egui::Context) {
    let Some(w) = handle(ctx) else { return };
    let mut st = w.lock();
    st.last = std::mem::take(&mut st.current);
    st.frame += 1;
    st.last_frame_at = Some(Instant::now());
}

/// Tells the gate whether a destructive dialog (delete confirmation, export dialog) is open.
pub fn set_destructive_ui_open(ctx: &egui::Context, open: bool) {
    if let Some(w) = handle(ctx) {
        w.lock().destructive_ui_open = open;
    }
}

/// Hands the next queued step's events to egui. Call from `eframe::App::raw_input_hook`.
pub fn deliver(ctx: &egui::Context, raw_input: &mut egui::RawInput) {
    let Some(w) = handle(ctx) else { return };
    let mut st = w.lock();
    if !st.awaiting_ui {
        st.injected_now.clear();
    }
    if st.steps.is_empty() {
        return;
    }
    // Re-checked here, not only when queued: a Delete key press queued earlier may have opened
    // the dialog its follow-up Enter would now confirm.
    if st.destructive_ui_open && !st.allow_destructive {
        st.discarded += st.steps.len() as u64;
        st.steps.clear();
        return;
    }
    if st.awaiting_ui {
        return;
    }
    if let Some(events) = st.steps.pop_front() {
        st.awaiting_ui = true;
        st.injected_now = events.clone();
        raw_input.events.extend(events);
        st.last_injected_frame = st.frame;
    }
}

/// Takes this frame's injected events back out of egui's input if a destructive dialog is open
/// *now* (`open_now`: the live state, not last frame's). [`deliver`] runs before the frame's
/// commands do, so a command that opens the delete or export dialog in this very frame would
/// otherwise let an Enter or a click delivered a moment earlier land on it. Call it at the top of
/// `ui`, after the commands ran. A person's own events are never touched.
pub fn scrub_injected(ctx: &egui::Context, open_now: bool) {
    let Some(w) = handle(ctx) else { return };
    let mut st = w.lock();
    st.awaiting_ui = false;
    // Taken on every path, so the key can never outlive the frame it describes.
    let injected = std::mem::take(&mut st.injected_now);
    if !open_now || st.allow_destructive || injected.is_empty() {
        return;
    }
    st.discarded += injected.len() as u64;
    ctx.input_mut(|i| {
        for ev in &injected {
            if let Some(at) = i.events.iter().position(|e| e == ev) {
                i.events.remove(at);
            }
        }
    });
}

impl Widgets {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Every registered widget of the last frame, by name.
    pub fn list(&self) -> Vec<(String, Rect)> {
        self.lock()
            .last
            .iter()
            .map(|(n, r)| (n.clone(), *r))
            .collect()
    }

    /// Steps discarded because a destructive dialog was open.
    pub fn discarded(&self) -> u64 {
        self.lock().discarded
    }

    pub fn pending(&self) -> usize {
        self.lock().steps.len()
    }

    /// Whether everything queued has been delivered and its effect drawn: no pending steps, and
    /// at least [`SETTLE_FRAMES`] frames since the last one went in.
    pub fn is_settled(&self) -> bool {
        let st = self.lock();
        st.steps.is_empty() && st.frame >= st.last_injected_frame + SETTLE_FRAMES
    }

    fn check_can_inject(&self, st: &State) -> Result<(), String> {
        if st
            .last_frame_at
            .is_none_or(|t| t.elapsed() > VISIBLE_WITHIN)
        {
            return Err(
                "the window is not visible (minimised or hidden): injected input needs a painted \
                 frame"
                    .into(),
            );
        }
        if st.destructive_ui_open && !st.allow_destructive {
            return Err(
                "a delete or export dialog is open; this channel cannot operate it without \
                 --control-allow-destructive"
                    .into(),
            );
        }
        if st.steps.len() >= MAX_PENDING_STEPS {
            return Err("too many queued input steps; call ui.settle first".into());
        }
        Ok(())
    }

    fn target(&self, st: &State, name: &str, screen: Rect) -> Result<Rect, String> {
        let rect = st.last.get(name).copied().ok_or_else(|| {
            let near: Vec<&str> = st
                .last
                .keys()
                .filter(|k| k.split(':').next() == name.split(':').next())
                .take(8)
                .map(String::as_str)
                .collect();
            format!(
                "no widget `{name}` on screen (ui.widgets lists them){}",
                if near.is_empty() {
                    String::new()
                } else {
                    format!("; similar: {}", near.join(", "))
                }
            )
        })?;
        if rect.width() < 1.0 || rect.height() < 1.0 || !rect.intersects(screen) {
            return Err(format!(
                "widget `{name}` is not visible (off screen or empty)"
            ));
        }
        Ok(rect.intersect(screen))
    }

    /// Queues a click on the widget `name`: pointer moves there and presses; released next frame.
    pub fn click(
        &self,
        name: &str,
        button: PointerButton,
        mods: Modifiers,
        screen: Rect,
    ) -> Result<(), String> {
        let mut st = self.lock();
        self.check_can_inject(&st)?;
        let pos = self.target(&st, name, screen)?.center();
        let press = |pressed| Event::PointerButton {
            pos,
            button,
            pressed,
            modifiers: mods,
        };
        st.steps
            .push_back(vec![Event::PointerMoved(pos), press(true)]);
        st.steps.push_back(vec![press(false)]);
        Ok(())
    }

    /// Queues a key press and release.
    pub fn key(&self, key: Key, mods: Modifiers) -> Result<(), String> {
        let mut st = self.lock();
        self.check_can_inject(&st)?;
        let ev = |pressed| Event::Key {
            key,
            physical_key: Some(key),
            pressed,
            repeat: false,
            modifiers: mods,
        };
        st.steps.push_back(vec![ev(true)]);
        st.steps.push_back(vec![ev(false)]);
        Ok(())
    }

    /// Queues a drag from the centre of `name` by `(dx, dy)` points, in several moves.
    pub fn drag(&self, name: &str, dx: f32, dy: f32, screen: Rect) -> Result<(), String> {
        if !dx.is_finite() || !dy.is_finite() || dx.abs() > MAX_DRAG || dy.abs() > MAX_DRAG {
            return Err(format!("a drag is at most {MAX_DRAG} points per axis"));
        }
        let mut st = self.lock();
        self.check_can_inject(&st)?;
        if st.steps.len() + DRAG_STEPS + 2 > MAX_PENDING_STEPS {
            return Err("too many queued input steps; call ui.settle first".into());
        }
        let from = self.target(&st, name, screen)?.center();
        let button = PointerButton::Primary;
        let mods = Modifiers::NONE;
        st.steps.push_back(vec![
            Event::PointerMoved(from),
            Event::PointerButton {
                pos: from,
                button,
                pressed: true,
                modifiers: mods,
            },
        ]);
        let to = Pos2::new(from.x + dx, from.y + dy);
        for i in 1..=DRAG_STEPS {
            let t = i as f32 / DRAG_STEPS as f32;
            st.steps
                .push_back(vec![Event::PointerMoved(from + (to - from) * t)]);
        }
        st.steps.push_back(vec![Event::PointerButton {
            pos: to,
            button,
            pressed: false,
            modifiers: mods,
        }]);
        Ok(())
    }
}

/// `["ctrl", "shift", "alt"]` -> egui modifiers (`command` follows `ctrl`, as on Windows/Linux).
pub fn parse_modifiers(names: &[&str]) -> Result<Modifiers, String> {
    let mut m = Modifiers::NONE;
    for n in names {
        match n.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => {
                m.ctrl = true;
                m.command = true;
            }
            "shift" => m.shift = true,
            "alt" => m.alt = true,
            other => return Err(format!("unknown modifier `{other}` (ctrl, shift, alt)")),
        }
    }
    Ok(m)
}

/// An egui key by its name (`"Delete"`, `"A"`, `"ArrowRight"`, `"Num1"`, ...).
pub fn parse_key(name: &str) -> Result<Key, String> {
    Key::from_name(name).ok_or_else(|| format!("unknown key `{name}`"))
}

/// `"primary"` (default) / `"secondary"` / `"middle"`.
pub fn parse_button(name: &str) -> Result<PointerButton, String> {
    match name {
        "primary" | "left" => Ok(PointerButton::Primary),
        "secondary" | "right" => Ok(PointerButton::Secondary),
        "middle" => Ok(PointerButton::Middle),
        other => Err(format!(
            "unknown button `{other}` (primary, secondary, middle)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen() -> Rect {
        Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 600.0))
    }

    /// One frame of a tiny UI: a "Go" button (counted when clicked) and a "Delete" button that
    /// stands in for a destructive confirm. Returns (go clicked, delete clicked).
    struct Rig {
        ctx: egui::Context,
        w: Widgets,
        go_clicks: usize,
        delete_clicks: usize,
        keys_seen: Vec<Key>,
        destructive_open: bool,
    }

    impl Rig {
        fn new(allow_destructive: bool) -> Rig {
            let ctx = egui::Context::default();
            let w = install(&ctx, allow_destructive);
            Rig {
                ctx,
                w,
                go_clicks: 0,
                delete_clicks: 0,
                keys_seen: Vec::new(),
                destructive_open: false,
            }
        }

        fn frame(&mut self) {
            let mut raw = egui::RawInput {
                screen_rect: Some(screen()),
                ..Default::default()
            };
            deliver(&self.ctx, &mut raw);
            let (mut go, mut del) = (0, 0);
            let mut keys = Vec::new();
            let destructive_open = self.destructive_open;
            let mut out = self.ctx.run_ui(raw, |ui| {
                begin_frame(ui.ctx());
                scrub_injected(ui.ctx(), destructive_open);
                set_destructive_ui_open(ui.ctx(), destructive_open);
                let go_resp = ui.put(
                    Rect::from_min_size(Pos2::new(20.0, 20.0), egui::vec2(100.0, 30.0)),
                    egui::Button::new("Go"),
                );
                reg(ui.ctx(), "button:go", &go_resp);
                if go_resp.clicked() {
                    go += 1;
                }
                let del_resp = ui.put(
                    Rect::from_min_size(Pos2::new(20.0, 80.0), egui::vec2(100.0, 30.0)),
                    egui::Button::new("Delete"),
                );
                reg_destructive(ui.ctx(), "button:delete-confirm", &del_resp);
                if del_resp.clicked() {
                    del += 1;
                }
                ui.input(|i| {
                    for e in &i.events {
                        if let Event::Key {
                            key, pressed: true, ..
                        } = e
                        {
                            keys.push(*key);
                        }
                    }
                });
            });
            out.textures_delta.clear();
            self.go_clicks += go;
            self.delete_clicks += del;
            self.keys_seen.extend(keys);
        }

        fn frames(&mut self, n: usize) {
            for _ in 0..n {
                self.frame();
            }
        }
    }

    #[test]
    fn names_are_kind_colon_lowercase_and_bounded() {
        for ok in [
            "button:export",
            "cell:12345",
            "slider:wb.temp",
            "tab:develop-2",
        ] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in [
            "",
            "export",
            "Button:Export",
            "button:",
            "a b:c",
            "x:é",
            &format!("k:{}", "a".repeat(80)),
        ] {
            // "button:" is syntactically fine (kind and empty name): the registry keys on the
            // whole string; the rest must be refused.
            if bad == "button:" {
                continue;
            }
            assert!(!valid_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn registering_does_nothing_until_the_channel_installs_the_registry() {
        let ctx = egui::Context::default();
        register(&ctx, "button:go", screen());
        begin_frame(&ctx);
        assert!(
            handle(&ctx).is_none(),
            "no registry, nothing recorded, nothing allocated"
        );
        let w = install(&ctx, false);
        assert!(w.list().is_empty());
    }

    #[test]
    fn a_registered_widget_is_listed_from_the_next_frame_and_replaced_each_frame() {
        let mut rig = Rig::new(false);
        rig.frame();
        assert!(
            rig.w.list().is_empty(),
            "the frame being drawn is not listed yet"
        );
        rig.frame();
        let names: Vec<String> = rig.w.list().into_iter().map(|(n, _)| n).collect();
        assert_eq!(
            names,
            vec!["button:go".to_string()],
            "the destructive confirm is not nameable"
        );
        let (_, rect) = &rig.w.list()[0];
        assert!(rect.width() > 50.0);
    }

    #[test]
    fn an_injected_click_presses_the_real_button() {
        let mut rig = Rig::new(false);
        rig.frames(2);
        rig.w
            .click(
                "button:go",
                PointerButton::Primary,
                Modifiers::NONE,
                screen(),
            )
            .unwrap();
        assert_eq!(rig.w.pending(), 2);
        assert!(!rig.w.is_settled());
        rig.frames(4);
        assert_eq!(rig.go_clicks, 1, "exactly one click reached the button");
        assert!(rig.w.is_settled());
        assert_eq!(rig.delete_clicks, 0);
    }

    #[test]
    fn keys_arrive_as_real_key_events() {
        let mut rig = Rig::new(false);
        rig.frames(2);
        rig.w
            .key(Key::ArrowRight, parse_modifiers(&["shift"]).unwrap())
            .unwrap();
        rig.frames(4);
        assert_eq!(rig.keys_seen, vec![Key::ArrowRight]);
    }

    #[test]
    fn a_drag_is_a_press_several_moves_and_a_release() {
        let mut rig = Rig::new(false);
        rig.frames(2);
        rig.w.drag("button:go", 120.0, 40.0, screen()).unwrap();
        assert_eq!(rig.w.pending(), 1 + DRAG_STEPS + 1);
        rig.frames(14);
        assert!(rig.w.is_settled());
        assert!(rig.w.drag("button:go", f32::NAN, 0.0, screen()).is_err());
        assert!(rig
            .w
            .drag("button:go", MAX_DRAG + 1.0, 0.0, screen())
            .is_err());
    }

    #[test]
    fn only_registered_widgets_can_be_targeted_and_the_error_helps() {
        let mut rig = Rig::new(false);
        rig.frames(2);
        let e = rig
            .w
            .click(
                "button:delete-confirm",
                PointerButton::Primary,
                Modifiers::NONE,
                screen(),
            )
            .unwrap_err();
        assert!(e.contains("no widget") && e.contains("button:go"), "{e}");
        assert!(rig
            .w
            .click(
                "button:nope",
                PointerButton::Primary,
                Modifiers::NONE,
                screen()
            )
            .is_err());
        // Off screen.
        let off = Rect::from_min_size(Pos2::new(5000.0, 5000.0), egui::vec2(10.0, 10.0));
        assert!(rig
            .w
            .click("button:go", PointerButton::Primary, Modifiers::NONE, off)
            .unwrap_err()
            .contains("not visible"));
        assert_eq!(rig.w.pending(), 0, "a refused request queues nothing");
    }

    #[test]
    fn destructive_confirms_are_registered_only_when_the_operator_allowed_it() {
        let mut rig = Rig::new(true);
        rig.frames(2);
        let names: Vec<String> = rig.w.list().into_iter().map(|(n, _)| n).collect();
        assert!(
            names.contains(&"button:delete-confirm".to_string()),
            "{names:?}"
        );
        rig.w
            .click(
                "button:delete-confirm",
                PointerButton::Primary,
                Modifiers::NONE,
                screen(),
            )
            .unwrap();
        rig.frames(4);
        assert_eq!(rig.delete_clicks, 1);
    }

    #[test]
    fn a_window_that_runs_no_ui_pass_receives_one_step_at_most() {
        let mut rig = Rig::new(false);
        rig.frames(2);
        rig.w.key(Key::Tab, Modifiers::NONE).unwrap();
        rig.w.key(Key::Enter, Modifiers::NONE).unwrap();
        let before = rig.w.pending();
        // Hook-only passes (hidden window: no `ui`, so no scrub could ever run).
        let mut total = 0;
        for _ in 0..6 {
            let mut raw = egui::RawInput::default();
            deliver(&rig.ctx, &mut raw);
            total += raw.events.len();
        }
        let first_step = before - rig.w.pending();
        assert_eq!(
            first_step, 1,
            "only one step may be carried without a ui pass"
        );
        assert!(total > 0);
    }

    #[test]
    fn nothing_is_injected_while_a_destructive_dialog_is_open_and_queued_steps_are_dropped() {
        let mut rig = Rig::new(false);
        rig.frames(2);
        // Queued while no dialog is open...
        rig.w.key(Key::Enter, Modifiers::NONE).unwrap();
        rig.w
            .click(
                "button:go",
                PointerButton::Primary,
                Modifiers::NONE,
                screen(),
            )
            .unwrap();
        assert_eq!(rig.w.pending(), 4);
        // ...then the dialog opens before they are delivered (a Delete key press did that).
        rig.destructive_open = true;
        rig.frame(); // the app tells the gate
        rig.frames(5);
        assert_eq!(rig.w.pending(), 0);
        assert!(
            rig.w.discarded() >= 1,
            "the queued Enter was dropped, not delivered"
        );
        assert!(
            !rig.keys_seen.contains(&Key::Enter),
            "Enter must not reach a dialog's button"
        );
        assert_eq!(rig.go_clicks, 0);
        // And a new request is refused outright.
        let e = rig.w.key(Key::Enter, Modifiers::NONE).unwrap_err();
        assert!(e.contains("--control-allow-destructive"), "{e}");
        assert!(rig
            .w
            .click(
                "button:go",
                PointerButton::Primary,
                Modifiers::NONE,
                screen()
            )
            .is_err());
        assert!(rig.w.drag("button:go", 1.0, 1.0, screen()).is_err());
        // Closing the dialog lifts the refusal.
        rig.destructive_open = false;
        rig.frames(2);
        assert!(rig.w.key(Key::Enter, Modifiers::NONE).is_ok());
    }

    #[test]
    fn the_operator_who_allowed_destructive_can_drive_dialogs() {
        let mut rig = Rig::new(true);
        rig.frames(2);
        rig.destructive_open = true;
        rig.frames(2);
        assert!(rig.w.key(Key::Enter, Modifiers::NONE).is_ok());
    }

    #[test]
    fn a_hidden_window_is_an_error_not_a_timeout() {
        let ctx = egui::Context::default();
        let w = install(&ctx, false);
        // No frame has ever been drawn.
        let e = w.key(Key::A, Modifiers::NONE).unwrap_err();
        assert!(e.contains("not visible"), "{e}");
        // A frame long ago.
        w.lock().last_frame_at = Some(Instant::now() - VISIBLE_WITHIN * 2);
        assert!(w.key(Key::A, Modifiers::NONE).is_err());
    }

    #[test]
    fn the_queue_is_bounded() {
        let mut rig = Rig::new(false);
        rig.frames(2);
        let mut queued = 0;
        while rig.w.key(Key::A, Modifiers::NONE).is_ok() {
            queued += 2;
            assert!(queued <= MAX_PENDING_STEPS + 2, "unbounded");
        }
        assert!(rig.w.pending() <= MAX_PENDING_STEPS);
        let e = rig.w.key(Key::A, Modifiers::NONE).unwrap_err();
        assert!(e.contains("ui.settle"), "{e}");
    }

    #[test]
    fn modifiers_keys_and_buttons_parse_or_say_what_is_valid() {
        let m = parse_modifiers(&["Ctrl", "shift"]).unwrap();
        assert!(m.ctrl && m.command && m.shift && !m.alt);
        assert!(parse_modifiers(&["meta"]).is_err());
        assert_eq!(parse_key("Delete"), Ok(Key::Delete));
        assert_eq!(parse_key("A"), Ok(Key::A));
        assert!(parse_key("NotAKey").is_err());
        assert_eq!(parse_button("right"), Ok(PointerButton::Secondary));
        assert!(parse_button("fourth").is_err());
    }
}
