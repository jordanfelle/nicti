//! The egui side of the library grid (#30): draws only the visible rows via
//! `ScrollArea::show_rows` (ADR-0068's virtualization primitive), handles keyboard/mouse
//! navigation, and reports when the user asks to open an image. All catalog and job work lives in
//! [`GridSession`]; this file is layout and input.

use egui::{Color32, Rect, Sense, Vec2};
use nicti_lair::AssetMeta;
use nicti_pounce::Pounce;

use super::layout::GridLayout;
use super::session::GridSession;
use crate::cull::badges::paint_marks;
use crate::cull::CullState;

/// Outer size of one cell (thumbnail plus gap), in points.
pub const CELL: f32 = 168.0;
/// Empty margin inside a cell around the thumbnail.
const GAP: f32 = 4.0;

/// Per-view scroll bookkeeping the UI keeps between frames (the session itself is UI-agnostic).
#[derive(Default)]
pub struct ViewState {
    scroll_y: f32,
    viewport_h: f32,
    /// Set when the cursor moved by keyboard (or from the loupe): the next frame scrolls just far
    /// enough to bring its row into view.
    reveal_cursor: bool,
}

impl ViewState {
    pub fn reveal_cursor(&mut self) {
        self.reveal_cursor = true;
    }
}

/// What the user asked for this frame.
#[derive(Default)]
pub struct GridOutcome {
    /// Index into `GridSession::ids()` to open in the loupe.
    pub open: Option<usize>,
}

/// The scroll offset that brings `row` fully into a viewport of height `viewport_h` currently
/// scrolled to `scroll_y`, moving as little as possible. Pure, so the edge cases are testable.
pub fn scroll_to_reveal(row: usize, row_h: f32, scroll_y: f32, viewport_h: f32) -> f32 {
    let top = row as f32 * row_h;
    let bottom = top + row_h;
    if top < scroll_y {
        top
    } else if bottom > scroll_y + viewport_h {
        // Align the row's bottom edge, but never scroll past its own top: in a viewport shorter
        // than one row, showing the row's top is the useful half.
        (bottom - viewport_h).clamp(0.0, top)
    } else {
        scroll_y
    }
}

/// Where a navigation key moves the cursor, `None` for a key that isn't navigation. Clamped to
/// `0..total`; with no cursor yet, any movement lands on the first cell.
pub fn navigate(
    key: egui::Key,
    cursor: Option<usize>,
    total: usize,
    cols: usize,
    rows_per_page: usize,
) -> Option<usize> {
    if total == 0 {
        return None;
    }
    let last = total - 1;
    let Some(c) = cursor else {
        return matches!(
            key,
            egui::Key::ArrowLeft
                | egui::Key::ArrowRight
                | egui::Key::ArrowUp
                | egui::Key::ArrowDown
                | egui::Key::Home
                | egui::Key::End
                | egui::Key::PageUp
                | egui::Key::PageDown
        )
        .then_some(0);
    };
    let page = cols * rows_per_page.max(1);
    Some(match key {
        egui::Key::ArrowLeft => c.saturating_sub(1),
        egui::Key::ArrowRight => (c + 1).min(last),
        egui::Key::ArrowUp => c.saturating_sub(cols),
        egui::Key::ArrowDown => (c + cols).min(last),
        egui::Key::PageUp => c.saturating_sub(page),
        egui::Key::PageDown => (c + page).min(last),
        egui::Key::Home => 0,
        egui::Key::End => last,
        _ => return None,
    })
}

const NAV_KEYS: [egui::Key; 8] = [
    egui::Key::ArrowLeft,
    egui::Key::ArrowRight,
    egui::Key::ArrowUp,
    egui::Key::ArrowDown,
    egui::Key::PageUp,
    egui::Key::PageDown,
    egui::Key::Home,
    egui::Key::End,
];

