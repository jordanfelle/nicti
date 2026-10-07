//! Preview-cache settings (#302): live/file bytes against the Larder's cap (#27), an editable cap
//! (default 8 GiB, persisted beside the cache), and purge / reclaim buttons.
//!
//! Every Larder touch from the UI thread uses `try_lock_larder`, never a blocking lock: a running
//! `CompactJob` can hold it for minutes, and the panel must not freeze the window on that. A busy
//! Larder shows the last stats it read and refuses an action with a message instead of queueing
//! it. A cap change or purge (#327: per-entry eviction / deleting the multi-GB pack) runs as a
//! Pounce [`CacheOpJob`] that blocks on the lock like `CompactJob` does, never on the UI thread;
//! the panel disables the buttons while one is in flight.
//! Purging empties the whole Larder -- the camera T2 and (#145) the rendered tier -- and `purge_all` reclaims disk
//! immediately where `purge_tier` would leave dead bytes for a later compaction. A T0 purge is a
//! catalog `preview` table operation the Larder doesn't cover; left out on purpose (the ticket
//! marks it optional).

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nicti_lair::larder::{LarderConfig, LarderStats};
use nicti_lair::pounce_jobs::ReportSlot;
use nicti_pounce::{
    ChunkedJob, JobError, JobKind, JobSpec, Lane, Pounce, Priority, Progress, Step,
};

use crate::t2::{self, CompactJob, CompactResult, SharedLarder};

const GIB: u64 = 1024 * 1024 * 1024;
const MIB: u64 = 1024 * 1024;
/// Below this a single T2 (~1.2 MB) barely fits and the cache thrashes on every browse step.
pub const MIN_CAP_BYTES: u64 = 256 * MIB;
/// Upper bound on the typed value: keeps `GiB * 1024^3` well inside `u64` and rejects typos.
pub const MAX_CAP_BYTES: u64 = 4096 * GIB;
const STATS_REFRESH: Duration = Duration::from_secs(1);
const PURGE_CONFIRM_WINDOW: Duration = Duration::from_secs(5);

/// Parses the cap text box (GiB, decimals allowed) into bytes, enforcing the min/max bounds.
pub fn parse_cap_gib(text: &str) -> Result<u64, String> {
    let gib: f64 = text
        .trim()
        .parse()
        .map_err(|_| format!("\"{}\" is not a number of GiB", text.trim()))?;
    if !gib.is_finite() {
        return Err("cap must be a finite number".into());
    }
    let bytes = gib * GIB as f64;
    if bytes < MIN_CAP_BYTES as f64 {
        return Err(format!(
            "cap must be at least {}",
            format_bytes(MIN_CAP_BYTES)
        ));
    }
    if bytes > MAX_CAP_BYTES as f64 {
        return Err(format!(
            "cap must be at most {}",
            format_bytes(MAX_CAP_BYTES)
        ));
    }
    Ok(bytes as u64)
}

/// "1.5 GiB" / "300.0 MiB" / "12 KiB" / "7 B", binary units to match the cap's own unit.
pub fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    if bytes >= GIB {
        format!("{:.2} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{} KiB", bytes / KIB)
    } else {
        format!("{bytes} B")
    }
}

/// The cap text box shows GiB with no trailing noise: 8 -> "8", 1.5 -> "1.5".
fn cap_text(cap_bytes: u64) -> String {
    // `f64`'s Display is the shortest string that parses back to the same value, so an untouched
    // box always re-applies the exact cap it was seeded from (no 2-decimal rounding).
    (cap_bytes as f64 / GIB as f64).to_string()
}

/// Where the persisted cap lives: a sibling of the Larder directory (the Larder owns everything
/// inside its own directory).
pub fn cap_file_for(catalog_path: &Path) -> PathBuf {
    let mut name = t2::larder_dir_for(catalog_path).into_os_string();
    name.push(".cap.json");
    PathBuf::from(name)
}

