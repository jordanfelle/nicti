//! Survey (#32): a handful of photos side by side on one screen, to pick the keepers out of a
//! burst. Opened with `N` on the Library's selection. Works on the embedded previews only (see
//! `previews.rs`), so opening it costs a few JPEG decodes, never a RAW decode.
//!
//! Marking keys act on the *active* tile (arrows or a click move it) and are handled app-wide
//! like everywhere else. Nothing is auto-advanced here: in a survey the picture you just marked
//! stays where it is while you look at the rest.

use egui::{Key, Rect, Sense, Vec2};
use nicti_lair::CatalogStore;
use nicti_pounce::Pounce;

use super::previews::{TilePreviews, Want};
use super::tile::{paint_tile, tile_grid, view_uv, TileStyle};
use super::CullState;

/// Photos a survey shows at once. More than this and the tiles stop being big enough to judge.
pub const MAX_TILES: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurveySession {
    ids: Vec<i64>,
    active: usize,
    /// Columns at the last layout, so arrow-up/down know how far a row is.
    cols: usize,
    /// More photos were selected than fit; the rest are not shown.
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

impl SurveySession {
    /// `None` unless there are at least two photos to survey.
    pub fn new(ids: &[i64]) -> Option<Self> {
        if ids.len() < 2 {
            return None;
        }
        Some(SurveySession {
            ids: ids.iter().copied().take(MAX_TILES).collect(),
            active: 0,
            cols: 1,
            truncated: ids.len() > MAX_TILES,
        })
    }

    pub fn ids(&self) -> &[i64] {
        &self.ids
    }

    pub fn active(&self) -> usize {
        self.active
    }

    pub fn active_id(&self) -> Option<i64> {
        self.ids.get(self.active).copied()
    }

    pub fn set_active(&mut self, index: usize) {
        if index < self.ids.len() {
            self.active = index;
        }
    }

    /// Moves the active tile one step, staying put at an edge.
    pub fn move_active(&mut self, dir: Dir) {
        let last = self.ids.len().saturating_sub(1);
        let cols = self.cols.max(1);
        self.active = match dir {
            Dir::Left => self.active.saturating_sub(1),
            Dir::Right => (self.active + 1).min(last),
            Dir::Up => self.active.checked_sub(cols).unwrap_or(self.active),
            Dir::Down if self.active + cols <= last => self.active + cols,
            Dir::Down => self.active,
        };
    }

    /// Drops deleted photos. `false` once fewer than two remain: not a survey any more.
    pub fn remove(&mut self, gone: &[i64]) -> bool {
        let active_id = self.active_id();
        self.ids.retain(|id| !gone.contains(id));
        self.active = active_id
            .and_then(|id| self.ids.iter().position(|x| *x == id))
            .unwrap_or_else(|| self.active.min(self.ids.len().saturating_sub(1)));
        self.ids.len() >= 2
    }
}

/// What the user asked for from the survey this frame.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SurveyOutcome {
    pub exit: bool,
    /// Photo to open in the loupe.
    pub open: Option<i64>,
}

