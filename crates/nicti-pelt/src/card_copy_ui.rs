//! "Copy from card" (#602, ADR-0436 sections 3-6): detects mounted camera cards, renders each
//! file's destination name from the naming templates, and runs `nicti_lair::pounce_jobs::CardCopyJob`
//! (verified copy, then registration). The card is only ever read; the panel says so, because the
//! user formats it. Everything that touches the card (volume probing, listing, reading capture
//! dates) runs on a worker thread, never the UI thread: an empty reader or a spinning-up card can
//! block for tens of seconds. The pure parts (plan building, summaries) are unit-tested.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nicti_lair::cardcopy::{CardCopy, CardCopyReport, PlannedCopy};
use nicti_lair::cards::{default_mount_roots, scan_cards, CardVolume};
use nicti_lair::offering::{list_card_files, CardFile};
use nicti_lair::pounce_jobs::{CardCopyJob, ReportSlot};
use nicti_lair::{CatalogStore, SqliteCatalog};
use nicti_pounce::Pounce;
use nicti_preen::metadata::SourceExif;
use nicti_preen::naming::{AssetFacts, DateParts, Template};

use crate::folder_dialog::FolderPicker;

/// Cards are re-detected at most this often while the panel is on screen.
const RESCAN_AFTER: Duration = Duration::from_secs(10);

pub const DEFAULT_FILENAME_TEMPLATE: &str = "{Filename}";
pub const DEFAULT_SUBFOLDER_TEMPLATE: &str = "{Date:YYYY-MM-DD}";

/// What naming needs from a card file: capture time, mtime (unix seconds), make, model.
type Capture = (Option<DateParts>, i64, Option<String>, Option<String>);

/// Renders every file's destination (relative to the destination root) from the two templates.
/// `capture` reads the capture time and camera of a file (the UI passes the card's EXIF; tests pass
/// a stub). The original extension is kept. Template errors are returned for the user to fix.
pub fn build_plan(
    files: Vec<CardFile>,
    filename_template: &str,
    subfolder_template: &str,
    capture: &dyn Fn(&Path) -> Capture,
) -> Result<Vec<PlannedCopy>, String> {
    let name =
        Template::parse(filename_template).map_err(|e| format!("File name template: {e}"))?;
    let sub = if subfolder_template.trim().is_empty() {
        None
    } else {
        Some(
            Template::parse_subfolder(subfolder_template)
                .map_err(|e| format!("Subfolder template: {e}"))?,
        )
    };
    let mut plan = Vec::with_capacity(files.len());
    for (index, file) in files.into_iter().enumerate() {
        let sequence = index as u32 + 1;
        let (captured, mtime_unix, make, model) = capture(&file.path);
        let facts = AssetFacts {
            asset_id: 0,
            stem: file
                .path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            folder: file
                .path
                .parent()
                .and_then(|p| p.file_name())
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            captured,
            mtime_unix,
            rating: None,
            make,
            model,
        };
        let mut dest_rel = PathBuf::new();
        if let Some(sub) = &sub {
            for component in sub.render_subfolder(&facts, sequence) {
                dest_rel.push(component);
            }
        }
        let mut file_name = name.render_filename(&facts, sequence);
        if let Some(ext) = file.path.extension() {
            file_name.push('.');
            file_name.push_str(&ext.to_string_lossy());
        }
        dest_rel.push(file_name);
        plan.push(PlannedCopy { file, dest_rel });
    }
    Ok(plan)
}

/// The capture time / mtime / camera of a card file, read from its own EXIF.
fn read_capture(path: &Path) -> Capture {
    let exif = SourceExif::from_path(path);
    let captured = exif
        .date_time_original
        .as_deref()
        .and_then(DateParts::parse);
    let mtime = std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64);
    (captured, mtime, exif.make, exif.model)
}