/// The saved cap, or `None` when absent, unreadable, or outside the allowed bounds -- a hand-edited
/// or corrupt file falls back to the default rather than opening a 0-byte or absurd cache.
pub fn load_cap(catalog_path: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(cap_file_for(catalog_path)).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let cap = value.get("cap_bytes")?.as_u64()?;
    (MIN_CAP_BYTES..=MAX_CAP_BYTES)
        .contains(&cap)
        .then_some(cap)
}

/// Writes the cap via a temp file + rename so a crash never leaves a half-written document.
pub fn save_cap(catalog_path: &Path, cap_bytes: u64) -> std::io::Result<()> {
    let path = cap_file_for(catalog_path);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(
        &tmp,
        serde_json::json!({ "cap_bytes": cap_bytes }).to_string(),
    )?;
    std::fs::rename(&tmp, &path)
}

/// The `LarderConfig` a session should open with: default, with the persisted cap if there is one.
pub fn larder_config_for(catalog_path: &Path) -> LarderConfig {
    let mut cfg = LarderConfig::default();
    if let Some(cap) = load_cap(catalog_path) {
        cfg.cap_bytes = cap;
    }
    cfg
}

/// A Larder-wide operation too slow for the UI thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheOp {
    SetCap(u64),
    Purge,
}

/// What a finished [`CacheOp`] did. `save_error` is a cap that applied for this session but could
/// not be persisted.
#[derive(Debug, PartialEq, Eq)]
pub enum CacheOpDone {
    Cap {
        bytes: u64,
        save_error: Option<String>,
    },
    Purged {
        freed: u64,
    },
}

pub type CacheOpResult = Result<CacheOpDone, String>;

/// Runs `op` to completion, blocking on the Larder lock (the caller is a worker thread, or a test).
/// A cap change persists itself, so it survives the panel being closed before the job finishes.
pub fn run_cache_op(larder: &SharedLarder, catalog_path: &Path, op: CacheOp) -> CacheOpResult {
    let mut guard = larder.lock().unwrap_or_else(|poisoned| {
        // Same repair `try_lock_larder` does: a panicked `put` can leave its transaction open.
        let mut guard = poisoned.into_inner();
        guard.recover_after_panic();
        larder.clear_poison();
        guard
    });
    match op {
        CacheOp::SetCap(bytes) => {
            let previous = guard.stats().ok().map(|s| s.cap_bytes);
            if let Err(e) = guard.set_cap(bytes) {
                // `set_cap` assigns the new cap before evicting, so a mid-eviction error would
                // leave the live cache on a cap that was never persisted; put the old one back.
                if let Some(previous) = previous {
                    let _ = guard.set_cap(previous);
                }
                return Err(format!("Setting the cap failed: {e}"));
            }
            drop(guard);
            Ok(CacheOpDone::Cap {
                bytes,
                save_error: save_cap(catalog_path, bytes).err().map(|e| e.to_string()),
            })
        }
        CacheOp::Purge => guard
            .purge_all()
            .map(|freed| CacheOpDone::Purged { freed })
            .map_err(|e| format!("Purge failed: {e}")),
    }
}

/// Runs a [`CacheOp`] as a Pounce background job.
pub struct CacheOpJob {
    larder: SharedLarder,
    catalog_path: PathBuf,
    op: CacheOp,
    done: bool,
    result: ReportSlot<CacheOpResult>,
}

impl CacheOpJob {
    pub fn new(
        larder: SharedLarder,
        catalog_path: PathBuf,
        op: CacheOp,
    ) -> (Self, ReportSlot<CacheOpResult>) {
        let result = Arc::new(Mutex::new(None));
        (
            CacheOpJob {
                larder,
                catalog_path,
                op,
                done: false,
                result: result.clone(),
            },
            result,
        )
    }
}

impl ChunkedJob for CacheOpJob {
    fn spec(&self) -> JobSpec {
        JobSpec {
            priority: Priority::Background,
            kind: JobKind::Preview,
            lane: Lane::Cpu,
            vram_bytes: 0,
            image_index: None,
        }
    }

    fn label(&self) -> String {
        match self.op {
            CacheOp::SetCap(_) => "Resize preview cache".to_string(),
            CacheOp::Purge => "Purge preview cache".to_string(),
        }
    }

    fn progress(&self) -> Progress {
        Progress {
            done: u64::from(self.done),
            total: Some(1),
        }
    }

