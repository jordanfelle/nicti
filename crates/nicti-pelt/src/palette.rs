//! The command palette (#719, ADR-0434 slice 4): Ctrl+K opens a list of actions, a typed query
//! fuzzy-matches their labels, Enter runs the highlighted one, and an action that cannot run now
//! shows why. It sits on the same command table the agent control channel uses: every action is
//! one `cmd::COMMANDS` entry with fixed parameters (switch view, rate, open Export), run through
//! `cmd::dispatch` with the default policy (nothing destructive), plus "reset this control" for
//! each Develop control in the registry (`develop_controls`), which runs `develop.set`.
//!
//! Not part of this: rebinding shortcuts (the ~40 inline key sites; a separate, larger change).

use serde_json::{json, Value};

use crate::cmd::{self, CommandHost, Policy};
use crate::develop_controls::{self, Control};

/// What running an entry does.
#[derive(Clone)]
pub enum Action {
    /// One table command with fixed parameters.
    Command { id: &'static str, params: Value },
    /// `develop.set` putting a Develop control back to its default (the loaded photo is named at
    /// run time).
    ResetControl(&'static Control),
}

#[derive(Clone)]
pub struct Entry {
    pub label: String,
    pub action: Action,
}

/// Every action the palette offers, in display order.
pub fn entries() -> Vec<Entry> {
    let cmd = |label: &str, id: &'static str, params: Value| Entry {
        label: label.to_owned(),
        action: Action::Command { id, params },
    };
    let mut v = vec![
        cmd("Go to the Library", "view.set", json!({"view": "library"})),
        cmd(
            "Open the selected photo in the Loupe",
            "view.set",
            json!({"view": "loupe"}),
        ),
        cmd("Export the selected photos...", "export.open", json!({})),
    ];
    for stars in 1..=5 {
        v.push(cmd(
            &format!("Rate {stars} star{}", if stars == 1 { "" } else { "s" }),
            "photo.rate",
            json!({"rating": stars}),
        ));
    }
    v.push(cmd(
        "Reject the selected photos",
        "photo.rate",
        json!({"rating": -1}),
    ));
    v.push(cmd(
        "Clear the rating",
        "photo.rate",
        json!({"rating": null}),
    ));
    for c in develop_controls::CONTROLS {
        v.push(Entry {
            label: format!("Develop: reset {} ({})", c.spec.label, c.id),
            action: Action::ResetControl(c),
        });
    }
    v
}

/// How well `query` matches `label`: `None` when the query's letters do not all appear in order
/// (case-insensitive). Higher is better: letters at the start of a word and runs of consecutive
/// letters score extra, gaps cost a little, and a shorter label wins a tie. An empty query
/// matches everything equally.
pub fn score(query: &str, label: &str) -> Option<i32> {
    let q: Vec<char> = query
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    if q.is_empty() {
        return Some(0);
    }
    let l: Vec<char> = label.chars().flat_map(char::to_lowercase).collect();
    let mut qi = 0;
    let mut score = 0;
    let mut last: Option<usize> = None;
    for (i, &c) in l.iter().enumerate() {
        if qi < q.len() && c == q[qi] {
            let word_start = i == 0 || !l[i - 1].is_alphanumeric();
            score += 1 + if word_start { 8 } else { 0 };
            match last {
                Some(p) if p + 1 == i => score += 5,
                Some(p) => score -= (i - p - 1).min(6) as i32,
                None => score -= (i as i32).min(6),
            }
            last = Some(i);
            qi += 1;
        }
    }
    (qi == q.len()).then(|| score * 4 - (l.len() as i32).min(64) / 8)
}

/// Indices into `entries` that match `query`, best first (ties keep the table's order).
pub fn rank(query: &str, entries: &[Entry]) -> Vec<usize> {
    let mut hits: Vec<(i32, usize)> = entries
        .iter()
        .enumerate()
        .filter_map(|(i, e)| score(query, &e.label).map(|s| (s, i)))
        .collect();
    hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    hits.into_iter().map(|(_, i)| i).collect()
}

/// `Ok` when the entry can run now, else the reason it cannot.
pub fn status(host: &dyn CommandHost, e: &Entry) -> Result<(), String> {
    match &e.action {
        Action::Command { id, .. } => cmd::COMMANDS
            .iter()
            .find(|c| c.id == *id)
            .ok_or_else(|| format!("unknown command `{id}`"))
            .and_then(|c| (c.enabled)(host)),
        Action::ResetControl(_) => host.develop_ready(),
    }
}

/// Runs the entry through the command table (the default policy: nothing destructive).
pub fn run(host: &mut dyn CommandHost, e: &Entry) -> Result<Value, String> {
    match &e.action {
        Action::Command { id, params } => cmd::dispatch(host, Policy::default(), id, params),
        Action::ResetControl(c) => {
            let asset = host
                .develop_asset()
                .ok_or("no photo is loaded in Develop (open one in the loupe first)")?;
            // `null` returns an optional control (white balance temperature) to as-shot.
            let default = if c.optional {
                Value::Null
            } else {
                json!(c.spec.default)
            };
            cmd::dispatch(
                host,
                Policy::default(),
                "develop.set",
                &json!({"stage": c.stage, "asset": asset, "set": {c.field: default}}),
            )
        }
    }
}

/// The palette's own state: whether it is open, what was typed, which row is highlighted and the
/// last failure to show.
#[derive(Default)]
pub struct Palette {
    open: bool,
    query: String,
    selected: usize,
    error: Option<String>,
    entries: Vec<Entry>,
}

/// What the overlay asks the app to do.
pub enum Outcome {
    None,
    /// Run `entries[index]` (an index into [`Palette::entry`]).
    Run(usize),
}

impl Palette {
    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
        self.query.clear();
        self.selected = 0;
        self.error = None;
        if self.open && self.entries.is_empty() {
            self.entries = entries();
        }
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    pub fn fail(&mut self, message: String) {
        self.error = Some(message);
    }

