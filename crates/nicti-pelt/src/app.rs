//! The `eframe::App` shell: top-level view routing (library/loupe/develop, each a placeholder
//! panel beyond Develop's real Tapetum viewport -- grid virtualization is #30, loupe is #31,
//! culling UX is #32, the filter bar is #242) and the wgpu device Tapetum's `GpuContext` shares
//! with eframe (ADR-0016). Also owns Pounce (#55): the job runtime plus the activity panel
//! (`crate::activity`) that reads it, and the Library view's Import/Sync buttons that submit real
//! jobs to it. Also polls Nine Lives (#25) on a slow timer and submits a `BackupJob` when it says
//! one is due -- see this file's own `poll_backup` doc comment.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nicti_lair::ninelives::{BackupOutcome, BackupPolicy, BackupReport, NineLives};
use nicti_lair::patrol::SyncOptions;
use nicti_lair::pounce_jobs::{BackupJob, IngestJob, ReportSlot, SyncJob};
use nicti_lair::{CatalogError, CatalogStore, SqliteCatalog};
use nicti_pounce::telemetry::{default_vram_source, TelemetrySampler};
use nicti_pounce::{JobKind, JobState, Pounce};
use nicti_tapetum::gpu::GpuContext;

use crate::render::DevelopView;
use crate::update::UpdateChecker;
use crate::viewport::{ViewportCallback, ViewportResources};
use crate::{catalog, CatalogOpenState};

/// VRAM Pounce's GPU-lane admission control (ADR-0054 decision rule #5) budgets against -- a
/// placeholder until a real bake pipeline (Tapetum's own cache tiers) exists to size this from
/// actual measured usage; nothing this shell submits to the GPU lane yet declares any VRAM cost.
const PLACEHOLDER_VRAM_BUDGET_BYTES: u64 = 512 * 1024 * 1024;
/// Telemetry is throttled independently of egui's own repaint rate -- see `telemetry.rs`'s own
/// doc comment for why re-sampling `sysinfo`/DXGI on every frame would be wasteful.
const TELEMETRY_MIN_INTERVAL: Duration = Duration::from_millis(500);
/// This shell has no real volume-identity system wired in yet (ADR-0071 is Proposed, and
/// `catalog::resolve_path`'s own doc comment already flags "a real per-platform app-data default
/// ... is left to whichever ticket adds catalog-picker UI" as out of scope here) -- every
/// Import/Sync root registers under one fixed placeholder volume, with the folder's own full path
/// as its `rel_path`, rather than inventing volume-mount detection prematurely.
const PLACEHOLDER_VOLUME_IDENTITY_KEY: &str = "nicti-pelt-local-placeholder";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Library,
    Loupe,
    Develop,
}

/// Which button the Library view's Import/Sync row was clicked for -- distinct from
/// `nicti_pounce::JobKind`, which labels a *submitted job's* own kind for the activity panel, not
/// a UI dispatch key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RootAction {
    Import,
    Sync,
}

pub struct PeltApp {
    view: View,
    version: String,
    catalog_path: PathBuf,
    catalog: CatalogOpenState,
    develop: Option<DevelopView>,
    /// Which of the HSL panel's 8 bands is currently shown (#46) -- UI-only selection state, not
    /// part of any edit document.
    hsl_band_selected: usize,
    pounce: Pounce,
    telemetry: TelemetrySampler,
    import_path_input: String,
    update: UpdateChecker,
    /// Nine Lives' (#25) own scheduler, `None` when the catalog itself failed to open (nothing to
    /// back up). See `poll_backup`'s own doc comment for how this gets checked and acted on.
    nine_lives: Option<NineLives>,
    last_backup_poll: Option<Instant>,
    /// The most recently submitted `BackupJob`'s result slot, polled once per `poll_backup` tick
    /// until it reports -- then folded into `last_backup_summary` and dropped.
    pending_backup_result: Option<ReportSlot<BackupReport>>,
    /// The catalog's own change counter at the moment the pending job was submitted -- fed into
    /// `NineLives::record_ran` only once that job's report actually resolves as `Verified` (never
    /// on submission itself, and never on a failed/skipped outcome). See `poll_backup`'s own doc
    /// comment for why: an adversarial review caught that recording it eagerly at submit time,
    /// regardless of outcome, would leave a failure permanently silent and could suppress every
    /// later scheduled attempt until the catalog happened to change again.
    pending_backup_changes: Option<u64>,
    last_backup_summary: Option<String>,
}