    fn step(&mut self) -> Result<Step, JobError> {
        // Must resolve the slot even on a panic, or the panel waits on it forever.
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            run_cache_op(&self.larder, &self.catalog_path, self.op)
        }))
        .unwrap_or_else(|_| Err("preview cache operation panicked".to_string()));
        *self.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(outcome);
        self.done = true;
        Ok(Step::Done)
    }
}

/// A job Pounce cancels while still queued is dropped without `step` ever running (the Activity
/// panel's cancel button does this); resolve the slot so the panel doesn't stay busy forever.
impl Drop for CacheOpJob {
    fn drop(&mut self) {
        let mut slot = self.result.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            *slot = Some(Err(
                "Cancelled; the preview cache was left unchanged.".into()
            ));
        }
    }
}

/// UI-only state for the panel.
#[derive(Default)]
pub struct CacheSettingsUi {
    /// The cap text box; `None` until first shown, then seeded from the Larder's real cap.
    cap_input: Option<String>,
    stats: Option<LarderStats>,
    last_refresh: Option<Instant>,
    message: Option<String>,
    /// Purge is destructive (the whole cache regenerates lazily), so it takes a second click.
    /// Armed at the first click; expires so leaving the Library view and coming back later never
    /// finds the destructive button already showing.
    confirm_purge: Option<Instant>,
    compaction: Option<ReportSlot<CompactResult>>,
    /// The in-flight cap change / purge -- at most one at a time.
    op: Option<ReportSlot<CacheOpResult>>,
}

impl CacheSettingsUi {
    fn purge_armed(&self) -> bool {
        self.confirm_purge
            .is_some_and(|t| t.elapsed() < PURGE_CONFIRM_WINDOW)
    }

    fn refresh_stats(&mut self, larder: &SharedLarder) {
        let due = self
            .last_refresh
            .is_none_or(|t| t.elapsed() >= STATS_REFRESH);
        if !due {
            return;
        }
        if let Some(guard) = t2::try_lock_larder(larder) {
            self.stats = guard.stats().ok();
            self.last_refresh = Some(Instant::now());
        }
    }

    fn poll_compaction(&mut self) {
        let Some(slot) = &self.compaction else {
            return;
        };
        let Some(result) = slot.lock().ok().and_then(|mut s| s.take()) else {
            return;
        };
        self.message = Some(match result {
            Ok(()) => "Reclaimed unused space in the preview cache.".into(),
            Err(e) => format!("Reclaiming space failed: {e}"),
        });
        self.compaction = None;
        // Force a stats re-read so the file size drops on screen right away.
        self.last_refresh = None;
    }

    fn busy(&self) -> bool {
        self.compaction.is_some() || self.op.is_some()
    }

    fn poll_op(&mut self) {
        let Some(slot) = &self.op else {
            return;
        };
        let Some(result) = slot.lock().ok().and_then(|mut s| s.take()) else {
            return;
        };
        self.message = Some(match result {
            Ok(CacheOpDone::Cap {
                bytes,
                save_error: None,
            }) => format!("Cap set to {}.", format_bytes(bytes)),
            Ok(CacheOpDone::Cap {
                bytes,
                save_error: Some(e),
            }) => format!(
                "Cap set to {} for this session, but saving it failed: {e}",
                format_bytes(bytes)
            ),
            Ok(CacheOpDone::Purged { freed }) => {
                format!("Purged {} of cached previews.", format_bytes(freed))
            }
            Err(e) => e,
        });
        self.op = None;
        self.last_refresh = None;
    }

    fn start_op(
        &mut self,
        larder: &SharedLarder,
        catalog_path: &Path,
        pounce: &Pounce,
        op: CacheOp,
    ) {
        if self.busy() {
            self.message = Some("Preview cache is busy; try again in a moment.".into());
            return;
        }
        let (job, slot) = CacheOpJob::new(larder.clone(), catalog_path.to_path_buf(), op);
        pounce.submit(Box::new(job));
        self.op = Some(slot);
        self.message = Some(match op {
            CacheOp::SetCap(_) => "Applying the cap in the background...".into(),
            CacheOp::Purge => "Purging previews in the background...".into(),
        });
    }
}

