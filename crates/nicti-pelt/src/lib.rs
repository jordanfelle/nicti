#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]
//! The `nicti-pelt` app shell (#241): one eframe window whose wgpu device *is* Tapetum's
//! `GpuContext` (ADR-0016's "one shared device for compute and display"), a Tapetum-rendered
//! frame painted via `egui_wgpu::CallbackTrait` (ADR-0068), placeholder library/loupe/develop view
//! routing, and a real `nicti-lair` `SqliteCatalog`. Named after the visible coat over the
//! tapetum lucidum, matching this repo's feline naming convention (see `CLAUDE.md`) -- not the
//! `nicti-ui` name #241 was filed under.
//!
//! The Library view's virtualized thumbnail grid is `grid` (#30); its filter bar is `filter_bar`
//! (#242); culling -- marking keys, undo, survey/compare and delete -- is `cull` (#32).

/// A typed stage-params struct as JSON. These are derive-`Serialize` structs of numbers, strings
/// and enums, which cannot fail to serialize (a NaN becomes `null`, not an error).
#[allow(clippy::expect_used)]
pub(crate) fn params_value<T: serde::Serialize>(value: &T) -> serde_json::Value {
    serde_json::to_value(value).expect("a coat params struct always serializes")
}

/// The first eight bytes of a hash as a little-endian `u64` (total: no slicing).
pub(crate) fn first_u64(hash: &blake3::Hash) -> u64 {
    let bytes = hash.as_bytes();
    u64::from_le_bytes(std::array::from_fn(|i| bytes[i]))
}

mod activity;
mod app;
mod archive_drives;
mod backup_copy_ui;
mod cache_settings;
mod camera_profiles;
mod card_copy_ui;
mod catalog;
mod catalog_problem;
mod catseye;
mod cmd;
mod color_mgmt;
mod color_settings;
mod control;
mod cull;
mod decode_job;
mod develop_controls;
mod develop_panel;
mod export;
mod eyeshine;
mod filter_bar;
mod folder_browse;
mod folder_dialog;
mod folder_panel;
mod fur;
mod geotag_ui;
mod grid;
mod heal_tool;
mod keyword_tag;
mod knead;
mod loupe;
mod lrc_import;
mod mask_edit;
mod mask_models;
mod mask_panel;
mod mask_tool;
mod notices;
mod palette;
mod pending;
mod prebake;
mod preview_color;
mod preview_settings;
mod render;
mod repaint_trace;
mod righting_ui;
mod section_state;
mod stash;
#[cfg(test)]
mod swat;
mod t0_fetch;
mod t2;
mod targeted;
#[cfg(test)]
mod test_gpu;
#[cfg(test)]
mod test_support;
mod update;
mod verify_ui;
mod view_orient;
mod viewport;
mod widgets;
mod xmp_sync;

use std::sync::Arc;

pub use app::PeltApp;
pub use control::ControlOptions;
/// Re-exported so `src/main.rs` (the root `nicti` binary) doesn't need its own direct `eframe`
/// dependency just to spell this return type.
pub use eframe::Result;

/// Runs the app. `WgpuSetup::CreateNew`'s `device_descriptor` closure requests
/// `nicti_tapetum::gpu::device_descriptor_for`'s own descriptor (the adapter's real limits plus
/// `TIMESTAMP_QUERY`/`SHADER_F16` when supported) instead of eframe's own conservative default
/// (`wgpu::Limits::default()`, no optional features) -- without this, the device
/// `GpuContext::from_device` then wraps in [`app::PeltApp::new`] would be undersized for a real
/// full-resolution Tapetum render (ADR-0016).
pub fn run(version: &str) -> eframe::Result {
    run_with(version, None)
}

/// [`run`] with the opt-in agent control channel (#543, ADR-0434) when `control` is `Some`.
pub fn run_with(version: &str, control: Option<ControlOptions>) -> eframe::Result {
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
        Box::new(move |cc| {
            Ok(Box::new(catalog_problem::Launcher::new(
                cc, version, control,
            )))
        }),
    )
}

#[cfg(test)]
mod conversion_tests {
    #[test]
    fn first_u64_is_the_little_endian_prefix_of_the_hash() {
        let h = blake3::hash(b"nicti");
        let want = u64::from_le_bytes(h.as_bytes()[..8].try_into().unwrap());
        assert_eq!(super::first_u64(&h), want);
    }

    #[test]
    fn params_value_serialises_a_params_struct_like_serde_json() {
        let crop = nicti_tapetum::coat::CropParams::default();
        assert_eq!(
            super::params_value(&crop),
            serde_json::to_value(crop).unwrap()
        );
    }
}