/// How often `poll_backup` even bothers checking `NineLives::due` -- `due` itself is cheap (one
/// directory listing), but there's no reason to run it every single frame.
const BACKUP_POLL_INTERVAL: Duration = Duration::from_secs(30);

impl PeltApp {
    pub fn new(cc: &eframe::CreationContext<'_>, version: String) -> Self {
        let render_state = cc
            .wgpu_render_state
            .as_ref()
            .expect("nicti-pelt requires the wgpu backend (eframe::Renderer::Wgpu)");

        let gpu = Arc::new(GpuContext::from_device(
            &render_state.adapter,
            render_state.device.clone(),
            render_state.queue.clone(),
        ));
        let develop = DevelopView::new(gpu);

        let resources = ViewportResources::new(&render_state.device, render_state.target_format);
        render_state
            .renderer
            .write()
            .callback_resources
            .insert(resources);

        let catalog_path = catalog::resolve_path();
        let (catalog, nine_lives) = match catalog::open(&catalog_path) {
            Ok(store) => {
                let policy = BackupPolicy::for_catalog(&catalog_path);
                (
                    CatalogOpenState::Open(Arc::new(store)),
                    Some(NineLives::new(policy)),
                )
            }
            Err(e) => (CatalogOpenState::Error(e.to_string()), None),
        };

        let egui_ctx = cc.egui_ctx.clone();
        let cpu_threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        let pounce = Pounce::new(
            PLACEHOLDER_VRAM_BUDGET_BYTES,
            cpu_threads,
            (cpu_threads / 2).max(1),
            move || egui_ctx.request_repaint(),
        );
        let telemetry = TelemetrySampler::new(default_vram_source(), TELEMETRY_MIN_INTERVAL);

        let mut update = UpdateChecker::new();
        // Startup check is best-effort and throttled to at most once per 24h
        // (`UpdateChecker::spawn_check`'s `force: false`) -- this is never the user's first
        // signal that an update exists, just a background nicety.
        update.spawn_check(&version, false);

        Self {
            view: View::Library,
            version,
            catalog_path,
            catalog,
            develop: Some(develop),
            hsl_band_selected: 0,
            pounce,
            telemetry,
            import_path_input: String::new(),
            update,
            nine_lives,
            last_backup_poll: None,
            pending_backup_result: None,
            pending_backup_changes: None,
            last_backup_summary: None,
        }
    }

