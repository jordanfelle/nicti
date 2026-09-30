//! egui side of copy/paste, sync and presets (#52): the stage checklist modal, the presets panel,
//! the Apply-preset menu, and the one summary/Undo line. It only *asks*; `PeltApp` owns the catalog
//! and the loaded `DevelopView`, so it runs each `Command` (see `PeltApp::run_knead_command`).

use std::path::{Path, PathBuf};

use nicti_pawprint::EditDocument;

use super::batch::{BatchOutcome, LastBatch, UndoOutcome};
use super::presets::{presets_file_for, PresetStore};
use super::{Clipboard, StageSet, GROUPS};

/// What the user asked the app to do.
pub enum Command {
    /// Paste `clip` onto `ids` (a sync, a paste, or an applied preset).
    Run {
        clip: Clipboard,
        ids: Vec<i64>,
        label: String,
    },
    Undo,
}

/// What a click in the presets panel or menu asked for.
#[derive(Debug, PartialEq, Eq)]
pub enum PanelAction {
    Copy,
    Paste,
    Sync,
    SavePreset,
    Apply(String),
}

enum Modal {
    Copy { doc: EditDocument },
    Sync { doc: EditDocument, ids: Vec<i64> },
    SavePreset { doc: EditDocument },
}

pub struct KneadUi {
    presets_path: PathBuf,
    presets: PresetStore,
    clipboard: Option<Clipboard>,
    /// The checklist, remembered between dialogs for the session.
    set: StageSet,
    modal: Option<Modal>,
    name_input: String,
    modal_error: Option<String>,
    renaming: Option<(String, String)>,
    last: Option<LastBatch>,
    status: Option<String>,
}

/// The one summary line for a finished batch (ADR-0101 rule 5).
pub fn summary(label: &str, out: &BatchOutcome) -> String {
    let mut line = format!("{label}: {} changed", out.applied);
    if out.unchanged > 0 {
        line += &format!(" \u{b7} {} already matched", out.unchanged);
    }
    if out.missing > 0 {
        line += &format!(" \u{b7} {} not in the catalog", out.missing);
    }
    line
}

pub fn undo_summary(label: &str, out: &UndoOutcome) -> String {
    let mut line = format!("Undid {label}: {} restored", out.restored);
    if out.skipped > 0 {
        line += &format!(" \u{b7} {} left alone (edited since)", out.skipped);
    }
    line
}

impl KneadUi {
    pub fn new(catalog_path: &Path) -> Self {
        let presets_path = presets_file_for(catalog_path);
        Self {
            presets: PresetStore::load(&presets_path),
            presets_path,
            clipboard: None,
            set: StageSet::default(),
            modal: None,
            name_input: String::new(),
            modal_error: None,
            renaming: None,
            last: None,
            status: None,
        }
    }

    pub fn has_clipboard(&self) -> bool {
        self.clipboard.is_some()
    }

    pub fn clipboard(&self) -> Option<&Clipboard> {
        self.clipboard.as_ref()
    }

    pub fn is_asking(&self) -> bool {
        self.modal.is_some()
    }

    /// Whether the line has anything to show: a message, or a batch that can still be undone.
    pub fn has_status(&self) -> bool {
        self.status.is_some() || self.last.is_some()
    }

    pub fn ask_copy(&mut self, doc: EditDocument) {
        self.open(Modal::Copy { doc });
    }

    /// `ids` are the photos to sync onto (the source already left out).
    pub fn ask_sync(&mut self, doc: EditDocument, ids: Vec<i64>) {
        self.open(Modal::Sync { doc, ids });
    }

    pub fn ask_save_preset(&mut self, doc: EditDocument) {
        self.name_input.clear();
        self.open(Modal::SavePreset { doc });
    }

    fn open(&mut self, modal: Modal) {
        self.modal_error = None;
        self.modal = Some(modal);
    }

    pub fn set_status(&mut self, text: String) {
        self.status = Some(text);
    }

    pub fn finish_batch(&mut self, label: &str, out: &BatchOutcome, last: Option<LastBatch>) {
        // A batch that changed nothing leaves the previous undo in place: it's still valid.
        if last.is_some() {
            self.last = last;
        }
        self.status = Some(summary(label, out));
    }

    pub fn take_undo(&mut self) -> Option<LastBatch> {
        self.last.take()
    }

    /// Puts an undo back after a failed attempt, so the user can retry.
    pub fn keep_undo(&mut self, last: LastBatch) {
        self.last = Some(last);
    }

    pub fn finish_undo(&mut self, label: &str, out: &UndoOutcome) {
        self.status = Some(undo_summary(label, out));
    }

    /// A preset as a clipboard, for `PanelAction::Apply`.
    pub fn preset_clipboard(&self, name: &str) -> Option<Clipboard> {
        self.presets
            .get(name)
            .map(|p| Clipboard::from_entries(&p.stages))
    }

