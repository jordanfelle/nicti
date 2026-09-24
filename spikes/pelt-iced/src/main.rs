//! `pelt-iced`: iced GUI-framework candidate spike for #68 (ADR-0006). A hand-rolled virtualized
//! 2M-cell grid (iced 0.14 has no `ScrollArea::show_rows` equivalent, unlike egui/GPUI), a
//! 50-frame loupe with next/prev, and a custom wgpu viewport via `iced::widget::shader`. Not a
//! production crate -- see `CLAUDE.md`'s package-map note on `spikes/*`.
//!
//! Driven externally by `bench/pelt/run-pelt.ps1` + `bench/pelt/pelt.ahk` for input-latency
//! measurement, the same screen-capture method `bench/run-hero.ps1` uses against the LRC
//! baseline.

mod program;
mod viewport;

use iced::keyboard;
use iced::widget::{column, image, row, scrollable, shader, slider, text, Space};
use iced::{Element, Length, Task};
use pelt::config::{GRID_CELL_COUNT, THUMB_TILE_SIZE};
use pelt::live_chain::LiveChainParams;
use pelt::loupe::{LOUPE_HEIGHT, LOUPE_WIDTH};
use pelt::virtualize::GridLayout;
use program::ViewportProgram;

const GRID_VIEWPORT_WIDTH: f32 = 900.0;
const GRID_VIEWPORT_HEIGHT: f32 = 800.0;
const OVERSCAN_ROWS: usize = 3;

fn main() -> iced::Result {
    iced::application(App::new, App::update, App::view)
        .title("pelt-iced")
        .subscription(App::subscription)
        .run()
}

#[derive(Debug, Clone)]
enum Message {
    NextImage,
    PrevImage,
    ExposureChanged(f32),
    ViewportDragged { dx: f32, dy: f32 },
    GridScrolled(scrollable::Viewport),
}

struct App {
    tile_handles: Vec<image::Handle>,
    loupe_handles: Vec<image::Handle>,
    loupe_index: usize,
    exposure_stops: f32,
    vibrance: f32,
    wb_gain: [f32; 3],
    grid_scroll_offset: f32,
}

impl App {
    fn new() -> (Self, Task<Message>) {
        let tile_handles = pelt::thumbnails::generate_tile_pool()
            .into_iter()
            .map(|bytes| image::Handle::from_rgba(THUMB_TILE_SIZE, THUMB_TILE_SIZE, bytes))
            .collect();
        let loupe_handles = pelt::loupe::generate_loupe_set()
            .into_iter()
            .map(|bytes| image::Handle::from_rgba(LOUPE_WIDTH, LOUPE_HEIGHT, bytes))
            .collect();

        (
            Self {
                tile_handles,
                loupe_handles,
                loupe_index: 0,
                exposure_stops: 0.0,
                vibrance: 0.0,
                wb_gain: [1.0, 1.0, 1.0],
                grid_scroll_offset: 0.0,
            },
            Task::none(),
        )
    }

    fn update(&mut self, message: Message) {
        match message {
            Message::NextImage => {
                self.loupe_index = (self.loupe_index + 1) % self.loupe_handles.len();
            }
            Message::PrevImage => {
                self.loupe_index =
                    (self.loupe_index + self.loupe_handles.len() - 1) % self.loupe_handles.len();
            }
            Message::ExposureChanged(value) => self.exposure_stops = value,
            Message::ViewportDragged { dx, dy } => {
                self.vibrance = (self.vibrance + dx).clamp(-1.0, 1.0);
                self.wb_gain[2] = (self.wb_gain[2] + dy).clamp(0.2, 2.0);
            }
            Message::GridScrolled(viewport) => {
                self.grid_scroll_offset = viewport.absolute_offset().y;
            }
        }
    }

    fn subscription(&self) -> iced::Subscription<Message> {
        iced::keyboard::listen().filter_map(|event| match event {
            keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::ArrowRight),
                ..
            } => Some(Message::NextImage),
            keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::ArrowLeft),
                ..
            } => Some(Message::PrevImage),
            _ => None,
        })
    }

    fn view(&self) -> Element<'_, Message> {
        let grid = self.view_grid();
        let loupe = self.view_loupe();
        let viewport = self.view_viewport();

        row![grid, column![loupe, viewport].width(Length::Fill),].into()
    }

    fn view_grid(&self) -> Element<'_, Message> {
        let layout = GridLayout::new(GRID_CELL_COUNT, THUMB_TILE_SIZE as f32, GRID_VIEWPORT_WIDTH);
        let visible =
            layout.visible_range(self.grid_scroll_offset, GRID_VIEWPORT_HEIGHT, OVERSCAN_ROWS);

        let start_row = visible.start / layout.columns;
        let end_row = visible.end.div_ceil(layout.columns.max(1));
        let top_spacer_height = start_row as f32 * THUMB_TILE_SIZE as f32;
        let bottom_spacer_height =
            (layout.rows().saturating_sub(end_row)) as f32 * THUMB_TILE_SIZE as f32;

        let mut rows_col = column![Space::new().height(Length::Fixed(top_spacer_height))];
        for row_index in start_row..end_row {
            let mut row_widgets = row![];
            for col in 0..layout.columns {
                let cell = row_index * layout.columns + col;
                if cell >= GRID_CELL_COUNT {
                    break;
                }
                let tile_index = pelt::config::cell_to_tile_index(cell);
                row_widgets = row_widgets.push(
                    image(self.tile_handles[tile_index].clone())
                        .width(Length::Fixed(THUMB_TILE_SIZE as f32))
                        .height(Length::Fixed(THUMB_TILE_SIZE as f32)),
                );
            }
            rows_col = rows_col.push(row_widgets);
        }
        rows_col = rows_col.push(Space::new().height(Length::Fixed(bottom_spacer_height)));

        scrollable(rows_col)
            .width(Length::Fixed(GRID_VIEWPORT_WIDTH))
            .height(Length::Fixed(GRID_VIEWPORT_HEIGHT))
            .on_scroll(Message::GridScrolled)
            .into()
    }

    fn view_loupe(&self) -> Element<'_, Message> {
        column![
            text(format!(
                "Loupe {}/{} (Right/Left to switch)",
                self.loupe_index + 1,
                self.loupe_handles.len()
            )),
            image(self.loupe_handles[self.loupe_index].clone()).height(Length::Fixed(200.0)),
        ]
        .into()
    }

    fn view_viewport(&self) -> Element<'_, Message> {
        let params = LiveChainParams {
            wb_gain: self.wb_gain,
            exposure_stops: self.exposure_stops,
            vibrance: self.vibrance,
            _pad: [0.0; 3],
        };

        column![
            text("Develop viewport (live_chain compute)"),
            slider(-2.0..=2.0, self.exposure_stops, Message::ExposureChanged),
            shader(ViewportProgram { params })
                .width(Length::Fixed(960.0))
                .height(Length::Fixed(540.0)),
        ]
        .into()
    }
}
