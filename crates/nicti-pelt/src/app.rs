//! The `eframe::App` shell: top-level view routing (library/loupe/develop, each a placeholder
//! panel beyond Develop's real Tapetum viewport -- grid virtualization is #30, loupe is #31,
//! culling UX is #32, the filter bar is #242) and the wgpu device Tapetum's `GpuContext` shares
//! with eframe (ADR-0016). Also owns Pounce (#55): the job runtime plus the activity panel
//! (`crate::activity`) that reads it, and the Library view's Import/Sync buttons that submit real
//! jobs to it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use nicti_lair::patrol::SyncOptions;
use nicti_lair::pounce_jobs::{IngestJob, SyncJob};
use nicti_lair::{CatalogError, CatalogStore, SqliteCatalog};
use nicti_pounce::telemetry::{default_vram_source, TelemetrySampler};
use nicti_pounce::Pounce;
use nicti_tapetum::gpu::GpuContext;

use crate::render::DevelopView;
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
    catalog_path: PathBuf,
    catalog: CatalogOpenState,
    develop: Option<DevelopView>,
    pounce: Pounce,
    telemetry: TelemetrySampler,
    import_path_input: String,
}

impl PeltApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
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
        let catalog = match catalog::open(&catalog_path) {
            Ok(store) => CatalogOpenState::Open(Arc::new(store)),
            Err(e) => CatalogOpenState::Error(e.to_string()),
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

        Self {
            view: View::Library,
            catalog_path,
            catalog,
            develop: Some(develop),
            pounce,
            telemetry,
            import_path_input: String::new(),
        }
    }
}

impl eframe::App for PeltApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("view_tabs").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.view, View::Library, "Library");
                ui.selectable_value(&mut self.view, View::Loupe, "Loupe");
                ui.selectable_value(&mut self.view, View::Develop, "Develop");
                ui.separator();
                ui.label(format!("Catalog: {}", self.catalog_path.display()));
            });
        });

        crate::activity::show(ui, &self.pounce, &mut self.telemetry);

        egui::CentralPanel::default().show(ui, |ui| match self.view {
            View::Library => self.show_library(ui),
            View::Loupe => {
                ui.heading("Loupe");
                ui.label("Prefetch + instant zoom lands in #31.");
            }
            View::Develop => self.show_develop(ui),
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

    fn show_develop(&mut self, ui: &mut egui::Ui) {
        ui.heading("Develop");
        let Some(develop) = self.develop.as_mut() else {
            return;
        };
        let frame = develop.render();

        let available = ui.available_size();
        let (rect, _response) = ui.allocate_exact_size(available, egui::Sense::hover());
        ui.painter().add(egui_wgpu::Callback::new_paint_callback(
            rect,
            ViewportCallback { frame },
        ));
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
