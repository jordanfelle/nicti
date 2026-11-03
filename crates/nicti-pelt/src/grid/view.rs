//! The egui side of the library grid (#30): draws only the visible rows via
//! `ScrollArea::show_rows` (ADR-0068's virtualization primitive), handles keyboard/mouse
//! navigation, and reports when the user asks to open an image. All catalog and job work lives in
//! [`GridSession`]; this file is layout and input.

use std::sync::Arc;
use std::time::Duration;

use egui::{Color32, Rect, Sense, Vec2};
use nicti_lair::pounce_jobs::ReportSlot;
use nicti_lair::{AssetMeta, SortField};
use nicti_pounce::{JobId, Pounce};

use super::justified::{Dir, IndexKey, LayoutGate, RowIndex, RowParams};
use super::layout::GridLayout;
use super::layout_build::{BuiltLayout, LayoutJob, LayoutResult};
use super::session::GridSession;
use crate::cull::badges::paint_marks;
use crate::cull::CullState;

/// Outer size of one cell (thumbnail plus gap), in points.
pub const CELL: f32 = 168.0;
/// Empty margin inside a cell around the thumbnail.
const GAP: f32 = 4.0;

/// The height a justified row aims for, the gap between cells and rows, and the height of a date
/// header (#656). Untuned: the reference-machine pass picks the real numbers.
pub const JUSTIFIED_ROW_HEIGHT: f32 = 160.0;
const JUSTIFIED_GAP: f32 = 4.0;
const JUSTIFIED_HEADER_HEIGHT: f32 = 28.0;

/// Per-view scroll bookkeeping the UI keeps between frames (the session itself is UI-agnostic).
pub struct ViewState {
    /// The last scroll offset of each body. They are separate scroll areas (own `id_salt`), so
    /// falling back to the square grid while a layout rebuilds does not lose the justified
    /// position.
    square_scroll_y: f32,
    pub(crate) justified_scroll_y: f32,
    viewport_h: f32,
    /// Set when the cursor moved by keyboard (or from the loupe): the next frame scrolls just far
    /// enough to bring its row into view.
    reveal_cursor: bool,
    /// Justified rows instead of the square grid (#656). Off by default.
    pub justified: bool,
    /// Date-group headers in the justified layout (only drawn while sorted by capture time).
    pub grouped: bool,
    layout: LayoutState,
    /// Cumulative per-frame work counters (ADR-0437), read by the swat tests.
    pub(crate) stats: GridStats,
}

impl Default for ViewState {
    fn default() -> Self {
        ViewState {
            square_scroll_y: 0.0,
            justified_scroll_y: 0.0,
            viewport_h: 0.0,
            reveal_cursor: false,
            justified: false,
            grouped: true,
            layout: LayoutState::default(),
            stats: GridStats::default(),
        }
    }
}

impl ViewState {
    pub fn reveal_cursor(&mut self) {
        self.reveal_cursor = true;
    }

    /// Whether the justified layout is on and a layout for the session's current list is ready.
    #[cfg(test)]
    pub(crate) fn justified_ready(&self, session: &GridSession) -> bool {
        self.justified && self.layout.usable(session.generation()).is_some()
    }
}

/// The justified layout's build state: the gate that decides when to rebuild, the job in flight,
/// and the last finished layout (kept on screen until the next one lands).
#[derive(Default)]
struct LayoutState {
    gate: LayoutGate,
    pending: Option<(JobId, ReportSlot<LayoutResult>)>,
    built: Option<Arc<BuiltLayout>>,
    error: Option<String>,
    /// The session generation the error happened in (a new list clears it and retries).
    error_generation: u64,
}

impl LayoutState {
    /// The finished layout, if it describes the session list of `generation`.
    fn usable(&self, generation: u64) -> Option<Arc<BuiltLayout>> {
        self.built
            .as_ref()
            .filter(|b| b.key.generation == generation)
            .cloned()
    }
}

