//! `pelt-egui`: egui/eframe GUI-framework candidate spike for #68 (ADR-0006). A virtualized
//! 2M-cell grid (egui's built-in `ScrollArea::show_rows`), a 50-frame loupe with next/prev, and a
//! custom wgpu viewport rendering `pelt::live_chain`'s compute kernel, driven by a slider
//! (exposure) and a pan drag (vibrance/white-balance). Not a production crate -- see
//! `CLAUDE.md`'s package-map note on `spikes/*`.
//!
//! Driven externally by `bench/pelt/run-pelt.ps1` + `bench/pelt/pelt.ahk` for input-latency
//! measurement (see `docs/adr/0006-gui-framework.md`), the same screen-capture method
//! `bench/run-hero.ps1` uses against the LRC baseline.

mod viewport;

use eframe::egui;
use pelt::config::{GRID_CELL_COUNT, THUMB_TILE_SIZE};
use pelt::live_chain::LiveChainParams;
use pelt::loupe::{LOUPE_HEIGHT, LOUPE_WIDTH};
use pelt::virtualize::GridLayout;
use viewport::{ViewportCallback, ViewportResources};

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        viewport: egui::ViewportBuilder::default().with_title("pelt-egui"),
        ..Default::default()
    };
    eframe::run_native(
        "pelt-egui",
        options,
        Box::new(|cc| Ok(Box::new(PeltApp::new(cc)))),
    )
}

struct PeltApp {
    tile_textures: Vec<egui::TextureHandle>,
    loupe_textures: Vec<egui::TextureHandle>,
    loupe_index: usize,
    exposure_stops: f32,
    vibrance: f32,
    wb_gain: [f32; 3],
    viewport_texture_size: [u32; 2],
}

impl PeltApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let tile_textures = pelt::thumbnails::generate_tile_pool()
            .into_iter()
            .enumerate()
            .map(|(i, bytes)| {
                let image = egui::ColorImage::from_rgba_unmultiplied(
                    [THUMB_TILE_SIZE as usize, THUMB_TILE_SIZE as usize],
                    &bytes,
                );
                cc.egui_ctx
                    .load_texture(format!("tile-{i}"), image, Default::default())
            })
            .collect();

        let loupe_textures = pelt::loupe::generate_loupe_set()
            .into_iter()
            .enumerate()
            .map(|(i, bytes)| {
                let image = egui::ColorImage::from_rgba_unmultiplied(
                    [LOUPE_WIDTH as usize, LOUPE_HEIGHT as usize],
                    &bytes,
                );
                cc.egui_ctx
                    .load_texture(format!("loupe-{i}"), image, Default::default())
            })
            .collect();

        let render_state = cc
            .wgpu_render_state
            .as_ref()
            .expect("pelt-egui requires the wgpu backend (eframe::Renderer::Wgpu)");
        let viewport_size = [pelt::config::VIEWPORT_WIDTH, pelt::config::VIEWPORT_HEIGHT];
        let resources = ViewportResources::new(
            &render_state.device,
            render_state.target_format,
            viewport_size[0],
            viewport_size[1],
        );
        resources.upload_input(
            &render_state.queue,
            &pelt::live_chain::generate_viewport_frame(),
        );
        render_state
            .renderer
            .write()
            .callback_resources
            .insert(resources);

        Self {
            tile_textures,
            loupe_textures,
            loupe_index: 0,
            exposure_stops: 0.0,
            vibrance: 0.0,
            wb_gain: [1.0, 1.0, 1.0],
            viewport_texture_size: viewport_size,
        }
    }
}

impl eframe::App for PeltApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        ctx.input(|i| {
            if i.key_pressed(egui::Key::ArrowRight) {
                self.loupe_index = (self.loupe_index + 1) % self.loupe_textures.len();
            }
            if i.key_pressed(egui::Key::ArrowLeft) {
                self.loupe_index =
                    (self.loupe_index + self.loupe_textures.len() - 1) % self.loupe_textures.len();
            }
        });

        egui::Panel::left("grid_panel").show(ui, |ui| {
            ui.heading("Grid (2M cells, virtualized)");
            let available_width = ui.available_width();
            let layout = GridLayout::new(GRID_CELL_COUNT, THUMB_TILE_SIZE as f32, available_width);
            egui::ScrollArea::vertical().show_rows(
                ui,
                THUMB_TILE_SIZE as f32,
                layout.rows(),
                |ui, row_range| {
                    for row in row_range {
                        ui.horizontal(|ui| {
                            for col in 0..layout.columns {
                                let cell = row * layout.columns + col;
                                if cell >= GRID_CELL_COUNT {
                                    break;
                                }
                                let tile_index = pelt::config::cell_to_tile_index(cell);
                                ui.image(&self.tile_textures[tile_index]);
                            }
                        });
                    }
                },
            );
        });

        egui::Panel::bottom("loupe_panel").show(ui, |ui| {
            ui.heading(format!(
                "Loupe {}/{} (Right/Left to switch)",
                self.loupe_index + 1,
                self.loupe_textures.len()
            ));
            ui.add(
                egui::Image::new(&self.loupe_textures[self.loupe_index])
                    .max_height(200.0)
                    .maintain_aspect_ratio(true),
            );
        });

        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("Develop viewport (live_chain compute)");
            ui.add(
                egui::Slider::new(&mut self.exposure_stops, -2.0..=2.0).text("Exposure (stops)"),
            );

            let (rect, response) = ui.allocate_exact_size(
                egui::vec2(
                    self.viewport_texture_size[0] as f32 / 2.0,
                    self.viewport_texture_size[1] as f32 / 2.0,
                ),
                egui::Sense::drag(),
            );
            if response.dragged() {
                let delta = response.drag_delta();
                self.vibrance = (self.vibrance + delta.x / rect.width()).clamp(-1.0, 1.0);
                let wb_delta = delta.y / rect.height();
                self.wb_gain[2] = (self.wb_gain[2] + wb_delta).clamp(0.2, 2.0);
            }

            let params = LiveChainParams {
                wb_gain: self.wb_gain,
                exposure_stops: self.exposure_stops,
                vibrance: self.vibrance,
                _pad: [0.0; 3],
            };
            ui.painter().add(egui_wgpu::Callback::new_paint_callback(
                rect,
                ViewportCallback { params },
            ));
        });

        // Continuous repaint so the viewport keeps dispatching every frame during a drag/slider
        // interaction -- matches how a real develop-module viewport redraws on every input, and
        // is what makes whisker's frame-interval measurement meaningful here.
        ctx.request_repaint();
    }
}