/// Draws the panel. `larder` is `None` when the cache couldn't be opened at startup.
pub fn show(
    ui: &mut egui::Ui,
    state: &mut CacheSettingsUi,
    larder: Option<&SharedLarder>,
    catalog_path: &Path,
    pounce: &Pounce,
) {
    ui.label("Preview cache (screen-size previews for the loupe: camera and rendered):");
    let Some(larder) = larder else {
        ui.label("The preview cache could not be opened (read-only location, or another Nicti instance holds it).");
        return;
    };

    state.poll_compaction();
    state.poll_op();
    state.refresh_stats(larder);
    if state.busy() {
        ui.ctx().request_repaint_after(Duration::from_millis(300));
    } else {
        ui.ctx().request_repaint_after(STATS_REFRESH);
    }

    let idle = !state.busy();
    let Some(stats) = state.stats else {
        ui.label("Reading preview cache...");
        return;
    };
    let cap_input = state
        .cap_input
        .get_or_insert_with(|| cap_text(stats.cap_bytes));

    let fraction = if stats.cap_bytes == 0 {
        0.0
    } else {
        (stats.live_bytes as f32 / stats.cap_bytes as f32).clamp(0.0, 1.0)
    };
    ui.add(egui::ProgressBar::new(fraction).text(format!(
        "{} of {} cap ({} previews)",
        format_bytes(stats.live_bytes),
        format_bytes(stats.cap_bytes),
        stats.entry_count
    )));
    ui.label(format!(
        "On disk: {} (includes {} awaiting reclaim)",
        format_bytes(stats.file_bytes),
        format_bytes(stats.file_bytes.saturating_sub(stats.live_bytes)),
    ));

    let mut apply = None;
    ui.horizontal(|ui| {
        ui.label("Cap (GiB):");
        let response = ui.add(egui::TextEdit::singleline(cap_input).desired_width(60.0));
        let enter = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if ui.add_enabled(idle, egui::Button::new("Apply")).clicked() || (enter && idle) {
            apply = Some(parse_cap_gib(cap_input));
        }
        if ui
            .add_enabled(idle, egui::Button::new("Default (8)"))
            .clicked()
        {
            *cap_input = cap_text(LarderConfig::default().cap_bytes);
            apply = Some(Ok(LarderConfig::default().cap_bytes));
        }
    });
    match apply {
        Some(Ok(bytes)) => state.start_op(larder, catalog_path, pounce, CacheOp::SetCap(bytes)),
        Some(Err(e)) => state.message = Some(e),
        None => {}
    }

    ui.horizontal(|ui| {
        let busy = state.busy();
        if ui
            .add_enabled(!busy, egui::Button::new("Reclaim disk space"))
            .clicked()
        {
            let (job, slot) = CompactJob::new(larder.clone());
            pounce.submit(Box::new(job));
            state.compaction = Some(slot);
            state.message = Some("Reclaiming space in the background...".into());
        }
        if state.purge_armed() {
            if ui
                .add_enabled(!busy, egui::Button::new("Really purge all previews"))
                .clicked()
            {
                state.confirm_purge = None;
                state.start_op(larder, catalog_path, pounce, CacheOp::Purge);
            }
            if ui.button("Cancel").clicked() {
                state.confirm_purge = None;
            }
        } else if ui
            .add_enabled(!busy, egui::Button::new("Purge all previews"))
            .clicked()
        {
            state.confirm_purge = Some(Instant::now());
        }
    });
    if state.purge_armed() {
        ui.label("Previews regenerate as you browse; nothing else is lost.");
    }
    if let Some(msg) = &state.message {
        ui.label(msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cap_accepts_whole_and_fractional_gib() {
        assert_eq!(parse_cap_gib("8").unwrap(), 8 * GIB);
        assert_eq!(parse_cap_gib(" 1.5 ").unwrap(), GIB + GIB / 2);
    }

    #[test]
    fn parse_cap_rejects_garbage_and_out_of_range() {
        for bad in ["", "abc", "nan", "inf", "-1", "0", "0.1", "5000"] {
            assert!(parse_cap_gib(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn parse_cap_bounds_are_inclusive() {
        assert_eq!(parse_cap_gib("0.25").unwrap(), MIN_CAP_BYTES);
        assert_eq!(parse_cap_gib("4096").unwrap(), MAX_CAP_BYTES);
    }

    #[test]
    fn cap_text_round_trips_through_parse() {
        for cap in [8 * GIB, GIB + GIB / 2, MIN_CAP_BYTES, 300_000_007] {
            assert_eq!(parse_cap_gib(&cap_text(cap)).unwrap(), cap);
        }
        assert_eq!(cap_text(8 * GIB), "8");
    }

    #[test]
    fn format_bytes_picks_a_unit() {
        assert_eq!(format_bytes(7), "7 B");
        assert_eq!(format_bytes(2048), "2 KiB");
        assert_eq!(format_bytes(3 * MIB), "3.0 MiB");
        assert_eq!(format_bytes(8 * GIB), "8.00 GiB");
    }

    #[test]
    fn saved_cap_is_loaded_and_bad_files_fall_back() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = dir.path().join("cat.db");
        assert_eq!(load_cap(&catalog), None);
        save_cap(&catalog, 3 * GIB).unwrap();
        assert_eq!(load_cap(&catalog), Some(3 * GIB));
        assert_eq!(larder_config_for(&catalog).cap_bytes, 3 * GIB);

        for bad in [
            "not json",
            "{}",
            r#"{"cap_bytes":0}"#,
            r#"{"cap_bytes":-5}"#,
            r#"{"cap_bytes":1}"#,
            r#"{"cap_bytes":18446744073709551615}"#,
        ] {
            std::fs::write(cap_file_for(&catalog), bad).unwrap();
            assert_eq!(load_cap(&catalog), None, "{bad}");
            assert_eq!(
                larder_config_for(&catalog).cap_bytes,
                LarderConfig::default().cap_bytes
            );
        }
    }

    #[test]
    fn apply_cap_persists_and_shrinks_the_live_larder() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = dir.path().join("cat.db");
        let larder = t2::open_larder(&catalog).unwrap();
        let done = run_cache_op(&larder, &catalog, CacheOp::SetCap(GIB)).unwrap();
        assert_eq!(
            done,
            CacheOpDone::Cap {
                bytes: GIB,
                save_error: None
            }
        );
        assert_eq!(larder.lock().unwrap().stats().unwrap().cap_bytes, GIB);
        assert_eq!(load_cap(&catalog), Some(GIB));
        // A fresh session opens with the saved cap.
        drop(larder);
        let reopened = t2::open_larder(&catalog).unwrap();
        assert_eq!(reopened.lock().unwrap().stats().unwrap().cap_bytes, GIB);
    }

    #[test]
    fn shrinking_the_cap_evicts_entries_down_to_it() {
        use nicti_lair::larder::{LarderKey, LarderTier};
        let dir = tempfile::tempdir().unwrap();
        let catalog = dir.path().join("cat.db");
        let larder = t2::open_larder(&catalog).unwrap();
        let payload = vec![7u8; MIB as usize];
        {
            let mut l = larder.lock().unwrap();
            for id in 0..MIN_CAP_BYTES / MIB * 2 {
                let key = LarderKey {
                    asset_id: id as i64,
                    tier: LarderTier::T2,
                    render_hash: "h",
                };
                assert!(l.put(key, &payload).unwrap());
            }
            assert!(l.stats().unwrap().live_bytes > MIN_CAP_BYTES);
        }
        run_cache_op(&larder, &catalog, CacheOp::SetCap(MIN_CAP_BYTES)).unwrap();
        let stats = larder.lock().unwrap().stats().unwrap();
        assert_eq!(stats.cap_bytes, MIN_CAP_BYTES);
        assert!(stats.live_bytes <= MIN_CAP_BYTES, "{stats:?}");
        assert!(stats.entry_count > 0, "eviction must be LRU, not a wipe");
    }

    #[test]
    fn purge_confirmation_expires() {
        let mut ui = CacheSettingsUi::default();
        assert!(!ui.purge_armed());
        ui.confirm_purge = Some(Instant::now());
        assert!(ui.purge_armed());
        ui.confirm_purge = Some(Instant::now() - PURGE_CONFIRM_WINDOW - Duration::from_secs(1));
        assert!(!ui.purge_armed());
    }

    #[test]
    fn purge_empties_the_larder_and_reports_freed_bytes() {
        use nicti_lair::larder::{LarderKey, LarderTier};
        let dir = tempfile::tempdir().unwrap();
        let catalog = dir.path().join("cat.db");
        let larder = t2::open_larder(&catalog).unwrap();
        let key = LarderKey {
            asset_id: 1,
            tier: LarderTier::T2,
            render_hash: "h",
        };
        assert!(larder.lock().unwrap().put(key, &[7u8; 4096]).unwrap());
        let done = run_cache_op(&larder, &catalog, CacheOp::Purge).unwrap();
        assert_eq!(done, CacheOpDone::Purged { freed: 4096 });
        let stats = larder.lock().unwrap().stats().unwrap();
        assert_eq!(
            (stats.entry_count, stats.live_bytes, stats.file_bytes),
            (0, 0, 0)
        );
    }

    /// Polls the panel until its in-flight op resolves (the job runs on a Pounce worker).
    fn finish(ui: &mut CacheSettingsUi) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while ui.op.is_some() {
            ui.poll_op();
            assert!(Instant::now() < deadline, "cache op never resolved");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn cap_change_runs_off_the_calling_thread_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = dir.path().join("cat.db");
        let larder = t2::open_larder(&catalog).unwrap();
        let pounce = Pounce::new(0, 2, 2, || {});
        let mut ui = CacheSettingsUi::default();
        // Holding the lock stands in for a slow eviction: `start_op` must return immediately
        // (it would deadlock this thread if it ran the op inline) and the op must wait for it.
        let held = larder.lock().unwrap();
        ui.start_op(&larder, &catalog, &pounce, CacheOp::SetCap(GIB));
        assert!(ui.busy());
        assert!(ui.message.as_deref().unwrap().contains("background"));
        // A second action while one is in flight is refused, not queued.
        ui.start_op(&larder, &catalog, &pounce, CacheOp::Purge);
        assert!(ui.message.as_deref().unwrap().contains("busy"));
        drop(held);
        finish(&mut ui);
        assert!(ui
            .message
            .as_deref()
            .unwrap()
            .contains("Cap set to 1.00 GiB"));
        assert_eq!(larder.lock().unwrap().stats().unwrap().cap_bytes, GIB);
        assert_eq!(load_cap(&catalog), Some(GIB));
    }

    #[test]
    fn a_cancelled_job_frees_the_panel() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = dir.path().join("cat.db");
        let larder = t2::open_larder(&catalog).unwrap();
        let mut ui = CacheSettingsUi::default();
        let (job, slot) = CacheOpJob::new(larder, catalog, CacheOp::Purge);
        ui.op = Some(slot);
        assert!(ui.busy());
        drop(job); // what Pounce does with a job cancelled while queued
        ui.poll_op();
        assert!(!ui.busy());
        assert!(ui.message.as_deref().unwrap().contains("Cancelled"));
    }

    #[test]
    fn purge_job_reports_freed_bytes() {
        use nicti_lair::larder::{LarderKey, LarderTier};
        let dir = tempfile::tempdir().unwrap();
        let catalog = dir.path().join("cat.db");
        let larder = t2::open_larder(&catalog).unwrap();
        let key = LarderKey {
            asset_id: 1,
            tier: LarderTier::T2,
            render_hash: "h",
        };
        assert!(larder.lock().unwrap().put(key, &[7u8; 4096]).unwrap());
        let pounce = Pounce::new(0, 2, 2, || {});
        let mut ui = CacheSettingsUi::default();
        ui.start_op(&larder, &catalog, &pounce, CacheOp::Purge);
        finish(&mut ui);
        assert!(ui.message.as_deref().unwrap().contains("4 KiB"));
        assert_eq!(larder.lock().unwrap().stats().unwrap().entry_count, 0);
    }
}
