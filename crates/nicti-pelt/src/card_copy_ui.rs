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

use nicti_lair::cardcopy::{check_destination, survey_card, CardCopy, CardCopyReport, PlannedCopy};
use nicti_lair::cards::{default_mount_roots, scan_cards, CardVolume};
use nicti_lair::offering::{list_card_files, CardFile};
use nicti_lair::pounce_jobs::{CardCopyJob, ReportSlot};
use nicti_lair::{CatalogStore, SqliteCatalog};
use nicti_pounce::Pounce;
use nicti_preen::metadata::SourceExif;
use nicti_preen::naming::{AssetFacts, DateParts, Template};

use crate::folder_dialog::FolderPicker;

/// Cards are re-detected at most this often while the panel is open.
const RESCAN_AFTER: Duration = Duration::from_secs(10);
/// A detection that has not answered by now (a dead network mount, an empty reader) is abandoned:
/// its thread may linger, but the panel stops waiting and repainting for it.
const SCAN_TIMEOUT: Duration = Duration::from_secs(30);

pub const DEFAULT_FILENAME_TEMPLATE: &str = "{Filename}";
pub const DEFAULT_SUBFOLDER_TEMPLATE: &str = "{Date:YYYY-MM-DD}";

/// What naming needs from a card file: capture time, mtime (unix seconds), make, model, lens, ISO.
type Capture = (
    Option<DateParts>,
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<u32>,
);

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
        let (captured, mtime_unix, make, model, lens, iso) = capture(&file.path);
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
            lens,
            iso,
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
    // Lens/ISO come from the same extractor ingest uses, so a card file is named exactly as the
    // catalog will later describe it (a different reader disagreed on high-ISO bodies).
    let (lens, iso) = nicti_lair::scruff::read_exif_extra(path).unwrap_or((None, None));
    let mtime = std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64);
    (captured, mtime, exif.make, exif.model, lens, iso)
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
    if r.baseline_recorded > 0 {
        s.push_str(&format!(
            "; {} verified hash(es) recorded for the bit-rot check",
            r.baseline_recorded
        ));
    }
    if let Some(e) = &r.baseline_error {
        // Not a copy problem (the files are copied and registered): the baseline pass hashes them later.
        s.push_str(&format!("; couldn't record their verified hashes: {e}"));
    }
    if let Some(e) = &r.ingest_error {
        s.push_str(&format!("; registering stopped: {e}"));
    } else if r.ingest.is_none() && !r.destinations.is_empty() {
        s.push_str("; adding the copies to the catalog did not finish");
    }
    if !r.not_considered.is_empty() {
        s.push_str(&format!(
            "; NOT copied (this tool copies .nef/.nrw/.dng/.jpg/.jpeg/.jpe/.jfif from the card): {}",
            r.not_considered.join(", ")
        ));
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

type PlanMsg = Result<(Vec<PlannedCopy>, Vec<String>, Vec<String>), String>;

pub struct CardCopyUi {
    cards: Vec<CardVolume>,
    scan: Option<Arc<Mutex<Option<Vec<CardVolume>>>>>,
    scan_started: Option<Instant>,
    scanned_at: Option<Instant>,
    /// The panel was open on the last frame: detection only runs then.
    panel_open: bool,
    selected: Option<PathBuf>,
    pub dest_input: String,
    picker: FolderPicker,
    pub filename_template: String,
    pub subfolder_template: String,
    planning: Option<Receiver<PlanMsg>>,
    /// The checked, canonical destination the running plan was made for (not the text field, which
    /// may have changed since).
    planning_dest: Option<PathBuf>,
    pending: Option<ReportSlot<CardCopyReport>>,
    /// The running copy's Pounce job, for the Cancel button (#659).
    job: Option<nicti_pounce::JobId>,
    pub status: Option<String>,
}

impl Default for CardCopyUi {
    fn default() -> Self {
        Self {
            cards: Vec::new(),
            scan: None,
            scan_started: None,
            scanned_at: None,
            panel_open: false,
            selected: None,
            dest_input: String::new(),
            picker: FolderPicker::default(),
            filename_template: DEFAULT_FILENAME_TEMPLATE.into(),
            subfolder_template: DEFAULT_SUBFOLDER_TEMPLATE.into(),
            planning: None,
            planning_dest: None,
            pending: None,
            job: None,
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
        self.scan_started = Some(Instant::now());
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
        if self.scan.is_some()
            && self
                .scan_started
                .is_some_and(|t| t.elapsed() >= SCAN_TIMEOUT)
        {
            self.scan = None;
            self.scanned_at = Some(Instant::now());
        }
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
        } else if self.panel_open
            && self.pending.is_none()
            && self.scanned_at.is_none_or(|t| t.elapsed() >= RESCAN_AFTER)
        {
            self.start_scan(ctx);
        }
        if let Some(chosen) = self.picker.poll() {
            self.dest_input = chosen.to_string_lossy().into_owned();
        }
    }

    /// Handles a finished listing/planning thread: registers the destination and submits the job.
    /// `other_job_running` is re-checked here, because the planning window itself is long.
    fn poll_planning(
        &mut self,
        pounce: &Pounce,
        store: &Arc<SqliteCatalog>,
        other_job_running: bool,
    ) {
        let Some(rx) = &self.planning else { return };
        let msg = match rx.try_recv() {
            Ok(m) => m,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err("The card could not be read.".into()),
        };
        self.planning = None;
        let Some(dest) = self.planning_dest.take() else {
            return;
        };
        match msg {
            Err(e) => self.status = Some(e),
            Ok((plan, problems, not_considered)) => {
                if plan.is_empty() && problems.is_empty() && not_considered.is_empty() {
                    self.status = Some("No photos found on the card.".into());
                    return;
                }
                if other_job_running {
                    self.status = Some(
                        "Another job started while the card was being read; try again when it finishes."
                            .into(),
                    );
                    return;
                }
                let root_id = match nicti_lair::scruff::register_root(store.as_ref(), &dest) {
                    Ok(id) => id,
                    Err(e) => {
                        self.status = Some(format!("Couldn't register {}: {e}", dest.display()));
                        return;
                    }
                };
                let n = plan.len();
                let copy = CardCopy::new(plan, problems, &dest, root_id)
                    .with_not_considered(not_considered);
                let dyn_store: Arc<dyn CatalogStore + Send + Sync> = store.clone();
                let (job, slot) =
                    CardCopyJob::new(dyn_store, copy, format!("Copy {n} file(s) from card"));
                self.job = Some(pounce.submit(Box::new(job)));
                self.pending = Some(slot);
                self.status = Some(format!("Copying {n} file(s) from the card\u{2026}"));
            }
        }
    }

    /// Asks the running copy to stop (#659). It is honoured between files: the file being copied
    /// is finished and verified first, so nothing half-written is left behind, and the files not
    /// reached are counted as not attempted in the report that follows.
    pub(crate) fn cancel(&mut self, pounce: &Pounce) {
        if let Some(job) = self.job {
            pounce.cancel(job);
            self.status = Some("Stopping after the current file\u{2026}".into());
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
        self.job = None;
        self.status = Some(summarize(&report));
    }

    fn begin(&mut self, ctx: &egui::Context, card: &CardVolume) {
        let (name_tpl, sub_tpl) = (
            self.filename_template.clone(),
            self.subfolder_template.clone(),
        );
        // Fail on a bad template or a destination on the card here, before touching the card.
        if let Err(e) = build_plan(Vec::new(), &name_tpl, &sub_tpl, &|_| {
            (None, 0, None, None, None, None)
        }) {
            self.status = Some(e);
            return;
        }
        let mounts: Vec<PathBuf> = self.cards.iter().map(|c| c.mount.clone()).collect();
        let dest = match check_destination(Path::new(self.dest_input.trim()), &mounts) {
            Ok(d) => d,
            Err(e) => {
                self.status = Some(e);
                return;
            }
        };
        let (tx, rx) = mpsc::channel();
        let (dcim, mount) = (card.dcim.clone(), card.mount.clone());
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("card-plan".into())
            .spawn(move || {
                let (files, problems) = list_card_files(&dcim);
                let copied: std::collections::HashSet<PathBuf> =
                    files.iter().map(|f| f.path.clone()).collect();
                let not_considered = survey_card(&mount, &copied);
                let msg = build_plan(files, &name_tpl, &sub_tpl, &read_capture)
                    .map(|plan| (plan, problems, not_considered));
                let _ = tx.send(msg);
                ctx.request_repaint();
            });
        if spawned.is_ok() {
            self.planning = Some(rx);
            self.planning_dest = Some(dest);
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
        self.poll_planning(pounce, store, other_job_running);
        if self.is_busy() || self.scan.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }

        let panel = egui::CollapsingHeader::new("Copy from card")
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
                        !self.picker.is_open() && !self.is_busy(),
                        egui::TextEdit::singleline(&mut self.dest_input),
                    );
                    if ui
                        .add_enabled(
                            !self.picker.is_open() && !self.is_busy(),
                            egui::Button::new("Browse..."),
                        )
                        .clicked()
                    {
                        self.picker
                            .open(ui.ctx(), "Copy into folder", &self.dest_input);
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Subfolder:");
                    ui.add_enabled(
                        !self.is_busy(),
                        egui::TextEdit::singleline(&mut self.subfolder_template),
                    )
                    .on_hover_text("e.g. {Date:YYYY-MM-DD}; empty = straight into the folder");
                    ui.label("File name:");
                    ui.add_enabled(
                        !self.is_busy(),
                        egui::TextEdit::singleline(&mut self.filename_template),
                    )
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
                    if let Some(card) = card {
                        self.begin(ui.ctx(), &card);
                    }
                }
                if self.job.is_some()
                    && ui
                        .button("Cancel")
                        .on_hover_text("Stops after the file being copied. Copies already made stay on the destination; run it again to register them.")
                        .clicked()
                {
                    self.cancel(pounce);
                }
                if let Some(s) = &self.status {
                    ui.label(s.clone());
                }
            });
        self.panel_open = panel.body_returned.is_some();
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
            Some("NIKKOR Z 50mm f/1.8 S".into()),
            Some(640),
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
    fn lens_iso_and_num_tokens_name_card_files() {
        let plan = build_plan(files(&["DSC_0001.NEF"]), "{Num}_{ISO}", "{Lens}", &stub).unwrap();
        assert_eq!(
            plan[0].dest_rel,
            PathBuf::from("NIKKOR Z 50mm f_1.8 S").join("0001_640.NEF")
        );
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
    fn every_unclean_cause_is_named_and_blocks_the_safe_to_format_line() {
        let base = || CardCopyReport {
            copied: 1,
            destinations: vec![PathBuf::from("d")],
            ingest: Some(Default::default()),
            ..Default::default()
        };
        assert!(summarize(&base()).contains("safe to format"));
        let mut r = base();
        r.not_considered = vec!["3 .mov".into()];
        let s = summarize(&r);
        assert!(
            s.contains("NOT copied") && s.contains("3 .mov") && !s.contains("safe to format"),
            "{s}"
        );
        let mut r = base();
        r.ingest = None;
        let s = summarize(&r);
        assert!(
            s.contains("did not finish") && !s.contains("safe to format"),
            "{s}"
        );
        let mut r = base();
        r.not_attempted = 2;
        let s = summarize(&r);
        assert!(
            s.contains("2 not attempted") && !s.contains("safe to format"),
            "{s}"
        );
        let mut r = base();
        r.listing_problems = vec!["x".into()];
        let s = summarize(&r);
        assert!(
            s.contains("could not be read") && !s.contains("safe to format"),
            "{s}"
        );
        let mut r = base();
        r.ingest_error = Some("db locked".into());
        let s = summarize(&r);
        assert!(
            s.contains("db locked") && !s.contains("safe to format"),
            "{s}"
        );
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

    #[test]
    fn recorded_hashes_are_reported_and_a_baseline_failure_never_blocks_safe_to_format() {
        let mut r = CardCopyReport {
            copied: 2,
            destinations: vec![PathBuf::from("d")],
            ingest: Some(Default::default()),
            baseline_recorded: 2,
            ..Default::default()
        };
        let s = summarize(&r);
        assert!(
            s.contains("2 verified hash(es) recorded") && s.contains("safe to format"),
            "{s}"
        );
        r.baseline_recorded = 0;
        r.baseline_error = Some("db busy".into());
        let s = summarize(&r);
        assert!(s.contains("db busy") && s.contains("safe to format"), "{s}");
    }

    /// Occupies the only worker until released, so a job submitted after it cannot have started.
    struct Blocker(Arc<std::sync::atomic::AtomicBool>);

    impl nicti_pounce::ChunkedJob for Blocker {
        fn spec(&self) -> nicti_pounce::JobSpec {
            nicti_pounce::JobSpec {
                priority: nicti_pounce::Priority::Foreground,
                kind: nicti_pounce::JobKind::Import,
                lane: nicti_pounce::Lane::Cpu,
                vram_bytes: 0,
                image_index: None,
            }
        }
        fn label(&self) -> String {
            "blocker".into()
        }
        fn progress(&self) -> nicti_pounce::Progress {
            nicti_pounce::Progress::default()
        }
        fn step(&mut self) -> Result<nicti_pounce::Step, nicti_pounce::JobError> {
            if self.0.load(std::sync::atomic::Ordering::SeqCst) {
                Ok(nicti_pounce::Step::Done)
            } else {
                std::thread::sleep(Duration::from_millis(2));
                Ok(nicti_pounce::Step::Yield)
            }
        }
    }

    #[test]
    fn cancel_stops_a_queued_copy_before_it_copies_anything_and_the_report_says_so() {
        use nicti_lair::pounce_jobs::CardCopyJob;
        use nicti_lair::{CatalogStore, SqliteCatalog};
        use std::sync::atomic::{AtomicBool, Ordering};
        let tmp = tempfile::tempdir().unwrap();
        let (card, dest) = (tmp.path().join("card"), tmp.path().join("dest"));
        std::fs::create_dir_all(&card).unwrap();
        std::fs::create_dir_all(&dest).unwrap();
        let plan: Vec<PlannedCopy> = (0..5)
            .map(|i| {
                let p = card.join(format!("{i}.NEF"));
                std::fs::write(&p, vec![7u8; 4096]).unwrap();
                PlannedCopy {
                    file: CardFile {
                        path: p,
                        size: 4096,
                    },
                    dest_rel: PathBuf::from(format!("{i}.NEF")),
                }
            })
            .collect();
        let catalog = Arc::new(SqliteCatalog::open_in_memory().unwrap());
        let vol = catalog.upsert_volume("v", None, None, 0).unwrap();
        let root = catalog.ensure_root(vol, &dest.to_string_lossy()).unwrap();
        let store: Arc<dyn CatalogStore + Send + Sync> = catalog;
        // One CPU worker, held by the blocker: the copy job is queued and cannot have started.
        let pounce = Pounce::new(0, 1, 1, || {});
        let release = Arc::new(AtomicBool::new(false));
        pounce.submit(Box::new(Blocker(release.clone())));
        let (job, slot) = CardCopyJob::new(
            store,
            CardCopy::new(plan, Vec::new(), &dest, root),
            "test".into(),
        );
        let mut ui = CardCopyUi {
            job: Some(pounce.submit(Box::new(job))),
            pending: Some(slot),
            ..Default::default()
        };
        ui.cancel(&pounce);
        assert!(ui
            .status
            .as_deref()
            .is_some_and(|s| s.starts_with("Stopping")));
        release.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(10);
        while ui.is_busy() {
            assert!(
                Instant::now() < deadline,
                "the cancelled job never reported"
            );
            ui.poll_result();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(ui.job.is_none());
        let status = ui.status.unwrap();
        assert!(status.contains("5 not attempted"), "{status}");
        assert!(!status.contains("safe to format"), "{status}");
        assert_eq!(
            std::fs::read_dir(&dest).unwrap().count(),
            0,
            "nothing was copied"
        );
    }
}