/// Cumulative work counters of the grid (ADR-0437): tests assert that a frame's cost follows the
/// viewport, not the library size, so a regression to "build every cell" fails a number instead of
/// only showing up as jank at 1M photos.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GridStats {
    /// Frames drawn with a loaded, non-empty grid.
    pub frames: u64,
    /// Cells built (allocated and painted), across all frames.
    pub cells_built: u64,
    /// Justified layout builds started (#656): flat while nothing that affects the layout changes.
    pub layout_builds: u64,
    /// Frames drawn from a justified layout.
    pub justified_frames: u64,
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
    let top = f64::from(row as f32 * row_h);
    scroll_to_reveal_span(top, top + f64::from(row_h), scroll_y, viewport_h)
}

/// [`scroll_to_reveal`] for a row that spans `top..bottom` (rows of different heights, #656).
pub fn scroll_to_reveal_span(top: f64, bottom: f64, scroll_y: f32, viewport_h: f32) -> f32 {
    let (scroll, viewport) = (f64::from(scroll_y), f64::from(viewport_h));
    let out = if top < scroll {
        top
    } else if bottom > scroll + viewport {
        // Align the row's bottom edge, but never scroll past its own top: in a viewport shorter
        // than one row, showing the row's top is the useful half.
        (bottom - viewport).clamp(0.0, top)
    } else {
        scroll
    };
    out as f32
}

/// [`navigate`] for the justified layout (#656): left/right/home/end move by cell, up/down and
/// page keys by row through the row index. `page_h` is the viewport height.
pub fn navigate_justified(
    key: egui::Key,
    cursor: Option<usize>,
    index: &RowIndex,
    aspects: &[f32],
    page_h: f32,
) -> Option<usize> {
    let total = index.cells();
    if total == 0 {
        return None;
    }
    let Some(c) = cursor else {
        return navigate(key, None, total, 1, 1);
    };
    Some(match key {
        egui::Key::ArrowUp => index.navigate(c, Dir::Up, aspects),
        egui::Key::ArrowDown => index.navigate(c, Dir::Down, aspects),
        egui::Key::PageUp => index.navigate_page(c, Dir::Up, page_h, aspects),
        egui::Key::PageDown => index.navigate_page(c, Dir::Down, page_h, aspects),
        // The cell-wise keys behave exactly as in the square grid (columns are irrelevant to them).
        _ => return navigate(key, cursor, total, 1, 1),
    })
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
    if session.is_empty() {
        ui.label("No images here yet. Import a folder to get started.");
        return outcome;
    }

    layout_toolbar(ui, session, state);
    // The width the cells share: what is left of the panel after the scrollbar.
    let width = ui.available_width() - ui.spacing().scroll.allocated_width();
    if state.justified {
        layout_tick(ui, session, state, pounce, width);
    }
    // Rows are placed edge to edge (the gap is inside each cell), so `show_rows`' row height is
    // exactly `CELL` and the scroll math in `scroll_to_reveal` holds.
    ui.spacing_mut().item_spacing = Vec2::ZERO;

    let justified = state
        .justified
        .then(|| state.layout.usable(session.generation()))
        .flatten();
    if state.justified && justified.is_none() {
        if let Some(err) = state.layout.error.clone() {
            ui.horizontal(|ui| {
                ui.colored_label(Color32::RED, format!("Couldn't lay out the grid: {err}"));
                if ui.button("Retry").clicked() {
                    state.layout.error = None;
                    state.layout.gate.invalidate();
                }
            });
        } else {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Laying out the grid...");
            });
        }
    }
    let visible = match justified {
        Some(built) => show_justified(ui, session, state, cull, flag_stale, &built, &mut outcome),
        None => show_square(ui, session, state, cull, flag_stale, width, &mut outcome),
    };
    state.stats.frames += 1;

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

/// The Square | Justified switch and the date-header toggle (#656).
fn layout_toolbar(ui: &mut egui::Ui, session: &GridSession, state: &mut ViewState) {
    ui.horizontal(|ui| {
        let before = (state.justified, state.grouped);
        ui.selectable_value(&mut state.justified, false, "Square");
        ui.selectable_value(&mut state.justified, true, "Justified");
        if state.justified {
            // Headers are capture days, so they only make sense in capture order.
            let by_date = session.applied_sort().field == SortField::Captured;
            ui.add_enabled(
                by_date,
                egui::Checkbox::new(&mut state.grouped, "Date headers"),
            )
            .on_disabled_hover_text("Date headers need the capture-time sort");
        }
        if before != (state.justified, state.grouped) {
            // A different layout was asked for: build it now, and forget a failed attempt.
            state.layout.gate.invalidate();
            state.layout.error = None;
        }
    });
}