/// One line for a finished run.
pub fn summarize(r: &CardCopyReport) -> String {
    let mut s = format!(
        "Copied {} file(s) ({}); {} already there",
        r.copied,
        human_bytes(r.bytes_copied),
        r.already_there
    );
    if let Some(i) = &r.ingest {
        s.push_str(&format!("; {} added to the catalog", i.added));
    }
    if r.verify_failed > 0 {
        s.push_str(&format!(
            "; {} did NOT read back identically (check the destination disk)",
            r.verify_failed
        ));
    }
    let other = r.failed.len() as u64 - r.verify_failed.min(r.failed.len() as u64);
    if other > 0 {
        s.push_str(&format!("; {other} could not be copied"));
    }
    if r.not_attempted > 0 {
        s.push_str(&format!("; {} not attempted (stopped)", r.not_attempted));
    }
    if !r.listing_problems.is_empty() {
        s.push_str(&format!(
            "; {} folder(s) on the card could not be read",
            r.listing_problems.len()
        ));
    }
    if let Some(e) = &r.ingest_error {
        s.push_str(&format!("; registering stopped: {e}"));
    }
    if r.is_clean() {
        s.push_str(
            ". The card was not modified; it is safe to format once you have checked the copies.",
        );
    } else {
        s.push_str(". Do not format the card yet.");
    }
    s
}

fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

type PlanMsg = Result<(Vec<PlannedCopy>, Vec<String>), String>;

pub struct CardCopyUi {
    cards: Vec<CardVolume>,
    scan: Option<Arc<Mutex<Option<Vec<CardVolume>>>>>,
    scanned_at: Option<Instant>,
    selected: Option<PathBuf>,
    pub dest_input: String,
    picker: FolderPicker,
    pub filename_template: String,
    pub subfolder_template: String,
    planning: Option<Receiver<PlanMsg>>,
    pending: Option<ReportSlot<CardCopyReport>>,
    pub status: Option<String>,
}

impl Default for CardCopyUi {
    fn default() -> Self {
        Self {
            cards: Vec::new(),
            scan: None,
            scanned_at: None,
            selected: None,
            dest_input: String::new(),
            picker: FolderPicker::default(),
            filename_template: DEFAULT_FILENAME_TEMPLATE.into(),
            subfolder_template: DEFAULT_SUBFOLDER_TEMPLATE.into(),
            planning: None,
            pending: None,
            status: None,
        }
    }
}

impl CardCopyUi {
    pub fn is_busy(&self) -> bool {
        self.planning.is_some() || self.pending.is_some()
    }

    fn start_scan(&mut self, ctx: &egui::Context) {
        if self.scan.is_some() {
            return;
        }
        let slot = Arc::new(Mutex::new(None));
        self.scan = Some(slot.clone());
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("card-scan".into())
            .spawn(move || {
                let found = scan_cards(&default_mount_roots());
                *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(found);
                ctx.request_repaint();
            });
        if spawned.is_err() {
            self.scan = None;
        }
    }

    fn poll(&mut self, ctx: &egui::Context) {
        if let Some(slot) = self.scan.clone() {
            if let Some(found) = slot.lock().unwrap_or_else(|e| e.into_inner()).take() {
                self.scan = None;
                self.scanned_at = Some(Instant::now());
                if let Some(sel) = &self.selected {
                    if !found.iter().any(|c| &c.mount == sel) {
                        self.selected = None;
                    }
                }
                if self.selected.is_none() && found.len() == 1 {
                    self.selected = Some(found[0].mount.clone());
                }
                self.cards = found;
            }
        } else if self.pending.is_none()
            && self.scanned_at.is_none_or(|t| t.elapsed() >= RESCAN_AFTER)
        {
            self.start_scan(ctx);
        }
        if let Some(chosen) = self.picker.poll() {
            self.dest_input = chosen.to_string_lossy().into_owned();
        }
    }

