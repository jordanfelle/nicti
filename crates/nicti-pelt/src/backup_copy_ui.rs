//! #622: backing up the library's RAW folders to a second drive from the Library controls.
//!
//! The engine is `nicti_lair::backup` (additive, verified, never overwrites or deletes; a marker
//! file with a random id identifies the target drive). This is the UI around it:
//!
//! - **Set up a target.** A folder path; before the marker is written the folder is assessed
//!   ([`assess_target_folder`]) and a non-empty folder, one inside the library, or one that
//!   contains a library folder needs an explicit "Set up anyway". The path and the marker's id are
//!   saved in `<catalog>.backup-target.json` beside the catalog (temp file + rename).
//! - **Run.** One button per registered folder, plus "Back up all folders", each a Pounce
//!   [`BackupCopyJob`] (one file per step, cancellable from the activity panel). The job opens the
//!   target by its *saved id* first: a different drive at the same path is an error, never a
//!   backup to the wrong disk. Jobs run one after another.
//! - **Report.** Per folder: copied / already there / **differs** (the backed-up copy is different
//!   and was left alone) / failed, with the lists expandable.
//!
//! Not here (deferred, see the ticket's follow-up): scheduling / back-up-after-import and cloud
//! targets. The target check and setup are single small file operations done on the click.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nicti_lair::backup::{self, assess_target_folder, TargetAssessment};
use nicti_lair::pounce_jobs::{BackupCopyJob, BackupCopyReport, ReportSlot};
use nicti_lair::CatalogStore;
use nicti_pounce::Pounce;
use serde::{Deserialize, Serialize};

use crate::cache_settings::format_bytes;

/// Rows of a Differs / failed list shown before "and N more".
const LIST_ROWS: usize = 50;
/// Finished reports kept on screen.
const KEPT_REPORTS: usize = 12;

/// The saved backup target: where it is and which drive it must be.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetSettings {
    pub path: String,
    pub id: String,
}

/// `<catalog file name>.backup-target.json`, next to the catalog.
pub fn settings_file_for(catalog_path: &Path) -> PathBuf {
    let mut name = catalog_path.as_os_str().to_owned();
    name.push(".backup-target.json");
    PathBuf::from(name)
}

/// The saved target; a missing, unreadable or corrupt file means none (the user sets one up again;
/// the marker on the drive is untouched).
pub fn load_settings(catalog_path: &Path) -> Option<TargetSettings> {
    let s: TargetSettings =
        serde_json::from_str(&std::fs::read_to_string(settings_file_for(catalog_path)).ok()?)
            .ok()?;
    // The id must be one a marker can carry (32 lowercase hex): a hand-edited or damaged file is
    // not a target, and nothing downstream slices a string it cannot trust.
    (!s.path.is_empty() && backup::valid_id(&s.id)).then_some(s)
}

/// Temp file + rename, so a crash never leaves a half-written file.
pub fn save_settings(catalog_path: &Path, s: &TargetSettings) -> std::io::Result<()> {
    let path = settings_file_for(catalog_path);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(
        &tmp,
        serde_json::to_string(s).map_err(std::io::Error::other)?,
    )?;
    std::fs::rename(&tmp, &path)
}

