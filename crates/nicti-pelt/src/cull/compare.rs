//! Compare (#32): two photos side by side -- the best one so far (*select*) against the next
//! contender (*candidate*) -- with synchronised zoom and pan, for picking one out of a burst.
//! Opened with `C`.
//!
//! Right/Left step the candidate through the list (skipping the select), Up promotes the candidate
//! to select, Tab switches which tile the marking keys act on. Marking auto-advances the
//! candidate when the candidate was the one marked, so a burst is "rate, rate, rate" with the
//! keeper staying put on the left. Previews only (see `previews.rs`).

use egui::{Key, Rect, Sense, Vec2};
use nicti_lair::CatalogStore;
use nicti_pounce::Pounce;

use super::previews::{TilePreviews, Want};
use super::tile::{paint_tile, pan_after_drag, view_uv, TileStyle};
use super::CullState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Select,
    Candidate,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompareSession {
    ids: Vec<i64>,
    select: usize,
    candidate: usize,
    /// Which tile the marking keys act on.
    pub active: Side,
    pub zoomed: bool,
    pan: [f32; 2],
}

impl CompareSession {
    /// `None` unless there are at least two photos to compare.
    pub fn new(ids: Vec<i64>) -> Option<Self> {
        if ids.len() < 2 {
            return None;
        }
        Some(CompareSession {
            ids,
            select: 0,
            candidate: 1,
            active: Side::Candidate,
            zoomed: false,
            pan: [0.0, 0.0],
        })
    }

    pub fn ids(&self) -> &[i64] {
        &self.ids
    }

    pub fn select_id(&self) -> i64 {
        self.ids[self.select]
    }

    pub fn candidate_id(&self) -> i64 {
        self.ids[self.candidate]
    }

    /// The photo marking keys act on.
    pub fn active_id(&self) -> i64 {
        match self.active {
            Side::Select => self.select_id(),
            Side::Candidate => self.candidate_id(),
        }
    }

    pub fn pan(&self) -> [f32; 2] {
        self.pan
    }

    pub fn set_pan(&mut self, pan: [f32; 2]) {
        self.pan = pan;
    }

    /// 1-based position of the candidate among the contenders (everything but the select), and
    /// how many contenders there are.
    pub fn candidate_position(&self) -> (usize, usize) {
        let before = usize::from(self.select < self.candidate);
        (self.candidate + 1 - before, self.ids.len() - 1)
    }

    /// Moves the candidate `delta` photos along the list, jumping over the select. `false`
    /// (and nothing changes) at either end.
    pub fn step_candidate(&mut self, delta: isize) -> bool {
        if delta == 0 {
            return false;
        }
        let mut next = self.candidate as isize + delta;
        if next == self.select as isize {
            next += delta;
        }
        if next < 0 || next >= self.ids.len() as isize {
            return false;
        }
        self.candidate = next as usize;
        true
    }

    /// Auto-advance after the candidate was marked.
    pub fn advance_candidate(&mut self) -> bool {
        self.step_candidate(1)
    }

    /// The candidate becomes the select; a new candidate is the next contender after it (or the
    /// one before it, at the end of the list).
    pub fn promote(&mut self) {
        let old = self.candidate;
        self.select = old;
        if !self.step_candidate(1) {
            // At the end: fall back to the closest photo before it.
            let mut prev = old.checked_sub(1);
            if prev == Some(self.select) {
                prev = prev.and_then(|p| p.checked_sub(1));
            }
            if let Some(p) = prev {
                self.candidate = p;
            }
        }
    }

    pub fn toggle_active(&mut self) {
        self.active = match self.active {
            Side::Select => Side::Candidate,
            Side::Candidate => Side::Select,
        };
    }