pub fn show(
    ui: &mut egui::Ui,
    session: &mut GridSession,
    state: &mut ViewState,
    cull: &mut CullState,
    pounce: &Pounce,
    flag_stale: bool,
) -> GridOutcome {
    let mut outcome = GridOutcome::default();
    session.poll(ui.ctx(), pounce);

    if let Some(err) = session.last_error() {
        ui.colored_label(Color32::RED, format!("Couldn't read the catalog: {err}"));
    }
    if !session.is_loaded() {
        if session.is_loading() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Reading catalog...");
            });
        } else if ui.button("Retry").clicked() {
            // The first read failed (the error is shown above): don't pretend the library is
            // empty, and don't resubmit on its own every frame -- ask again on request.
            session.reload(pounce);
        }
        return outcome;
    }
    let total = session.len();
    if session.is_empty() {
        ui.label("No images here yet. Import a folder to get started.");
        return outcome;
    }

    // Rows are placed edge to edge (the gap is inside each cell), so `show_rows`' row height is
    // exactly `CELL` and the scroll math in `scroll_to_reveal` holds.
    ui.spacing_mut().item_spacing = Vec2::ZERO;
    let scrollbar_w = ui.spacing().scroll.allocated_width();
    let layout = GridLayout::new(ui.available_width() - scrollbar_w, CELL);
    let rows_per_page = ((state.viewport_h / CELL).floor() as usize).max(1);

    // Keyboard navigation and selection -- skipped while a text field (the folder box) has focus.
    // Marking keys (stars, pick/reject, labels, undo) are handled app-wide, not here.
    if !ui.ctx().egui_wants_keyboard_input() {
        let mut open_cursor = false;
        let mut moved_to = None;
        let mut extend = false;
        let mut select_all = false;
        let mut clear = false;
        ui.input_mut(|i| {
            extend = i.modifiers.shift;
            for key in NAV_KEYS {
                if i.key_pressed(key) {
                    moved_to = navigate(key, session.cursor(), total, layout.cols, rows_per_page)
                        .or(moved_to);
                }
            }
            open_cursor = i.key_pressed(egui::Key::Enter);
            select_all = i.consume_key(egui::Modifiers::COMMAND, egui::Key::A);
            clear = i.key_pressed(egui::Key::Escape);
        });
        if let Some(index) = moved_to {
            // Shift+arrow grows the selection from its anchor; a plain arrow drops it, like
            // clicking the destination.
            if extend {
                session.select_range_to(index);
            } else {
                session.click(index);
            }
            state.reveal_cursor = true;
        }
        if select_all {
            session.select_all();
        }
        if clear && session.has_selection() {
            session.clear_selection();
        }
        if open_cursor {
            outcome.open = session.cursor();
        }
    }

    let mut scroll = egui::ScrollArea::vertical().auto_shrink([false, false]);
    if state.reveal_cursor {
        state.reveal_cursor = false;
        if let Some(cursor) = session.cursor() {
            scroll = scroll.vertical_scroll_offset(scroll_to_reveal(
                layout.row_of(cursor),
                CELL,
                state.scroll_y,
                state.viewport_h,
            ));
        }
    }

    let cursor = session.cursor();
    let mut visible_rows = 0..0;
    let output = scroll.show_rows(ui, CELL, layout.rows(total), |ui, rows| {
        visible_rows = rows.clone();
        for row in rows {
            ui.horizontal(|ui| {
                for col in 0..layout.cols {
                    let index = row * layout.cols + col;
                    if index >= total {
                        break;
                    }
                    let id = session.ids()[index];
                    let (rect, response) =
                        ui.allocate_exact_size(Vec2::splat(CELL), Sense::click());
                    // Cells are custom-painted, so without this they have no accessibility node: a
                    // screen reader (and the headless UI harness, `swat/`) couldn't find them.
                    // Only evaluated while AccessKit is on.
                    response.widget_info(|| {
                        egui::WidgetInfo::selected(
                            egui::WidgetType::SelectableLabel,
                            true,
                            session.is_selected(index),
                            format!("Photo {id}"),
                        )
                    });
                    let marks = Marks {
                        cursor: cursor == Some(index),
                        selected: session.is_selected(index),
                        meta: cull.meta(id),
                        stale: flag_stale && session.is_edited(id),
                    };
                    paint_cell(ui, session, id, rect, &marks);
                    if response.clicked() {
                        let mods = ui.input(|i| i.modifiers);
                        if mods.command {
                            session.toggle_select(index);
                        } else if mods.shift {
                            session.select_range_to(index);
                        } else {
                            session.click(index);
                        }
                    }
                    if response.double_clicked() {
                        session.click(index);
                        outcome.open = Some(index);
                    }
                }
            });
        }
    });
    state.scroll_y = output.state.offset.y;
    state.viewport_h = output.inner_rect.height();

    let visible = layout.indices_for_rows(visible_rows, total);
    // Markers for what is on screen (plus the same overscan the thumbnails get), read off-thread.
    cull.ensure(
        session.ids()[visible.clone()]
            .iter()
            .copied()
            .chain(session.cursor().map(|c| session.ids()[c])),
    );
    session.request_visible(visible, pounce);
    outcome
}

/// What a cell shows besides its thumbnail.
struct Marks<'a> {
    /// The keyboard/click focus.
    cursor: bool,
    /// Part of the multi-selection.
    selected: bool,
    /// Rating, pick/reject and label, once read; `None` draws nothing (never a guess).
    meta: Option<&'a AssetMeta>,
    /// The photo has edits its camera-derived thumbnail doesn't show (#145).
    stale: bool,
}

