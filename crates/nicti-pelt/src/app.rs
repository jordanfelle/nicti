//! The `eframe::App` shell: top-level view routing (library/loupe/develop, each a placeholder
//! panel beyond Develop's real Tapetum viewport -- grid virtualization is #30, loupe is #31,
//! culling UX is #32, the filter bar is #242) and the wgpu device Tapetum's `GpuContext` shares
//! with eframe (ADR-0016).

use std::path::PathBuf;
use std::sync::Arc;

use nicti_tapetum::gpu::GpuContext;

use crate::render::DevelopView;
use crate::viewport::{ViewportCallback, ViewportResources};
use crate::{catalog, CatalogOpenState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Library,
    Loupe,
    Develop,
}

pub struct PeltApp {
    view: View,
    catalog_path: PathBuf,
    catalog: CatalogOpenState,
    develop: Option<DevelopView>,
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

        Self {
            view: View::Library,
            catalog_path,
            catalog,
            develop: Some(develop),
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
    fn show_library(&self, ui: &mut egui::Ui) {
        ui.heading("Library");
        match &self.catalog {
            CatalogOpenState::Open(store) => match store.asset_count() {
                Ok(count) => {
                    ui.label(format!("{count} asset(s) in this catalog."));
                }
                Err(e) => {
                    ui.colored_label(egui::Color32::RED, format!("Failed to query catalog: {e}"));
                }
            },
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
