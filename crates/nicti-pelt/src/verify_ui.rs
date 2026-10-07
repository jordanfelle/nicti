//! "Verify folder" (#386): submits `nicti_lair::pounce_jobs::VerifyJob` for a root picked from the
//! folder panel's context menu, shows its `VerifyReport`, and -- only when the user asks -- submits
//! a `BaselineJob` that records a trusted checksum for the files that have none yet. The jobs do the
//! work; this module is state, summaries and the report drawing. The pure parts are unit-tested.

use std::path::PathBuf;
use std::sync::Arc;

use nicti_lair::pounce_jobs::{BaselineJob, ReportSlot, VerifyJob};
use nicti_lair::verify::{BaselineReport, VerifyReport};
use nicti_lair::CatalogStore;
use nicti_pounce::Pounce;

/// At most this many paths per list in the details view; the rest are a "+N more" line.
const LIST_CAP: usize = 20;

/// The folder a verify/baseline pass targets.
#[derive(Debug, Clone)]
struct Target {
    root_id: i64,
    path: PathBuf,
}

enum Pending {
    Verify(ReportSlot<VerifyReport>),
    Baseline(ReportSlot<BaselineReport>),
}

#[derive(Default)]
pub struct VerifyUi {
    target: Option<Target>,
    pending: Option<Pending>,
    last_verify: Option<VerifyReport>,
    last_baseline: Option<BaselineReport>,
    /// One-line status: running, the last summary, or why a request was refused.
    note: Option<String>,
}

impl VerifyUi {
    /// `true` while a verify or baseline job is in flight.
    pub fn running(&self) -> bool {
        self.pending.is_some()
    }

    /// Starts a verify of `root_id` at `path`.
    pub fn submit_verify(
        &mut self,
        store: &Arc<dyn CatalogStore + Send + Sync>,
        pounce: &Pounce,
        root_id: i64,
        path: PathBuf,
    ) {
        if self.running() {
            self.note = Some("A verify is already running.".into());
            return;
        }
        let (job, slot) = VerifyJob::new(store.clone(), root_id, &path);
        pounce.submit(Box::new(job));
        self.note = Some(format!("Verifying {} \u{2026}", path.display()));
        self.target = Some(Target { root_id, path });
        self.pending = Some(Pending::Verify(slot));
        self.last_verify = None;
        self.last_baseline = None;
    }

    fn submit_baseline(&mut self, store: &Arc<dyn CatalogStore + Send + Sync>, pounce: &Pounce) {
        let Some(target) = self.target.clone() else {
            return;
        };
        if self.running() {
            return;
        }
        let (job, slot) = BaselineJob::new(store.clone(), target.root_id, &target.path);
        pounce.submit(Box::new(job));
        self.note = Some(format!(
            "Recording baseline for {} \u{2026}",
            target.path.display()
        ));
        self.pending = Some(Pending::Baseline(slot));
        self.last_baseline = None;
    }

    /// Refuses a request (e.g. a move is running) without starting anything.
    pub fn refuse(&mut self, why: &str) {
        self.note = Some(why.to_string());
    }

    /// Folds a finished job's report in. Call once per frame.
    pub fn poll(&mut self) {
        let finished = match &self.pending {
            Some(Pending::Verify(slot)) => slot
                .lock()
                .unwrap()
                .take()
                .map(|r| {
                    self.note = Some(summarize_verify(&r));
                    self.last_verify = Some(r);
                })
                .is_some(),
            Some(Pending::Baseline(slot)) => slot
                .lock()
                .unwrap()
                .take()
                .map(|r| {
                    self.note = Some(summarize_baseline(&r));
                    self.last_baseline = Some(r);
                })
                .is_some(),
            None => false,
        };
        if finished {
            self.pending = None;
        }
    }
}

/// One-line summary of a verify pass.
pub fn summarize_verify(r: &VerifyReport) -> String {
    if r.root_unreachable {
        return "Verify: the folder isn't reachable (drive unplugged?) -- nothing was checked."
            .into();
    }
    if let Some(e) = &r.error {
        return format!("Verify failed: {e}");
    }
    let mut s = if r.cancelled {
        format!(
            "Verify cancelled: {} checked, {} not checked",
            r.checked,
            r.unchecked.len()
        )
    } else {
        format!("Verify: {} checked, {} match", r.checked, r.matched)
    };
    if !r.mismatched.is_empty() {
        s += &format!(", {} CHANGED", r.mismatched.len());
    }
    if !r.missing.is_empty() {
        s += &format!(", {} missing", r.missing.len());
    }
    if !r.unreadable.is_empty() {
        s += &format!(", {} unreadable", r.unreadable.len());
    }
    if r.unhashed > 0 {
        s += &format!(", {} have no checksum yet", r.unhashed);
    }
    if r.is_clean() && r.unhashed == 0 {
        s += " -- all good";
    }
    s.push('.');
    s
}

/// One-line summary of a baseline pass.
pub fn summarize_baseline(r: &BaselineReport) -> String {
    if r.root_unreachable {
        return "Baseline: the folder isn't reachable -- nothing was recorded.".into();
    }
    let noun = if r.recorded == 1 {
        "checksum"
    } else {
        "checksums"
    };
    let mut s = format!("Baseline: recorded {} {noun}", r.recorded);
    if r.cancelled {
        s += " before it was cancelled";
    }
    if r.skipped_changed > 0 {
        s += &format!(", {} skipped (changed since import)", r.skipped_changed);
    }
    if !r.missing.is_empty() {
        s += &format!(", {} missing", r.missing.len());
    }
    if !r.unreadable.is_empty() {
        s += &format!(", {} unreadable", r.unreadable.len());
    }
    if let Some(e) = &r.error {
        s += &format!(" -- stopped: {e}");
    }
    s.push('.');
    s
}

