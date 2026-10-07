//! The `eframe::App` shell: top-level view routing (library/loupe/develop -- the Library view's
//! virtualized grid is #30 (`crate::grid`), its filter bar #242 (`crate::filter_bar`), the loupe
//! is #31, culling is #32 (`crate::cull`)) and the wgpu device Tapetum's `GpuContext` shares
//! with eframe (ADR-0016). Also owns Pounce (#55): the job runtime plus the activity panel
//! (`crate::activity`) that reads it, and the Library view's Import/Sync buttons that submit real
//! jobs to it. Also polls Nine Lives (#25) on a slow timer and submits a `BackupJob` when it says
//! one is due -- see this file's own `poll_backup` doc comment.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::folder_dialog::FolderPicker;
use crate::folder_panel;
use nicti_cornea::{LibRawDecoder, RawDecoder};
use nicti_lair::carry::{self, CarryOptions, CarryOutcome, Resumed};
use nicti_lair::ninelives::{BackupOutcome, BackupPolicy, BackupReport, NineLives};
use nicti_lair::patrol::SyncOptions;
use nicti_lair::pounce_jobs::{BackupJob, IngestJob, MoveJob, ReportSlot, ResumeMovesJob, SyncJob};
use nicti_lair::shred;
use nicti_lair::{CatalogError, CatalogStore, Sort, SortDirection, SortField, SqliteCatalog};
use nicti_pounce::hackles;
use nicti_pounce::telemetry::{default_load_source, default_vram_source, TelemetrySampler};
use nicti_pounce::{JobKind, JobState, Pounce};
use nicti_preen::exporters::builtin_registry;
use nicti_preen::ExporterRegistry;
use nicti_tapetum::gpu::GpuContext;

use nicti_shed::state::Channel as UpdateChannel;

use crate::cache_settings::{self, CacheSettingsUi};
use crate::color_mgmt::ColorManagement;
use crate::cull::compare::{self as cull_compare, CompareSession, Side};
use crate::cull::delete::DeleteFlow;
use crate::cull::input::KeyCommand;
use crate::cull::keys::CullAction;
use crate::cull::previews::{preview_texture, TilePreviews};
use crate::cull::survey::{self as cull_survey, SurveySession};
use crate::cull::CullState;
use crate::export::{facts_for, ExportEnv, ExportUi};
use crate::eyeshine::{self, Badge, EyeshineService};
use crate::filter_bar::FilterBar;
use crate::grid::{self, GridSession};
use crate::heal_tool::HealUi;
use crate::knead::batch::run_batch;
use crate::knead::ui::{Command as KneadCommand, KneadUi, PanelAction};
use crate::knead::Clipboard;
use crate::loupe::{asset_cache_key, LoupeSession};
use crate::lrc_import::{self, LrcImportUi};
use crate::mask_panel::MaskUi;
use crate::prebake::{self, PrebakeService};
use crate::preview_settings::{self, PreviewSettings, Surface};
use crate::render::DevelopView;
use crate::t2::{self, SharedLarder};
use crate::update::UpdateChecker;
use crate::verify_ui::{self, VerifyUi};
use crate::viewport::{fit_rect, fit_scale, one_to_one_scale, ViewportCallback, ViewportResources};
use crate::xmp_sync::{self, XmpMeta, XmpUi, XmpWriter};
use crate::{catalog, CatalogOpenState};

/// VRAM Pounce's GPU-lane admission control (ADR-0054 decision rule #5) budgets against -- a
/// placeholder until a real bake pipeline (Tapetum's own cache tiers) exists to size this from
/// actual measured usage; nothing this shell submits to the GPU lane yet declares any VRAM cost.
const PLACEHOLDER_VRAM_BUDGET_BYTES: u64 = 512 * 1024 * 1024;
/// Telemetry is throttled independently of egui's own repaint rate -- see `telemetry.rs`'s own
/// doc comment for why re-sampling `sysinfo`/DXGI on every frame would be wasteful.
const TELEMETRY_MIN_INTERVAL: Duration = Duration::from_millis(500);
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
    /// A handful of photos side by side (#32). Only reachable while a `SurveySession` exists.
    Survey,
    /// Select vs. candidate (#32). Only reachable while a `CompareSession` exists.
    Compare,
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
    /// The device Develop and export both render on (one shared device, ADR-0016).
    gpu: Arc<GpuContext>,
    /// Export (#57): dialog, presets, the active run and its report.
    export: ExportUi,
    /// Copy/paste, sync and presets for develop settings (#52).
    knead: KneadUi,
    export_registry: Arc<ExporterRegistry>,
    /// A save of Develop's edits that failed, and the document it failed for: the autosave doesn't
    /// retry the same document every frame, and the message shows in the top bar.
    edit_save_failed: Option<(nicti_pawprint::EditDocument, String)>,
    /// Which of the HSL panel's 8 bands is currently shown (#46) -- UI-only selection state, not
    /// part of any edit document.
    hsl_band_selected: usize,
    /// Hints and the low-confidence marker for Auto / Auto-level (#311, ADR-0101).
    auto_hint: crate::develop_panel::AutoHintUi,
    /// The Heal / Remove tool's UI state and its AI-removal service (#51).
    heal_ui: HealUi,
    mask_ui: MaskUi,
    pounce: Pounce,
    telemetry: TelemetrySampler,
    /// The bottleneck classifier's last verdict (#70/ADR-0070) -- kept across frames so
    /// `hackles::classify`'s hysteresis has a `previous` to compare a fresh reading against.
    /// `None` until the first real telemetry sample lands (`TelemetrySampler::sample` itself
    /// returns `None` that whole time), distinct from a classified `Idle`.
    bottleneck: Option<hackles::Verdict>,
    import_path_input: String,
    /// #342: Browse dialogs for the Import/Sync folder and the Move destination.
    import_picker: FolderPicker,
    move_picker: FolderPicker,
    /// #367: why the last Import/Sync/Open in Loupe click did nothing (blank, missing or
    /// non-directory path, registration failure). Cleared on the next valid submit.
    import_status: Option<String>,
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
    /// The folder panel's (#303) throttled read of roots/journal rows/mounted drives.
    folder_cache: folder_panel::Cache,
    /// Drives/folders whose contents are archived (thumbnails as sidecars) -- #72.
    archive_drives: crate::archive_drives::ArchiveDrives,
    /// The in-flight `MoveJob`'s result slot, folded into `last_move_summary` once it resolves.
    pending_move_result: Option<ReportSlot<CarryOutcome>>,
    last_move_summary: Option<String>,
    /// The startup `ResumeMovesJob`'s result slot (#307), folded in by `poll_recovery`.
    pending_recovery_result: Option<ReportSlot<Vec<Resumed>>>,
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
    /// `ColorManagement::generation` the preview surfaces were last converted for (#319).
    preview_color_generation: u64,
    /// The T2 preview cache (#301), shared with every `LoupeSession`. `None` if it couldn't be
    /// opened (read-only location, another instance holding its lock) -- the loupe then falls back
    /// to T0 alone, exactly as before.
    larder: Option<SharedLarder>,
    /// The preview-cache settings panel's UI state (#302).
    cache_settings: CacheSettingsUi,
    /// Rendered previews (#145): which photos get one and which views use it.
    preview_settings: PreviewSettings,
    /// Queues/dedupes the background renders of edited photos' screen previews (#145).
    eyeshine: EyeshineService,
    /// Bakes the AI masks of photos a sync/paste touched, in the background (#353).
    prebake: PrebakeService,
    /// The loupe fallback's installed *rendered* preview: `(asset, render hash)`. Lets the fallback
    /// swap in a newer render without ever downgrading to a camera preview.
    loupe_rendered: Option<(i64, String)>,
    /// What the loupe fallback's current preview should be flagged with (#145).
    loupe_badge: Badge,
    /// A stored render whose bytes wouldn't decode, so it isn't re-read every frame.
    loupe_render_undecodable: Option<(i64, String)>,
    /// Switching to `Off` empties the rendered tier; retried each frame while the Larder is busy.
    purge_rendered_pending: bool,
    /// The current pending purge's failure was already shown (see `try_purge_rendered`).
    purge_error_reported: bool,
    /// Sidecar write-back for marker changes and its settings/review panel (#60); `None` when
    /// the catalog failed to open.
    xmp: Option<XmpWriter>,
    xmp_ui: XmpUi,
    /// The Lightroom Classic catalog import panel (#62).
    lrc_ui: LrcImportUi,
    verify_ui: VerifyUi,
    /// The asset whose cached T2 bytes failed to decode as an image, so the fallback doesn't
    /// re-read and re-decode them every frame.
    loupe_t2_undecodable: Option<i64>,
    /// The loupe photo with no T0 anywhere, and when we last looked (see `TilePreviews::no_t0`).
    loupe_t0_miss: Option<(i64, std::time::Instant)>,
    /// A sidecar read for the loupe photo in flight on a worker (#72).
    loupe_t0_fetch: Option<(i64, crate::t0_fetch::FetchSlot)>,
    /// The Library view's virtualized grid (#30). Created lazily on first show, once the catalog
    /// is known to be open.
    grid: Option<GridSession>,
    grid_view: grid::ViewState,
    /// The grid's root selector: `None` = every folder.
    grid_root: Option<i64>,
    /// #242: the Library filter bar's controls (`filter_bar.rs`).
    filter_bar: FilterBar,
    grid_sort: Sort,
    /// Whether an import/sync/move was running last frame -- the busy -> idle edge is what
    /// triggers a full grid refresh (new assets, replaced previews).
    grid_was_busy: bool,
    grid_last_live_reload: Option<Instant>,
    /// `true` while the current `LoupeSession` was built from the grid's own id list, so its
    /// cursor is an index into the grid and can be mirrored back onto the grid selection. A loupe
    /// opened from the folder box (`open_in_loupe`) has its own list and must not touch the grid.
    loupe_from_grid: bool,
    /// Culling (#32): marking, undo, and the markers the views draw. `None` when the catalog
    /// failed to open (nothing to mark).
    cull: Option<CullState>,
    /// The Delete flow's prompt, job and status line (#32).
    delete: DeleteFlow,
    survey: Option<SurveySession>,
    compare: Option<CompareSession>,
    /// Preview textures for the survey/compare tiles.
    tile_previews: TilePreviews,
    /// When marking last happened, while the filter bar's facet counts ("Unrated (N)") are stale
    /// because of it. Marking never changes the filter, so nothing else would refresh them.
    facets_dirty_since: Option<Instant>,
    /// A short hint about the last culling action that couldn't happen ("select two or more
    /// photos to survey"), cleared by the next successful one.
    cull_notice: Option<String>,
}

/// How often `poll_backup` even bothers checking `NineLives::due` -- `due` itself is cheap (one
/// directory listing), but there's no reason to run it every single frame.
const BACKUP_POLL_INTERVAL: Duration = Duration::from_secs(30);

