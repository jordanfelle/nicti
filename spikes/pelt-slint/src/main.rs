//! `pelt-slint`: Slint GUI-framework candidate spike for #68 (ADR-0006). A Rust-driven
//! virtualized 2M-cell grid (Slint has no built-in equivalent to egui's `ScrollArea::show_rows`
//! or GPUI's `uniform_list`, so this spike hand-rolls the visible-slice logic exactly like
//! `pelt-iced` does), loupe next/prev, and a develop viewport importing an externally-rendered
//! `wgpu::Texture` straight into the scene (see `viewport.rs`). Not a production crate -- see
//! `CLAUDE.md`'s package-map note on `spikes/*`.
//!
//! Driven externally by `bench/pelt/run-pelt.ps1` + `bench/pelt/pelt.ahk` for input-latency
//! measurement, the same screen-capture method `bench/run-hero.ps1` uses against the LRC
//! baseline.

mod viewport;

use std::cell::RefCell;
use std::rc::Rc;

use pelt::config::{GRID_CELL_COUNT, THUMB_TILE_SIZE};
use pelt::live_chain::LiveChainParams;
use pelt::loupe::{LOUPE_HEIGHT, LOUPE_WIDTH};
use pelt::virtualize::GridLayout;
use viewport::ViewportRenderer;

slint::include_modules!();

const GRID_VIEWPORT_WIDTH: f32 = 960.0;
const GRID_VIEWPORT_HEIGHT: f32 = 840.0;
const OVERSCAN_ROWS: usize = 3;

#[derive(Default, Clone, Copy)]
struct DevelopParams {
    exposure_stops: f32,
    vibrance: f32,
    wb_gain_b: f32,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    slint::BackendSelector::new()
        .require_wgpu_30(slint::wgpu_30::WGPUConfiguration::default())
        .select()?;

    let app = AppWindow::new()?;

    let tile_pool: Vec<slint::Image> = pelt::thumbnails::generate_tile_pool()
        .into_iter()
        .map(|bytes| rgba_bytes_to_slint_image(&bytes, THUMB_TILE_SIZE, THUMB_TILE_SIZE))
        .collect();
    let loupe_set: Vec<slint::Image> = pelt::loupe::generate_loupe_set()
        .into_iter()
        .map(|bytes| rgba_bytes_to_slint_image(&bytes, LOUPE_WIDTH, LOUPE_HEIGHT))
        .collect();

    let layout = GridLayout::new(GRID_CELL_COUNT, THUMB_TILE_SIZE as f32, GRID_VIEWPORT_WIDTH);
    app.set_grid_columns(layout.columns as i32);
    app.set_grid_total_rows(layout.rows() as i32);

    let update_visible_tiles = {
        let app_weak = app.as_weak();
        let tile_pool = tile_pool.clone();
        move |scroll_y: f32| {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            let visible =
                layout.visible_range(scroll_y.max(0.0), GRID_VIEWPORT_HEIGHT, OVERSCAN_ROWS);
            let start_row = visible.start / layout.columns;
            let tiles: Vec<slint::Image> = visible
                .clone()
                .map(|cell| tile_pool[pelt::config::cell_to_tile_index(cell)].clone())
                .collect();
            app.set_visible_start_row(start_row as i32);
            app.set_visible_tiles(std::rc::Rc::new(slint::VecModel::from(tiles)).into());
        }
    };
    update_visible_tiles(0.0);

    app.on_scrolled({
        let update_visible_tiles = update_visible_tiles.clone();
        move |offset| update_visible_tiles(offset)
    });

    let loupe_count = loupe_set.len();
    app.set_loupe_count(loupe_count as i32);
    app.set_loupe_image(loupe_set[0].clone());

    app.on_next_image({
        let app_weak = app.as_weak();
        let loupe_set = loupe_set.clone();
        move || {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            let next = (app.get_loupe_index() + 1) % loupe_count as i32;
            app.set_loupe_index(next);
            app.set_loupe_image(loupe_set[next as usize].clone());
        }
    });
    app.on_prev_image({
        let app_weak = app.as_weak();
        let loupe_set = loupe_set.clone();
        move || {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            let prev = (app.get_loupe_index() - 1 + loupe_count as i32) % loupe_count as i32;
            app.set_loupe_index(prev);
            app.set_loupe_image(loupe_set[prev as usize].clone());
        }
    });

    let develop_params = Rc::new(RefCell::new(DevelopParams {
        wb_gain_b: 1.0,
        ..Default::default()
    }));
    let renderer: Rc<RefCell<Option<ViewportRenderer>>> = Rc::new(RefCell::new(None));

    app.on_exposure_changed({
        let develop_params = develop_params.clone();
        move |value| {
            develop_params.borrow_mut().exposure_stops = value;
        }
    });
    app.on_viewport_dragged({
        let develop_params = develop_params.clone();
        move |dx, dy| {
            let mut params = develop_params.borrow_mut();
            params.vibrance = (params.vibrance + dx).clamp(-1.0, 1.0);
            params.wb_gain_b = (params.wb_gain_b + dy).clamp(0.2, 2.0);
        }
    });

    let app_weak_for_notifier = app.as_weak();
    let renderer_for_notifier = renderer.clone();
    let develop_params_for_notifier = develop_params.clone();
    app.window()
        .set_rendering_notifier(move |state, graphics_api| match (state, graphics_api) {
            (
                slint::RenderingState::RenderingSetup,
                slint::GraphicsAPI::WGPU30 { device, queue, .. },
            ) => {
                *renderer_for_notifier.borrow_mut() = Some(ViewportRenderer::new(
                    device.clone(),
                    queue.clone(),
                    pelt::config::VIEWPORT_WIDTH,
                    pelt::config::VIEWPORT_HEIGHT,
                ));
            }
            (slint::RenderingState::BeforeRendering, _) => {
                let Some(app) = app_weak_for_notifier.upgrade() else {
                    return;
                };
                let borrowed = renderer_for_notifier.borrow();
                let Some(renderer) = borrowed.as_ref() else {
                    return;
                };
                let params = *develop_params_for_notifier.borrow();
                let live_chain_params = LiveChainParams {
                    wb_gain: [1.0, 1.0, params.wb_gain_b],
                    exposure_stops: params.exposure_stops,
                    vibrance: params.vibrance,
                    _pad: [0.0; 3],
                };
                let texture = renderer.render_frame(live_chain_params);
                if let Ok(image) = slint::Image::try_from(texture) {
                    app.set_viewport_image(image);
                }
                app.window().request_redraw();
            }
            _ => {}
        })?;

    app.run()?;
    Ok(())
}

fn rgba_bytes_to_slint_image(bytes: &[u8], width: u32, height: u32) -> slint::Image {
    let mut buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(width, height);
    buffer.make_mut_bytes().copy_from_slice(bytes);
    slint::Image::from_rgba8(buffer)
}