    pub fn entry(&self, index: usize) -> Option<Entry> {
        self.entries.get(index).cloned()
    }

    /// The palette overlay. `host` supplies each row's availability. Returns the entry to run.
    pub fn show(&mut self, ctx: &egui::Context, host: &dyn CommandHost) -> Outcome {
        if !self.open {
            return Outcome::None;
        }
        let matches = rank(&self.query, &self.entries);
        let mut outcome = Outcome::None;
        let (mut down, mut up, mut enter, mut escape) = (false, false, false, false);
        ctx.input_mut(|i| {
            down = i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown);
            up = i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp);
            enter = i.consume_key(egui::Modifiers::NONE, egui::Key::Enter);
            escape = i.consume_key(egui::Modifiers::NONE, egui::Key::Escape);
        });
        if escape {
            self.open = false;
            return Outcome::None;
        }
        if down && !matches.is_empty() {
            self.selected = (self.selected + 1).min(matches.len() - 1);
        }
        if up {
            self.selected = self.selected.saturating_sub(1);
        }
        self.selected = self.selected.min(matches.len().saturating_sub(1));

        egui::Area::new(egui::Id::NULL.with("nicti_command_palette"))
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 80.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_width(520.0);
                    let edit = ui.add(
                        egui::TextEdit::singleline(&mut self.query)
                            .hint_text("Type a command...")
                            .desired_width(f32::INFINITY),
                    );
                    edit.request_focus();
                    if edit.changed() {
                        self.selected = 0;
                        self.error = None;
                    }
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .max_height(320.0)
                        .show(ui, |ui| {
                            if matches.is_empty() {
                                ui.weak("No matching command");
                            }
                            for (row, &i) in matches.iter().enumerate().take(60) {
                                let entry = &self.entries[i];
                                let state = status(host, entry);
                                let text = if state.is_ok() {
                                    egui::RichText::new(&entry.label)
                                } else {
                                    egui::RichText::new(&entry.label).weak()
                                };
                                let mut resp = ui.selectable_label(row == self.selected, text);
                                if let Err(reason) = &state {
                                    resp = resp.on_hover_text(reason);
                                }
                                if resp.clicked() && state.is_ok() {
                                    outcome = Outcome::Run(i);
                                }
                                if row == self.selected {
                                    resp.scroll_to_me(None);
                                }
                            }
                        });
                    if let Some(&i) = matches.get(self.selected) {
                        if let Err(reason) = status(host, &self.entries[i]) {
                            ui.separator();
                            ui.weak(format!("Unavailable: {reason}"));
                        }
                    }
                    if let Some(e) = &self.error {
                        ui.colored_label(ui.visuals().error_fg_color, e);
                    }
                });
            });
        if enter {
            if let Some(&i) = matches.get(self.selected) {
                if status(host, &self.entries[i]).is_ok() {
                    outcome = Outcome::Run(i);
                }
            }
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::tests::Mock;

    fn labels(query: &str, es: &[Entry]) -> Vec<String> {
        rank(query, es)
            .into_iter()
            .map(|i| es[i].label.clone())
            .collect()
    }

    #[test]
    fn an_empty_query_lists_everything_in_table_order_and_a_missing_letter_matches_nothing() {
        let es = entries();
        assert_eq!(rank("", &es), (0..es.len()).collect::<Vec<_>>());
        assert!(rank("qqqqzz", &es).is_empty());
        assert!(
            score("abc", "cba").is_none(),
            "letters must appear in order"
        );
        assert!(score("RATE", "rate 3 stars").is_some(), "case-insensitive");
    }

    #[test]
    fn word_starts_and_runs_rank_first() {
        let es = entries();
        // "rat" matches "Rate N stars" and "Reject..." less well than the word start of "Rate".
        let hits = labels("rate 3", &es);
        assert_eq!(hits.first().map(String::as_str), Some("Rate 3 stars"));
        // Initials: "gtl" finds "Go to the Library".
        assert_eq!(
            labels("gtl", &es).first().map(String::as_str),
            Some("Go to the Library")
        );
        // A consecutive run beats scattered letters in labels of the same length.
        assert!(score("abc", "abcxxxxxxx") > score("abc", "axxbxxcxxx"));
        // A match at word starts beats one in the middle of words.
        assert!(score("sp", "Save Preset") > score("sp", "Rasping tool"));
        // A shorter label wins a tie.
        assert!(score("exp", "Export") > score("exp", "Export the selected photos..."));
    }

    #[test]
    fn every_entry_names_a_real_command_with_params_it_accepts() {
        let host = Mock::default();
        for e in entries() {
            match &e.action {
                Action::Command { id, params } => {
                    let c = cmd::COMMANDS
                        .iter()
                        .find(|c| c.id == *id)
                        .unwrap_or_else(|| panic!("`{id}` is not a command"));
                    assert!(
                        c.risk != cmd::Risk::Destructive,
                        "the palette runs with the default policy: {id}"
                    );
                    assert!(params.is_object() || params.is_null());
                    let _ = status(&host, &e);
                }
                Action::ResetControl(c) => {
                    assert!(e.label.contains(c.id), "{}", e.label);
                }
            }
        }
        let n = entries().len();
        let unique: std::collections::HashSet<String> =
            entries().into_iter().map(|e| e.label).collect();
        assert_eq!(unique.len(), n, "labels are unique");
    }

    #[test]
    fn status_shows_why_an_action_cannot_run_and_run_goes_through_the_command_table() {
        let mut host = Mock::default(); // no photo loaded in Develop
        let es = entries();
        let reset = es
            .iter()
            .find(|e| matches!(e.action, Action::ResetControl(_)))
            .unwrap();
        let why = status(&host, reset).unwrap_err();
        assert!(why.contains("no photo is loaded in Develop"), "{why}");
        assert!(run(&mut host, reset).is_err());
        // A table command runs through `dispatch`: the view switch reaches the host.
        let go = es.iter().find(|e| e.label == "Go to the Library").unwrap();
        assert!(status(&host, go).is_ok());
        assert!(run(&mut host, go).is_ok());
    }

    #[test]
    fn toggle_resets_the_query_and_selection() {
        let mut p = Palette::default();
        assert!(!p.is_open());
        p.toggle();
        assert!(p.is_open());
        p.query = "rate".into();
        p.selected = 3;
        p.toggle();
        assert!(!p.is_open());
        p.toggle();
        assert!(p.query.is_empty() && p.selected == 0);
        assert!(p.entry(0).is_some());
        p.close();
        assert!(!p.is_open());
    }

    /// One frame of the overlay with `events` as input, returning what it asked for.
    fn frame(
        ctx: &egui::Context,
        p: &mut Palette,
        host: &Mock,
        events: Vec<egui::Event>,
    ) -> Outcome {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            events,
            ..Default::default()
        };
        let mut out = Outcome::None;
        let mut full = ctx.run_ui(raw, |ui| {
            out = p.show(ui.ctx(), host);
        });
        full.textures_delta.clear();
        out
    }

    fn key(k: egui::Key) -> egui::Event {
        egui::Event::Key {
            key: k,
            physical_key: Some(k),
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    #[test]
    fn typing_then_enter_runs_the_best_match_and_the_arrows_and_escape_work() {
        let ctx = egui::Context::default();
        let host = Mock::default();
        let mut p = Palette::default();
        // Closed: nothing happens, even with Enter.
        assert!(matches!(
            frame(&ctx, &mut p, &host, vec![key(egui::Key::Enter)]),
            Outcome::None
        ));
        p.toggle();
        frame(&ctx, &mut p, &host, vec![]); // the text box takes focus
        frame(
            &ctx,
            &mut p,
            &host,
            vec![egui::Event::Text("rate 3".into())],
        );
        let Outcome::Run(i) = frame(&ctx, &mut p, &host, vec![key(egui::Key::Enter)]) else {
            panic!("Enter on a match must run it");
        };
        assert_eq!(p.entry(i).unwrap().label, "Rate 3 stars");
        // Arrow keys move the highlight: Down once then Enter runs the second match.
        let mut p = Palette::default();
        p.toggle();
        frame(&ctx, &mut p, &host, vec![]);
        frame(&ctx, &mut p, &host, vec![egui::Event::Text("rate".into())]);
        frame(&ctx, &mut p, &host, vec![key(egui::Key::ArrowDown)]);
        let Outcome::Run(second) = frame(&ctx, &mut p, &host, vec![key(egui::Key::Enter)]) else {
            panic!("Enter must run the highlighted row");
        };
        frame(&ctx, &mut p, &host, vec![key(egui::Key::ArrowUp)]);
        let Outcome::Run(first) = frame(&ctx, &mut p, &host, vec![key(egui::Key::Enter)]) else {
            panic!("Enter must run the highlighted row");
        };
        assert_ne!(first, second, "Down and Up moved the highlight");
        // Escape closes.
        frame(&ctx, &mut p, &host, vec![key(egui::Key::Escape)]);
        assert!(!p.is_open());
    }

    #[test]
    fn enter_on_an_unavailable_action_runs_nothing() {
        let ctx = egui::Context::default();
        let host = Mock::default(); // nothing loaded in Develop: resets are unavailable
        let mut p = Palette::default();
        p.toggle();
        frame(&ctx, &mut p, &host, vec![]);
        frame(
            &ctx,
            &mut p,
            &host,
            vec![egui::Event::Text("reset exposure".into())],
        );
        let out = frame(&ctx, &mut p, &host, vec![key(egui::Key::Enter)]);
        assert!(
            matches!(out, Outcome::None),
            "a greyed-out action must not run"
        );
        assert!(p.is_open(), "and the palette stays open to show why");
    }
}