fn path_list(ui: &mut egui::Ui, title: &str, paths: &[String]) {
    if paths.is_empty() {
        return;
    }
    ui.label(format!("{title} ({}):", paths.len()));
    for p in paths.iter().take(LIST_CAP) {
        ui.monospace(p);
    }
    if paths.len() > LIST_CAP {
        ui.weak(format!("+{} more", paths.len() - LIST_CAP));
    }
}

/// Draws the status line, the last report's details and the baseline button. `blocked` is why a
/// baseline can't start right now (another job is running), if so.
pub fn show(
    ui: &mut egui::Ui,
    state: &mut VerifyUi,
    store: &Arc<dyn CatalogStore + Send + Sync>,
    pounce: &Pounce,
    blocked: Option<&str>,
) {
    state.poll();
    if let Some(note) = &state.note {
        ui.separator();
        ui.label(note);
    }
    let mut start_baseline = false;
    if let Some(r) = &state.last_verify {
        ui.collapsing("Last folder verify: details", |ui| {
            path_list(ui, "Changed since copied", &r.mismatched);
            path_list(ui, "Missing from disk", &r.missing);
            let unreadable: Vec<String> = r
                .unreadable
                .iter()
                .map(|(p, e)| format!("{p}: {e}"))
                .collect();
            path_list(ui, "Couldn't be read", &unreadable);
            path_list(ui, "Not checked (cancelled)", &r.unchecked);
            if r.unhashed > 0 && !r.root_unreachable && r.error.is_none() && !r.cancelled {
                ui.separator();
                ui.label(format!(
                    "{} photos have no checksum to compare against.",
                    r.unhashed
                ));
                ui.weak(
                    "Recording a baseline trusts these files as they are right now: a file \
                     that is already damaged would be recorded as-is.",
                );
                let can = blocked.is_none() && !state.running();
                if ui
                    .add_enabled(
                        can,
                        egui::Button::new(format!("Record baseline for {} files", r.unhashed)),
                    )
                    .clicked()
                {
                    start_baseline = true;
                }
                if let Some(why) = blocked {
                    ui.weak(why);
                }
            }
        });
    }
    if let Some(b) = &state.last_baseline {
        if !b.missing.is_empty() || !b.unreadable.is_empty() {
            ui.collapsing("Last baseline: details", |ui| {
                path_list(ui, "Missing from disk", &b.missing);
                let unreadable: Vec<String> = b
                    .unreadable
                    .iter()
                    .map(|(p, e)| format!("{p}: {e}"))
                    .collect();
                path_list(ui, "Couldn't be read", &unreadable);
            });
        }
    }
    if start_baseline {
        state.submit_baseline(store, pounce);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_verify_says_all_good_and_a_dirty_one_names_each_problem() {
        let clean = VerifyReport {
            checked: 4,
            matched: 4,
            ..Default::default()
        };
        assert_eq!(
            summarize_verify(&clean),
            "Verify: 4 checked, 4 match -- all good."
        );
        let dirty = VerifyReport {
            checked: 4,
            matched: 2,
            mismatched: vec!["a".into(), "b".into()],
            missing: vec!["c".into()],
            unhashed: 7,
            ..Default::default()
        };
        assert_eq!(
            summarize_verify(&dirty),
            "Verify: 4 checked, 2 match, 2 CHANGED, 1 missing, 7 have no checksum yet."
        );
    }

    #[test]
    fn unhashed_only_is_not_called_all_good() {
        let r = VerifyReport {
            unhashed: 3,
            ..Default::default()
        };
        assert_eq!(
            summarize_verify(&r),
            "Verify: 0 checked, 0 match, 3 have no checksum yet."
        );
    }

    #[test]
    fn cancelled_unreachable_and_failed_verifies_say_so() {
        let cancelled = VerifyReport {
            checked: 1,
            matched: 1,
            cancelled: true,
            unchecked: vec!["x".into(), "y".into()],
            ..Default::default()
        };
        assert_eq!(
            summarize_verify(&cancelled),
            "Verify cancelled: 1 checked, 2 not checked."
        );
        let gone = VerifyReport {
            root_unreachable: true,
            ..Default::default()
        };
        assert!(summarize_verify(&gone).contains("isn't reachable"));
        let failed = VerifyReport {
            error: Some("disk I/O".into()),
            ..Default::default()
        };
        assert_eq!(summarize_verify(&failed), "Verify failed: disk I/O");
    }

    #[test]
    fn baseline_summary_reports_recorded_and_skipped() {
        let r = BaselineReport {
            recorded: 5,
            skipped_changed: 2,
            missing: vec!["m".into()],
            ..Default::default()
        };
        assert_eq!(
            summarize_baseline(&r),
            "Baseline: recorded 5 checksums, 2 skipped (changed since import), 1 missing."
        );
        let c = BaselineReport {
            recorded: 1,
            cancelled: true,
            ..Default::default()
        };
        assert_eq!(
            summarize_baseline(&c),
            "Baseline: recorded 1 checksum before it was cancelled."
        );
    }
}