pub fn show(
    ui: &mut egui::Ui,
    session: &mut SurveySession,
    cull: &mut CullState,
    previews: &mut TilePreviews,
    store: &dyn CatalogStore,
    pounce: &Pounce,
) -> SurveyOutcome {
    let mut outcome = SurveyOutcome::default();
    previews.poll();
    cull.ensure(session.ids().iter().copied());

    ui.horizontal(|ui| {
        ui.heading("Survey");
        ui.label(format!("{} photos", session.ids().len()));
        if session.truncated {
            ui.label(format!("(first {MAX_TILES} of your selection)"));
        }
        if ui.button("Back to Library").clicked() {
            outcome.exit = true;
        }
        ui.weak("arrows or click pick a photo, 0-5 / P / X / labels mark it, Enter opens it, Esc returns");
    });

    if !ui.ctx().egui_wants_keyboard_input() {
        ui.input(|i| {
            for (key, dir) in [
                (Key::ArrowLeft, Dir::Left),
                (Key::ArrowRight, Dir::Right),
                (Key::ArrowUp, Dir::Up),
                (Key::ArrowDown, Dir::Down),
            ] {
                if i.key_pressed(key) {
                    session.move_active(dir);
                }
            }
            if i.key_pressed(Key::Enter) {
                outcome.open = session.active_id();
            }
            if i.key_pressed(Key::Escape) || i.key_pressed(Key::G) {
                outcome.exit = true;
            }
        });
    }

    let area = ui.available_rect_before_wrap();
    let (cols, rows) = tile_grid(session.ids().len(), area.width(), area.height(), 1.5);
    session.cols = cols.max(1);
    if cols == 0 || rows == 0 {
        return outcome;
    }
    let cell = Vec2::new(area.width() / cols as f32, area.height() / rows as f32);
    let uv = view_uv(false, [0.0, 0.0]);

    let ids = session.ids().to_vec();
    for (index, id) in ids.iter().enumerate() {
        let (col, row) = (index % cols, index / cols);
        let rect = Rect::from_min_size(
            area.min + Vec2::new(col as f32 * cell.x, row as f32 * cell.y),
            cell,
        )
        .shrink(3.0);
        let response = ui.interact(rect, ui.id().with(("survey-tile", *id)), Sense::click());
        let texture = previews
            .get(ui.ctx(), store, pounce, *id, Want::Small)
            .cloned();
        paint_tile(
            ui,
            rect,
            texture.as_ref(),
            uv,
            &TileStyle {
                label: None,
                meta: cull.meta(*id),
                active: index == session.active(),
            },
        );
        if response.clicked() {
            session.set_active(index);
        }
        if response.double_clicked() {
            session.set_active(index);
            outcome.open = Some(*id);
        }
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(n: i64) -> SurveySession {
        SurveySession::new(&(1..=n).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn a_survey_needs_at_least_two_photos() {
        assert!(SurveySession::new(&[]).is_none());
        assert!(SurveySession::new(&[7]).is_none());
        assert!(SurveySession::new(&[7, 8]).is_some());
    }

    #[test]
    fn a_survey_caps_at_sixteen_and_says_so() {
        let s = SurveySession::new(&(0..40).collect::<Vec<_>>()).unwrap();
        assert_eq!(s.ids().len(), MAX_TILES);
        assert!(s.truncated);
        assert!(!session(5).truncated);
    }

    #[test]
    fn arrows_move_within_the_layout_and_stop_at_the_edges() {
        let mut s = session(7);
        s.cols = 3; // rows: 1 2 3 / 4 5 6 / 7
        s.move_active(Dir::Right);
        assert_eq!(s.active(), 1);
        s.move_active(Dir::Down);
        assert_eq!(s.active(), 4, "one row down");
        s.move_active(Dir::Down);
        assert_eq!(s.active(), 4, "no row below a short last row's gap");
        s.set_active(6);
        s.move_active(Dir::Right);
        assert_eq!(s.active(), 6, "clamped at the last tile");
        s.move_active(Dir::Up);
        assert_eq!(s.active(), 3);
        s.set_active(0);
        s.move_active(Dir::Left);
        s.move_active(Dir::Up);
        assert_eq!(s.active(), 0);
    }

    #[test]
    fn set_active_ignores_an_out_of_range_tile() {
        let mut s = session(3);
        s.set_active(9);
        assert_eq!(s.active(), 0);
        s.set_active(2);
        assert_eq!(s.active_id(), Some(3));
    }

    #[test]
    fn removing_photos_keeps_the_active_one_and_ends_the_survey_below_two() {
        let mut s = session(5);
        s.set_active(3); // photo 4
        assert!(s.remove(&[1, 2]));
        assert_eq!(s.active_id(), Some(4), "the active photo stays active");
        assert_eq!(s.ids(), &[3, 4, 5]);
        // Removing the active photo lands on a neighbour, not out of bounds.
        assert!(s.remove(&[4]));
        assert!(s.active_id().is_some());
        assert!(!s.remove(&[3]), "one photo left is not a survey");
    }
}