    /// Whether the presets file was written; on failure `status` says why.
    fn save_presets(&mut self) -> bool {
        match self.presets.save(&self.presets_path) {
            Ok(()) => true,
            Err(e) => {
                self.status = Some(format!("Couldn't save presets: {e}"));
                false
            }
        }
    }

    /// The checklist modal. Returns a command when the user confirmed a sync.
    pub fn show_modal(&mut self, ctx: &egui::Context) -> Option<Command> {
        let modal = self.modal.as_ref()?;
        let (title, verb, wants_name) = match modal {
            Modal::Copy { .. } => ("Copy settings".to_string(), "Copy", false),
            Modal::Sync { ids, .. } => (
                format!(
                    "Sync settings to {} photo{}",
                    ids.len(),
                    if ids.len() == 1 { "" } else { "s" }
                ),
                "Sync",
                false,
            ),
            Modal::SavePreset { .. } => ("Save as preset".to_string(), "Save", true),
        };
        let mut confirmed = false;
        let mut close = false;
        let response = egui::Modal::new(egui::Id::new("nicti_knead_prompt")).show(ctx, |ui| {
            ui.heading(&title);
            if wants_name {
                ui.horizontal(|ui| {
                    ui.label("Name");
                    ui.text_edit_singleline(&mut self.name_input);
                });
            }
            ui.label("Checked settings replace the same settings on the other photos.");
            ui.add_space(4.0);
            for g in GROUPS {
                let mut on = self.set.contains(g.id);
                if ui.checkbox(&mut on, g.label).changed() {
                    self.set.set(g.id, on);
                }
            }
            ui.horizontal(|ui| {
                if ui.small_button("Check all").clicked() {
                    self.set = StageSet::all();
                }
                if ui.small_button("Check none").clicked() {
                    self.set = StageSet::none();
                }
                if ui.small_button("Defaults").clicked() {
                    self.set = StageSet::default();
                }
            });
            if let Some(e) = &self.modal_error {
                ui.colored_label(egui::Color32::RED, e);
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let ok =
                    !self.set.is_empty() && (!wants_name || !self.name_input.trim().is_empty());
                if ui.add_enabled(ok, egui::Button::new(verb)).clicked() {
                    confirmed = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
        if response.should_close() {
            close = true;
        }
        if confirmed {
            return self.confirm();
        }
        if close {
            self.modal = None;
        }
        None
    }

    fn confirm(&mut self) -> Option<Command> {
        match self.modal.take()? {
            Modal::Copy { doc } => {
                self.clipboard = Some(Clipboard::from_document(&doc, &self.set));
                self.status = Some(format!(
                    "Copied {} setting group{}. Paste with Ctrl+Shift+V.",
                    self.set.iter().count(),
                    if self.set.iter().count() == 1 {
                        ""
                    } else {
                        "s"
                    }
                ));
                None
            }
            Modal::Sync { doc, ids } => Some(Command::Run {
                clip: Clipboard::from_document(&doc, &self.set),
                ids,
                label: "Sync".into(),
            }),
            Modal::SavePreset { doc } => {
                let stages = Clipboard::from_document(&doc, &self.set).entries();
                match self.presets.add(&self.name_input, stages) {
                    Ok(()) => {
                        if self.save_presets() {
                            self.status =
                                Some(format!("Saved preset \"{}\".", self.name_input.trim()));
                        }
                    }
                    Err(e) => {
                        // Keep the dialog up so the name can be fixed.
                        self.modal_error = Some(e.to_string());
                        self.modal = Some(Modal::SavePreset { doc });
                    }
                }
                None
            }
        }
    }

    /// The Apply-preset dropdown (Library toolbar). `None` when nothing was picked.
    pub fn preset_menu(&self, ui: &mut egui::Ui, enabled: bool) -> Option<PanelAction> {
        let mut picked = None;
        ui.add_enabled_ui(enabled, |ui| {
            ui.menu_button("Apply preset", |ui| {
                if self.presets.all().is_empty() {
                    ui.label("No presets yet. Save one from the Develop view.");
                }
                for p in self.presets.all() {
                    if ui.button(&p.name).clicked() {
                        picked = Some(PanelAction::Apply(p.name.clone()));
                        ui.close();
                    }
                }
            });
        });
        picked
    }

    /// The Develop view's left panel: copy/paste buttons and the preset list.
    pub fn show_panel(&mut self, ui: &mut egui::Ui) -> Option<PanelAction> {
        let mut action = None;
        ui.heading("Presets");
        ui.horizontal_wrapped(|ui| {
            if ui
                .button("Copy settings\u{2026}")
                .on_hover_text("Ctrl+Shift+C")
                .clicked()
            {
                action = Some(PanelAction::Copy);
            }
            if ui
                .add_enabled(self.has_clipboard(), egui::Button::new("Paste"))
                .on_hover_text("Ctrl+Shift+V")
                .clicked()
            {
                action = Some(PanelAction::Paste);
            }
        });
        if ui.button("Save as preset\u{2026}").clicked() {
            action = Some(PanelAction::SavePreset);
        }
        ui.separator();
        if self.presets.all().is_empty() {
            ui.weak("No presets yet.");
        }
        let names: Vec<String> = self.presets.all().iter().map(|p| p.name.clone()).collect();
        let mut remove = None;
        let mut rename_to = None;
        for name in names {
            let editing = self
                .renaming
                .as_ref()
                .is_some_and(|(from, _)| *from == name);
            ui.horizontal(|ui| {
                if editing {
                    if let Some((from, text)) = self.renaming.as_mut() {
                        ui.text_edit_singleline(text);
                        if ui.small_button("OK").clicked() {
                            rename_to = Some((from.clone(), text.clone()));
                        }
                        if ui.small_button("Cancel").clicked() {
                            self.renaming = None;
                        }
                    }
                } else {
                    if ui
                        .button(&name)
                        .on_hover_text("Apply to this photo")
                        .clicked()
                    {
                        action = Some(PanelAction::Apply(name.clone()));
                    }
                    if ui.small_button("Rename").clicked() {
                        self.renaming = Some((name.clone(), name.clone()));
                    }
                    if ui
                        .small_button("\u{2715}")
                        .on_hover_text("Delete")
                        .clicked()
                    {
                        remove = Some(name.clone());
                    }
                }
            });
        }
        if let Some((from, to)) = rename_to {
            match self.presets.rename(&from, &to) {
                Ok(()) => {
                    self.renaming = None;
                    self.save_presets();
                }
                Err(e) => self.status = Some(e.to_string()),
            }
        }
        if let Some(name) = remove {
            if self.presets.remove(&name).is_ok() {
                self.save_presets();
            }
        }
        action
    }

    /// The summary line with Undo and Dismiss. Returns `Command::Undo` when Undo was clicked.
    pub fn show_status(&mut self, ui: &mut egui::Ui) -> Option<Command> {
        let text = match (&self.status, &self.last) {
            (Some(text), _) => text.clone(),
            (None, Some(last)) => format!("{} can still be undone.", last.label),
            (None, None) => return None,
        };
        let mut command = None;
        let mut dismiss = false;
        ui.horizontal(|ui| {
            ui.label(text);
            // Named for its batch: the line may be showing an unrelated message by now.
            if let Some(last) = &self.last {
                if ui.button(format!("Undo {}", last.label)).clicked() {
                    command = Some(Command::Undo);
                }
            }
            let label = if self.status.is_some() {
                "Dismiss"
            } else {
                "Discard undo"
            };
            dismiss = ui.button(label).clicked();
        });
        if dismiss {
            // The message goes first; only a second click (now "Discard undo") gives up the undo.
            if self.status.is_some() {
                self.status = None;
            } else {
                self.last = None;
            }
        }
        command
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_lists_only_the_nonzero_extras() {
        let s = summary(
            "Sync",
            &BatchOutcome {
                applied: 388,
                unchanged: 0,
                missing: 0,
            },
        );
        assert_eq!(s, "Sync: 388 changed");
        let s = summary(
            "Paste",
            &BatchOutcome {
                applied: 1,
                unchanged: 12,
                missing: 2,
            },
        );
        assert_eq!(
            s,
            "Paste: 1 changed \u{b7} 12 already matched \u{b7} 2 not in the catalog"
        );
    }

    #[test]
    fn undo_summary_reports_photos_left_alone() {
        let s = undo_summary(
            "Sync",
            &UndoOutcome {
                restored: 5,
                skipped: 1,
            },
        );
        assert_eq!(
            s,
            "Undid Sync: 5 restored \u{b7} 1 left alone (edited since)"
        );
    }

    #[test]
    fn saving_a_preset_with_a_taken_name_keeps_the_dialog_up() {
        let dir = tempfile::tempdir().unwrap();
        let mut ui = KneadUi::new(&dir.path().join("c.db"));
        let mut doc = EditDocument::default();
        doc.stages.insert(
            nicti_tapetum::stages::WB.to_string(),
            nicti_pawprint::StageEntry {
                schema_version: 1,
                params: serde_json::json!({ "k": 1 }),
            },
        );
        ui.ask_save_preset(doc.clone());
        ui.name_input = "Warm".into();
        assert!(ui.confirm().is_none());
        assert!(!ui.is_asking());
        assert!(ui.preset_clipboard("Warm").is_some());

        ui.ask_save_preset(doc);
        ui.name_input = "Warm".into();
        assert!(ui.confirm().is_none());
        assert!(ui.is_asking(), "duplicate name: dialog stays open");
        assert!(ui.modal_error.is_some());
    }
}