    /// Checks Nine Lives' (#25) own `due()` at most once per `BACKUP_POLL_INTERVAL` and submits a
    /// `BackupJob` when it says yes -- nothing runs on exit, by this ticket's own design (see
    /// `ninelives`'s module doc comment), so a slow poll while the app is open is the only place
    /// this ever fires. Also folds a previously-submitted job's result into
    /// `last_backup_summary` once it's ready, and skips submitting a new one while one is still
    /// running or queued (checked via `Pounce::snapshot`, not local state, since that's the same
    /// source of truth the activity panel itself reads).
    ///
    /// `NineLives::record_ran` is called only once a pending job's report resolves as
    /// `BackupOutcome::Verified`, never at submission time and never on any other outcome. An
    /// adversarial review caught that the original version called it eagerly, right after
    /// `submit`, regardless of what the job later did -- combined with `BackupJob::step` itself
    /// (before its own fix) never resolving its `ReportSlot` at all on a genuine error, a single
    /// transient I/O failure would silently and permanently suppress every later scheduled backup
    /// until the catalog happened to change again. `BackupJob` now always resolves its slot (see
    /// its own doc comment), and this method now only ever advances the "last backup" baseline on
    /// an actual success -- a failure leaves the baseline where it was, so `NineLives::due` keeps
    /// retrying on the very next poll rather than waiting for unrelated further edits.
    fn poll_backup(&mut self) {
        if let Some(result) = self.pending_backup_result.clone() {
            if let Some(report) = result.lock().unwrap().take() {
                if matches!(report.outcome, BackupOutcome::Verified(_)) {
                    if let (Some(nine_lives), Some(changes)) =
                        (self.nine_lives.as_mut(), self.pending_backup_changes)
                    {
                        nine_lives.record_ran(changes);
                    }
                }
                self.last_backup_summary = Some(summarize_backup(&report));
                self.pending_backup_result = None;
                self.pending_backup_changes = None;
            }
        }

        let CatalogOpenState::Open(store) = &self.catalog else {
            return;
        };
        let Some(nine_lives) = self.nine_lives.as_mut() else {
            return;
        };

        let now = Instant::now();
        if self
            .last_backup_poll
            .is_some_and(|last| now.duration_since(last) < BACKUP_POLL_INTERVAL)
        {
            return;
        }
        self.last_backup_poll = Some(now);

        let already_running = self.pounce.snapshot().into_iter().any(|s| {
            s.kind == JobKind::Backup && matches!(s.state, JobState::Queued | JobState::Running)
        });
        if already_running {
            return;
        }

        let now_unix = now_unix();
        let changes = store.change_counter();
        let due = match nine_lives.due(now_unix, changes) {
            Ok(due) => due,
            Err(e) => {
                self.last_backup_summary = Some(format!("backup scheduling check failed: {e}"));
                return;
            }
        };
        if !due {
            return;
        }

        let (job, result) = BackupJob::new(store.clone(), nine_lives.policy().clone(), now_unix);
        self.pounce.submit(Box::new(job));
        self.pending_backup_result = Some(result);
        self.pending_backup_changes = Some(changes);
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// One line for the Library view -- `poll_backup`'s own result handling calls this once a
/// submitted `BackupJob`'s report is ready.
fn summarize_backup(report: &BackupReport) -> String {
    match &report.outcome {
        BackupOutcome::Verified(path) => {
            format!("Last backup: {}", path.display())
        }
        BackupOutcome::LiveCorrupt(msg) => {
            format!("Backup skipped -- catalog failed its own integrity check: {msg}")
        }
        BackupOutcome::VerifyFailed(msg) => {
            format!("Backup failed verification and was discarded: {msg}")
        }
        BackupOutcome::Failed(msg) => {
            format!("Backup failed: {msg}")
        }
    }
}

impl eframe::App for PeltApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_backup();
        self.update.poll();
        if self.update.is_checking() || self.update.is_applying() {
            // Nothing else drives a repaint while a background check or apply is in flight
            // (it's not user input), so without this a result would only ever show up once
            // something else happens to trigger one.
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(300));
        }

        egui::Panel::top("view_tabs").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.view, View::Library, "Library");
                ui.selectable_value(&mut self.view, View::Loupe, "Loupe");
                ui.selectable_value(&mut self.view, View::Develop, "Develop");
                ui.separator();
                ui.label(format!("Catalog: {}", self.catalog_path.display()));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(format!("v{}", self.version));
                    let check_label = if self.update.is_checking() {
                        "Checking..."
                    } else {
                        "Check for updates"
                    };
                    if ui
                        .add_enabled(
                            !self.update.is_checking() && !self.update.is_applying(),
                            egui::Button::new(check_label),
                        )
                        .clicked()
                    {
                        self.update.spawn_check(&self.version, true);
                    }
                });
            });
        });

        crate::activity::show(ui, &self.pounce, &mut self.telemetry);

        if let Some(new_version) = self.update.available_version().cloned() {
            egui::Panel::top("update_banner").show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(format!("Nicti {new_version} is available."));
                    let button_label = if self.update.is_applying() {
                        "Restarting..."
                    } else {
                        "Restart to update"
                    };
                    if ui
                        .add_enabled(!self.update.is_applying(), egui::Button::new(button_label))
                        .clicked()
                    {
                        self.update.apply();
                    }
                });
            });
        }
        if let Some(err) = self.update.last_error() {
            egui::Panel::top("update_error").show(ui, |ui| {
                ui.colored_label(egui::Color32::RED, format!("Update failed: {err}"));
            });
        }

        // The Develop panel needs a render to show its histogram against (a live-only change
        // costs 0 bake dispatches, so this is cheap). The panel itself then mutates `develop`'s
        // document via its sliders -- so the viewport paint below re-renders *after* the panel,
        // not from this same texture, or a slider drag would visibly lag its own edit by one UI
        // frame (the histogram itself still reflects the pre-edit state at this point in the
        // frame; a real re-render for it too would need restructuring the panel to render at its
        // own end instead of its own start, not worth it for a histogram bar's one-frame lag).
        let panel_frame = if self.view == View::Develop {
            self.develop.as_mut().map(|d| d.render())
        } else {
            None
        };

        if let (View::Develop, Some(frame)) = (self.view, &panel_frame) {
            egui::Panel::right("develop_panel")
                .min_size(280.0)
                .show(ui, |ui| {
                    if let Some(develop) = self.develop.as_mut() {
                        crate::develop_panel::show(ui, develop, frame, &mut self.hsl_band_selected);
                    }
                });
        }

        let viewport_frame = if self.view == View::Develop {
            self.develop.as_mut().map(|d| d.render())
        } else {
            None
        };

        egui::CentralPanel::default().show(ui, |ui| match self.view {
            View::Library => self.show_library(ui),
            View::Loupe => {
                ui.heading("Loupe");
                ui.label("Prefetch + instant zoom lands in #31.");
            }
            View::Develop => {
                ui.heading("Develop");
                if let Some(frame) = viewport_frame {
                    let available = ui.available_size();
                    let (rect, response) = ui.allocate_exact_size(available, egui::Sense::drag());
                    ui.painter().add(egui_wgpu::Callback::new_paint_callback(
                        rect,
                        ViewportCallback { frame },
                    ));
                    if let Some(develop) = self.develop.as_mut() {
                        crate::develop_panel::handle_viewport_gesture(ui, &response, rect, develop);
                    }
                }
            }
        });
    }
}