/// Removes the saved target (the drive's marker file is left alone).
pub fn forget_settings(catalog_path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(settings_file_for(catalog_path)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// One line for a finished run.
pub fn summarize(r: &BackupCopyReport) -> String {
    let name = r.source.display();
    if let Some(e) = &r.error {
        return format!("Backup of {name} did not run: {e}");
    }
    let rep = &r.report;
    let mut s = format!(
        "Backup of {name}{}: {} copied ({}), {} already there",
        if r.stopped || rep.cancelled {
            " (stopped)"
        } else {
            ""
        },
        rep.copied,
        format_bytes(rep.copied_bytes),
        rep.unchanged
    );
    if !rep.differs.is_empty() {
        s.push_str(&format!(
            ", {} differ from the library (kept, not replaced)",
            rep.differs.len()
        ));
    }
    if !rep.failed.is_empty() {
        s.push_str(&format!(", {} failed", rep.failed.len()));
    }
    s
}

/// A pending "set up this folder" awaiting the user's confirmation.
struct PendingSetup {
    path: PathBuf,
    assessment: TargetAssessment,
    /// The folder already carries a marker: the choice is to re-adopt its drive id (the saved
    /// settings were lost, or the catalog moved), not to set it up.
    adopt: bool,
}

pub struct BackupCopyUi {
    /// The catalog file the settings sit beside.
    catalog_path: PathBuf,
    path_input: String,
    saved: Option<TargetSettings>,
    setup: Option<PendingSetup>,
    queue: VecDeque<PathBuf>,
    pending: Option<ReportSlot<BackupCopyReport>>,
    reports: Vec<BackupCopyReport>,
    status: Option<String>,
    #[cfg(test)]
    force_open: bool,
}

impl BackupCopyUi {
    pub fn new(catalog_path: &Path) -> Self {
        let saved = load_settings(catalog_path);
        BackupCopyUi {
            catalog_path: catalog_path.to_path_buf(),
            path_input: saved.as_ref().map(|s| s.path.clone()).unwrap_or_default(),
            saved,
            setup: None,
            queue: VecDeque::new(),
            pending: None,
            reports: Vec::new(),
            status: None,
            #[cfg(test)]
            force_open: false,
        }
    }

    pub fn is_running(&self) -> bool {
        self.pending.is_some() || !self.queue.is_empty()
    }

    /// Folds in a finished job and starts the next queued folder. Returns `true` the frame a run
    /// finishes.
    pub fn poll(&mut self, pounce: &Pounce) -> bool {
        let mut finished = false;
        if let Some(slot) = self.pending.clone() {
            if let Some(report) = slot.lock().unwrap_or_else(|e| e.into_inner()).take() {
                self.pending = None;
                // A target problem would fail every queued folder the same way, and a cancelled
                // run means the user wants the backup to stop: drop the rest in both cases. A
                // problem with just this one folder (offline, overlapping) does not stop the rest.
                if report.target_problem || report.stopped || report.report.cancelled {
                    self.queue.clear();
                }
                self.status = Some(summarize(&report));
                self.reports.insert(0, report);
                self.reports.truncate(KEPT_REPORTS);
                finished = true;
            }
        }
        if self.pending.is_none() {
            self.start_next(pounce);
        }
        finished
    }

    fn start_next(&mut self, pounce: &Pounce) {
        let Some(saved) = self.saved.clone() else {
            self.queue.clear();
            return;
        };
        if let Some(source) = self.queue.pop_front() {
            let (job, slot) = BackupCopyJob::new(PathBuf::from(&saved.path), saved.id, source);
            pounce.submit(Box::new(job));
            self.pending = Some(slot);
        }
    }

    fn enqueue(&mut self, pounce: &Pounce, roots: impl IntoIterator<Item = PathBuf>) {
        if self.saved.is_none() {
            self.status = Some("Set up a backup target first.".into());
            return;
        }
        // A fresh request clears the last run's "target is wrong" marker so it is retried.
        self.reports.retain(|r| r.error.is_none());
        self.queue.extend(roots);
        self.status = Some("Backing up\u{2026}".into());
        if self.pending.is_none() {
            self.start_next(pounce);
        }
    }

    fn begin_setup(&mut self, library_roots: &[PathBuf]) {
        let path = PathBuf::from(self.path_input.trim().trim_matches('"'));
        if path.as_os_str().is_empty() {
            self.status = Some("Enter the folder on the backup drive.".into());
            return;
        }
        match assess_target_folder(&path, library_roots) {
            // Already a backup target (its settings were lost, or the catalog moved): offer to
            // take the marker's id back rather than leave the drive stranded.
            Ok(a) if a.already_a_target => {
                self.setup = Some(PendingSetup {
                    path,
                    assessment: a,
                    adopt: true,
                });
            }
            Ok(a) if a.needs_confirmation() => {
                self.setup = Some(PendingSetup {
                    path,
                    assessment: a,
                    adopt: false,
                });
            }
            Ok(_) => self.finish_setup(path, false),
            Err(e) => self.status = Some(format!("Cannot use that folder: {e}")),
        }
    }

    fn finish_setup(&mut self, path: PathBuf, adopt: bool) {
        self.setup = None;
        let result = if adopt {
            backup::adopt_target(&path)
        } else {
            backup::init_target(&path)
        };
        match result {
            Ok(target) => {
                let settings = TargetSettings {
                    path: target.root().to_string_lossy().into_owned(),
                    id: target.id().to_owned(),
                };
                // The marker is on the drive now; if the id cannot be saved the target still
                // works until restart, and the message says so rather than pretending.
                self.status = Some(match save_settings(&self.catalog_path, &settings) {
                    Ok(()) => format!("Backup target ready: {}", target.root().display()),
                    Err(e) => format!(
                        "Backup target ready at {}, but its id could not be saved ({e}), so it \
                         will be forgotten when Nicti closes. The folder keeps its marker: pick \
                         it again and choose \"Use this target\" to re-adopt it.",
                        target.root().display()
                    ),
                });
                self.saved = Some(settings);
            }
            Err(e) => {
                self.status = Some(format!(
                    "Could not {} the target: {e}",
                    if adopt { "adopt" } else { "set up" }
                ));
            }
        }
    }

    /// Draws the "Back up to another drive" section. `library_roots` are the registered folders
    /// (`(path, archived)`); `busy_elsewhere` is true while a job that moves or deletes library
    /// files runs.
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        store: &Arc<dyn CatalogStore + Send + Sync>,
        pounce: &Pounce,
        busy_elsewhere: bool,
    ) {
        let header = egui::CollapsingHeader::new("Back up to another drive").default_open(false);
        #[cfg(test)]
        let header = header.open(self.force_open.then_some(true));
        header.show(ui, |ui| {
            let roots: Vec<(PathBuf, bool)> = match store.list_roots() {
                Ok(r) => r
                    .into_iter()
                    .map(|r| (PathBuf::from(r.path), r.archived))
                    .collect(),
                Err(e) => {
                    ui.colored_label(egui::Color32::RED, format!("Cannot list folders: {e}"));
                    return;
                }
            };
            ui.weak(
                "Copies every RAW/JPEG to the same place on the backup drive, checking each copy. \
                 It only adds files: nothing is overwritten or deleted, and a file whose backed-up \
                 copy differs is reported and left alone.",
            );
            match self.saved.clone() {
                Some(saved) => {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(format!(
                            "Target: {} (drive id {}\u{2026})",
                            saved.path,
                            saved.id.chars().take(8).collect::<String>()
                        ));
                        if ui
                            .add_enabled(!self.is_running(), egui::Button::new("Forget"))
                            .on_hover_text(
                                "Stops using this target. The backup files and the marker on the \
                                 drive are left alone.",
                            )
                            .clicked()
                        {
                            match forget_settings(&self.catalog_path) {
                                Ok(()) => {
                                    self.saved = None;
                                    self.status = Some("Backup target forgotten.".into());
                                }
                                Err(e) => {
                                    self.status = Some(format!("Could not forget the target: {e}"));
                                }
                            }
                        }
                    });
                }
                None => {
                    ui.horizontal(|ui| {
                        ui.label("Backup folder:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.path_input)
                                .align(egui::Align2::LEFT_TOP)
                                .hint_text("A folder on the backup drive")
                                .desired_width(280.0),
                        );
                        if ui
                            .button("Set up as backup target")
                            .on_hover_text(
                                "Marks this folder with a random drive id, so a backup never lands \
                                 on a different drive that happens to be mounted at the same path.",
                            )
                            .clicked()
                        {
                            let library: Vec<PathBuf> =
                                roots.iter().map(|(p, _)| p.clone()).collect();
                            self.begin_setup(&library);
                        }
                    });
                }
            }
            let mut confirmed: Option<(PathBuf, bool)> = None;
            let mut cancelled = false;
            if let Some(setup) = &self.setup {
                for w in setup.assessment.warnings() {
                    ui.colored_label(egui::Color32::from_rgb(230, 160, 40), w);
                }
                ui.horizontal(|ui| {
                    let label = if setup.adopt {
                        "Use this target"
                    } else {
                        "Set up anyway"
                    };
                    if ui.button(label).clicked() {
                        confirmed = Some((setup.path.clone(), setup.adopt));
                    }
                    if ui.button("Cancel").clicked() {
                        cancelled = true;
                    }
                });
            }
            if let Some((path, adopt)) = confirmed {
                self.finish_setup(path, adopt);
            }
            if cancelled {
                self.setup = None;
            }
            if self.saved.is_some() {
                let can_run = !busy_elsewhere;
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            can_run && roots.iter().any(|(_, a)| !a) && !self.is_running(),
                            egui::Button::new("Back up all folders"),
                        )
                        .on_hover_text("Every folder that is not on an archived drive.")
                        .clicked()
                    {
                        // Archived roots live on drives that are usually unplugged; they are
                        // backed up by their own button, not as part of "all".
                        self.enqueue(
                            pounce,
                            roots.iter().filter(|(_, a)| !a).map(|(p, _)| p.clone()),
                        );
                    }
                });
                for (root, archived) in &roots {
                    ui.horizontal(|ui| {
                        ui.label(format!(
                            "{}{}",
                            root.display(),
                            if *archived { " (archived)" } else { "" }
                        ));
                        if ui
                            .add_enabled(
                                can_run && !self.is_running(),
                                egui::Button::new("Back up"),
                            )
                            .clicked()
                        {
                            self.enqueue(pounce, [root.clone()]);
                        }
                    });
                }
            }
            if self.is_running() {
                ui.label(format!(
                    "Running ({} more folder(s) queued); progress and Cancel are in the activity \
                     panel.",
                    self.queue.len()
                ));
            }
            if let Some(msg) = &self.status {
                ui.label(msg.clone());
            }
            for (i, r) in self.reports.iter().enumerate() {
                if r.report.differs.is_empty() && r.report.failed.is_empty() && r.error.is_none() {
                    continue;
                }
                egui::CollapsingHeader::new(summarize(r))
                    .id_salt(("backup_report", i))
                    .default_open(false)
                    .show(ui, |ui| {
                        if let Some(e) = &r.error {
                            ui.colored_label(egui::Color32::RED, e);
                        }
                        let differs: Vec<String> = r
                            .report
                            .differs
                            .iter()
                            .map(|p| p.display().to_string())
                            .collect();
                        list_rows(ui, "Differ (the backup keeps its own copy)", &differs);
                        list_rows(ui, "Failed", &r.report.failed);
                    });
            }
        });
    }
}

