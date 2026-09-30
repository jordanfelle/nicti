//! The `nicti-pelt` app shell (#241): one eframe window whose wgpu device *is* Tapetum's
//! `GpuContext` (ADR-0016's "one shared device for compute and display"), a Tapetum-rendered
//! frame painted via `egui_wgpu::CallbackTrait` (ADR-0068), placeholder library/loupe/develop view
//! routing, and a real `nicti-lair` `SqliteCatalog`. Named after the visible coat over the
//! tapetum lucidum, matching this repo's feline naming convention (see `CLAUDE.md`) -- not the
//! `nicti-ui` name #241 was filed under.
//!
//! The Library view's virtualized thumbnail grid is `grid` (#30). Explicitly out of scope here
//! (each has, or will have, its own ticket): culling UX (#32). The Library filter bar is
//! `filter_bar` (#242).

mod activity;
mod app;
mod cache_settings;
mod camera_profiles;
mod catalog;
mod color_mgmt;
mod decode_job;
mod develop_panel;
mod filter_bar;
mod grid;
mod heal_tool;
mod loupe;
mod render;
mod t2;
#[cfg(test)]
mod test_gpu;
mod update;
mod viewport;

use std::sync::Arc;

use nicti_lair::SqliteCatalog;

pub use app::PeltApp;
/// Re-exported so `src/main.rs` (the root `nicti` binary) doesn't need its own direct `eframe`
/// dependency just to spell this return type.
pub use eframe::Result;

/// The catalog's open state -- kept as an enum rather than an unwrapped `Arc<SqliteCatalog>` so a
/// catalog that fails to open (a bad path, a permissions error) shows an in-window message instead
/// of panicking the whole app on startup.
enum CatalogOpenState {
    Open(Arc<SqliteCatalog>),
    Error(String),
}

/// Runs the app. `WgpuSetup::CreateNew`'s `device_descriptor` closure requests
/// `nicti_tapetum::gpu::device_descriptor_for`'s own descriptor (the adapter's real limits plus
/// `TIMESTAMP_QUERY`/`SHADER_F16` when supported) instead of eframe's own conservative default
/// (`wgpu::Limits::default()`, no optional features) -- without this, the device
/// `GpuContext::from_device` then wraps in [`app::PeltApp::new`] would be undersized for a real
/// full-resolution Tapetum render (ADR-0016).
pub fn run(version: &str) -> eframe::Result {
    let wgpu_setup = egui_wgpu::WgpuSetup::CreateNew(egui_wgpu::WgpuSetupCreateNew {
        device_descriptor: Arc::new(nicti_tapetum::gpu::device_descriptor_for),
        ..egui_wgpu::WgpuSetupCreateNew::without_display_handle()
    });
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        viewport: egui::ViewportBuilder::default().with_title("Nicti"),
        wgpu_options: egui_wgpu::WgpuConfiguration {
            wgpu_setup,
            ..Default::default()
        },
        ..Default::default()
    };
    let version = version.to_string();
    eframe::run_native(
        "nicti",
        options,
        Box::new(move |cc| Ok(Box::new(PeltApp::new(cc, version)))),
    )
}