impl PeltApp {
    fn show_library(&mut self, ui: &mut egui::Ui) {
        ui.heading("Library");
        match &self.catalog {
            CatalogOpenState::Open(store) => {
                match store.asset_count() {
                    Ok(count) => {
                        ui.label(format!("{count} asset(s) in this catalog."));
                    }
                    Err(e) => {
                        ui.colored_label(
                            egui::Color32::RED,
                            format!("Failed to query catalog: {e}"),
                        );
                    }
                }

                let store = store.clone();
                ui.horizontal(|ui| {
                    ui.label("Folder:");
                    ui.text_edit_singleline(&mut self.import_path_input);
                    if ui.button("Import").clicked() {
                        self.submit_root_job(&store, RootAction::Import);
                    }
                    if ui.button("Sync").clicked() {
                        self.submit_root_job(&store, RootAction::Sync);
                    }
                });
            }
            CatalogOpenState::Error(msg) => {
                ui.colored_label(
                    egui::Color32::RED,
                    format!(
                        "Failed to open catalog {}: {msg}",
                        self.catalog_path.display()
                    ),
                );
            }
        }
        if let Some(summary) = &self.last_backup_summary {
            ui.label(summary);
        }
        ui.label("Grid virtualization at scale lands in #30. Filter bar lands in #242.");
    }

    /// Registers `self.import_path_input` as a root under the placeholder volume (see this
    /// module's own `PLACEHOLDER_VOLUME_IDENTITY_KEY` doc comment) and submits an
    /// `IngestJob`/`SyncJob` for it to Pounce's CPU lane. A blank path or a registration failure
    /// is a no-op -- there's no toast/error-banner mechanism in this placeholder shell yet to
    /// surface it more visibly than the catalog-open error label above already does for a bad
    /// catalog path.
    fn submit_root_job(&mut self, store: &Arc<SqliteCatalog>, action: RootAction) {
        let path = PathBuf::from(self.import_path_input.trim());
        if path.as_os_str().is_empty() {
            return;
        }
        let Ok(root_id) = register_root(store.as_ref(), &path) else {
            return;
        };
        let dyn_store: Arc<dyn CatalogStore + Send + Sync> = store.clone();
        match action {
            RootAction::Import => {
                let (job, _result) = IngestJob::new(dyn_store, root_id, &path);
                self.pounce.submit(Box::new(job));
            }
            RootAction::Sync => {
                let (job, _result) =
                    SyncJob::new(dyn_store, root_id, &path, SyncOptions::default());
                self.pounce.submit(Box::new(job));
            }
        }
    }
}

/// Registers `path` as a root under the fixed placeholder volume this shell uses (see
/// `PLACEHOLDER_VOLUME_IDENTITY_KEY`'s own doc comment) and returns its root id.
fn register_root(store: &dyn CatalogStore, path: &Path) -> Result<i64, CatalogError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let volume_id = store.upsert_volume(PLACEHOLDER_VOLUME_IDENTITY_KEY, None, None, now)?;
    store.ensure_root(volume_id, &path.to_string_lossy())
}