/// Starts a layout build when the key (list generation, width bucket, row height, headers)
/// changed and lands a finished one. The build runs over the session's own id list, so a finished
/// layout always describes the list of its generation; it is shown only while that generation is
/// still current (a list change falls back to the square grid until the new layout lands).
fn layout_tick(
    ui: &egui::Ui,
    session: &mut GridSession,
    state: &mut ViewState,
    pounce: &Pounce,
    width: f32,
) {
    // Headers follow the sort the ids are *in*, not the one just asked for.
    let grouped = state.grouped && session.applied_sort().field == SortField::Captured;
    let params = RowParams {
        width,
        target_height: JUSTIFIED_ROW_HEIGHT,
        gap: JUSTIFIED_GAP,
        header_height: if grouped {
            JUSTIFIED_HEADER_HEIGHT
        } else {
            0.0
        },
    };
    let layout = &mut state.layout;
    if let Some((_, slot)) = &layout.pending {
        let landed = slot.lock().unwrap().take();
        if let Some(result) = landed {
            layout.pending = None;
            match result {
                Ok(built) if built.index.is_complete() => {
                    layout.error = None;
                    layout.built = Some(Arc::new(built));
                }
                Ok(_) => {
                    layout.error = Some("the layout build did not finish".to_string());
                    layout.error_generation = session.generation();
                }
                Err(e) => {
                    layout.error = Some(e.message);
                    layout.error_generation = e.generation;
                }
            }
        }
    }
    // A failure is retried once the list changes (or on request), not on every frame.
    if layout.error.is_some() && layout.error_generation != session.generation() {
        layout.error = None;
    }
    let key = IndexKey::new(session.generation(), grouped, params);
    if layout.error.is_none() && layout.gate.request(key) {
        if let Some((old, _)) = layout.pending.take() {
            pounce.cancel(old);
        }
        let (job, slot) = LayoutJob::new(session.store(), session.ids().to_vec(), key, params);
        layout.pending = Some((pounce.submit(Box::new(job)), slot));
    }
    state.stats.layout_builds = state.layout.gate.layout_builds;
    if state.layout.pending.is_some() {
        // The job's completion doesn't wake the UI by itself.
        ui.ctx().request_repaint_after(Duration::from_millis(60));
    }
}

