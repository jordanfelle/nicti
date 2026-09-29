//! The `eframe::App` shell: top-level view routing (library/loupe/develop -- the Library view's
//! virtualized grid is #30 (`crate::grid`), the loupe is #31; culling UX is #32 and the filter
//! bar is #242, both still to come) and the wgpu device Tapetum's `GpuContext` shares
//! with eframe (ADR-0016). Also owns Pounce (#55): the job runtime plus the activity panel
//! (`crate::activity`) that reads it, and the Library view's Import/Sync buttons that submit real
//! jobs to it. Also polls Nine Lives (#25) on a slow timer and submits a `BackupJob` when it says
//! one is due -- see this file's own `poll_backup` doc comment.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nicti_cornea::{LibRawDecoder, RawDecoder};
use nicti_lair::carry::{self, CarryOptions, CarryOutcome, Resumed};
use nicti_lair::ninelives::{BackupOutcome, BackupPolicy, BackupReport, NineLives};
use nicti_lair::patrol::SyncOptions;
use nicti_lair::pounce_jobs::{BackupJob, IngestJob, MoveJob, ReportSlot, SyncJob};
use nicti_lair::{
    CatalogError, CatalogStore, Filter, PreviewTier, Sort, SortDirection, SortField, SqliteCatalog,
};
use nicti_pounce::hackles;
use nicti_pounce::telemetry::{default_load_source, default_vram_source, TelemetrySampler};
use nicti_pounce::{JobKind, JobState, Pounce};
use nicti_tapetum::gpu::GpuContext;

use nicti_shed::state::Channel as UpdateChannel;