fn paint_cell(ui: &egui::Ui, session: &mut GridSession, id: i64, rect: Rect, marks: &Marks<'_>) {
    let inner = rect.shrink(GAP);
    let painter = ui.painter_at(rect);
    let visuals = ui.visuals();
    painter.rect_filled(inner, 2.0, visuals.extreme_bg_color);

    if let Some(texture) = session.texture(id) {
        let [w, h] = texture.size();
        if w > 0 && h > 0 {
            // Fit inside the cell preserving aspect, centred.
            let scale = (inner.width() / w as f32).min(inner.height() / h as f32);
            let size = Vec2::new(w as f32 * scale, h as f32 * scale);
            let image_rect = Rect::from_center_size(inner.center(), size);
            painter.image(
                texture.id(),
                image_rect,
                Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                Color32::WHITE,
            );
        }
    } else if session.has_failed(id) {
        painter.text(
            inner.center(),
            egui::Align2::CENTER_CENTER,
            "no preview",
            egui::FontId::proportional(11.0),
            visuals.weak_text_color(),
        );
    }

    if let Some(meta) = marks.meta {
        paint_marks(&painter, inner, meta);
    }
    if marks.stale {
        crate::cull::badges::paint_stale(&painter, inner, crate::eyeshine::Badge::Stale);
    }

    if marks.selected {
        let tint = visuals.selection.bg_fill.gamma_multiply(0.35);
        painter.rect_filled(inner, 2.0, tint);
    }
    if marks.cursor || marks.selected {
        let width = if marks.cursor { 2.5 } else { 1.5 };
        painter.rect_stroke(
            inner,
            2.0,
            egui::Stroke::new(width, visuals.selection.stroke.color),
            egui::StrokeKind::Inside,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::Key;

    #[test]
    fn reveal_moves_the_minimum_distance() {
        // 100-tall rows, 300-tall viewport scrolled to 200 shows rows 2..5 (200..500).
        assert_eq!(scroll_to_reveal(3, 100.0, 200.0, 300.0), 200.0); // already visible
        assert_eq!(scroll_to_reveal(2, 100.0, 200.0, 300.0), 200.0); // top edge, fully visible
        assert_eq!(scroll_to_reveal(1, 100.0, 200.0, 300.0), 100.0); // above: align to its top
        assert_eq!(scroll_to_reveal(5, 100.0, 200.0, 300.0), 300.0); // below: align to its bottom
        assert_eq!(scroll_to_reveal(0, 100.0, 200.0, 300.0), 0.0);
    }

    #[test]
    fn reveal_never_scrolls_negative_with_a_tall_row_or_short_viewport() {
        assert_eq!(scroll_to_reveal(0, 100.0, 0.0, 50.0), 0.0);
        assert!(scroll_to_reveal(0, 100.0, 40.0, 0.0) >= 0.0);
    }

    #[test]
    fn arrows_move_by_one_cell_and_by_a_row() {
        // 5 columns, 23 items (last row partial).
        let nav = |key, cursor| navigate(key, cursor, 23, 5, 3);
        assert_eq!(nav(Key::ArrowRight, Some(4)), Some(5));
        assert_eq!(nav(Key::ArrowLeft, Some(5)), Some(4));
        assert_eq!(nav(Key::ArrowDown, Some(2)), Some(7));
        assert_eq!(nav(Key::ArrowUp, Some(7)), Some(2));
    }

    #[test]
    fn navigation_clamps_at_both_ends_including_a_partial_last_row() {
        let nav = |key, cursor| navigate(key, cursor, 23, 5, 3);
        assert_eq!(nav(Key::ArrowLeft, Some(0)), Some(0));
        assert_eq!(nav(Key::ArrowUp, Some(3)), Some(0));
        assert_eq!(nav(Key::ArrowRight, Some(22)), Some(22));
        // Down from row 3 (15..19) into the partial last row (20..22) past its end clamps to it.
        assert_eq!(nav(Key::ArrowDown, Some(19)), Some(22));
        assert_eq!(nav(Key::ArrowDown, Some(22)), Some(22));
    }

    #[test]
    fn paging_and_home_end() {
        let nav = |key, cursor| navigate(key, cursor, 100, 5, 4);
        assert_eq!(nav(Key::PageDown, Some(0)), Some(20));
        assert_eq!(nav(Key::PageUp, Some(20)), Some(0));
        assert_eq!(nav(Key::PageUp, Some(3)), Some(0));
        assert_eq!(nav(Key::PageDown, Some(95)), Some(99));
        assert_eq!(nav(Key::Home, Some(50)), Some(0));
        assert_eq!(nav(Key::End, Some(50)), Some(99));
    }

    #[test]
    fn with_no_cursor_a_navigation_key_selects_the_first_cell_and_others_do_nothing() {
        assert_eq!(navigate(Key::ArrowDown, None, 10, 3, 2), Some(0));
        assert_eq!(navigate(Key::End, None, 10, 3, 2), Some(0));
        assert_eq!(navigate(Key::A, None, 10, 3, 2), None);
        assert_eq!(navigate(Key::A, Some(4), 10, 3, 2), None);
    }

    #[test]
    fn an_empty_grid_never_navigates() {
        assert_eq!(navigate(Key::ArrowDown, None, 0, 3, 2), None);
        assert_eq!(navigate(Key::Home, Some(0), 0, 3, 2), None);
    }
}
