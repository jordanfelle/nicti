//! The Library grid under the headless harness: layout, virtualisation, click selection, and a
//! pixel snapshot. Seeded in-memory catalog, no files on disk, no GPU -- so no thumbnails; cells
//! are drawn as empty frames, which is exactly what the layout assertions need.

use std::sync::Arc;
use std::time::Duration;

use egui_kittest::kittest::{NodeT, Queryable};
use nicti_lair::{CatalogStore, Filter, Sort, SortDirection, SortField, SqliteCatalog};
use nicti_pounce::Pounce;

use super::{click_at, click_at_with, harness, wait_until};
use crate::cull::worker::CatalogMeta;
use crate::cull::CullState;
use crate::grid::view::{self, CELL};
use crate::grid::{GridSession, ViewState};
use crate::test_support::seed_assets;

struct Rig {
    session: GridSession,
    view: ViewState,
    cull: CullState,
    pounce: Pounce,
}

impl Rig {
    fn new(n: usize) -> Rig {
        let catalog = Arc::new(SqliteCatalog::open_in_memory().unwrap());
        seed_assets(&catalog, n);
        let store: Arc<dyn CatalogStore + Send + Sync> = catalog;
        let cull = CullState::new(Arc::new(CatalogMeta(store.clone())), || {});
        let pounce = Pounce::new(0, 2, 2, || {});
        let mut session = GridSession::new(store, 1 << 20);
        let sort = Sort {
            field: SortField::Filename,
            direction: SortDirection::Asc,
        };
        session.set_query(Filter::default(), sort, &pounce);
        Rig {
            session,
            view: ViewState::default(),
            cull,
            pounce,
        }
    }
}

fn grid_harness(n: usize) -> egui_kittest::Harness<'static, Rig> {
    let mut h = harness(
        egui::vec2(900.0, 600.0),
        |ui, rig: &mut Rig| {
            view::show(
                ui,
                &mut rig.session,
                &mut rig.view,
                &mut rig.cull,
                &rig.pounce,
                false,
            );
        },
        Rig::new(n),
    );
    wait_until(&mut h, "the grid snapshot", Duration::from_secs(10), |r| {
        r.session.is_loaded() && !r.session.is_loading()
    });
    h.run_steps(2);
    h
}

#[test]
fn cells_tile_the_viewport_edge_to_edge() {
    let h = grid_harness(200);
    let first = h
        .get_by_label(&format!("Photo {}", h.state().session.ids()[0]))
        .rect();
    let second = h
        .get_by_label(&format!("Photo {}", h.state().session.ids()[1]))
        .rect();
    assert_eq!(
        first.size(),
        egui::Vec2::splat(CELL),
        "a cell is CELL square"
    );
    assert_eq!(
        second.min.y, first.min.y,
        "the second cell sits in the same row"
    );
    assert_eq!(
        second.min.x, first.max.x,
        "cells abut: the gap is inside each cell"
    );

    // Every drawn cell is one of the first rows, none overlap, and the row width fits the viewport.
    let cells: Vec<_> = h
        .query_all_by_label_contains("Photo ")
        .map(|n| n.rect())
        .collect();
    assert!(!cells.is_empty());
    for (i, a) in cells.iter().enumerate() {
        assert!(a.max.x <= 900.0, "cell {i} fits the width");
        for b in &cells[i + 1..] {
            assert!(!a.shrink(0.5).intersects(*b), "{a:?} overlaps {b:?}");
        }
    }
    // 900 px minus the scrollbar at 168 px a cell is five columns.
    let cols = cells.iter().filter(|r| r.min.y == first.min.y).count();
    assert_eq!(cols, 5);
}

#[test]
fn only_the_visible_rows_are_built_and_scrolling_reveals_the_rest() {
    let mut h = grid_harness(200);
    let last = h.state().session.ids()[199];
    assert!(
        h.query_by_label(&format!("Photo {last}")).is_none(),
        "the last photo is far below the fold, so it isn't built"
    );
    let built = h.query_all_by_label_contains("Photo ").count();
    assert!(built < 40, "a 200-photo grid built {built} cells");

    // End is handled by the grid itself. With no cursor yet it lands on the first cell, so put the
    // cursor somewhere first, as a user clicking into the grid would.
    let first = h.state().session.ids()[0];
    let rect = h.get_by_label(&format!("Photo {first}")).rect();
    click_at(&mut h, rect.center());
    h.key_press(egui::Key::End);
    h.run_steps(4);
    assert_eq!(h.state().session.cursor(), Some(199));
    assert!(
        h.query_by_label(&format!("Photo {last}")).is_some(),
        "End scrolled the last photo into view"
    );
}

#[test]
fn clicking_moves_the_cursor_and_ctrl_click_builds_a_selection() {
    let mut h = grid_harness(200);
    let label = |h: &egui_kittest::Harness<'_, Rig>, i: usize| {
        format!("Photo {}", h.state().session.ids()[i])
    };

    let rect = h.get_by_label(&label(&h, 7)).rect();
    click_at(&mut h, rect.center());
    assert_eq!(h.state().session.cursor(), Some(7));
    assert!(
        !h.state().session.has_selection(),
        "a plain click only moves the cursor"
    );

    // The first Ctrl-click on a bare cursor keeps that photo selected too.
    let rect = h.get_by_label(&label(&h, 9)).rect();
    click_at_with(&mut h, rect.center(), egui::Modifiers::COMMAND);
    let session = &h.state().session;
    assert!(session.is_selected(7) && session.is_selected(9));
    assert!(!session.is_selected(8));
    // The selection is exposed to assistive tech too, which is what a screen reader reads.
    let toggled = |h: &egui_kittest::Harness<'_, Rig>, i: usize| {
        h.get_by_label(&label(h, i)).accesskit_node().toggled()
    };
    assert_eq!(toggled(&h, 9), Some(egui::accesskit::Toggled::True));
    assert_eq!(toggled(&h, 8), Some(egui::accesskit::Toggled::False));
}

#[test]
fn library_grid_snapshot() {
    let mut h = grid_harness(40);
    h.snapshot("library_grid");
}