impl PeltApp {
    pub fn new(cc: &eframe::CreationContext<'_>, version: String) -> Self {
        crate::fur::install_fonts(&cc.egui_ctx);
        crate::fur::apply(&cc.egui_ctx);
        let render_state = cc
            .wgpu_render_state
            .as_ref()
            .expect("nicti-pelt requires the wgpu backend (eframe::Renderer::Wgpu)");

        let gpu = Arc::new(GpuContext::from_device(
            &render_state.adapter,
            render_state.device.clone(),
            render_state.queue.clone(),
        ));
        let develop = DevelopView::new(gpu.clone());

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
        let export = ExportUi::new(&catalog_path);
        let knead = KneadUi::new(&catalog_path);

        // A crash mid-move (#26) leaves a `root_move` journal row; it's finished or rolled back
        // by a Pounce job submitted once Pounce exists below (#307), not here, so a big
        // interrupted move's cleanup can't stall startup.
        let archive_drives = crate::archive_drives::ArchiveDrives::load(&catalog_path);

        // A crash mid-delete (#32) leaves `delete_item` journal rows: settle them (finish the ones
        // whose files are in the Recycle Bin, keep the ones whose files are still on disk) before
        // the grid reads the catalog.
        let mut delete = DeleteFlow::new();
        if let CatalogOpenState::Open(store) = &catalog {
            match shred::resume_open_deletes(&**store, |ids| {
                crate::cull::delete::purge_previews(larder.as_ref(), ids);
            }) {
                Ok(r) if r.finished + r.rolled_back + r.stuck > 0 => {
                    delete.last_summary = Some(format!(
                        "Recovered an interrupted delete: {} finished, {} kept (file still on \
                         disk), {} waiting for their drive.",
                        r.finished, r.rolled_back, r.stuck
                    ));
                }
                Ok(_) => {}
                Err(e) => {
                    delete.last_summary =
                        Some(format!("Couldn't recover an interrupted delete: {e}"));
                }
            }
        }

        let egui_ctx = cc.egui_ctx.clone();
        let xmp = match &catalog {
            CatalogOpenState::Open(store) => {
                let dyn_store: Arc<dyn CatalogStore + Send + Sync> = store.clone();
                Some(XmpWriter::spawn(
                    dyn_store,
                    xmp_sync::load_autowrite(&catalog_path),
                ))
            }
            CatalogOpenState::Error(_) => None,
        };
        let cull = match (&catalog, &xmp) {
            (CatalogOpenState::Open(store), Some(xmp)) => {
                let dyn_store: Arc<dyn CatalogStore + Send + Sync> = store.clone();
                let ctx = egui_ctx.clone();
                Some(CullState::new(
                    Arc::new(XmpMeta::new(dyn_store, xmp.clone())),
                    move || ctx.request_repaint(),
                ))
            }
            _ => None,
        };
        let cpu_threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        let pounce = Pounce::new(
            PLACEHOLDER_VRAM_BUDGET_BYTES,
            cpu_threads,
            (cpu_threads / 2).max(1),
            move || egui_ctx.request_repaint(),
        );
        // #307: recovery runs under `JobKind::Move`, so every "a move is running" guard (import,
        // sync, move, delete, loupe open) holds until it's done; `poll_recovery` folds in the
        // result.
        let (pending_recovery_result, last_move_summary) = match &catalog {
            CatalogOpenState::Open(store) => {
                let dyn_store: Arc<dyn CatalogStore + Send + Sync> = store.clone();
                match ResumeMovesJob::new(dyn_store) {
                    Some((job, result)) => {
                        pounce.submit(Box::new(job));
                        (
                            Some(result),
                            Some("Recovering an interrupted folder move\u{2026}".to_string()),
                        )
                    }
                    None => (None, None),
                }
            }
            CatalogOpenState::Error(_) => (None, None),
        };
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

        let prebake = PrebakeService::new(match (&catalog, &larder) {
            (CatalogOpenState::Open(store), Some(larder)) => Some(prebake::Env {
                decoder: Arc::new(LibRawDecoder),
                store: store.clone(),
                larder: larder.clone(),
                submitter: pounce.submitter(),
            }),
            _ => None,
        });
        let preview_settings = preview_settings::load(&catalog_path);
        let eyeshine = EyeshineService::new(larder.as_ref().map(|l| {
            eyeshine::Env::new(
                Arc::new(LibRawDecoder),
                gpu.clone(),
                l.clone(),
                pounce.submitter(),
            )
        }));

        Self {
            view: View::Library,
            color: ColorManagement::new(),
            version,
            catalog_path,
            catalog,
            develop: Some(develop),
            hsl_band_selected: 0,
            auto_hint: Default::default(),
            heal_ui: HealUi::new(),
            mask_ui: {
                // The disk tier for baked AI alphas (#353).
                let mut mask_ui = MaskUi::new();
                mask_ui.service.set_larder(larder.clone());
                mask_ui
            },
            pounce,
            telemetry,
            bottleneck: None,
            import_path_input: String::new(),
            import_picker: FolderPicker::default(),
            move_picker: FolderPicker::default(),
            import_status: None,
            update,
            nine_lives,
            last_backup_poll: None,
            pending_backup_result: None,
            pending_backup_changes: None,
            last_backup_summary: None,
            move_dest_input: String::new(),
            folder_cache: folder_panel::Cache::default(),
            archive_drives,
            pending_move_result: None,
            pending_recovery_result,
            last_move_summary,
            decoder: Arc::new(LibRawDecoder),
            loupe: None,
            loupe_loaded_asset: None,
            gpu,
            export,
            knead,
            export_registry: Arc::new(builtin_registry()),
            edit_save_failed: None,
            loupe_zoomed: false,
            loupe_pan: [0.0, 0.0],
            loupe_preview: None,
            preview_color_generation: 0,
            larder: larder.clone(),
            cache_settings: CacheSettingsUi::default(),
            preview_settings,
            eyeshine,
            prebake,
            loupe_rendered: None,
            loupe_badge: Badge::None,
            loupe_render_undecodable: None,
            purge_rendered_pending: false,
            purge_error_reported: false,
            xmp,
            xmp_ui: XmpUi::default(),
            lrc_ui: LrcImportUi::default(),
            verify_ui: VerifyUi::default(),
            loupe_t2_undecodable: None,
            loupe_t0_miss: None,
            loupe_t0_fetch: None,
            grid: None,
            grid_view: grid::ViewState::default(),
            grid_root: None,
            filter_bar: FilterBar::default(),
            grid_sort: grid::DEFAULT_SORT,
            grid_was_busy: false,
            grid_last_live_reload: None,
            loupe_from_grid: false,
            cull,
            delete,
            survey: None,
            compare: None,
            tile_previews: TilePreviews::new(larder.clone()),
            cull_notice: None,
            facets_dirty_since: None,
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

    /// Folds the startup `ResumeMovesJob`'s report (#307) into `last_move_summary`, and re-derives
    /// the archived flag / thumbnail settle for the roots recovery finished committing (#72): that
    /// work rides on a live `MoveJob`, which a crashed move no longer has.
    fn poll_recovery(&mut self) {
        let Some(slot) = self.pending_recovery_result.clone() else {
            return;
        };
        let Some(resumed) = slot.lock().unwrap().take() else {
            return;
        };
        self.pending_recovery_result = None;
        if let CatalogOpenState::Open(store) = &self.catalog {
            for r in &resumed {
                if let carry::Resumed::Committed { root_id }
                | carry::Resumed::CleanedUp { root_id, .. } = r
                {
                    reconcile_tier(&**store, *root_id, &self.archive_drives);
                }
            }
        }
        self.last_move_summary = summarize_resumed(&resumed);
    }

    /// Keeps the grid's snapshot in step with catalog-changing jobs (#30), whichever view is
    /// showing: re-reads the ids every `GRID_LIVE_RELOAD_INTERVAL` while an import/sync/move runs
    /// (so new assets appear as they land), and does a full refresh -- ids *and* thumbnails, since
    /// a rescan can replace previews -- on the busy -> idle edge. Also stops thumbnail decoding
    /// when the Library view isn't the one on screen.
    fn drive_grid(&mut self, ui: &egui::Ui) {
        let busy = self.job_active(&[
            JobKind::Import,
            JobKind::Sync,
            JobKind::Move,
            JobKind::Delete,
        ]);
        let was_busy = std::mem::replace(&mut self.grid_was_busy, busy);
        if was_busy && !busy {
            // New assets may bring new keywords/makes/labels and shift every facet count.
            self.filter_bar.invalidate_options();
        }
        let Some(grid) = self.grid.as_mut() else {
            return;
        };
        if was_busy && !busy {
            grid.refresh(&self.pounce);
            // New rows can reuse the ids of photos deleted earlier; don't let them inherit a
            // stale cached marker.
            if let Some(cull) = self.cull.as_mut() {
                cull.invalidate();
            }
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

    /// Re-converts every JPEG-sourced surface when the monitor profile changes (#319): the grid's
    /// thumbnails, the survey/compare tiles and the loupe's T0/T2/rendered fallback were all
    /// converted for the previous display.
    fn sync_preview_color(&mut self) {
        let generation = self.color.generation();
        if generation == self.preview_color_generation {
            return;
        }
        self.preview_color_generation = generation;
        let color = self.color.source_transforms();
        if let Some(grid) = self.grid.as_mut() {
            grid.set_color(color.clone(), generation, &self.pounce);
        }
        self.tile_previews.set_color(color, generation);
        self.loupe_preview = None;
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
        self.submit_move_to(store, root_id, dest);
    }

    /// Shared by the typed-path controls and the folder panel's drag-and-drop (#303).
    fn submit_move_to(&mut self, store: &Arc<SqliteCatalog>, root_id: i64, dest: PathBuf) {
        if self.job_active(&[
            JobKind::Import,
            JobKind::Sync,
            JobKind::Move,
            JobKind::Delete,
            JobKind::Export,
            JobKind::Verify,
            JobKind::Baseline,
        ]) {
            self.last_move_summary =
                Some("Wait for the running import/sync/move/delete/verify to finish first.".into());
            return;
        }
        let dyn_store: Arc<dyn CatalogStore + Send + Sync> = store.clone();
        // #72: moving across the archive boundary changes where thumbnails live.
        // The stored tier (root.archived), not the current setting: the thumbnails are wherever
        // the last move/recovery left them.
        let src_archived = store
            .list_roots()
            .ok()
            .and_then(|r| r.into_iter().find(|r| r.id == root_id))
            .map(|r| r.archived);
        let dest_archived = self.archive_drives.is_archive(&dest);
        let opts = CarryOptions {
            export_sidecars: dest_archived && src_archived != Some(true),
            set_archived: (src_archived != Some(dest_archived)).then_some(dest_archived),
            ..CarryOptions::default()
        };
        let (job, result) = MoveJob::new(dyn_store, root_id, &dest, opts, now_unix());
        let larder = self.larder.clone();
        let job = job.with_archived_hook(Box::new(move |ids| {
            crate::cull::delete::purge_previews(larder.as_ref(), ids);
        }));
        self.pounce.submit(Box::new(job));
        self.pending_move_result = Some(result);
        self.last_move_summary = Some("Moving\u{2026}".into());
    }
}

/// #32: culling -- the marking keys, undo, survey/compare, and the Delete flow. The vocabulary
/// (which key does what) is `crate::cull::keys`; this is the wiring between it and the views.
impl PeltApp {
    /// Folds culling worker replies and delete progress into the UI. Once per frame.
    fn poll_cull(&mut self) {
        if let Some(cull) = self.cull.as_mut() {
            cull.poll();
        }
        // Marking changes what the filter bar's counts say without changing the filter. Once it
        // goes quiet, recompute them -- not per keypress, which at 1M photos would be a facet
        // scan for every mark.
        if facets_refresh_due(self.facets_dirty_since, Instant::now()) {
            self.facets_dirty_since = None;
            self.filter_bar.invalidate_facets();
        }
        let poll = self.delete.poll();
        if !poll.removed.is_empty() {
            self.after_photos_removed(&poll.removed);
        }
    }

    /// Photos left the catalog: forget their markers/previews, and drop any session that listed
    /// them (the loupe's id list is frozen at open time, so it would still walk onto them).
    fn after_photos_removed(&mut self, gone: &[i64]) {
        if let Some(cull) = self.cull.as_mut() {
            cull.forget(gone);
        }
        for id in gone {
            self.tile_previews.forget(*id);
        }
        if self.loupe.as_ref().is_some_and(|l| l.contains_any(gone)) {
            if let Some(mut old) = self.loupe.take() {
                old.cancel_all(&self.pounce);
            }
            self.loupe_preview = None;
            if self.view == View::Loupe {
                self.view = View::Library;
            }
        }
        if self.survey.as_mut().is_some_and(|s| !s.remove(gone)) {
            self.survey = None;
        }
        if self.compare.as_mut().is_some_and(|c| !c.remove(gone)) {
            self.compare = None;
        }
        self.leave_tiles_if_gone();
    }

    /// Back to the Library if the survey/compare being shown no longer exists.
    fn leave_tiles_if_gone(&mut self) {
        if (self.view == View::Survey && self.survey.is_none())
            || (self.view == View::Compare && self.compare.is_none())
        {
            self.view = View::Library;
        }
        if self.survey.is_none() && self.compare.is_none() {
            self.tile_previews.clear(&self.pounce);
        }
    }

    /// Reads this frame's culling keys and acts on them. Off while the Develop view is showing
    /// (its sliders own the digit keys) and while the delete prompt is up.
    fn handle_cull_keys(&mut self, ctx: &egui::Context) {
        if self.cull.is_none()
            || !cull_keys_active(self.view, self.delete.is_confirming())
            || self.knead.is_asking()
        {
            return;
        }
        for command in crate::cull::input::poll(ctx) {
            match command {
                KeyCommand::Mark {
                    action,
                    invert_advance,
                } => self.apply_mark(action, invert_advance),
                KeyCommand::Undo => {
                    if let Some(cull) = &self.cull {
                        cull.undo();
                        self.facets_dirty_since = Some(Instant::now());
                    }
                }
                KeyCommand::Redo => {
                    if let Some(cull) = &self.cull {
                        cull.redo();
                        self.facets_dirty_since = Some(Instant::now());
                    }
                }
                KeyCommand::Delete => self.request_delete(),
                KeyCommand::Survey => self.open_survey(),
                KeyCommand::Compare => self.open_compare(),
            }
        }
    }

    /// The photos a marking key or Delete acts on in the current view.
    fn mark_targets(&self) -> Vec<i64> {
        match self.view {
            View::Library => self
                .grid
                .as_ref()
                .map(GridSession::target_ids)
                .unwrap_or_default(),
            View::Loupe => self
                .loupe
                .as_ref()
                .and_then(LoupeSession::current_asset_id)
                .into_iter()
                .collect(),
            View::Survey => self
                .survey
                .as_ref()
                .and_then(SurveySession::active_id)
                .into_iter()
                .collect(),
            View::Compare => self
                .compare
                .as_ref()
                .map(|c| c.active_id())
                .into_iter()
                .collect(),
            View::Develop => Vec::new(),
        }
    }

    /// Marks the current targets and, for a single photo with auto-advance on (Shift flips it for
    /// this press), moves on to the next.
    fn apply_mark(&mut self, action: CullAction, invert_advance: bool) {
        let ids = self.mark_targets();
        if ids.is_empty() {
            return;
        }
        self.facets_dirty_since = Some(Instant::now());
        let auto = self.cull.as_ref().is_some_and(|c| c.auto_advance);
        let advance = crate::cull::should_advance(auto, invert_advance, ids.len());
        if let Some(cull) = self.cull.as_mut() {
            cull.mark(ids, action);
        }
        self.cull_notice = None;
        if !advance {
            return;
        }
        match self.view {
            View::Library => {
                if let Some(grid) = self.grid.as_mut() {
                    if grid.advance_after_mark() {
                        self.grid_view.reveal_cursor();
                    }
                }
            }
            View::Loupe => {
                let CatalogOpenState::Open(store) = &self.catalog else {
                    return;
                };
                if let Some(loupe) = self.loupe.as_mut() {
                    if loupe.cursor() + 1 < loupe.len() {
                        let _ = loupe.set_cursor(loupe.cursor() + 1, store.as_ref(), &self.pounce);
                        self.loupe_zoomed = false;
                        self.loupe_pan = [0.0, 0.0];
                    }
                }
            }
            View::Compare => {
                if let Some(compare) = self.compare.as_mut() {
                    if compare.active == Side::Candidate {
                        compare.advance_candidate();
                    }
                }
            }
            View::Survey | View::Develop => {}
        }
    }

    /// Opens the delete prompt for the current targets.
    fn request_delete(&mut self) {
        let ids = self.mark_targets();
        let scope = match self.view {
            View::Library => match self.grid.as_ref() {
                Some(g) if g.has_selection() => format!("the {} selected photos", ids.len()),
                _ => "the photo under the cursor".to_string(),
            },
            View::Loupe => "the photo in the loupe".to_string(),
            View::Survey | View::Compare => "the photo picked in this view".to_string(),
            View::Develop => return,
        };
        if self.job_active(&[
            JobKind::Import,
            JobKind::Sync,
            JobKind::Move,
            JobKind::Delete,
            JobKind::Export,
            JobKind::Verify,
            JobKind::Baseline,
        ]) {
            self.delete.last_summary = Some(
                "Wait for the running import, sync, move, delete or verify to finish first.".into(),
            );
            return;
        }
        self.delete.request(ids, scope);
    }

    /// Draws the delete prompt and, once a mode is chosen, starts the job.
    fn show_delete_modal(&mut self, ctx: &egui::Context) {
        let Some((request, mode)) = self.delete.show_modal(ctx) else {
            return;
        };
        let CatalogOpenState::Open(store) = &self.catalog else {
            return;
        };
        if self.job_active(&[
            JobKind::Import,
            JobKind::Sync,
            JobKind::Move,
            JobKind::Delete,
            JobKind::Export,
            JobKind::Verify,
            JobKind::Baseline,
        ]) {
            self.delete.last_summary = Some(
                "Wait for the running import, sync, move, delete or verify to finish first.".into(),
            );
            return;
        }
        let dyn_store: Arc<dyn CatalogStore + Send + Sync> = store.clone();
        self.delete.submit(
            dyn_store,
            self.larder.clone(),
            mode,
            request.ids,
            &self.pounce,
        );
    }

    /// `N`: survey the Library's multi-selection (or, from a compare, its photos).
    fn open_survey(&mut self) {
        let ids = match self.view {
            View::Library => self
                .grid
                .as_ref()
                .filter(|g| g.has_selection())
                .map(GridSession::target_ids)
                .unwrap_or_default(),
            View::Compare => self
                .compare
                .as_ref()
                .map(|c| c.ids().to_vec())
                .unwrap_or_default(),
            _ => return,
        };
        match SurveySession::new(&ids) {
            Some(session) => {
                self.survey = Some(session);
                self.view = View::Survey;
                self.cull_notice = None;
            }
            None => {
                self.cull_notice = Some(
                    "Select two or more photos (Ctrl-click, Shift-click or Ctrl+A) to survey them."
                        .into(),
                );
            }
        }
    }

    /// `C`: compare the Library's selection, or -- with fewer than two selected -- the photo under
    /// the cursor against the ones after it, in the grid's own order.
    fn open_compare(&mut self) {
        let ids: Vec<i64> = match self.view {
            View::Library => match self.grid.as_ref() {
                Some(g) if g.target_count() >= 2 && g.has_selection() => g.target_ids(),
                Some(g) => match g.cursor() {
                    Some(c) => g.ids()[c..(c + COMPARE_FROM_CURSOR).min(g.len())].to_vec(),
                    None => Vec::new(),
                },
                None => Vec::new(),
            },
            View::Survey => match self.survey.as_ref() {
                Some(s) => {
                    // The active tile becomes the select; the rest follow in order.
                    let mut ids = s.ids().to_vec();
                    ids.rotate_left(s.active());
                    ids
                }
                None => Vec::new(),
            },
            _ => return,
        };
        match CompareSession::new(ids) {
            Some(session) => {
                self.compare = Some(session);
                self.view = View::Compare;
                self.cull_notice = None;
            }
            None => {
                self.cull_notice =
                    Some("Put the cursor on a photo that has another after it to compare.".into());
            }
        }
    }

    fn show_survey(&mut self, ui: &mut egui::Ui) {
        let (CatalogOpenState::Open(store), Some(session), Some(cull)) =
            (&self.catalog, self.survey.as_mut(), self.cull.as_mut())
        else {
            self.view = View::Library;
            return;
        };
        let store = store.clone();
        let outcome = cull_survey::show(
            ui,
            session,
            cull,
            &mut self.tile_previews,
            store.as_ref(),
            &self.pounce,
        );
        let open = outcome.open.map(|id| {
            (
                session.ids().to_vec(),
                session.ids().iter().position(|i| *i == id),
            )
        });
        if outcome.exit {
            self.survey = None;
            self.view = if self.compare.is_some() {
                View::Compare
            } else {
                View::Library
            };
            self.leave_tiles_if_gone();
        }
        if let Some((ids, Some(index))) = open {
            self.start_loupe(&store, ids, index, false);
        }
    }

    fn show_compare(&mut self, ui: &mut egui::Ui) {
        let (CatalogOpenState::Open(store), Some(session), Some(cull)) =
            (&self.catalog, self.compare.as_mut(), self.cull.as_mut())
        else {
            self.view = View::Library;
            return;
        };
        let store = store.clone();
        let outcome = cull_compare::show(
            ui,
            session,
            cull,
            &mut self.tile_previews,
            store.as_ref(),
            &self.pounce,
        );
        if outcome.exit {
            self.compare = None;
            self.view = if self.survey.is_some() {
                View::Survey
            } else {
                View::Library
            };
            self.leave_tiles_if_gone();
        }
    }
}

/// How long marking must be quiet before the filter bar's facet counts are recomputed.
const FACET_REFRESH_AFTER: Duration = Duration::from_millis(1500);

/// Whether stale facet counts are due for a refresh: marking happened, and has been quiet for
/// [`FACET_REFRESH_AFTER`]. Pure, so the debounce is testable.
fn facets_refresh_due(dirty_since: Option<Instant>, now: Instant) -> bool {
    dirty_since.is_some_and(|t| now.saturating_duration_since(t) >= FACET_REFRESH_AFTER)
}

/// Whether the culling keys are live: in the Library, Loupe, Survey and Compare views, and not
/// while the delete prompt is up. Not in Develop, whose sliders own the digit keys.
fn cull_keys_active(view: View, delete_prompt_open: bool) -> bool {
    !delete_prompt_open && view != View::Develop
}

/// Photos `C` puts into a comparison when only the cursor's photo is selected: it plus the ones
/// that follow, enough to walk a whole burst.
const COMPARE_FROM_CURSOR: usize = 200;

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

/// Re-derives `root_id`'s archived flag from the archive-drive setting and settles its
/// thumbnails to match (#72). Best effort and idempotent.
fn reconcile_tier(
    store: &dyn CatalogStore,
    root_id: i64,
    archive_drives: &crate::archive_drives::ArchiveDrives,
) {
    let Some(root) = store
        .list_roots()
        .ok()
        .and_then(|r| r.into_iter().find(|r| r.id == root_id))
    else {
        return;
    };
    let want = archive_drives.is_archive(Path::new(&root.path));
    if want != root.archived && store.set_root_archived(root_id, want).is_err() {
        return;
    }
    let _ = nicti_lair::tier::settle_root(store, root_id);
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
    let leftovers: u64 = resumed
        .iter()
        .map(|r| match r {
            Resumed::CleanedUp { leftover_count, .. } => *leftover_count,
            _ => 0,
        })
        .sum();
    let mut msg = if stuck.is_empty() {
        format!("Recovered {} interrupted folder move(s).", resumed.len())
    } else {
        format!(
            "{} interrupted folder move(s) need attention: {}",
            stuck.len(),
            stuck.join("; ")
        )
    };
    if leftovers > 0 {
        msg.push_str(&format!(
            " {leftovers} file(s) couldn't be removed from the original location."
        ));
    }
    Some(msg)
}

impl eframe::App for PeltApp {
    fn on_exit(&mut self) {
        self.export.cancel();
        self.save_develop_edits(true);
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.color.handle_shortcuts(ui.ctx());
        self.color.sync(frame);
        self.sync_preview_color();
        self.poll_backup();
        self.poll_move();
        self.poll_recovery();
        self.poll_cull();
        self.poll_eyeshine(ui.ctx());
        self.poll_prebake(ui.ctx());
        self.export.poll();
        if self.export.is_running() {
            // Progress text; nothing else repaints an otherwise idle window.
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(250));
        }
        if !ui.ctx().egui_wants_keyboard_input()
            && ui.ctx().input_mut(|i| {
                i.consume_shortcut(&egui::KeyboardShortcut::new(
                    egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                    egui::Key::E,
                ))
            })
        {
            self.request_export();
        }
        // Ctrl+Shift+C / V / S: copy, paste and sync develop settings (#52). Plain `C` is Compare.
        if !ui.ctx().egui_wants_keyboard_input() {
            let chord = |key| {
                egui::KeyboardShortcut::new(egui::Modifiers::COMMAND | egui::Modifiers::SHIFT, key)
            };
            let action = ui.ctx().input_mut(|i| {
                if i.consume_shortcut(&chord(egui::Key::C)) {
                    Some(PanelAction::Copy)
                } else if i.consume_shortcut(&chord(egui::Key::V)) {
                    Some(PanelAction::Paste)
                } else if i.consume_shortcut(&chord(egui::Key::S)) {
                    Some(PanelAction::Sync)
                } else {
                    None
                }
            });
            if let Some(action) = action {
                self.handle_knead_action(action);
            }
        }
        // Autosave Develop's edits once the pointer is up (not on every slider-drag frame).
        if !ui.ctx().input(|i| i.pointer.any_down()) {
            self.save_develop_edits(false);
        }
        self.handle_cull_keys(ui.ctx());
        if self.facets_dirty_since.is_some() {
            // Nothing else repaints an otherwise idle window once marking stops.
            ui.ctx().request_repaint_after(FACET_REFRESH_AFTER);
        }
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
                if self.survey.is_some() {
                    ui.selectable_value(&mut self.view, View::Survey, "Survey");
                }
                if self.compare.is_some() {
                    ui.selectable_value(&mut self.view, View::Compare, "Compare");
                }
                ui.separator();
                if let Some(cull) = self.cull.as_mut() {
                    ui.checkbox(&mut cull.auto_advance, "Auto-advance")
                        .on_hover_text(
                            "After marking a photo, move to the next one. Hold Shift while \
                             marking to do the opposite for that one press.",
                        );
                    if let Some(err) = cull.last_error().map(str::to_string) {
                        ui.colored_label(egui::Color32::RED, err);
                        if ui.small_button("Dismiss").clicked() {
                            cull.clear_error();
                        }
                    }
                    ui.separator();
                }
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
        if self.export.has_status() {
            egui::Panel::top("export_status").show(ui, |ui| self.export.show_status(ui));
        }
        if self.knead.has_status() {
            let mut command = None;
            egui::Panel::top("knead_status").show(ui, |ui| command = self.knead.show_status(ui));
            if let Some(command) = command {
                self.run_knead_command(command);
            }
        }
        if let Some((_, err)) = &self.edit_save_failed {
            egui::Panel::top("edit_save_error").show(ui, |ui| {
                ui.colored_label(egui::Color32::RED, format!("Couldn't save edits: {err}"));
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
        if let Some(d) = self.develop.as_mut() {
            // The heal tool works on the whole, uncropped image (see `heal_tool`'s docs). Set on
            // every frame and tied to the view: the Loupe renders the same `DevelopView`, and a
            // flag left over from Develop would show it without its crop and straighten.
            if self.view == View::Develop {
                crate::heal_tool::handle_tool_keys(ui, &mut self.heal_ui);
            }
            d.uncropped_preview = self.view == View::Develop && self.heal_ui.shows_uncropped();
            if self.view == View::Develop {
                crate::heal_tool::poll(ui, d, &mut self.heal_ui);
                // Files this photo's baked alphas in the disk tier (#353).
                self.mask_ui
                    .service
                    .set_open_asset(self.loupe_loaded_asset.map(|(id, _)| id));
                crate::mask_panel::poll(ui, d, &self.pounce, &mut self.mask_ui);
            }
        }
        let panel_frame = if self.view == View::Develop {
            self.develop.as_mut().map(|d| d.render())
        } else {
            None
        };

        if let (View::Develop, Some(frame)) = (self.view, &panel_frame) {
            let mut knead_action = None;
            egui::Panel::left("presets_panel")
                .resizable(true)
                .show(ui, |ui| knead_action = self.knead.show_panel(ui));
            if let Some(action) = knead_action {
                self.handle_knead_action(action);
            }
            egui::Panel::right("develop_panel")
                .min_size(280.0)
                .show(ui, |ui| {
                    if let Some(develop) = self.develop.as_mut() {
                        crate::develop_panel::show(
                            ui,
                            &mut develop.doc,
                            Some((&mut develop.engine, frame)),
                            &mut self.hsl_band_selected,
                            &mut self.auto_hint,
                            &mut self.heal_ui,
                            &mut self.mask_ui,
                            &self.pounce,
                        );
                    }
                });
        }

        // The panel above may have switched tool this frame: re-derive the flag so the frame we
        // paint (and hit-test against) matches the tool that will handle it, not last frame's.
        if let Some(d) = self.develop.as_mut() {
            d.uncropped_preview = self.view == View::Develop && self.heal_ui.shows_uncropped();
        }
        let viewport_frame = if self.view == View::Develop {
            self.develop.as_mut().map(|d| d.render())
        } else {
            None
        };

        egui::CentralPanel::default().show(ui, |ui| match self.view {
            View::Library => self.show_library(ui),
            View::Loupe => self.show_loupe(ui),
            View::Survey => self.show_survey(ui),
            View::Compare => self.show_compare(ui),
            View::Develop => {
                ui.heading("Develop");
                if let Some(frame) = viewport_frame {
                    let available = ui.available_size();
                    // Only the heal and mask tools need clicks. Sensing them makes egui report
                    // `drag_started` after the pointer has crossed its drag threshold, which would
                    // offset the crop tool's handle hit tests and lag every crop/rotate/pan drag.
                    let sense = if self.heal_ui.heal_active()
                        || self.heal_ui.mask_active()
                        || self.heal_ui.point_color_active()
                    {
                        egui::Sense::click_and_drag()
                    } else {
                        egui::Sense::drag()
                    };
                    // The frame is the crop rect while idle and the whole image under a tool (#272):
                    // letterbox it at its own aspect ratio, and give every tool that fitted rect.
                    let (area, _) = ui.allocate_exact_size(available, egui::Sense::hover());
                    let rect = fit_rect(
                        area,
                        (frame.extent.width as f32, frame.extent.height as f32),
                    );
                    let response = ui.interact(rect, ui.id().with("develop_viewport"), sense);
                    ui.painter().add(egui_wgpu::Callback::new_paint_callback(
                        rect,
                        ViewportCallback::identity(frame),
                    ));
                    if let Some(develop) = self.develop.as_mut() {
                        if self.heal_ui.heal_active() {
                            crate::heal_tool::handle_viewport(
                                ui,
                                &response,
                                rect,
                                develop,
                                &mut self.heal_ui,
                                &self.pounce,
                            );
                        } else if self.heal_ui.mask_active() {
                            crate::mask_panel::handle_viewport(
                                ui,
                                &response,
                                rect,
                                develop,
                                &mut self.mask_ui,
                            );
                        } else if self.heal_ui.point_color_active() {
                            crate::catseye::handle_viewport(
                                ui,
                                &response,
                                rect,
                                develop,
                                &mut self.heal_ui,
                            );
                        } else if self.heal_ui.crop_active() {
                            crate::develop_panel::handle_viewport_gesture(
                                ui, &response, rect, develop,
                            );
                        }
                    }
                }
            }
        });

        self.show_delete_modal(ui.ctx());
        self.show_export_dialog(ui.ctx());
        self.show_knead_modal(ui.ctx());
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
            let mut grid = GridSession::new(dyn_store, PLACEHOLDER_GRID_TEXTURE_BUDGET_BYTES);
            // Born after the display profile was resolved: start on the current conversion, not
            // the sRGB default `sync_preview_color` already moved past.
            grid.set_color(
                self.color.source_transforms(),
                self.color.generation(),
                &self.pounce,
            );
            self.grid = Some(grid);
        }

        self.show_folder_panel(ui, &store);

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
            // Never read "All folders" while a root filter is live (a loaded smart collection can
            // name a folder that's no longer registered -- that matches nothing).
            let root_label = match root_sel {
                None => "All folders".to_string(),
                Some(id) => roots
                    .iter()
                    .find(|r| r.id == id)
                    .map_or_else(|| format!("Missing folder (#{id})"), |r| r.path.clone()),
            };
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
        // #242: the filter bar. Loading a smart collection may retarget the folder selection.
        let header = if self.filter_bar.is_active() {
            "Filters (active)"
        } else {
            "Filters"
        };
        let dyn_store: Arc<dyn CatalogStore + Send + Sync> = store.clone();
        egui::CollapsingHeader::new(header)
            .id_salt("nicti_pelt_library_filters")
            .default_open(true)
            .show(ui, |ui| {
                self.filter_bar.show(ui, &dyn_store, &mut self.grid_root);
            });
        // Read after the header, and even while it's collapsed: a collapsed bar still filters.
        let filter = self.filter_bar.to_filter(self.grid_root);
        if let Some(grid) = self.grid.as_mut() {
            grid.set_query(filter, sort, &self.pounce);
        }

        // Selection and what to do with it.
        let (targets, has_selection, total) = self.grid.as_ref().map_or((0, false, 0), |g| {
            (g.target_count(), g.has_selection(), g.len())
        });
        let busy = self.job_active(&[
            JobKind::Import,
            JobKind::Sync,
            JobKind::Move,
            JobKind::Delete,
            JobKind::Export,
        ]);
        let (mut select_all, mut delete, mut survey, mut compare, mut export) =
            (false, false, false, false, false);
        let mut knead_action: Option<PanelAction> = None;
        ui.horizontal(|ui| {
            if ui
                .add_enabled(total > 0, egui::Button::new("Select all"))
                .on_hover_text("Ctrl+A. Every photo matching the current filter.")
                .clicked()
            {
                select_all = true;
            }
            if has_selection {
                ui.label(format!("{targets} selected"));
            }
            if ui
                .add_enabled(
                    targets > 0 && !busy && !self.delete.is_running(),
                    egui::Button::new(format!("Delete {targets}\u{2026}")),
                )
                .on_hover_text("Delete / Backspace. Asks whether to use the Recycle Bin.")
                .clicked()
            {
                delete = true;
            }
            if ui
                .add_enabled(
                    targets >= 2 && has_selection,
                    egui::Button::new("Survey (N)"),
                )
                .clicked()
            {
                survey = true;
            }
            if ui
                .add_enabled(total >= 2, egui::Button::new("Compare (C)"))
                .clicked()
            {
                compare = true;
            }
            if ui
                .add_enabled(
                    targets > 0 && !busy && !self.export.is_running(),
                    egui::Button::new(format!("Export {targets}\u{2026}")),
                )
                .on_hover_text(
                    "Ctrl+Shift+E. Render these photos with their edits to JPEG, PNG or TIFF.",
                )
                .clicked()
            {
                export = true;
            }
            let can_edit = targets > 0 && !busy && !self.knead.is_asking();
            if ui
                .add_enabled(total > 0, egui::Button::new("Copy settings\u{2026}"))
                .on_hover_text("Ctrl+Shift+C. Copy the cursor photo's develop settings.")
                .clicked()
            {
                knead_action = Some(PanelAction::Copy);
            }
            if ui
                .add_enabled(
                    can_edit && self.knead.has_clipboard(),
                    egui::Button::new(format!("Paste settings {targets}")),
                )
                .on_hover_text("Ctrl+Shift+V. Paste the copied settings onto the selection.")
                .clicked()
            {
                knead_action = Some(PanelAction::Paste);
            }
            if ui
                .add_enabled(
                    can_edit && targets >= 2,
                    egui::Button::new(format!("Sync settings {targets}\u{2026}")),
                )
                .on_hover_text(
                    "Ctrl+Shift+S. Copy the cursor photo's settings onto the rest of the selection.",
                )
                .clicked()
            {
                knead_action = Some(PanelAction::Sync);
            }
            if let Some(picked) = self.knead.preset_menu(ui, can_edit) {
                knead_action = Some(picked);
            }
        });
        if let Some(action) = knead_action {
            self.handle_knead_action(action);
        }
        if select_all {
            if let Some(grid) = self.grid.as_mut() {
                grid.select_all();
            }
        }
        if delete {
            self.request_delete();
        }
        if survey {
            self.open_survey();
        }
        if compare {
            self.open_compare();
        }
        if export {
            self.request_export();
        }
        if let Some(notice) = &self.cull_notice {
            ui.colored_label(egui::Color32::YELLOW, notice);
        }
        if let Some(summary) = &self.delete.last_summary {
            ui.label(summary);
        }

        // Polled outside the collapsing header so a pick made while it is collapsed lands now,
        // not whenever the header is next opened.
        if let Some(chosen) = self.import_picker.poll() {
            self.import_path_input = chosen.to_string_lossy().into_owned();
            self.import_status = None;
        }
        if let Some(chosen) = self.move_picker.poll() {
            self.move_dest_input = chosen.to_string_lossy().into_owned();
        }
        egui::CollapsingHeader::new("Folders: import, sync, move")
            .id_salt("nicti_pelt_library_folders")
            .default_open(true)
            .show(ui, |ui| self.show_library_controls(ui));
        ui.separator();

        let outcome = match (self.grid.as_mut(), self.cull.as_mut()) {
            (Some(grid), Some(cull)) => grid::view::show(
                ui,
                grid,
                &mut self.grid_view,
                cull,
                &self.pounce,
                self.preview_settings.policy_for(Surface::Grid) != eyeshine::RenderPolicy::Off,
            ),
            _ => grid::view::GridOutcome::default(),
        };
        if let Some(index) = outcome.open {
            self.open_from_grid(&store, index);
        }
    }

    /// #303: the left-hand folder/drive tree; a drop on a drive or folder starts a verified move.
    fn show_folder_panel(&mut self, ui: &mut egui::Ui, store: &Arc<SqliteCatalog>) {
        // A delete is busy work too: dragging a folder to another drive while its photos are
        // being deleted would race the two.
        let moving = self.job_active(&[
            JobKind::Import,
            JobKind::Sync,
            JobKind::Move,
            JobKind::Delete,
            JobKind::Export,
            JobKind::Verify,
            JobKind::Baseline,
        ]);
        let move_running = self.job_active(&[JobKind::Move]);
        // Blocked by anything other than our own verify/baseline job, decided from the live job
        // list (not `verify_ui.running()`, which lags a frame behind `poll`).
        let other_busy = self.job_active(&[
            JobKind::Import,
            JobKind::Sync,
            JobKind::Move,
            JobKind::Delete,
            JobKind::Export,
        ]);
        let blocked = other_busy
            .then_some("An import, sync, move, delete or export is running; wait for it.");
        // Re-read on a busy edge or the cache's own cadence -- never per frame.
        // Nothing else repaints an idle window, so wake up when the cache goes stale.
        ui.ctx().request_repaint_after(folder_panel::CACHE_TTL);
        self.folder_cache.refresh(moving, || {
            (
                store.list_roots().unwrap_or_default(),
                store.open_root_moves().unwrap_or_default(),
            )
        });
        let tree = folder_panel::build_tree(&self.folder_cache.roots, &self.folder_cache.drives);
        let attention = folder_panel::attention_lines(&self.folder_cache.open_moves, move_running);
        let mut out = folder_panel::PanelOutput::default();
        let mut set_archive = None;
        let selected_root = self.grid_root;
        egui::Panel::left("folder_panel")
            .resizable(true)
            .show(ui, |ui| {
                ui.heading("Folders");
                egui::ScrollArea::vertical().show(ui, |ui| {
                    out = folder_panel::show(
                        ui,
                        &tree,
                        &attention,
                        self.last_move_summary.as_deref(),
                        moving,
                        &self.archive_drives,
                        &mut set_archive,
                        selected_root,
                        !moving,
                    );
                    let dyn_store: Arc<dyn CatalogStore + Send + Sync> = store.clone();
                    verify_ui::show(ui, &mut self.verify_ui, &dyn_store, &self.pounce, blocked);
                });
            });
        if let Some((drive, archive)) = set_archive {
            if let Err(e) = self.archive_drives.set(&drive, archive) {
                self.last_move_summary =
                    Some(format!("Couldn't save the archive-drive setting: {e}"));
            }
        }
        if let Some(sel) = out.select_root {
            self.grid_root = sel;
        }
        if let Some(drive) = out.import_from {
            // Never clobber what the user already typed.
            if self.import_path_input.trim().is_empty() {
                self.import_path_input = drive;
                self.import_status = None;
            } else {
                self.import_status =
                    Some("The Folder field already has a path -- clear it first.".to_string());
            }
        }
        if let Some(r) = out.drop {
            self.submit_move_to(store, r.root_id, r.dest_parent);
        }
        if let Some(root_id) = out.verify {
            self.submit_verify(store, root_id);
        }
    }

    /// #386: "Verify folder" from the folder panel's context menu. Refused while another job that
    /// touches the folder's files is running -- a move re-points the root and a delete removes
    /// files, either of which would turn into false `missing`s.
    fn submit_verify(&mut self, store: &Arc<SqliteCatalog>, root_id: i64) {
        if self.job_active(&[
            JobKind::Import,
            JobKind::Sync,
            JobKind::Move,
            JobKind::Delete,
            JobKind::Export,
            JobKind::Verify,
            JobKind::Baseline,
        ]) {
            self.verify_ui
                .refuse("Wait for the running import/sync/move/delete/verify to finish first.");
            return;
        }
        let Some(root) = self.folder_cache.roots.iter().find(|r| r.id == root_id) else {
            self.verify_ui
                .refuse("That folder is no longer registered.");
            return;
        };
        let dyn_store: Arc<dyn CatalogStore + Send + Sync> = store.clone();
        let path = PathBuf::from(&root.path);
        self.verify_ui
            .submit_verify(&dyn_store, &self.pounce, root_id, path);
    }

    /// The import/sync/move controls and their status lines -- everything the Library view had
    /// before the grid (#30) except the asset count, which the grid's own snapshot now supplies.
    fn show_library_controls(&mut self, ui: &mut egui::Ui) {
        match &self.catalog {
            CatalogOpenState::Open(store) => {
                let store = store.clone();
                ui.horizontal(|ui| {
                    ui.label("Folder:");
                    // Read-only while Browse is open, so a pick can't clobber a newer edit.
                    if ui
                        .add_enabled(
                            !self.import_picker.is_open(),
                            egui::TextEdit::singleline(&mut self.import_path_input),
                        )
                        .changed()
                    {
                        self.import_status = None;
                    }
                    if ui
                        .add_enabled(
                            !self.import_picker.is_open(),
                            egui::Button::new("Browse..."),
                        )
                        .clicked()
                    {
                        self.import_picker.open(
                            ui.ctx(),
                            "Choose a folder",
                            &self.import_path_input,
                        );
                    }
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
                if let Some(msg) = &self.import_status {
                    ui.colored_label(egui::Color32::RED, msg);
                }

                // #26: verified folder move. Copies and hash-verifies every file, re-points the
                // catalog, then removes the original -- edits/ratings/keywords follow the folder.
                ui.separator();
                ui.label(
                    "Move a folder to another drive (verified copy, then the original is removed):",
                );
                ui.horizontal(|ui| {
                    ui.label("Move into:");
                    ui.add_enabled(
                        !self.move_picker.is_open(),
                        egui::TextEdit::singleline(&mut self.move_dest_input),
                    );
                    if ui
                        .add_enabled(!self.move_picker.is_open(), egui::Button::new("Browse..."))
                        .clicked()
                    {
                        self.move_picker
                            .open(ui.ctx(), "Move into folder", &self.move_dest_input);
                    }
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
        ui.separator();
        cache_settings::show(
            ui,
            &mut self.cache_settings,
            self.larder.as_ref(),
            &self.catalog_path,
            &self.pounce,
        );
        if preview_settings::show(ui, &mut self.preview_settings) {
            self.apply_preview_settings();
        }
        ui.separator();
        if let (CatalogOpenState::Open(store), Some(xmp)) = (&self.catalog, &self.xmp) {
            let dyn_store: Arc<dyn CatalogStore + Send + Sync> = store.clone();
            xmp_sync::show(ui, &mut self.xmp_ui, &dyn_store, xmp, &self.catalog_path);
            ui.separator();
        }
        if let CatalogOpenState::Open(store) = &self.catalog {
            let dyn_store: Arc<dyn CatalogStore + Send + Sync> = store.clone();
            let blocked = self
                .job_active(&[
                    JobKind::Import,
                    JobKind::Sync,
                    JobKind::Move,
                    JobKind::Delete,
                    JobKind::Export,
                    JobKind::Verify,
                    JobKind::Baseline,
                ])
                .then_some(
                    "An import, sync, move, delete, export or verify is running; wait for it.",
                );
            lrc_import::show(ui, &mut self.lrc_ui, &dyn_store, &self.pounce, blocked);
            ui.separator();
        }
        if let Some(summary) = &self.last_backup_summary {
            ui.label(summary);
        }
        if let Some(summary) = &self.last_move_summary {
            ui.label(summary);
        }
    }

    /// Validates the Folder field (#367): a trimmed, non-blank path; with `must_exist`, also an
    /// existing directory (Import/Sync read the disk; Open in Loupe is catalog-only, so an
    /// unplugged drive's cached assets must still open). On failure sets `import_status` and returns `None`, so a click is never silent.
    fn checked_import_path(&mut self, must_exist: bool) -> Option<PathBuf> {
        match validate_folder_input(&self.import_path_input, must_exist) {
            Ok(path) => {
                self.import_status = None;
                Some(path)
            }
            Err(msg) => {
                self.import_status = Some(msg);
                None
            }
        }
    }

    /// Registers `self.import_path_input` as a root under the placeholder volume (see this
    /// module's own `PLACEHOLDER_VOLUME_IDENTITY_KEY` doc comment) and submits an
    /// `IngestJob`/`SyncJob` for it to Pounce's CPU lane. A blank/invalid path or a registration
    /// failure is reported on the `import_status` line under the Folder field.
    fn submit_root_job(&mut self, store: &Arc<SqliteCatalog>, action: RootAction) {
        let Some(path) = self.checked_import_path(true) else {
            return;
        };
        if self.job_active(&[
            JobKind::Move,
            JobKind::Delete,
            JobKind::Export,
            JobKind::Verify,
            JobKind::Baseline,
        ]) {
            self.last_move_summary = Some(
                "A folder move, delete or verify is running; import/sync waits until it finishes."
                    .into(),
            );
            return;
        }
        let root_id = match register_root(store.as_ref(), &path) {
            Ok(id) => id,
            Err(e) => {
                self.import_status = Some(format!("Couldn't register {}: {e}", path.display()));
                return;
            }
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
    /// blank/invalid path or a registration/listing failure is reported on `import_status`.
    fn open_in_loupe(&mut self, store: &Arc<SqliteCatalog>) {
        let Some(path) = self.checked_import_path(false) else {
            return;
        };
        if self.job_active(&[JobKind::Move, JobKind::Delete, JobKind::Export]) {
            self.last_move_summary =
                Some("A folder move or delete is running; wait for it to finish first.".into());
            return;
        }
        // Catalog-only, so an already-registered root on an unplugged drive still opens -- but an
        // unregistered path must be a real folder, or a typo would register a junk root.
        let canonical = nicti_lair::scruff::canonical_root_string(&path);
        let registered = match store.list_roots() {
            Ok(roots) => roots.iter().any(|r| r.path == canonical),
            Err(e) => {
                self.import_status = Some(format!("Couldn't read the folder list: {e}"));
                return;
            }
        };
        if !registered {
            if let Err(msg) = check_is_dir(&path) {
                self.import_status = Some(msg);
                return;
            }
        }
        let root_id = match register_root(store.as_ref(), &path) {
            Ok(id) => id,
            Err(e) => {
                self.import_status = Some(format!("Couldn't register {}: {e}", path.display()));
                return;
            }
        };
        let assets = match store.list_assets_by_root(root_id) {
            Ok(a) => a,
            Err(e) => {
                self.import_status = Some(format!("Couldn't list {}: {e}", path.display()));
                return;
            }
        };
        let ids: Vec<i64> = assets.iter().map(|a| a.id).collect();
        self.start_loupe(store, ids, 0, false);
    }

    /// Opens the loupe on the grid's current ordering (#30), starting at `index` -- so Left/Right
    /// in the loupe walk the same sequence the grid shows, in its sort and filter.
    fn open_from_grid(&mut self, store: &Arc<SqliteCatalog>, index: usize) {
        if self.job_active(&[JobKind::Move, JobKind::Delete, JobKind::Export]) {
            self.last_move_summary =
                Some("A folder move or delete is running; wait for it to finish first.".into());
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

    /// The photos an export acts on in the current view. Unlike `mark_targets`, Develop counts:
    /// it exports the photo it has loaded.
    fn export_targets(&self) -> Vec<i64> {
        match self.view {
            View::Develop => self
                .loupe_loaded_asset
                .map(|(id, _)| id)
                .into_iter()
                .collect(),
            _ => self.mark_targets(),
        }
    }

    /// Opens the Export dialog for the current view's photos (#57).
    fn request_export(&mut self) {
        if self.export.is_running() {
            return;
        }
        if self.job_active(&[
            JobKind::Import,
            JobKind::Sync,
            JobKind::Move,
            JobKind::Delete,
            JobKind::Export,
        ]) || self.delete.is_confirming()
        {
            self.cull_notice = Some(
                "Wait for the running import, sync, move or delete to finish before exporting."
                    .into(),
            );
            return;
        }
        let ids = self.export_targets();
        if ids.is_empty() {
            return;
        }
        // Export renders what the catalog holds, so put Develop's pending edits there first.
        // If that save fails, export would silently render the older stored edits: stop instead.
        if !self.save_develop_edits(true) {
            self.cull_notice = Some(
                "Couldn't save Develop's edits, so nothing was exported (see the message above)."
                    .into(),
            );
            return;
        }
        let CatalogOpenState::Open(store) = &self.catalog else {
            return;
        };
        let samples = ids
            .iter()
            .take(3)
            .filter_map(|&id| {
                let asset = store.get_asset(id).ok().flatten()?;
                let root = store.get_root_path(asset.root_id).ok().flatten()?;
                Some(facts_for(
                    &asset,
                    &PathBuf::from(root).join(&asset.rel_path),
                ))
            })
            .collect();
        let scope = match ids.len() {
            1 => "1 photo".to_string(),
            n => format!("{n} photos"),
        };
        self.export.request(ids, scope, samples);
    }

    /// The photos a copy/paste/preset acts on. Develop edits the one photo `DevelopView` has
    /// loaded; everywhere else it's the marked targets (the photo on screen, in the Loupe).
    fn knead_targets(&self) -> Vec<i64> {
        match self.view {
            View::Develop => self
                .loupe_loaded_asset
                .map(|(id, _)| id)
                .into_iter()
                .collect(),
            // The Loupe's own cursor, not `loupe_loaded_asset`: that only moves once the next
            // photo has decoded, so while one is loading (or failed) it still names the last one.
            _ => self.mark_targets(),
        }
    }

    /// The photo whose settings a copy/sync/preset-save reads, and its current document: the live
    /// one when it's the photo `DevelopView` has loaded, else the catalog's.
    fn knead_source_doc(&self) -> Option<(i64, nicti_pawprint::EditDocument)> {
        let id = match self.view {
            View::Develop => self.loupe_loaded_asset?.0,
            View::Library => self.grid.as_ref()?.cursor_id()?,
            _ => *self.mark_targets().first()?,
        };
        // The live (possibly unsaved) document, but only when it really is this photo's.
        if self.loupe_loaded_asset.map(|(loaded, _)| loaded) == Some(id) {
            if let Some(develop) = self.develop.as_ref() {
                return Some((id, develop.document().clone()));
            }
        }
        let CatalogOpenState::Open(store) = &self.catalog else {
            return None;
        };
        store
            .get_master_edit(id)
            .ok()
            .flatten()
            .map(|doc| (id, doc))
    }

    /// Whether a job or prompt that also rewrites photos is in flight (the same set Export refuses
    /// on, minus Export itself, which only reads).
    fn knead_busy(&mut self) -> bool {
        let busy = self.job_active(&[
            JobKind::Import,
            JobKind::Sync,
            JobKind::Move,
            JobKind::Delete,
        ]) || self.delete.is_confirming()
            || self.export.is_running();
        if busy {
            self.knead.set_status(
                "Wait for the running import, sync, move or delete to finish first.".into(),
            );
        }
        busy
    }

    fn handle_knead_action(&mut self, action: PanelAction) {
        if self.knead.is_asking() {
            return;
        }
        // A second prompt on top of the delete prompt would let the confirmed one be dropped by
        // `knead_busy` (which also refuses while that prompt is up).
        if self.delete.is_confirming()
            && matches!(
                action,
                PanelAction::Copy | PanelAction::Sync | PanelAction::SavePreset
            )
        {
            return;
        }
        match action {
            PanelAction::Copy | PanelAction::SavePreset => {
                let Some((_, doc)) = self.knead_source_doc() else {
                    self.knead
                        .set_status("Open or select a photo to copy settings from.".into());
                    return;
                };
                if action == PanelAction::Copy {
                    self.knead.ask_copy(doc);
                } else {
                    self.knead.ask_save_preset(doc);
                }
            }
            PanelAction::Sync => {
                let Some((source, doc)) = self.knead_source_doc() else {
                    self.knead
                        .set_status("Put the cursor on the photo to copy settings from.".into());
                    return;
                };
                let ids: Vec<i64> = self
                    .knead_targets()
                    .into_iter()
                    .filter(|id| *id != source)
                    .collect();
                if ids.is_empty() {
                    self.knead.set_status(
                        "Select the photos to sync onto. The photo under the cursor is the source."
                            .into(),
                    );
                    return;
                }
                self.knead.ask_sync(doc, ids);
            }
            PanelAction::Paste => match self.knead.clipboard().cloned() {
                Some(clip) => self.run_knead_paste(clip, "Paste"),
                None => self.knead.set_status("Copy settings first.".into()),
            },
            PanelAction::Apply(name) => {
                if let Some(clip) = self.knead.preset_clipboard(&name) {
                    self.run_knead_paste(clip, &format!("Apply \"{name}\""));
                }
            }
        }
    }

    fn run_knead_paste(&mut self, clip: Clipboard, label: &str) {
        let ids = self.knead_targets();
        if ids.is_empty() {
            self.knead
                .set_status("Open or select the photos to paste onto.".into());
            return;
        }
        self.run_knead_command(KneadCommand::Run {
            clip,
            ids,
            label: label.to_string(),
        });
    }

    /// Draws the checklist prompt; a confirmed sync runs here.
    fn show_knead_modal(&mut self, ctx: &egui::Context) {
        if let Some(command) = self.knead.show_modal(ctx) {
            self.run_knead_command(command);
        }
    }

    /// Runs a paste/sync/preset or an undo against the catalog (#52). The loaded photo's unsaved
    /// edits are flushed first so the batch sees them, and its `DevelopView` is refreshed after,
    /// or the per-frame autosave would write the stale in-memory document back over the batch.
    fn run_knead_command(&mut self, command: KneadCommand) {
        let CatalogOpenState::Open(store) = &self.catalog else {
            return;
        };
        let store = store.clone();
        if self.knead_busy() {
            return;
        }
        if !self.save_develop_edits_to(store.as_ref(), true) {
            self.knead.set_status(
                "Couldn't save this photo's edits first, so nothing was changed.".into(),
            );
            return;
        }
        // Only a paste/sync/preset has new masks worth baking ahead; an undo just restores old ones.
        let prebake_new_masks = matches!(command, KneadCommand::Run { .. });
        let touched: Vec<i64> = match command {
            KneadCommand::Run { clip, ids, label } => {
                match run_batch(store.as_ref(), &clip, &ids, &label) {
                    Ok((outcome, last)) => self.knead.finish_batch(&label, &outcome, last),
                    Err(e) => self.knead.set_status(format!("{label} failed: {e}")),
                }
                ids
            }
            KneadCommand::Undo => {
                let Some(last) = self.knead.take_undo() else {
                    return;
                };
                let ids: Vec<i64> = last.asset_ids().collect();
                match last.undo(store.as_ref()) {
                    Ok(outcome) => self.knead.finish_undo(&last.label, &outcome),
                    Err(e) => {
                        self.knead.set_status(format!("Undo failed: {e}"));
                        self.knead.keep_undo(last);
                    }
                }
                ids
            }
        };
        self.refresh_loaded_develop(store.as_ref(), &touched);
        if prebake_new_masks {
            self.enqueue_prebake(&touched);
        }
        // #145: paste / sync / preset / undo rewrote these photos' edits without going through
        // Develop's save, so their grid dots and cached render state must be re-read too.
        for id in &touched {
            self.eyeshine.invalidate(*id);
        }
        if let (Some(grid), Ok(docs)) = (self.grid.as_mut(), store.get_master_edits(&touched)) {
            for (id, doc) in docs {
                grid.mark_edited(id, doc.is_some_and(|d| !d.stages.is_empty()));
            }
        }
    }

    /// Re-reads the loaded photo's document into `DevelopView` if a batch touched it.
    fn refresh_loaded_develop(&mut self, store: &dyn CatalogStore, touched: &[i64]) {
        let (Some((id, _)), Some(develop)) = (self.loupe_loaded_asset, self.develop.as_mut())
        else {
            return;
        };
        if !touched.contains(&id) {
            return;
        }
        if let Ok(Some(doc)) = store.get_master_edit(id) {
            if &doc != develop.document() {
                develop.replace_document(doc);
                // Selected corrections and spots may no longer exist.
                self.mask_ui.selected = None;
                self.heal_ui.clear_selection();
            }
        }
    }

    /// Draws the Export dialog while open; starting it builds the run's environment from live
    /// app state.
    fn show_export_dialog(&mut self, ctx: &egui::Context) {
        let CatalogOpenState::Open(store) = &self.catalog else {
            return;
        };
        let store: Arc<dyn CatalogStore + Send + Sync> = store.clone();
        let env = || {
            Some(ExportEnv {
                submitter: self.pounce.submitter(),
                store: store.clone(),
                decoder: self.decoder.clone(),
                gpu: self.gpu.clone(),
                registry: self.export_registry.clone(),
                software: format!("Nicti {}", self.version),
                larder: self.larder.clone(),
            })
        };
        self.export.show_dialog(ctx, &env);
    }

    /// Persists Develop's edits for the photo it has loaded (#57) if they differ from what the
    /// catalog holds. `Ok`/nothing-to-do -> `true`; a failed save -> `false` (the message is kept
    /// in `edit_save_failed`). Unless `force`, a document that already failed to save isn't
    /// retried until it changes -- the autosave calls this every frame.
    fn save_develop_edits_to(&mut self, store: &dyn CatalogStore, force: bool) -> bool {
        let (Some(develop), Some((asset_id, _))) = (self.develop.as_mut(), self.loupe_loaded_asset)
        else {
            return true;
        };
        if !develop.is_dirty() {
            self.edit_save_failed = None;
            return true;
        }
        if !force {
            if let Some((doc, _)) = &self.edit_save_failed {
                if doc == develop.document() {
                    return false;
                }
            }
        }
        match store.put_master_edit(asset_id, develop.document()) {
            Ok(()) => {
                let edited = eyeshine::has_edits(develop.document());
                develop.mark_saved();
                self.edit_save_failed = None;
                if let Some(grid) = self.grid.as_mut() {
                    grid.mark_edited(asset_id, edited);
                }
                // #145: the saved edits supersede any cached/queued render of this photo.
                self.eyeshine.invalidate(asset_id);
                let policy = self.preview_settings.effective_policy();
                self.eyeshine
                    .request(&self.pounce, store, asset_id, 0, policy);
                true
            }
            Err(e) => {
                self.edit_save_failed = Some((develop.document().clone(), e.to_string()));
                false
            }
        }
    }

    /// [`Self::save_develop_edits_to`] against the app's own catalog.
    fn save_develop_edits(&mut self, force: bool) -> bool {
        let CatalogOpenState::Open(store) = &self.catalog else {
            return true;
        };
        let store = store.clone();
        self.save_develop_edits_to(store.as_ref(), force)
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
        // Not while a text field (the folder box) has the keyboard: typing a space there must not
        // toggle the loupe's zoom.
        if !ui.ctx().egui_wants_keyboard_input() {
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
        }
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

        // #32: the markers of the photo on screen -- and the ones the auto-advance is about to
        // land on, so the header is right the instant the cursor moves.
        if let Some(cull) = self.cull.as_mut() {
            cull.ensure([asset_id]);
        }
        let marks = self.cull.as_ref().and_then(|c| c.meta(asset_id)).cloned();

        let mut retry_clicked = false;
        ui.horizontal(|ui| {
            ui.heading("Loupe");
            ui.label(cursor_label);
            crate::cull::badges::show_marks_inline(ui, marks.as_ref());
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
                    // #57: edits are saved to the catalog first, so this only blocks when the save
                    // itself failed.
                    self.save_develop_edits_to(store.as_ref(), false);
                    let develop_has_unsaved_edits =
                        self.develop.as_ref().is_some_and(|d| d.is_dirty());
                    if develop_has_unsaved_edits {
                        let why = self
                            .edit_save_failed
                            .as_ref()
                            .map_or("", |(_, msg)| msg.as_str());
                        ui.colored_label(
                            egui::Color32::YELLOW,
                            format!(
                                "Develop's edits for the previous photo couldn't be saved ({why})."
                            ),
                        );
                        if ui
                            .button("Discard those edits and view this photo")
                            .clicked()
                        {
                            if let (Some(develop), Some(asset)) =
                                (self.develop.as_mut(), &current_asset)
                            {
                                let identity = asset_cache_key(asset);
                                let doc = stored_edit_document(store.as_ref(), asset_id);
                                develop.load_real_frame(frame, identity, doc);
                                self.loupe_loaded_asset = Some((asset_id, identity));
                                self.edit_save_failed = None;
                            }
                        }
                        return;
                    }
                    if let (Some(develop), Some(asset)) = (self.develop.as_mut(), &current_asset) {
                        let identity = asset_cache_key(asset);
                        let doc = stored_edit_document(store.as_ref(), asset_id);
                        develop.load_real_frame(frame, identity, doc);
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
        // The rendered frame is the crop rect, not the source (#272).
        let tex_extent = (rendered.extent.width as f32, rendered.extent.height as f32);

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

    /// Persists a changed rendered-preview setting and applies it: in-flight renders are dropped,
    /// per-photo resolutions re-read, and switching to `Off` empties the rendered tier.
    fn apply_preview_settings(&mut self) {
        if let Err(e) = preview_settings::save(&self.catalog_path, &self.preview_settings) {
            self.cull_notice = Some(format!("Couldn't save the preview settings: {e}"));
        }
        self.eyeshine.cancel_all(&self.pounce);
        self.eyeshine.invalidate_all();
        self.loupe_rendered = None;
        self.loupe_preview = None;
        self.purge_rendered_pending = self.preview_settings.policy == eyeshine::RenderPolicy::Off;
        self.purge_error_reported = false;
        self.try_purge_rendered();
    }

    /// Empties the rendered tier once switched to `Off`; a busy Larder (a compaction holds it for
    /// minutes) leaves the request pending and it is retried from `poll_eyeshine`.
    fn try_purge_rendered(&mut self) {
        if !self.purge_rendered_pending {
            return;
        }
        match self.larder.as_ref().map(crate::t2::try_lock_larder) {
            Some(Some(mut guard)) => {
                match guard.purge_tier(nicti_lair::larder::LarderTier::Rendered) {
                    Ok(_) => self.purge_rendered_pending = false,
                    // Stay pending (retried next frame) and say so.
                    Err(e) => {
                        // Once per pending purge: this retries every frame and must not keep
                        // replacing whatever other notice the user hasn't read yet.
                        if !self.purge_error_reported {
                            self.purge_error_reported = true;
                            self.cull_notice =
                                Some(format!("Couldn't empty the rendered previews yet: {e}"));
                        }
                    }
                }
            }
            Some(None) => {}
            None => self.purge_rendered_pending = false,
        }
    }

    /// Collects finished rendered previews (#145). A render landing needs a repaint to be picked
    /// up; while any is running, poll again shortly since nothing else wakes an idle window.
    fn poll_eyeshine(&mut self, ctx: &egui::Context) {
        self.try_purge_rendered();
        if !self.eyeshine.poll().is_empty() {
            ctx.request_repaint();
        }
        if self.eyeshine.busy() {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
    }

    /// Advances the background mask pre-bake (#353) and tells the mask tool which bakes it owns,
    /// so opening one of those photos waits for the stored alpha instead of running the model
    /// again. Polls on a timer while it works: nothing else wakes an idle window.
    /// The photo the pre-bake must leave alone: the one Develop is showing, which `MaskBakeService`
    /// bakes itself. In Loupe and Library nothing bakes the loaded photo in the foreground (the mask
    /// tool only runs in Develop), so there the pre-bake may take it too.
    fn open_for_prebake(&self) -> Option<(i64, blake3::Hash)> {
        (self.view == View::Develop)
            .then_some(self.loupe_loaded_asset)
            .flatten()
    }

    fn poll_prebake(&mut self, ctx: &egui::Context) {
        let open = self.open_for_prebake();
        self.prebake
            .poll(&self.pounce, &mut self.mask_ui.service, open);
        self.mask_ui
            .service
            .set_deferred_keys(self.prebake.inflight_keys());
        if self.prebake.busy() {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
    }

    /// Queues the photos a paste/sync touched for background mask pre-baking, nearest the grid
    /// cursor first. The photo open in Develop is skipped: `MaskBakeService` already handles it.
    fn enqueue_prebake(&mut self, touched: &[i64]) {
        let open = self.open_for_prebake().map(|(id, _)| id);
        let ids: Vec<i64> = touched
            .iter()
            .copied()
            .filter(|id| Some(*id) != open)
            .collect();
        let ordered = match self.grid.as_ref() {
            Some(grid) => prebake::nearest_first(grid.ids(), grid.cursor(), &ids),
            None => ids,
        };
        self.prebake.enqueue(ordered);
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
        let color = self.color.source_transforms();
        if self.loupe_preview.as_ref().map(|(id, _, _)| *id) != Some(asset_id) {
            self.loupe_preview = None;
        }
        // `loupe_rendered` describes `loupe_preview`'s texture: if that was cleared (a reopened
        // loupe), the render must be re-read, not assumed installed.
        if self.loupe_preview.is_none()
            || self.loupe_rendered.as_ref().map(|(id, _)| *id) != Some(asset_id)
        {
            self.loupe_rendered = None;
        }
        // #145: an edited photo's screen preview is a render of its edits. Queue one if none is
        // cached, and swap a stored one in the moment it exists -- never downgrading to a camera
        // preview. Until then the camera preview stays, flagged stale.
        let policy = self.preview_settings.policy_for(Surface::Loupe);
        let index = self.loupe.as_ref().map_or(0, |l| l.cursor());
        let resolved = self
            .eyeshine
            .request(&self.pounce, store, asset_id, index, policy);
        // A render shown for edits that no longer apply (Reset all, rendering switched off) must
        // not linger: drop it so the camera preview is reinstalled below. ("Never downgrade"
        // protects against a *slower* source winning, not against edits going away.)
        if let Some(r) = &resolved {
            // Drop a shown render that no longer applies: rendering is off for this photo, or the
            // photo is now unedited while the installed render is of something else (Reset all
            // under "All photos" still wants a render, but the old edited one must not linger as
            // "updating").
            let installed_is_current = self
                .loupe_rendered
                .as_ref()
                .is_some_and(|(_, h)| Some(h.as_str()) == r.hash.as_deref());
            if self.loupe_rendered.is_some()
                && (!policy.wants_render(r.edited) || (!r.edited && !installed_is_current))
            {
                self.loupe_rendered = None;
                self.loupe_preview = None;
            }
        }
        if let (Some(r), Some(larder)) = (&resolved, &self.larder) {
            if policy.wants_render(r.edited) {
                let installed = self.loupe_rendered.as_ref().map(|(_, h)| h.clone());
                if let Some(mut guard) = crate::t2::try_lock_larder(larder) {
                    let stored = guard
                        .stored_hash(asset_id, nicti_lair::larder::LarderTier::Rendered)
                        .ok()
                        .flatten();
                    let known_bad = self
                        .loupe_render_undecodable
                        .as_ref()
                        .is_some_and(|(id, h)| *id == asset_id && Some(h) == stored.as_ref());
                    if stored.is_some() && stored != installed && !known_bad {
                        if let Ok(Some((hash, bytes))) =
                            guard.get_latest(asset_id, nicti_lair::larder::LarderTier::Rendered)
                        {
                            drop(guard);
                            // A render of a *replaced* file (a re-ingest keeps the asset id) must
                            // never be shown, even as an "older" one.
                            let usable = r
                                .hash
                                .as_deref()
                                .is_some_and(|cur| hash == cur || eyeshine::same_file(&hash, cur));
                            let texture = usable
                                .then(|| {
                                    preview_texture(
                                        ui.ctx(),
                                        format!("loupe-r-{asset_id}"),
                                        &bytes,
                                        &color,
                                    )
                                })
                                .flatten();
                            match texture {
                                Some(texture) => {
                                    self.loupe_preview = Some((asset_id, true, texture));
                                    self.loupe_rendered = Some((asset_id, hash));
                                }
                                None => self.loupe_render_undecodable = Some((asset_id, hash)),
                            }
                        }
                    }
                }
            }
        }
        let has_t2 = matches!(&self.loupe_preview, Some((_, true, _)));
        if !has_t2 && self.loupe_t2_undecodable != Some(asset_id) {
            if let Some(bytes) = self.loupe.as_mut().and_then(|l| l.current_t2(store)) {
                match preview_texture(ui.ctx(), format!("loupe-t2-{asset_id}"), &bytes, &color) {
                    Some(texture) => self.loupe_preview = Some((asset_id, true, texture)),
                    None => self.loupe_t2_undecodable = Some(asset_id),
                }
            }
        }
        // A sidecar read for this photo finished on a worker.
        if let Some((id, slot)) = &self.loupe_t0_fetch {
            let id = *id;
            let outcome = slot.lock().unwrap().take();
            if id != asset_id {
                self.loupe_t0_fetch = None;
            } else if let Some(bytes) = outcome {
                self.loupe_t0_fetch = None;
                // Never downgrade: T2 may have been installed while the sidecar read ran.
                let texture = bytes
                    .filter(|_| self.loupe_preview.is_none())
                    .and_then(|b| {
                        preview_texture(ui.ctx(), format!("loupe-t0-{asset_id}"), &b, &color)
                    });
                match texture {
                    _ if self.loupe_preview.is_some() => {}
                    Some(texture) => self.loupe_preview = Some((asset_id, false, texture)),
                    None => self.loupe_t0_miss = Some((asset_id, std::time::Instant::now())),
                }
            } else {
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(100));
            }
        }
        let recently_missed = matches!(
            self.loupe_t0_miss,
            Some((id, t)) if id == asset_id && t.elapsed() < std::time::Duration::from_secs(5)
        );
        if self.loupe_preview.is_none() && !recently_missed && self.loupe_t0_fetch.is_none() {
            // Catalog only on the UI thread; an archived folder's sidecar is read on a worker.
            match store.get_preview(asset_id, nicti_lair::PreviewTier::T0) {
                Ok(Some(preview)) => {
                    if let Some(texture) = preview_texture(
                        ui.ctx(),
                        format!("loupe-t0-{asset_id}"),
                        &preview.bytes,
                        &color,
                    ) {
                        self.loupe_preview = Some((asset_id, false, texture));
                    }
                }
                _ => match crate::t0_fetch::request(&self.pounce, store, asset_id) {
                    Some(slot) => self.loupe_t0_fetch = Some((asset_id, slot)),
                    None => self.loupe_t0_miss = Some((asset_id, std::time::Instant::now())),
                },
            }
        }

        ui.label("Decoding full-resolution image...");
        if let Some(r) = &resolved {
            let rendered = self.loupe_rendered.as_ref().map(|(_, h)| h.as_str());
            let have = eyeshine::Available {
                rendered_current: rendered
                    .filter(|h| Some(*h) == r.hash.as_deref())
                    .map(eyeshine::hash_is_partial),
                rendered_older: rendered.is_some() && rendered != r.hash.as_deref(),
                camera_t2: rendered.is_none() && matches!(&self.loupe_preview, Some((_, true, _))),
                camera_t0: rendered.is_none() && matches!(&self.loupe_preview, Some((_, false, _))),
            };
            self.loupe_badge = eyeshine::choose_preview(policy, r.edited, have).1;
            if let Some(text) = self.loupe_badge.label() {
                ui.colored_label(egui::Color32::from_rgb(0xf2, 0xa3, 0x3b), text);
            }
        }
        if let Some((_, _, texture)) = &self.loupe_preview {
            let available = ui.available_size();
            ui.centered_and_justified(|ui| {
                ui.add(egui::Image::new((texture.id(), texture.size_vec2())).max_size(available));
            });
        }
    }
}

/// Registers `path` as a root under the fixed placeholder volume this shell uses (see
/// `PLACEHOLDER_VOLUME_IDENTITY_KEY`'s own doc comment) and returns its root id.
/// The catalog's stored master edit document for `asset_id`, or an empty one when there is none or
/// it can't be read (a corrupt document must not stop the photo from opening; Develop then starts
/// fresh and the next save replaces it).
fn stored_edit_document(store: &dyn CatalogStore, asset_id: i64) -> nicti_pawprint::EditDocument {
    store
        .get_master_edit(asset_id)
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// The Folder field's check (#367), pure so it's unit-testable: trimmed, non-blank and, when
/// `must_exist`, exists and is a directory. `Err` is the user-facing message.
fn validate_folder_input(raw: &str, must_exist: bool) -> Result<PathBuf, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("Enter a folder path first.".into());
    }
    let path = PathBuf::from(trimmed);
    if must_exist {
        check_is_dir(&path)?;
    }
    Ok(path)
}

/// One `metadata` call so a real inspection failure (permission denied on a parent, a dead share)
/// is reported as such instead of being flattened to "not found" the way `Path::exists` does.
fn check_is_dir(path: &Path) -> Result<(), String> {
    match std::fs::metadata(path) {
        Ok(m) if m.is_dir() => Ok(()),
        Ok(_) => Err(format!("Not a folder: {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(format!("Folder not found: {}", path.display()))
        }
        Err(e) => Err(format!("Can't access {}: {e}", path.display())),
    }
}

fn register_root(store: &dyn CatalogStore, path: &Path) -> Result<i64, CatalogError> {
    nicti_lair::scruff::register_root(store, path)
}

#[cfg(test)]
mod cull_wiring_tests {
    use super::*;

    #[test]
    fn culling_keys_are_live_in_the_four_culling_views_only() {
        for view in [View::Library, View::Loupe, View::Survey, View::Compare] {
            assert!(cull_keys_active(view, false), "{view:?}");
        }
        assert!(
            !cull_keys_active(View::Develop, false),
            "Develop owns the digit keys"
        );
    }

    #[test]
    fn facet_counts_refresh_only_after_marking_has_been_quiet() {
        let t0 = Instant::now();
        assert!(
            !facets_refresh_due(None, t0),
            "nothing marked, nothing stale"
        );
        assert!(
            !facets_refresh_due(Some(t0), t0),
            "just marked: still typing"
        );
        let almost = t0 + FACET_REFRESH_AFTER - Duration::from_millis(1);
        assert!(!facets_refresh_due(Some(t0), almost));
        assert!(facets_refresh_due(Some(t0), t0 + FACET_REFRESH_AFTER));
        // A clock that appears to go backwards must not underflow or fire early.
        assert!(!facets_refresh_due(Some(t0 + Duration::from_secs(5)), t0));
    }

    #[test]
    fn culling_keys_are_dead_while_the_delete_prompt_is_open() {
        for view in [
            View::Library,
            View::Loupe,
            View::Survey,
            View::Compare,
            View::Develop,
        ] {
            assert!(!cull_keys_active(view, true), "{view:?}");
        }
    }
}

#[cfg(test)]
mod folder_input_tests {
    use super::validate_folder_input;

    #[test]
    fn blank_and_whitespace_are_rejected() {
        assert!(validate_folder_input("", false).is_err());
        assert!(validate_folder_input("   ", true).is_err());
    }

    #[test]
    fn missing_path_and_plain_file_are_rejected_with_distinct_messages() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_path_buf();
        let file = dir.join("f.txt");
        std::fs::write(&file, b"x").unwrap();
        let missing = validate_folder_input(dir.join("nope").to_str().unwrap(), true).unwrap_err();
        let not_dir = validate_folder_input(file.to_str().unwrap(), true).unwrap_err();
        assert!(missing.starts_with("Folder not found"), "{missing}");
        assert!(not_dir.starts_with("Not a folder"), "{not_dir}");
        // A real directory, with surrounding whitespace, is accepted and trimmed.
        let ok = validate_folder_input(&format!("  {}  ", dir.display()), true).unwrap();
        assert_eq!(ok, dir);
        // Open in Loupe is catalog-only: a path that is gone (unplugged drive) still passes.
        let gone = dir.join("nope");
        assert_eq!(
            validate_folder_input(gone.to_str().unwrap(), false).unwrap(),
            gone
        );
    }
}