    /// Handles a finished listing/planning thread: registers the destination and submits the job.
    fn poll_planning(&mut self, pounce: &Pounce, store: &Arc<SqliteCatalog>, dest: &Path) {
        let Some(rx) = &self.planning else { return };
        let msg = match rx.try_recv() {
            Ok(m) => m,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err("The card could not be read.".into()),
        };
        self.planning = None;
        match msg {
            Err(e) => self.status = Some(e),
            Ok((plan, problems)) => {
                if plan.is_empty() && problems.is_empty() {
                    self.status = Some("No photos found on the card.".into());
                    return;
                }
                let root_id = match nicti_lair::scruff::register_root(store.as_ref(), dest) {
                    Ok(id) => id,
                    Err(e) => {
                        self.status = Some(format!("Couldn't register {}: {e}", dest.display()));
                        return;
                    }
                };
                let n = plan.len();
                let copy = CardCopy::new(plan, problems, dest, root_id);
                let dyn_store: Arc<dyn CatalogStore + Send + Sync> = store.clone();
                let (job, slot) =
                    CardCopyJob::new(dyn_store, copy, format!("Copy {n} file(s) from card"));
                pounce.submit(Box::new(job));
                self.pending = Some(slot);
                self.status = Some(format!("Copying {n} file(s) from the card\u{2026}"));
            }
        }
    }