    /// Drops deleted photos. `false` once fewer than two remain.
    ///
    /// Whichever of the select and the candidate survives keeps its place -- deleting the
    /// candidate must not silently swap out the keeper the user chose. If the select itself was
    /// deleted, the candidate becomes the select; the new candidate is the nearest surviving photo
    /// after the old candidate's place (or before it, at the end).
    pub fn remove(&mut self, gone: &[i64]) -> bool {
        let (sel, cand) = (self.select_id(), self.candidate_id());
        let old_cand_at = self.candidate;
        self.ids.retain(|id| !gone.contains(id));
        if self.ids.len() < 2 {
            return false;
        }
        let pos = |id: i64, ids: &[i64]| ids.iter().position(|i| *i == id);
        let (s, c) = (pos(sel, &self.ids), pos(cand, &self.ids));
        let last = self.ids.len() - 1;
        // The nearest photo to `near` that is not `avoid`, preferring later ones.
        let nearest = |near: usize, avoid: usize| {
            let near = near.min(last);
            (near..=last)
                .chain((0..near).rev())
                .find(|i| *i != avoid)
                .unwrap_or(0)
        };
        match (s, c) {
            (Some(s), Some(c)) => {
                self.select = s;
                self.candidate = c;
            }
            (Some(s), None) => {
                self.select = s;
                self.candidate = nearest(old_cand_at, s);
            }
            (None, Some(c)) => {
                self.select = c;
                self.candidate = nearest(c + 1, c);
            }
            (None, None) => {
                self.select = 0;
                self.candidate = 1;
            }
        }
        true
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct CompareOutcome {
    pub exit: bool,
}

pub fn show(
    ui: &mut egui::Ui,
    session: &mut CompareSession,
    cull: &mut CullState,
    previews: &mut TilePreviews,
    store: &dyn CatalogStore,
    pounce: &Pounce,
) -> CompareOutcome {
    let mut outcome = CompareOutcome::default();
    previews.poll();
    cull.ensure([session.select_id(), session.candidate_id()]);

    ui.horizontal(|ui| {
        ui.heading("Compare");
        let (pos, total) = session.candidate_position();
        ui.label(format!("candidate {pos} of {total}"));
        if ui.button("Previous").clicked() {
            session.step_candidate(-1);
        }
        if ui.button("Next").clicked() {
            session.step_candidate(1);
        }
        if ui.button("Make candidate the select").clicked() {
            session.promote();
        }
        ui.checkbox(&mut session.zoomed, "Zoom (Space)");
        if ui.button("Back to Library").clicked() {
            outcome.exit = true;
        }
        ui.weak("Left/Right candidate, Up promote, Tab switch marked side, Esc returns");
    });

    let mut tab_pressed = false;
    if !ui.ctx().egui_wants_keyboard_input() {
        ui.input(|i| {
            if i.key_pressed(Key::ArrowRight) {
                session.step_candidate(1);
            }
            if i.key_pressed(Key::ArrowLeft) {
                session.step_candidate(-1);
            }
            if i.key_pressed(Key::ArrowUp) {
                session.promote();
            }
            if i.key_pressed(Key::Tab) {
                session.toggle_active();
                tab_pressed = true;
            }
            if i.key_pressed(Key::Space) {
                session.zoomed = !session.zoomed;
            }
            if i.key_pressed(Key::Escape) || i.key_pressed(Key::G) {
                outcome.exit = true;
            }
        });
    }
    // Tab also moves egui's own keyboard focus onto a toolbar button; a following Space would
    // then activate that button *and* toggle zoom. Tab is this view's "switch marked side" key,
    // so drop whatever it just focused.
    if tab_pressed {
        ui.ctx().memory_mut(|m| {
            if let Some(id) = m.focused() {
                m.surrender_focus(id);
            }
        });
    }

    let area = ui.available_rect_before_wrap();
    let half = Vec2::new(area.width() / 2.0, area.height());
    let uv = view_uv(session.zoomed, session.pan());
    let tiles = [
        (Side::Select, session.select_id(), "Select"),
        (Side::Candidate, session.candidate_id(), "Candidate"),
    ];
    let mut drag: Option<(Vec2, Vec2)> = None;
    for (n, (side, id, label)) in tiles.into_iter().enumerate() {
        let rect =
            Rect::from_min_size(area.min + Vec2::new(n as f32 * half.x, 0.0), half).shrink(3.0);
        let response = ui.interact(
            rect,
            ui.id().with(("compare-tile", n)),
            Sense::click_and_drag(),
        );
        let texture = previews
            .get(ui.ctx(), store, pounce, id, Want::Large)
            .cloned();
        let picture = paint_tile(
            ui,
            rect,
            texture.as_ref(),
            uv,
            &TileStyle {
                label: Some(label),
                meta: cull.meta(id),
                active: session.active == side,
            },
        );
        if response.clicked() {
            session.active = side;
        }
        if response.dragged() && session.zoomed {
            drag = Some((response.drag_delta(), picture.size()));
        }
    }
    // Both tiles share one pan, so dragging either moves both -- the same spot in both photos.
    if let Some((delta, shown)) = drag {
        let pan = pan_after_drag(session.pan(), delta, shown);
        session.set_pan(pan);
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(n: i64) -> CompareSession {
        CompareSession::new((1..=n).collect()).unwrap()
    }

    #[test]
    fn a_compare_needs_two_photos_and_starts_on_the_first_two() {
        assert!(CompareSession::new(vec![]).is_none());
        assert!(CompareSession::new(vec![1]).is_none());
        let s = session(4);
        assert_eq!((s.select_id(), s.candidate_id()), (1, 2));
        assert_eq!(
            s.active,
            Side::Candidate,
            "marks go to the contender by default"
        );
        assert_eq!(s.candidate_position(), (1, 3));
    }

    #[test]
    fn stepping_walks_the_candidate_and_stops_at_both_ends() {
        let mut s = session(4);
        assert!(s.step_candidate(1));
        assert_eq!(s.candidate_id(), 3);
        assert!(s.step_candidate(1));
        assert_eq!(s.candidate_id(), 4);
        assert!(!s.step_candidate(1), "no photo after the last");
        assert_eq!(s.candidate_id(), 4);
        assert!(s.step_candidate(-1));
        assert!(s.step_candidate(-1));
        assert_eq!(s.candidate_id(), 2);
        assert!(
            !s.step_candidate(-1),
            "the select (photo 1) is not a candidate"
        );
        assert_eq!(s.candidate_id(), 2);
        assert!(!s.step_candidate(0));
    }

    #[test]
    fn stepping_jumps_over_the_select_wherever_it_sits() {
        let mut s = session(5);
        // Make photo 3 the select, candidate on 2.
        s.select = 2;
        s.candidate = 1;
        assert!(s.step_candidate(1));
        assert_eq!(s.candidate_id(), 4, "3 is the select, skipped");
        assert!(s.step_candidate(-1));
        assert_eq!(s.candidate_id(), 2, "and skipped going back");
        assert_eq!(
            s.candidate_position(),
            (2, 4),
            "photo 2 is contender 2 of 4"
        );
    }

    #[test]
    fn promoting_makes_the_candidate_the_select_and_moves_on() {
        let mut s = session(4);
        s.promote();
        assert_eq!((s.select_id(), s.candidate_id()), (2, 3));
        s.promote();
        assert_eq!((s.select_id(), s.candidate_id()), (3, 4));
        s.promote(); // at the end: the candidate falls back to a photo before it
        assert_eq!(s.select_id(), 4);
        assert_ne!(s.candidate_id(), 4);
        assert!(s.ids().contains(&s.candidate_id()));
    }

    #[test]
    fn promoting_at_the_end_never_lands_the_candidate_on_the_select() {
        let mut s = session(2);
        s.step_candidate(0);
        s.promote();
        assert_ne!(s.select_id(), s.candidate_id());
        s.promote();
        assert_ne!(s.select_id(), s.candidate_id());
    }

    #[test]
    fn the_active_side_switches_and_names_the_right_photo() {
        let mut s = session(3);
        assert_eq!(s.active_id(), 2);
        s.toggle_active();
        assert_eq!(s.active, Side::Select);
        assert_eq!(s.active_id(), 1);
        s.toggle_active();
        assert_eq!(s.active_id(), 2);
    }

    #[test]
    fn auto_advance_moves_the_candidate_and_reports_the_end() {
        let mut s = session(3);
        assert!(s.advance_candidate());
        assert_eq!(s.candidate_id(), 3);
        assert!(!s.advance_candidate());
    }

    #[test]
    fn removing_photos_keeps_both_tiles_or_resets_and_ends_below_two() {
        let mut s = session(5);
        s.step_candidate(2); // candidate = photo 4
        assert!(s.remove(&[2, 3]));
        assert_eq!((s.select_id(), s.candidate_id()), (1, 4), "both tiles kept");
        assert!(
            s.remove(&[4]),
            "the candidate was deleted: reset to the first two"
        );
        assert_ne!(s.select_id(), s.candidate_id());
        assert!(!s.remove(&[1]), "one photo left is not a comparison");
    }

    #[test]
    fn pan_is_shared_state_the_views_write_back() {
        let mut s = session(2);
        assert_eq!(s.pan(), [0.0, 0.0]);
        s.set_pan([0.1, -0.2]);
        assert_eq!(s.pan(), [0.1, -0.2]);
    }

    #[test]
    fn deleting_the_candidate_keeps_the_keeper_the_user_chose() {
        let mut s = session(5);
        s.promote(); // select = 2, candidate = 3
        s.promote(); // select = 3, candidate = 4
        assert_eq!((s.select_id(), s.candidate_id()), (3, 4));
        assert!(s.remove(&[4]));
        assert_eq!(s.select_id(), 3, "the chosen keeper is untouched");
        assert_ne!(s.candidate_id(), 3);
        assert_eq!(s.candidate_id(), 5, "the nearest surviving photo after it");
    }

    #[test]
    fn deleting_the_select_promotes_the_candidate() {
        let mut s = session(5);
        s.step_candidate(1); // select = 1, candidate = 3
        assert!(s.remove(&[1]));
        assert_eq!(s.select_id(), 3, "the candidate becomes the select");
        assert_ne!(s.candidate_id(), s.select_id());
        assert!(s.ids().contains(&s.candidate_id()));
    }

    #[test]
    fn deleting_the_last_photos_falls_back_to_a_valid_pair() {
        let mut s = session(3);
        s.step_candidate(1); // candidate = 3 (the last)
        assert!(s.remove(&[3]));
        assert_ne!(s.select_id(), s.candidate_id());
        assert_eq!(s.ids().len(), 2);
    }
}