/// Keyboard handling shared by both layouts -- skipped while a text field (the folder box) has
/// focus. `nav` maps a navigation key to the cell the cursor moves to. Marking keys (stars,
/// pick/reject, labels, undo) are handled app-wide, not here.
fn handle_keys(
    ui: &mut egui::Ui,
    session: &mut GridSession,
    state: &mut ViewState,
    outcome: &mut GridOutcome,
    nav: &dyn Fn(egui::Key, Option<usize>) -> Option<usize>,
) {
    if ui.ctx().egui_wants_keyboard_input() {
        return;
    }
    let mut open_cursor = false;
    let mut moved_to = None;
    let mut extend = false;
    let mut select_all = false;
    let mut clear = false;
    ui.input_mut(|i| {
        extend = i.modifiers.shift;
        for key in NAV_KEYS {
            if i.key_pressed(key) {
                moved_to = nav(key, session.cursor()).or(moved_to);
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

/// One cell: its accessibility node, painting, and click/double-click handling. `rect` is the full
/// cell, `margin` the empty border inside it (the square grid keeps a gap there; justified rows
/// carry their gap in the layout instead).
#[allow(clippy::too_many_arguments)]
fn show_cell(
    ui: &mut egui::Ui,
    session: &mut GridSession,
    cull: &CullState,
    flag_stale: bool,
    index: usize,
    rect: Rect,
    margin: f32,
    outcome: &mut GridOutcome,
) {
    let id = session.ids()[index];
    let response = ui.interact(rect, ui.scope_id().with(("cell", index)), Sense::click());
    // Cells are custom-painted, so without this they have no accessibility node: a
    // screen reader (and the headless UI harness, `swat/`) couldn't find them.
    // Only evaluated while AccessKit is on.
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            // Same role egui 0.37's own `selectable_label` reports, so the grid
            // speaks the same dialect as the app's other selectable lists.
            egui::Role::Button,
            true,
            session.is_selected(index),
            format!("Photo {id}"),
        )
    });
    let marks = Marks {
        cursor: session.cursor() == Some(index),
        selected: session.is_selected(index),
        meta: cull.meta(id),
        stale: flag_stale && session.is_stale(id),
    };
    paint_cell(ui, session, id, rect, margin, &marks);
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

/// The square grid (`show_rows`); returns the visible cell range.
fn show_square(
    ui: &mut egui::Ui,
    session: &mut GridSession,
    state: &mut ViewState,
    cull: &mut CullState,
    flag_stale: bool,
    width: f32,
    outcome: &mut GridOutcome,
) -> std::ops::Range<usize> {
    let total = session.len();
    let layout = GridLayout::new(width, CELL);
    let rows_per_page = ((state.viewport_h / CELL).floor() as usize).max(1);
    handle_keys(ui, session, state, outcome, &|key, cursor| {
        navigate(key, cursor, total, layout.cols, rows_per_page)
    });

    let mut scroll = egui::ScrollArea::vertical()
        .id_salt("nicti_grid_square")
        .auto_shrink([false, false]);
    // While the justified layout is only waiting to be built this body is a stand-in: leave the
    // request for the justified body, which can place the cursor's row.
    if state.reveal_cursor && !state.justified {
        state.reveal_cursor = false;
        if let Some(cursor) = session.cursor() {
            scroll = scroll.vertical_scroll_offset(scroll_to_reveal(
                layout.row_of(cursor),
                CELL,
                state.square_scroll_y,
                state.viewport_h,
            ));
        }
    }

    let mut cells_built = 0u64;
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
                    cells_built += 1; // ADR-0437: swat test asserts this stays <30/frame
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(CELL), Sense::hover());
                    show_cell(ui, session, cull, flag_stale, index, rect, GAP, outcome);
                }
            });
        }
    });
    state.stats.cells_built += cells_built; // ADR-0437: asserted <30/frame in swat/tests_grid.rs
    state.square_scroll_y = output.state.clamped_offset().y;
    state.viewport_h = output.inner_rect.height();
    layout.indices_for_rows(visible_rows, total)
}