    fn poll_result(&mut self) {
        let Some(slot) = self.pending.clone() else {
            return;
        };
        let Some(report) = slot.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return;
        };
        self.pending = None;
        self.status = Some(summarize(&report));
    }

    fn begin(&mut self, ctx: &egui::Context, card: &CardVolume) {
        let (name_tpl, sub_tpl) = (
            self.filename_template.clone(),
            self.subfolder_template.clone(),
        );
        // Fail on a bad template here, before touching the card.
        if let Err(e) = build_plan(Vec::new(), &name_tpl, &sub_tpl, &|_| (None, 0, None, None)) {
            self.status = Some(e);
            return;
        }
        let (tx, rx) = mpsc::channel();
        let dcim = card.dcim.clone();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("card-plan".into())
            .spawn(move || {
                let (files, problems) = list_card_files(&dcim);
                let msg = build_plan(files, &name_tpl, &sub_tpl, &read_capture)
                    .map(|plan| (plan, problems));
                let _ = tx.send(msg);
                ctx.request_repaint();
            });
        if spawned.is_ok() {
            self.planning = Some(rx);
            self.status = Some("Reading the card\u{2026}".into());
        } else {
            self.status = Some("Couldn't start reading the card.".into());
        }
    }

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        pounce: &Pounce,
        store: &Arc<SqliteCatalog>,
        other_job_running: bool,
    ) {
        self.poll(ui.ctx());
        self.poll_result();
        let dest = PathBuf::from(self.dest_input.trim());
        self.poll_planning(pounce, store, &dest);
        if self.is_busy() || self.scan.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }

        egui::CollapsingHeader::new("Copy from card")
            .id_salt("nicti_pelt_copy_from_card")
            .show(ui, |ui| {
                ui.label(
                    "Copies a camera card's photos into a folder and checks each copy by reading it \
                     back. The card is never written to or deleted from; format it yourself once \
                     you have checked the copies.",
                );
                if self.cards.is_empty() {
                    ui.label(if self.scan.is_some() {
                        "Looking for cards\u{2026}"
                    } else {
                        "No card detected (a volume with a DCIM folder)."
                    });
                }
                for card in self.cards.clone() {
                    let selected = self.selected.as_ref() == Some(&card.mount);
                    if ui
                        .selectable_label(
                            selected,
                            format!("{}  ({})", card.label, card.mount.display()),
                        )
                        .clicked()
                    {
                        self.selected = Some(card.mount.clone());
                    }
                }
                if ui
                    .add_enabled(self.scan.is_none(), egui::Button::new("Rescan"))
                    .clicked()
                {
                    self.scanned_at = None;
                }
                ui.horizontal(|ui| {
                    ui.label("Copy into:");
                    ui.add_enabled(
                        !self.picker.is_open(),
                        egui::TextEdit::singleline(&mut self.dest_input),
                    );
                    if ui
                        .add_enabled(!self.picker.is_open(), egui::Button::new("Browse..."))
                        .clicked()
                    {
                        self.picker
                            .open(ui.ctx(), "Copy into folder", &self.dest_input);
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Subfolder:");
                    ui.text_edit_singleline(&mut self.subfolder_template)
                        .on_hover_text("e.g. {Date:YYYY-MM-DD}; empty = straight into the folder");
                    ui.label("File name:");
                    ui.text_edit_singleline(&mut self.filename_template)
                        .on_hover_text("e.g. {Filename} or {Date:YYYYMMDD}_{Filename}");
                });
                let card = self
                    .selected
                    .as_ref()
                    .and_then(|m| self.cards.iter().find(|c| &c.mount == m))
                    .cloned();
                let can_start = card.is_some()
                    && !self.dest_input.trim().is_empty()
                    && !self.is_busy()
                    && !other_job_running;
                if ui
                    .add_enabled(can_start, egui::Button::new("Copy from card"))
                    .clicked()
                {
                    if !dest.is_dir() {
                        self.status = Some(format!("{} is not an existing folder.", dest.display()));
                    } else if let Some(card) = card {
                        self.begin(ui.ctx(), &card);
                    }
                }
                if let Some(s) = &self.status {
                    ui.label(s.clone());
                }
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(names: &[&str]) -> Vec<CardFile> {
        names
            .iter()
            .map(|n| CardFile {
                path: PathBuf::from(format!("/card/DCIM/100NIKON/{n}")),
                size: 10,
            })
            .collect()
    }

    fn stub(_: &Path) -> Capture {
        (
            DateParts::parse("2026-11-03 09:15:00"),
            0,
            Some("NIKON".into()),
            Some("Z8".into()),
        )
    }

    #[test]
    fn default_templates_make_a_dated_folder_and_keep_the_name_and_extension() {
        let plan = build_plan(
            files(&["DSC_0001.NEF", "DSC_0002.JPG"]),
            DEFAULT_FILENAME_TEMPLATE,
            DEFAULT_SUBFOLDER_TEMPLATE,
            &stub,
        )
        .unwrap();
        assert_eq!(plan[0].dest_rel, PathBuf::from("2026-11-03/DSC_0001.NEF"));
        assert_eq!(plan[1].dest_rel, PathBuf::from("2026-11-03/DSC_0002.JPG"));
    }

    #[test]
    fn an_empty_subfolder_template_copies_straight_into_the_folder() {
        let plan = build_plan(files(&["a.NEF"]), "{Filename}", "  ", &stub).unwrap();
        assert_eq!(plan[0].dest_rel, PathBuf::from("a.NEF"));
    }

    #[test]
    fn a_hostile_card_file_name_cannot_climb_out_of_the_destination() {
        let hostile = vec![CardFile {
            path: PathBuf::from("/card/DCIM/100NIKON/..\\..\\evil.NEF"),
            size: 1,
        }];
        let plan = build_plan(hostile, "{Filename}", "{Folder}", &stub).unwrap();
        let rel = &plan[0].dest_rel;
        assert!(
            rel.components()
                .all(|c| matches!(c, std::path::Component::Normal(_))),
            "{rel:?}"
        );
    }

    #[test]
    fn bad_templates_are_reported_with_which_one() {
        let e = build_plan(Vec::new(), "{Nope}", "", &stub).unwrap_err();
        assert!(e.starts_with("File name template"), "{e}");
        let e = build_plan(Vec::new(), "{Filename}", "{Nope}", &stub).unwrap_err();
        assert!(e.starts_with("Subfolder template"), "{e}");
    }

    #[test]
    fn the_summary_only_says_safe_to_format_for_a_clean_run() {
        let mut r = CardCopyReport {
            copied: 2,
            bytes_copied: 3 << 20,
            ..Default::default()
        };
        r.ingest = Some(Default::default());
        let s = summarize(&r);
        assert!(s.contains("not modified") && s.contains("3.0 MB"), "{s}");
        r.failed.push((PathBuf::from("x"), "boom".into()));
        r.verify_failed = 1;
        let s = summarize(&r);
        assert!(
            s.contains("did NOT read back") && s.contains("Do not format"),
            "{s}"
        );
        assert!(!s.contains("safe to format"));
    }
}