use crate::color_mgmt::ColorManagement;
use crate::grid::{self, GridSession};
use crate::loupe::{asset_cache_key, LoupeSession};
use crate::render::DevelopView;
use crate::t2::{self, SharedLarder};
use crate::update::UpdateChecker;
use crate::viewport::{fit_scale, one_to_one_scale, ViewportCallback, ViewportResources};
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
/// RAM budget for the loupe's decoded-frame cache (#31) -- a placeholder pending real tuning
/// against actual machine RAM, matching `PLACEHOLDER_VRAM_BUDGET_BYTES`'s own "not measured yet"
/// status.
const PLACEHOLDER_LOUPE_CACHE_BUDGET_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// VRAM budget for the library grid's thumbnail textures (#30) -- a placeholder like the two
/// above. ~4.2 GiB would hold every 256px thumbnail of a 1M-asset catalog; 512 MiB holds ~8k, far
/// more than the few hundred a screen plus overscan ever shows, and stays flat as the catalog grows.
const PLACEHOLDER_GRID_TEXTURE_BUDGET_BYTES: u64 = 512 * 1024 * 1024;
/// How often the grid re-reads its id snapshot while an import is running, so new assets appear
/// as they land instead of only when the whole import finishes.
const GRID_LIVE_RELOAD_INTERVAL: Duration = Duration::from_secs(2);

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
    /// Display ICC profile, soft-proofing and gamut warning (#42, ADR-0042).
    color: ColorManagement,
    version: String,
    catalog_path: PathBuf,
    catalog: CatalogOpenState,
    develop: Option<DevelopView>,
    /// Which of the HSL panel's 8 bands is currently shown (#46) -- UI-only selection state, not
    /// part of any edit document.
    hsl_band_selected: usize,
    pounce: Pounce,
    telemetry: TelemetrySampler,
    /// The bottleneck classifier's last verdict (#70/ADR-0070) -- kept across frames so
    /// `hackles::classify`'s hysteresis has a `previous` to compare a fresh reading against.
    /// `None` until the first real telemetry sample lands (`TelemetrySampler::sample` itself
    /// returns `None` that whole time), distinct from a classified `Idle`.
    bottleneck: Option<hackles::Verdict>,
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
    /// Destination *parent* folder typed for a verified folder move (#26).
    move_dest_input: String,
    /// The in-flight `MoveJob`'s result slot, folded into `last_move_summary` once it resolves.
    pending_move_result: Option<ReportSlot<CarryOutcome>>,
    last_move_summary: Option<String>,
    /// The real RAW decoder every `LoupeSession` (#31) this app creates shares -- constructed
    /// once here rather than per-session, since `LibRawDecoder` is a stateless unit struct with
    /// no per-session setup.
    decoder: Arc<dyn RawDecoder + Send + Sync>,
    loupe: Option<LoupeSession>,
    /// Which asset id *and identity* is currently loaded into `develop` -- so the loupe doesn't
    /// re-`load_real_frame` (which resets the in-memory edit document) every single frame just
    /// because the cursor hasn't moved. The identity half matters, not just the id: `insert_asset`
    /// upserts an existing `(root_id, rel_path)` row in place on a re-ingest, so the same asset id
    /// can get a new `asset_cache_key` without ever changing rows -- comparing on id alone would
    /// leave Develop showing a stale decode (and any edits pinned to it) forever after a re-ingest
    /// produced fresher content for the same id (caught by CodeRabbit's review).
    loupe_loaded_asset: Option<(i64, blake3::Hash)>,
    /// `false` = "Fit" (aspect-correct, the default on every fresh cursor move), `true` = "100%"
    /// (1:1 pixel zoom for focus-checking, #31's own ticket title). Toggled by Space.
    loupe_zoomed: bool,
    /// Pan offset in texture-UV units, only meaningful (and only adjustable, via drag) in 100%
    /// mode -- reset to `[0.0, 0.0]` on every cursor move, matching a fresh image's own natural
    /// centered view rather than wherever the previous image happened to be panned to.
    loupe_pan: [f32; 2],
    /// The current asset's fallback-preview texture, decoded once and cached here -- shown while a
    /// real decode is still in flight. The `bool` is whether it's the T2 screen-resolution preview
    /// (#301, from the Larder) rather than the T0 grid thumbnail: T0 shows instantly, then upgrades
    /// to T2 the moment the Larder has one, and never downgrades. `None` once the real decode
    /// lands (nothing clears it eagerly; it's simply not looked at once `current_frame` starts
    /// returning `Some`, and gets replaced the next time a *different* asset needs it).
    loupe_preview: Option<(i64, bool, egui::TextureHandle)>,
    /// The T2 preview cache (#301), shared with every `LoupeSession`. `None` if it couldn't be
    /// opened (read-only location, another instance holding its lock) -- the loupe then falls back
    /// to T0 alone, exactly as before.
    larder: Option<SharedLarder>,
    /// The asset whose cached T2 bytes failed to decode as an image, so the fallback doesn't
    /// re-read and re-decode them every frame.
    loupe_t2_undecodable: Option<i64>,
    /// The Library view's virtualized grid (#30). Created lazily on first show, once the catalog
    /// is known to be open.
    grid: Option<GridSession>,
    grid_view: grid::ViewState,
    /// The grid's root selector: `None` = every folder.
    grid_root: Option<i64>,
    grid_sort: Sort,
    /// Whether an import/sync/move was running last frame -- the busy -> idle edge is what
    /// triggers a full grid refresh (new assets, replaced previews).
    grid_was_busy: bool,
    grid_last_live_reload: Option<Instant>,
    /// `true` while the current `LoupeSession` was built from the grid's own id list, so its
    /// cursor is an index into the grid and can be mirrored back onto the grid selection. A loupe
    /// opened from the folder box (`open_in_loupe`) has its own list and must not touch the grid.
    loupe_from_grid: bool,
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

        // The T2 preview cache (#301) lives beside the catalog. Failing to open it (read-only
        // location, another instance holding its lock) only costs the T2 upgrade, never the loupe.
        let larder = t2::open_larder(&catalog_path);

        // A crash mid-move (#26) leaves a `root_move` journal row: finish or roll it back before
        // anything else touches that root.
        let last_move_summary = match &catalog {
            CatalogOpenState::Open(store) => summarize_resumed(&carry::resume_open_moves(&**store)),
            CatalogOpenState::Error(_) => None,
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
        let telemetry_ctx = cc.egui_ctx.clone();
        let telemetry = TelemetrySampler::spawn(
            default_vram_source(),
            default_load_source(),
            TELEMETRY_MIN_INTERVAL,
            move || telemetry_ctx.request_repaint(),
        );

        let mut update = UpdateChecker::new();
        // Startup check is best-effort and throttled to at most once per 24h
        // (`UpdateChecker::spawn_check`'s `force: false`) -- this is never the user's first
        // signal that an update exists, just a background nicety.
        update.spawn_check(&version, false);

        Self {
            view: View::Library,
            color: ColorManagement::new(),
            version,
            catalog_path,
            catalog,
            develop: Some(develop),
            hsl_band_selected: 0,
            pounce,
            telemetry,
            bottleneck: None,
            import_path_input: String::new(),
            update,
            nine_lives,
            last_backup_poll: None,
            pending_backup_result: None,
            pending_backup_changes: None,
            last_backup_summary: None,
            move_dest_input: String::new(),
            pending_move_result: None,
            last_move_summary,
            decoder: Arc::new(LibRawDecoder),
            loupe: None,
            loupe_loaded_asset: None,
            loupe_zoomed: false,
            loupe_pan: [0.0, 0.0],
            loupe_preview: None,
            larder,
            loupe_t2_undecodable: None,
            grid: None,
            grid_view: grid::ViewState::default(),
            grid_root: None,
            grid_sort: grid::DEFAULT_SORT,
            grid_was_busy: false,
            grid_last_live_reload: None,
            loupe_from_grid: false,
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

impl PeltApp {
    /// Folds a finished `MoveJob`'s report into `last_move_summary` (#26).
    fn poll_move(&mut self) {
        if let Some(slot) = self.pending_move_result.clone() {
            if let Some(outcome) = slot.lock().unwrap().take() {
                self.last_move_summary = Some(summarize_move(&outcome));
                self.pending_move_result = None;
            }
        }
    }

    /// Keeps the grid's snapshot in step with catalog-changing jobs (#30), whichever view is
    /// showing: re-reads the ids every `GRID_LIVE_RELOAD_INTERVAL` while an import/sync/move runs
    /// (so new assets appear as they land), and does a full refresh -- ids *and* thumbnails, since
    /// a rescan can replace previews -- on the busy -> idle edge. Also stops thumbnail decoding
    /// when the Library view isn't the one on screen.
    fn drive_grid(&mut self, ui: &egui::Ui) {
        let busy = self.job_active(&[JobKind::Import, JobKind::Sync, JobKind::Move]);
        let was_busy = std::mem::replace(&mut self.grid_was_busy, busy);
        let Some(grid) = self.grid.as_mut() else {
            return;
        };
        if was_busy && !busy {
            grid.refresh(&self.pounce);
        } else if busy {
            let due = self
                .grid_last_live_reload
                .is_none_or(|t| t.elapsed() >= GRID_LIVE_RELOAD_INTERVAL);
            if due {
                grid.reload(&self.pounce);
                self.grid_last_live_reload = Some(Instant::now());
            }
            ui.ctx().request_repaint_after(GRID_LIVE_RELOAD_INTERVAL);
        }
        if self.view != View::Library {
            grid.pause(&self.pounce);
        }
    }

    fn job_active(&self, kinds: &[JobKind]) -> bool {
        self.pounce.snapshot().into_iter().any(|s| {
            kinds.contains(&s.kind) && matches!(s.state, JobState::Queued | JobState::Running)
        })
    }

    /// Submits a verified move (#26) of `root_id` into the folder typed in `move_dest_input`.
    /// Refused up front while an import/sync/other move is running -- a scan walking the root
    /// while it's being copied and deleted would race the move.
    fn submit_move(&mut self, store: &Arc<SqliteCatalog>, root_id: i64) {
        let dest = PathBuf::from(self.move_dest_input.trim());
        if dest.as_os_str().is_empty() {
            self.last_move_summary = Some("Type a destination folder first.".into());
            return;
        }
        if self.job_active(&[JobKind::Import, JobKind::Sync, JobKind::Move]) {
            self.last_move_summary =
                Some("Wait for the running import/sync/move to finish first.".into());
            return;
        }
        let dyn_store: Arc<dyn CatalogStore + Send + Sync> = store.clone();
        let (job, result) = MoveJob::new(
            dyn_store,
            root_id,
            &dest,
            CarryOptions::default(),
            now_unix(),
        );
        self.pounce.submit(Box::new(job));
        self.pending_move_result = Some(result);
        self.last_move_summary = Some("Moving\u{2026}".into());
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

/// One line for the Library view once a `MoveJob`'s report is ready (#26).
fn summarize_move(outcome: &CarryOutcome) -> String {
    match outcome {
        CarryOutcome::Moved {
            files,
            bytes,
            renamed,
            leftover_count,
            ..
        } => {
            let how = if *renamed {
                "renamed in place".to_string()
            } else {
                format!(
                    "{files} file(s), {} MiB copied and verified",
                    bytes / (1024 * 1024)
                )
            };
            if *leftover_count > 0 {
                format!(
                    "Folder moved ({how}); {leftover_count} file(s) couldn't be removed from the original location."
                )
            } else {
                format!("Folder moved ({how}).")
            }
        }
        CarryOutcome::Refused(why) => format!("Move refused: {why}"),
        CarryOutcome::VerifyFailed { path } => format!(
            "Move stopped: a copy of {} didn't match its original. Nothing was changed.",
            path.display()
        ),
        CarryOutcome::Failed(msg) => format!("Move failed: {msg}"),
    }
}

/// Startup crash-recovery note, `None` if there was nothing to recover.
fn summarize_resumed(resumed: &[Resumed]) -> Option<String> {
    if resumed.is_empty() {
        return None;
    }
    let stuck: Vec<&str> = resumed
        .iter()
        .filter_map(|r| match r {
            Resumed::Stuck { reason, .. } => Some(reason.as_str()),
            _ => None,
        })
        .collect();
    Some(if stuck.is_empty() {
        format!("Recovered {} interrupted folder move(s).", resumed.len())
    } else {
        format!(
            "{} interrupted folder move(s) need attention: {}",
            stuck.len(),
            stuck.join("; ")
        )
    })
}

impl eframe::App for PeltApp {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.color.handle_shortcuts(ui.ctx());
        self.color.sync(frame);
        self.poll_backup();
        self.poll_move();
        self.drive_grid(ui);
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
                self.color.show_menu(ui);
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

                    // #282: switching channel here re-checks immediately (`force: true`) rather
                    // than waiting for the next 24h auto-check, so picking Edge surfaces whatever
                    // edge build is currently out right away instead of looking like a no-op.
                    //
                    // Disabled (not just guarded after the fact) while a check/apply is in
                    // flight: `channel` below is a fresh local copy re-read from
                    // `self.update.channel()` every frame, so a click accepted mid-check would
                    // only render for that one frame before silently snapping back once
                    // `self.update.channel()` is re-read next frame -- an adversarial review
                    // caught this landing as a picked value with no visible effect and no error.
                    // `add_enabled_ui` stops the click from ever registering in the first place,
                    // matching the "Check for updates" button's own disabled state above.
                    let mut channel = self.update.channel();
                    let busy = self.update.is_checking() || self.update.is_applying();
                    ui.add_enabled_ui(!busy, |ui| {
                        egui::ComboBox::from_id_salt("update_channel")
                            .selected_text(match channel {
                                UpdateChannel::Stable => "Stable",
                                UpdateChannel::Edge => "Edge",
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut channel, UpdateChannel::Stable, "Stable");
                                ui.selectable_value(&mut channel, UpdateChannel::Edge, "Edge");
                            });
                    });
                    if !busy && channel != self.update.channel() {
                        self.update.set_channel(channel, &self.version);
                    }
                });
            });
        });

        crate::activity::show(ui, &self.pounce, &self.telemetry, &mut self.bottleneck);

        if let Some(new_version) = self.update.available_label() {
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
            View::Loupe => self.show_loupe(ui),
            View::Develop => {
                ui.heading("Develop");
                if let Some(frame) = viewport_frame {
                    let available = ui.available_size();
                    let (rect, response) = ui.allocate_exact_size(available, egui::Sense::drag());
                    ui.painter().add(egui_wgpu::Callback::new_paint_callback(
                        rect,
                        ViewportCallback::identity(frame),
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
    /// #30: the Library view -- a root/sort toolbar, the import/sync/move controls (collapsible,
    /// so a big catalog can have the whole window), and the virtualized thumbnail grid.
    fn show_library(&mut self, ui: &mut egui::Ui) {
        let store = match &self.catalog {
            CatalogOpenState::Open(store) => store.clone(),
            CatalogOpenState::Error(_) => {
                ui.heading("Library");
                self.show_library_controls(ui);
                return;
            }
        };
        if self.grid.is_none() {
            let dyn_store: Arc<dyn CatalogStore + Send + Sync> = store.clone();
            self.grid = Some(GridSession::new(
                dyn_store,
                PLACEHOLDER_GRID_TEXTURE_BUDGET_BYTES,
            ));
        }

        ui.horizontal(|ui| {
            ui.heading("Library");
            if let Some(grid) = &self.grid {
                if grid.is_loaded() {
                    ui.label(format!("{} image(s)", grid.len()));
                }
                if grid.is_loading() {
                    ui.spinner();
                }
            }
        });

        let roots = store.list_roots().unwrap_or_default();
        let mut root_sel = self.grid_root;
        let mut sort = self.grid_sort;
        ui.horizontal(|ui| {
            let root_label = root_sel
                .and_then(|id| roots.iter().find(|r| r.id == id))
                .map_or("All folders", |r| r.path.as_str());
            egui::ComboBox::from_id_salt("grid_root")
                .selected_text(root_label)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut root_sel, None, "All folders");
                    for root in &roots {
                        ui.selectable_value(&mut root_sel, Some(root.id), &root.path);
                    }
                });
            egui::ComboBox::from_id_salt("grid_sort_field")
                .selected_text(match sort.field {
                    SortField::Captured => "Capture time",
                    SortField::Imported => "Import time",
                    SortField::Filename => "Filename",
                    SortField::Rating => "Rating",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut sort.field, SortField::Captured, "Capture time");
                    ui.selectable_value(&mut sort.field, SortField::Imported, "Import time");
                    ui.selectable_value(&mut sort.field, SortField::Filename, "Filename");
                    ui.selectable_value(&mut sort.field, SortField::Rating, "Rating");
                });
            ui.selectable_value(&mut sort.direction, SortDirection::Asc, "Ascending");
            ui.selectable_value(&mut sort.direction, SortDirection::Desc, "Descending");
        });
        self.grid_root = root_sel;
        self.grid_sort = sort;
        let filter = Filter {
            root_id: root_sel,
            ..Default::default()
        };
        if let Some(grid) = self.grid.as_mut() {
            grid.set_query(filter, sort, &self.pounce);
        }

        egui::CollapsingHeader::new("Folders: import, sync, move")
            .id_salt("nicti_pelt_library_folders")
            .default_open(true)
            .show(ui, |ui| self.show_library_controls(ui));
        ui.separator();

        let outcome = match self.grid.as_mut() {
            Some(grid) => grid::view::show(ui, grid, &mut self.grid_view, &self.pounce),
            None => grid::view::GridOutcome::default(),
        };
        if let Some(index) = outcome.open {
            self.open_from_grid(&store, index);
        }
    }

    /// The import/sync/move controls and their status lines -- everything the Library view had
    /// before the grid (#30) except the asset count, which the grid's own snapshot now supplies.
    fn show_library_controls(&mut self, ui: &mut egui::Ui) {
        match &self.catalog {
            CatalogOpenState::Open(store) => {
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
                    // #31: a folder-ordered asset list, not real grid/filter-driven selection
                    // (#30/#242's job, confirmed not a hard blocker for the loupe) -- reuses the
                    // same path field and root-registration path Import/Sync already use.
                    if ui.button("Open in Loupe").clicked() {
                        self.open_in_loupe(&store);
                    }
                });

                // #26: verified folder move. Copies and hash-verifies every file, re-points the
                // catalog, then removes the original -- edits/ratings/keywords follow the folder.
                ui.separator();
                ui.label(
                    "Move a folder to another drive (verified copy, then the original is removed):",
                );
                ui.horizontal(|ui| {
                    ui.label("Move into:");
                    ui.text_edit_singleline(&mut self.move_dest_input);
                });
                let mut move_root = None;
                match store.list_roots() {
                    Ok(roots) => {
                        for root in roots {
                            ui.horizontal(|ui| {
                                ui.label(&root.path);
                                if ui.button("Move").clicked() {
                                    move_root = Some(root.id);
                                }
                            });
                        }
                    }
                    Err(e) => {
                        ui.colored_label(
                            egui::Color32::RED,
                            format!("Failed to list folders: {e}"),
                        );
                    }
                }
                if let Some(root_id) = move_root {
                    self.submit_move(&store, root_id);
                }
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
        if let Some(summary) = &self.last_move_summary {
            ui.label(summary);
        }
        ui.label("Filter bar lands in #242.");
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
        if self.job_active(&[JobKind::Move]) {
            self.last_move_summary =
                Some("A folder move is running; import/sync waits until it finishes.".into());
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

    /// Registers `self.import_path_input` the same way `submit_root_job` does, then builds a
    /// fresh `LoupeSession` over every asset already cataloged under that root (in `id` order --
    /// a real grid/filter-driven ordering is #30/#242's job) and switches to the Loupe view. A
    /// blank path or a registration/listing failure is a no-op, same as `submit_root_job`'s own.
    fn open_in_loupe(&mut self, store: &Arc<SqliteCatalog>) {
        let path = PathBuf::from(self.import_path_input.trim());
        if path.as_os_str().is_empty() {
            return;
        }
        if self.job_active(&[JobKind::Move]) {
            self.last_move_summary =
                Some("A folder move is running; wait for it to finish first.".into());
            return;
        }
        let Ok(root_id) = register_root(store.as_ref(), &path) else {
            return;
        };
        let Ok(assets) = store.list_assets_by_root(root_id) else {
            return;
        };
        let ids: Vec<i64> = assets.iter().map(|a| a.id).collect();
        self.start_loupe(store, ids, 0, false);
    }

    /// Opens the loupe on the grid's current ordering (#30), starting at `index` -- so Left/Right
    /// in the loupe walk the same sequence the grid shows, in its sort and filter.
    fn open_from_grid(&mut self, store: &Arc<SqliteCatalog>, index: usize) {
        if self.job_active(&[JobKind::Move]) {
            self.last_move_summary =
                Some("A folder move is running; wait for it to finish first.".into());
            return;
        }
        let Some(grid) = self.grid.as_ref() else {
            return;
        };
        if index >= grid.len() {
            return;
        }
        let ids = grid.ids().to_vec();
        self.start_loupe(store, ids, index, true);
    }

    /// Builds a fresh `LoupeSession` over `ids` at `cursor` and switches to the Loupe view.
    /// `from_grid` records that `ids` is the grid's own list, so the loupe's cursor can be
    /// mirrored back onto the grid selection (see `show_loupe`).
    fn start_loupe(
        &mut self,
        store: &Arc<SqliteCatalog>,
        ids: Vec<i64>,
        cursor: usize,
        from_grid: bool,
    ) {
        // A prior session's in-flight decodes are for a now-abandoned folder -- cancel them
        // rather than let them keep running to a result nothing will ever look at.
        if let Some(mut old) = self.loupe.take() {
            old.cancel_all(&self.pounce);
        }
        let mut session = LoupeSession::new(
            ids,
            self.decoder.clone(),
            PLACEHOLDER_LOUPE_CACHE_BUDGET_BYTES,
        );
        if let Some(larder) = &self.larder {
            session = session.with_larder(larder.clone());
        }
        let _ = session.set_cursor(cursor, store.as_ref(), &self.pounce);
        self.loupe = Some(session);
        self.loupe_from_grid = from_grid;
        // Deliberately NOT resetting `loupe_loaded_asset` here: it tracks which asset id is
        // currently loaded into the shared `develop` view, independent of which `LoupeSession`
        // object exists -- if the new session's first asset happens to be the same one already
        // loaded (adversarial review caught this: an earlier version reset it unconditionally,
        // which forced a needless reset-to-default-document even when re-opening Loupe onto the
        // very same photo already being edited on the Develop tab), `show_loupe`'s own
        // already-loaded check should skip reloading it, not discard those edits for no reason.
        self.loupe_zoomed = false;
        self.loupe_pan = [0.0, 0.0];
        self.loupe_preview = None;
        self.loupe_t2_undecodable = None;
        self.view = View::Loupe;
    }

    /// #31: the Loupe view. Navigates with Left/Right (directional prefetch keeps the neighbors
    /// decoding ahead of the cursor, `LoupeSession`'s own job), toggles Fit/100% zoom with Space,
    /// and drags to pan while zoomed. Shows the T0 embedded preview instantly while a real decode
    /// is still in flight -- the real "< 50ms next/prev" target a RAW decode itself can't hit.
    fn show_loupe(&mut self, ui: &mut egui::Ui) {
        let CatalogOpenState::Open(store) = &self.catalog else {
            ui.heading("Loupe");
            ui.colored_label(egui::Color32::RED, "Catalog is not open.");
            return;
        };
        let store = store.clone();

        let Some(loupe) = self.loupe.as_mut() else {
            ui.heading("Loupe");
            ui.label("Open a folder from the Library view to browse it here.");
            return;
        };

        loupe.poll(store.as_ref(), &self.pounce);

        if loupe.is_empty() {
            ui.heading("Loupe");
            ui.label("This folder has no assets.");
            return;
        }

        let mut cursor_moved = false;
        ui.input(|i| {
            if i.key_pressed(egui::Key::ArrowRight) && loupe.cursor() + 1 < loupe.len() {
                let _ = loupe.set_cursor(loupe.cursor() + 1, store.as_ref(), &self.pounce);
                cursor_moved = true;
            } else if i.key_pressed(egui::Key::ArrowLeft) && loupe.cursor() > 0 {
                let _ = loupe.set_cursor(loupe.cursor() - 1, store.as_ref(), &self.pounce);
                cursor_moved = true;
            }
            if i.key_pressed(egui::Key::Space) {
                self.loupe_zoomed = !self.loupe_zoomed;
            }
        });
        if cursor_moved {
            self.loupe_zoomed = false;
            self.loupe_pan = [0.0, 0.0];
        }

        let loupe = self.loupe.as_mut().expect("checked Some above");
        let Some(asset_id) = loupe.current_asset_id() else {
            return; // is_empty() already checked above; unreachable in practice
        };
        let frame = loupe.current_frame(store.as_ref());
        let cursor_label = format!("{} / {}", loupe.cursor() + 1, loupe.len());
        let error_label = loupe.current_error(store.as_ref()).map(str::to_string);
        let cursor = loupe.cursor();

        // Keep the grid selection on whatever the loupe is showing, so switching back to the
        // Library lands on (and scrolls to) the photo the user just walked to.
        // By asset id, not index: the loupe's id list was frozen when it opened, while the grid's
        // has since been reloaded (an import adding assets, a refresh), so index `i` in one is
        // not index `i` in the other.
        if self.loupe_from_grid {
            if let Some(grid) = self.grid.as_mut() {
                if grid.cursor_id() != Some(asset_id) {
                    grid.select_asset(asset_id);
                    self.grid_view.reveal_cursor();
                }
            }
        }

        let mut retry_clicked = false;
        ui.horizontal(|ui| {
            ui.heading("Loupe");
            ui.label(cursor_label);
            if let Some(err) = &error_label {
                ui.colored_label(egui::Color32::RED, err);
                retry_clicked = ui.button("Retry").clicked();
            }
        });
        if retry_clicked {
            let loupe = self.loupe.as_mut().expect("checked Some above");
            loupe.retry(asset_id);
            // retry() only clears the error -- request_prefetch has to actually run again to
            // resubmit it, same as any other cursor-move-triggered prefetch.
            let _ = loupe.set_cursor(cursor, store.as_ref(), &self.pounce);
        }

        match frame {
            Some(frame) => {
                // Compare on (asset id, identity), not just the id: `insert_asset` upserts an
                // existing `(root_id, rel_path)` row in place on a re-ingest, so a re-imported
                // file can get a fresh `asset_cache_key` without ever changing its asset id --
                // comparing on id alone would skip `load_real_frame` forever after that, leaving
                // Develop stuck showing the stale pre-reingest decode (caught by CodeRabbit).
                let current_asset = store.get_asset(asset_id).ok().flatten();
                let current_identity = current_asset.as_ref().map(asset_cache_key);
                let already_loaded = current_identity
                    .is_some_and(|identity| self.loupe_loaded_asset == Some((asset_id, identity)));

                if !already_loaded {
                    // Adversarial review caught a real data-loss path here: `develop` is one
                    // instance shared with the Develop tab, and switching Loupe to a different
                    // photo (or a fresher revision of the same one) than whatever Develop
                    // currently has loaded would otherwise silently discard any unsaved edits on
                    // it the instant this decode landed -- no warning, no user action beyond
                    // having navigated in a different tab. Refuse to swap (and don't paint a
                    // viewport this frame) until the user explicitly says to discard those edits.
                    let develop_has_unsaved_edits =
                        self.develop.as_ref().is_some_and(DevelopView::has_edits);
                    if develop_has_unsaved_edits {
                        ui.colored_label(
                            egui::Color32::YELLOW,
                            "Develop has unsaved edits for a different photo or source revision.",
                        );
                        if ui
                            .button("Discard those edits and view this photo")
                            .clicked()
                        {
                            if let (Some(develop), Some(asset)) =
                                (self.develop.as_mut(), &current_asset)
                            {
                                let identity = asset_cache_key(asset);
                                develop.load_real_frame(frame, identity);
                                self.loupe_loaded_asset = Some((asset_id, identity));
                            }
                        }
                        return;
                    }
                    if let (Some(develop), Some(asset)) = (self.develop.as_mut(), &current_asset) {
                        let identity = asset_cache_key(asset);
                        develop.load_real_frame(frame, identity);
                        self.loupe_loaded_asset = Some((asset_id, identity));
                    }
                }
                self.paint_loupe_viewport(ui);
            }
            None => self.show_loupe_t0_fallback(ui, store.as_ref(), asset_id),
        }
    }

    /// Renders `self.develop`'s currently-loaded frame and paints it at the current zoom mode's
    /// scale/offset -- split out from `show_loupe` only because borrowing `self.develop` mutably
    /// (for `render()`) alongside reading `self.loupe_zoomed`/`self.loupe_pan` and writing
    /// `self.loupe_pan` from a drag is otherwise an awkward simultaneous-borrow shape.
    fn paint_loupe_viewport(&mut self, ui: &mut egui::Ui) {
        let Some(develop) = self.develop.as_mut() else {
            return;
        };
        let rendered = develop.render();
        let tex_extent = develop.source_extent();

        let available = ui.available_size();
        let (rect, response) = ui.allocate_exact_size(available, egui::Sense::click_and_drag());
        let rect_size = (rect.width(), rect.height());
        // `rect`/`drag_delta()` are egui logical points, not device pixels -- "100%" needs actual
        // physical pixels for one texel to map to one *physical* pixel (this ticket's own
        // "focus-checking" goal), or a HiDPI display (e.g. 150% OS scaling) would show the image
        // magnified past true 1:1 (caught by CodeRabbit's review). "Fit" is unaffected -- it only
        // ever uses an aspect *ratio*, which `pixels_per_point` doesn't change.
        let ppp = ui.ctx().pixels_per_point();
        let rect_size_px = (rect_size.0 * ppp, rect_size.1 * ppp);

        let scale = if self.loupe_zoomed {
            one_to_one_scale(rect_size_px, tex_extent)
        } else {
            fit_scale(rect_size, tex_extent)
        };

        if self.loupe_zoomed && response.dragged() {
            let delta = response.drag_delta();
            // Screen-pixel drag -> texture-UV delta: dividing by the physical-pixel screen extent
            // the current scale maps to (rect size / scale) keeps the drag 1:1 with the cursor
            // regardless of the actual zoom factor or display scaling.
            if scale[0] > 0.0 {
                self.loupe_pan[0] -= (delta.x * ppp) / (rect_size_px.0 / scale[0]);
            }
            if scale[1] > 0.0 {
                self.loupe_pan[1] -= (delta.y * ppp) / (rect_size_px.1 / scale[1]);
            }
        }
        let offset = if self.loupe_zoomed {
            self.loupe_pan
        } else {
            [0.0, 0.0]
        };

        ui.painter().add(egui_wgpu::Callback::new_paint_callback(
            rect,
            ViewportCallback {
                frame: rendered,
                view_scale: scale,
                view_offset: offset,
            },
        ));
    }

    /// The instant fallback while a real decode is still in flight: shows the asset's T2 preview
    /// from the Larder when one is cached (#301), else its T0 embedded preview (extracted at
    /// import time, `nicti_lair::scruff::Ingest`). The texture is cached per asset, so each tier
    /// is decoded once, not every frame; a T0 texture upgrades to T2 the moment one exists.
    fn show_loupe_t0_fallback(
        &mut self,
        ui: &mut egui::Ui,
        store: &dyn CatalogStore,
        asset_id: i64,
    ) {
        if self.loupe_preview.as_ref().map(|(id, _, _)| *id) != Some(asset_id) {
            self.loupe_preview = None;
        }
        let has_t2 = matches!(&self.loupe_preview, Some((_, true, _)));
        if !has_t2 && self.loupe_t2_undecodable != Some(asset_id) {
            if let Some(bytes) = self.loupe.as_mut().and_then(|l| l.current_t2(store)) {
                match preview_texture(ui.ctx(), format!("loupe-t2-{asset_id}"), &bytes) {
                    Some(texture) => self.loupe_preview = Some((asset_id, true, texture)),
                    None => self.loupe_t2_undecodable = Some(asset_id),
                }
            }
        }
        if self.loupe_preview.is_none() {
            if let Ok(Some(preview)) = store.get_preview(asset_id, PreviewTier::T0) {
                if let Some(texture) =
                    preview_texture(ui.ctx(), format!("loupe-t0-{asset_id}"), &preview.bytes)
                {
                    self.loupe_preview = Some((asset_id, false, texture));
                }
            }
        }

        ui.label("Decoding full-resolution image...");
        if let Some((_, _, texture)) = &self.loupe_preview {
            let available = ui.available_size();
            ui.centered_and_justified(|ui| {
                ui.add(egui::Image::new((texture.id(), texture.size_vec2())).max_size(available));
            });
        }
    }
}

/// JPEG-decodes `bytes` into an egui texture, `None` if they aren't a decodable image.
fn preview_texture(ctx: &egui::Context, name: String, bytes: &[u8]) -> Option<egui::TextureHandle> {
    let rgba = image::load_from_memory(bytes).ok()?.to_rgba8();
    let (w, h) = rgba.dimensions();
    let color_image =
        egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], rgba.as_raw());
    Some(ctx.load_texture(name, color_image, egui::TextureOptions::default()))
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