/// The justified layout (#656): a `show_viewport` loop over the row index's `rows_between`, the
/// date headers, and navigation through the index. Returns the visible cell range.
fn show_justified(
    ui: &mut egui::Ui,
    session: &mut GridSession,
    state: &mut ViewState,
    cull: &mut CullState,
    flag_stale: bool,
    built: &BuiltLayout,
    outcome: &mut GridOutcome,
) -> std::ops::Range<usize> {
    let (index, aspects) = (&built.index, built.aspects.as_slice());
    let page_h = state.viewport_h;
    handle_keys(ui, session, state, outcome, &|key, cursor| {
        navigate_justified(key, cursor, index, aspects, page_h)
    });

    let mut scroll = egui::ScrollArea::vertical()
        .id_salt("nicti_grid_justified")
        .auto_shrink([false, false]);
    if state.reveal_cursor {
        state.reveal_cursor = false;
        if let Some(row) = session.cursor().and_then(|c| index.row_of(c)) {
            let top = index.row_top(row);
            let bottom = top
                + f64::from(index.row_height(row))
                + if index.header_of_row(row).is_some() {
                    f64::from(index.params().header_height)
                } else {
                    0.0
                };
            scroll = scroll.vertical_scroll_offset(scroll_to_reveal_span(
                top,
                bottom,
                state.justified_scroll_y,
                state.viewport_h,
            ));
        }
    }

    let mut cells_built = 0u64;
    let mut visible_rows = 0..0;
    let output = scroll.show_viewport(ui, |ui, viewport| {
        let content = Vec2::new(ui.available_width(), index.content_height() as f32);
        let (content_rect, _) = ui.allocate_exact_size(content, Sense::hover());
        // Content y of a cell, as screen y: subtract the scroll offset in f64 first (rows sit
        // ~10^7 points down at 1M photos, where an f32 can no longer tell neighbours apart).
        let scroll_y = f64::from(viewport.min.y);
        let screen_top = content_rect.min.y + viewport.min.y;
        let to_screen = |y: f64| screen_top + (y - scroll_y) as f32;
        let rows = index.rows_between(scroll_y, f64::from(viewport.max.y));
        visible_rows = rows.clone();
        for row in rows {
            if let Some(group) = index.header_of_row(row) {
                let top = to_screen(index.row_top(row));
                let header = Rect::from_min_size(
                    egui::pos2(content_rect.min.x, top),
                    Vec2::new(content.x, index.params().header_height),
                );
                ui.painter_at(header).text(
                    header.left_center() + Vec2::new(GAP, 0.0),
                    egui::Align2::LEFT_CENTER,
                    built.header_label(group),
                    egui::FontId::proportional(15.0),
                    ui.visuals().strong_text_color(),
                );
            }
            for (i, cell) in index.row_cells(row, aspects) {
                cells_built += 1;
                let rect = Rect::from_min_size(
                    egui::pos2(content_rect.min.x + cell.x, to_screen(cell.y)),
                    Vec2::new(cell.w, cell.h),
                );
                show_cell(ui, session, cull, flag_stale, i, rect, 0.0, outcome);
            }
        }
    });
    state.stats.cells_built += cells_built;
    state.stats.justified_frames += 1;
    state.justified_scroll_y = output.state.clamped_offset().y;
    state.viewport_h = output.inner_rect.height();
    index.indices_for_rows(visible_rows)
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

fn paint_cell(
    ui: &egui::Ui,
    session: &mut GridSession,
    id: i64,
    rect: Rect,
    margin: f32,
    marks: &Marks<'_>,
) {
    let inner = rect.shrink(margin);
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
    fn reveal_span_handles_rows_of_different_heights() {
        // A 250-tall row at 400..650 in a 300-tall viewport scrolled to 0 (0..300).
        assert_eq!(scroll_to_reveal_span(400.0, 650.0, 0.0, 300.0), 350.0);
        // Already visible: unchanged. Above the view: align to its top.
        assert_eq!(scroll_to_reveal_span(400.0, 650.0, 380.0, 300.0), 380.0);
        assert_eq!(scroll_to_reveal_span(400.0, 650.0, 500.0, 300.0), 400.0);
        // A row taller than the viewport shows its top, never a negative offset.
        assert_eq!(scroll_to_reveal_span(0.0, 900.0, 300.0, 300.0), 0.0);
        // Far down a 1M-photo list (f64 keeps neighbouring rows apart).
        let top = 123_456_789.0_f64;
        assert!(scroll_to_reveal_span(top, top + 160.0, 0.0, 600.0) > 0.0);
    }

    #[test]
    fn justified_navigation_moves_by_row_and_clamps() {
        use crate::grid::justified::{IndexKey, RowIndex};
        let params = IndexKey::bucketed(RowParams {
            width: 400.0,
            target_height: 100.0,
            gap: 0.0,
            header_height: 0.0,
        });
        // Squares at 100 high: four to a row, so cells 0-3, 4-7, 8-9.
        let aspects = vec![1.0f32; 10];
        let mut index = RowIndex::new(params);
        index.push_cells(0, &aspects).unwrap();
        index.finish();
        let nav = |key, cursor| navigate_justified(key, cursor, &index, &aspects, 100.0);
        assert_eq!(nav(Key::ArrowRight, Some(3)), Some(4));
        assert_eq!(nav(Key::ArrowLeft, Some(0)), Some(0));
        assert_eq!(nav(Key::ArrowDown, Some(1)), Some(5));
        assert_eq!(nav(Key::ArrowUp, Some(5)), Some(1));
        // Down into the short last row lands on its nearest cell, not past the end.
        assert_eq!(nav(Key::ArrowDown, Some(7)), Some(9));
        assert_eq!(nav(Key::End, Some(0)), Some(9));
        assert_eq!(nav(Key::Home, Some(9)), Some(0));
        assert_eq!(nav(Key::PageDown, Some(0)), Some(4));
        assert_eq!(nav(Key::ArrowDown, None), Some(0));
        assert_eq!(nav(Key::A, Some(2)), None);
        let empty = RowIndex::new(params);
        assert_eq!(
            navigate_justified(Key::ArrowDown, None, &empty, &[], 100.0),
            None
        );
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