fn list_rows(ui: &mut egui::Ui, title: &str, rows: &[String]) {
    if rows.is_empty() {
        return;
    }
    ui.label(format!("{title}: {}", rows.len()));
    for row in rows.iter().take(LIST_ROWS) {
        ui.monospace(row.to_string());
    }
    if rows.len() > LIST_ROWS {
        ui.weak(format!("\u{2026}and {} more", rows.len() - LIST_ROWS));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_lair::backup::Report;
    use nicti_lair::SqliteCatalog;

    fn report(source: &str) -> BackupCopyReport {
        BackupCopyReport {
            source: PathBuf::from(source),
            ..Default::default()
        }
    }

    #[test]
    fn settings_round_trip_beside_the_catalog_and_a_corrupt_file_means_none() {
        let dir = tempfile::tempdir().unwrap();
        let cat = dir.path().join("cat.db");
        assert!(settings_file_for(&cat).ends_with("cat.db.backup-target.json"));
        assert!(load_settings(&cat).is_none());
        let s = TargetSettings {
            path: "/mnt/backup/nicti".into(),
            id: "0123456789abcdef0123456789abcdef".into(),
        };
        save_settings(&cat, &s).unwrap();
        assert_eq!(load_settings(&cat), Some(s));
        assert!(!settings_file_for(&cat).with_extension("json.tmp").exists());
        std::fs::write(settings_file_for(&cat), "not json").unwrap();
        assert!(load_settings(&cat).is_none());
        std::fs::write(settings_file_for(&cat), r#"{"path":"","id":""}"#).unwrap();
        assert!(
            load_settings(&cat).is_none(),
            "empty fields are not a target"
        );
        forget_settings(&cat).unwrap();
        forget_settings(&cat).unwrap();
        assert!(!settings_file_for(&cat).exists());
    }

    #[test]
    fn a_summary_names_copies_differs_failures_and_a_target_that_did_not_open() {
        let mut r = report("/lib/2026");
        r.report = Report {
            copied: 3,
            copied_bytes: 3 * 1024 * 1024,
            unchanged: 7,
            differs: vec![PathBuf::from("/backup/a.NEF")],
            failed: vec!["x".into(), "y".into()],
            cancelled: false,
        };
        let s = summarize(&r);
        for want in [
            "/lib/2026",
            "3 copied",
            "3.0 MiB",
            "7 already there",
            "1 differ",
            "2 failed",
        ] {
            assert!(s.contains(want), "{s}");
        }
        r.stopped = true;
        assert!(summarize(&r).contains("(stopped)"));
        let mut bad = report("/lib/2026");
        bad.error = Some("is a different backup target".into());
        let s = summarize(&bad);
        assert!(
            s.contains("did not run") && s.contains("different backup target"),
            "{s}"
        );
    }

    #[test]
    fn setting_up_asks_before_a_non_empty_or_library_folder_and_never_for_an_empty_one() {
        let t = tempfile::tempdir().unwrap();
        let cat = t.path().join("cat.db");
        let lib = t.path().join("lib");
        let empty = t.path().join("empty");
        let full = t.path().join("full");
        for d in [&lib, &empty, &full] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(full.join("x.txt"), b"x").unwrap();
        let mut ui = BackupCopyUi::new(&cat);

        // Empty folder elsewhere: set up at once, id saved.
        ui.path_input = empty.display().to_string();
        ui.begin_setup(std::slice::from_ref(&lib));
        assert!(ui.setup.is_none());
        let saved = ui.saved.clone().expect("set up");
        assert_eq!(saved.id.len(), 32);
        assert!(empty.join(nicti_lair::backup::MARKER_NAME).exists());
        // Saved beside the catalog: a restart (a fresh UI) finds the same target.
        assert_eq!(load_settings(&cat), Some(saved.clone()));
        assert_eq!(BackupCopyUi::new(&cat).saved, Some(saved));
        forget_settings(&cat).unwrap();

        // Non-empty: waits for confirmation, nothing written yet.
        let mut ui = BackupCopyUi::new(&cat);
        ui.path_input = full.display().to_string();
        ui.begin_setup(std::slice::from_ref(&lib));
        assert!(ui.setup.is_some() && ui.saved.is_none());
        assert!(!full.join(nicti_lair::backup::MARKER_NAME).exists());
        let path = ui.setup.as_ref().unwrap().path.clone();
        ui.finish_setup(path, false);
        assert!(full.join(nicti_lair::backup::MARKER_NAME).exists());
        assert!(ui.saved.is_some());

        // Inside the library: waits, and the warning names the library folder.
        let inner = lib.join("backups");
        std::fs::create_dir_all(&inner).unwrap();
        let mut ui = BackupCopyUi::new(&cat);
        ui.path_input = inner.display().to_string();
        ui.begin_setup(std::slice::from_ref(&lib));
        let pending = ui.setup.as_ref().expect("asks first");
        assert!(pending.assessment.warnings()[0].contains("inside your library folder"));

        // Blank, missing and already-a-target folders are refused with a message (no target
        // saved from the steps above).
        forget_settings(&cat).unwrap();
        for input in ["", "   ", &t.path().join("nope").display().to_string()] {
            let mut ui = BackupCopyUi::new(&cat);
            ui.path_input = input.into();
            ui.begin_setup(&[]);
            assert!(ui.setup.is_none() && ui.saved.is_none(), "{input:?}");
            assert!(ui.status.is_some(), "{input:?}");
        }
        let mut ui = BackupCopyUi::new(&cat);
        ui.path_input = empty.display().to_string();
        ui.begin_setup(&[]);
        let pending = ui
            .setup
            .as_ref()
            .expect("an existing marker is offered for adoption");
        assert!(pending.adopt && pending.assessment.already_a_target);
        assert!(ui.saved.is_none());
        // Adopting takes the marker's own id back (settings had been lost).
        let original = backup::adopt_target(&empty).unwrap();
        let path = pending.path.clone();
        ui.finish_setup(path, true);
        assert_eq!(
            ui.saved.as_ref().map(|s| s.id.as_str()),
            Some(original.id())
        );
        assert_eq!(BackupCopyUi::new(&cat).saved, ui.saved, "and it was saved");
    }

    #[test]
    fn a_queued_run_goes_one_folder_at_a_time_and_stops_queueing_after_a_wrong_target() {
        let t = tempfile::tempdir().unwrap();
        let cat = t.path().join("cat.db");
        let dst = t.path().join("backup");
        let (a, b) = (t.path().join("a"), t.path().join("b"));
        for d in [&dst, &a, &b] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(a.join("1.NEF"), b"1").unwrap();
        std::fs::write(b.join("2.NEF"), b"2").unwrap();
        let target = backup::init_target(&dst).unwrap();
        let pounce = Pounce::new(u64::MAX, 2, 2, || {});
        let mut ui = BackupCopyUi::new(&cat);
        ui.saved = Some(TargetSettings {
            path: dst.display().to_string(),
            id: target.id().to_owned(),
        });

        ui.enqueue(&pounce, [a.clone(), b.clone()]);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while ui.is_running() && std::time::Instant::now() < deadline {
            ui.poll(&pounce);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(!ui.is_running(), "both folders finished");
        assert_eq!(ui.reports.len(), 2);
        assert!(ui
            .reports
            .iter()
            .all(|r| r.error.is_none() && r.report.copied == 1));
        assert_eq!(std::fs::read(dst.join("2.NEF")).unwrap(), b"2");

        // A wrong drive id: the first folder reports it, the rest are dropped, nothing is copied.
        ui.reports.clear();
        std::fs::write(a.join("3.NEF"), b"3").unwrap();
        ui.saved.as_mut().unwrap().id = "0".repeat(32);
        ui.enqueue(&pounce, [a.clone(), b.clone()]);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while ui.is_running() && std::time::Instant::now() < deadline {
            ui.poll(&pounce);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            ui.reports.len(),
            1,
            "the same error is not reported per folder"
        );
        assert!(ui.reports[0].error.is_some());
        assert!(!dst.join("3.NEF").exists());
        pounce.shutdown();
    }

    #[test]
    fn the_section_draws_with_and_without_a_target() {
        let store: Arc<dyn CatalogStore + Send + Sync> =
            Arc::new(SqliteCatalog::open_in_memory().unwrap());
        let dir = tempfile::tempdir().unwrap();
        let cat = dir.path().join("cat.db");
        let pounce = Pounce::new(u64::MAX, 2, 1, || {});
        let ctx = egui::Context::default();
        let mut ui_state = BackupCopyUi::new(&cat);
        ui_state.force_open = true;
        let frame = |s: &mut BackupCopyUi, busy: bool| {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                s.show(ui, &store, &pounce, busy);
            });
            out.textures_delta.clear();
        };
        frame(&mut ui_state, false);
        ui_state.saved = Some(TargetSettings {
            path: "/mnt/b".into(),
            id: "0123456789abcdef0123456789abcdef".into(),
        });
        ui_state.setup = None;
        let mut with_lists = report("/lib");
        with_lists.report.differs = (0..80)
            .map(|i| PathBuf::from(format!("/b/{i}.NEF")))
            .collect();
        with_lists.report.failed = vec!["boom".into()];
        ui_state.reports.push(with_lists);
        ui_state.status = Some("done".into());
        frame(&mut ui_state, false);
        frame(&mut ui_state, true);
        pounce.shutdown();
    }

    #[test]
    fn a_hand_edited_settings_file_with_a_bad_id_is_not_a_target_and_cannot_panic_the_ui() {
        let dir = tempfile::tempdir().unwrap();
        let cat = dir.path().join("cat.db");
        // A multibyte character across byte 8 used to panic the id-prefix slice every frame.
        for id in [
            "aaaaaaa\u{e9}b",
            "\u{e9}\u{e9}\u{e9}\u{e9}",
            "short",
            &"G".repeat(32),
            &"A".repeat(32),
        ] {
            std::fs::write(
                settings_file_for(&cat),
                serde_json::json!({"path": "/x", "id": id}).to_string(),
            )
            .unwrap();
            assert!(load_settings(&cat).is_none(), "{id:?}");
            let mut ui = BackupCopyUi::new(&cat);
            assert!(ui.saved.is_none());
            // And even a UI holding such a value in memory draws without slicing it.
            ui.saved = Some(TargetSettings {
                path: "/x".into(),
                id: id.to_string(),
            });
            ui.force_open = true;
            let store: Arc<dyn CatalogStore + Send + Sync> =
                Arc::new(SqliteCatalog::open_in_memory().unwrap());
            let pounce = Pounce::new(u64::MAX, 2, 1, || {});
            let ctx = egui::Context::default();
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui_| {
                ui.show(ui_, &store, &pounce, false);
            });
            out.textures_delta.clear();
            pounce.shutdown();
        }
    }

    #[test]
    fn a_cancelled_run_stops_the_queue_and_a_folder_problem_does_not() {
        let t = tempfile::tempdir().unwrap();
        let cat = t.path().join("cat.db");
        let dst = t.path().join("backup");
        let (good, missing) = (t.path().join("good"), t.path().join("offline"));
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::create_dir_all(&good).unwrap();
        std::fs::write(good.join("1.NEF"), b"1").unwrap();
        let target = backup::init_target(&dst).unwrap();
        let pounce = Pounce::new(u64::MAX, 2, 2, || {});
        let mut ui = BackupCopyUi::new(&cat);
        ui.saved = Some(TargetSettings {
            path: dst.display().to_string(),
            id: target.id().to_owned(),
        });

        // Cancelled (the job was dropped by the activity panel's Cancel): the rest does not start.
        let slot: ReportSlot<BackupCopyReport> =
            Arc::new(std::sync::Mutex::new(Some(BackupCopyReport {
                stopped: true,
                ..report("/lib/a")
            })));
        ui.pending = Some(slot);
        ui.queue.push_back(good.clone());
        assert!(ui.poll(&pounce));
        assert!(ui.queue.is_empty() && !ui.is_running(), "cancel means stop");

        // An offline folder first: it reports its own problem, the next folder still runs.
        ui.reports.clear();
        ui.enqueue(&pounce, [missing, good.clone()]);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while ui.is_running() && std::time::Instant::now() < deadline {
            ui.poll(&pounce);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(ui.reports.len(), 2, "{:?}", ui.reports);
        let ok = ui
            .reports
            .iter()
            .find(|r| r.error.is_none())
            .expect("the good folder ran");
        assert_eq!(ok.report.copied, 1);
        assert!(ui
            .reports
            .iter()
            .any(|r| r.error.is_some() && !r.target_problem));
        pounce.shutdown();
    }
}
