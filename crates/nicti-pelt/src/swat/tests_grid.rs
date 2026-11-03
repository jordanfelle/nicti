//! The Library grid under the headless harness: layout, virtualisation, click selection, and a
//! pixel snapshot. Seeded in-memory catalog, no files on disk, no GPU -- so no thumbnails; cells
//! are drawn as empty frames, which is exactly what the layout assertions need.

use std::sync::Arc;
use std::time::Duration;

use egui_kittest::kittest::{NodeT, Queryable};
use nicti_lair::{CatalogStore, Filter, NewAsset, Sort, SortDirection, SortField, SqliteCatalog};
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
    // The snapshot only says which ids exist. Each cell's "no preview" comes from a thumbnail batch
    // job failing on a Pounce worker thread, which lands whenever that thread gets scheduled, so
    // wait for the failures of the first thumbnail batch, which holds every visible cell (a few
    // frames is not enough on a loaded machine).
    let visible = h
        .state()
        .session
        .ids()
        .len()
        .min(crate::grid::layout::BATCH);
    wait_until(
        &mut h,
        "the visible cells' thumbnails to fail",
        Duration::from_secs(10),
        |r| {
            r.session.ids()[..visible]
                .iter()
                .all(|&id| r.session.has_failed(id))
        },
    );
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

/// ADR-0437: a frame's work follows the viewport, not the library. 5000 photos at 900x600 is five
/// columns by four or five rows (20-25 cells); 30 is five columns by six rows, room for partial rows and no more.
#[test]
fn a_frame_builds_only_the_cells_near_the_viewport_whatever_the_library_size() {
    let mut h = grid_harness(5000);
    let per_frame = |h: &egui_kittest::Harness<'static, Rig>, before: &view::GridStats| {
        let s = &h.state().view.stats;
        let frames = s.frames - before.frames;
        assert!(frames > 0, "no frames ran");
        (s.cells_built - before.cells_built) / frames
    };
    let before = h.state().view.stats.clone();
    h.run_steps(5);
    let top = per_frame(&h, &before);
    assert!(
        top > 0 && top < 30,
        "built {top} of 5000 cells per frame at the top"
    );

    // The far end of the library costs the same as the top.
    let first = h.state().session.ids()[0];
    let rect = h.get_by_label(&format!("Photo {first}")).rect();
    click_at(&mut h, rect.center());
    h.key_press(egui::Key::End);
    h.run_steps(4);
    let last = h.state().session.ids()[4999];
    assert_eq!(
        h.state().session.cursor(),
        Some(4999),
        "End moved the cursor to the last photo"
    );
    assert!(
        h.query_by_label(&format!("Photo {last}")).is_some(),
        "End scrolled the last photo into view"
    );
    let before = h.state().view.stats.clone();
    h.run_steps(5);
    let end = per_frame(&h, &before);
    assert!(
        end > 0 && end < 30,
        "built {end} of 5000 cells per frame at the end"
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

/// A catalog whose photos differ in shape, orientation and capture day, sorted by capture time:
/// what the justified layout needs (#656). Every fifth photo is portrait, every seventh is stored
/// landscape with a 90-degree orientation tag, and a new day starts every 40 photos.
fn shaped_rig(n: usize) -> Rig {
    let catalog = Arc::new(SqliteCatalog::open_in_memory().unwrap());
    let volume = catalog.upsert_volume("v", None, None, 0).unwrap();
    let root = catalog.ensure_root(volume, "").unwrap();
    for i in 0..n {
        let path = format!("{i:05}.NEF");
        let (w, h) = if i % 5 == 0 {
            (2000, 3000)
        } else {
            (3000, 2000)
        };
        let asset = NewAsset {
            rel_path_fold: path.to_lowercase(),
            rel_path: path,
            size_bytes: 1,
            mtime_unix: 0,
            fingerprint: None,
            natural_key: None,
            make: None,
            model: None,
            captured_at: Some(format!("2026:03:{:02} 10:00:{:02}", 1 + i / 40, i % 40)),
            width: Some(w),
            height: Some(h),
            imported_at: 0,
        };
        let id = catalog.insert_asset(root, &asset, None).unwrap();
        if i % 7 == 0 {
            catalog.set_orientation(id, 6).unwrap();
        }
    }
    let store: Arc<dyn CatalogStore + Send + Sync> = catalog;
    let cull = CullState::new(Arc::new(CatalogMeta(store.clone())), || {});
    let pounce = Pounce::new(0, 2, 2, || {});
    let mut session = GridSession::new(store, 1 << 20);
    let sort = Sort {
        field: SortField::Captured,
        direction: SortDirection::Asc,
    };
    session.set_query(Filter::default(), sort, &pounce);
    let mut view = ViewState::default();
    view.justified = true;
    Rig {
        session,
        view,
        cull,
        pounce,
    }
}

fn justified_harness(n: usize) -> egui_kittest::Harness<'static, Rig> {
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
        shaped_rig(n),
    );
    wait_until(
        &mut h,
        "the justified layout",
        Duration::from_secs(20),
        |r| r.session.is_loaded() && r.view.justified_ready(&r.session),
    );
    h.run_steps(3);
    h
}

#[test]
fn justified_rows_share_a_height_and_fill_the_width() {
    let h = justified_harness(400);
    let cells: Vec<_> = h
        .query_all_by_label_contains("Photo ")
        .map(|n| n.rect())
        .collect();
    assert!(cells.len() > 4, "built {} cells", cells.len());
    // Group the drawn cells into rows by their top edge.
    let mut rows: Vec<(f32, Vec<egui::Rect>)> = Vec::new();
    for r in cells {
        match rows.iter_mut().find(|(top, _)| (top - r.min.y).abs() < 0.5) {
            Some((_, row)) => row.push(r),
            None => rows.push((r.min.y, vec![r])),
        }
    }
    assert!(rows.len() >= 2, "at least two rows are on screen");
    for (_, row) in &rows {
        let h0 = row[0].height();
        assert!(
            row.iter().all(|r| (r.height() - h0).abs() < 0.5),
            "one height per row"
        );
        assert!(
            row.iter().all(|r| r.max.x <= 900.0),
            "no cell overflows the width"
        );
    }
    // Different shapes mean different widths within a row (this is not the square grid).
    let widths: Vec<f32> = rows
        .iter()
        .flat_map(|(_, row)| row.iter().map(|r| r.width()))
        .collect();
    let (lo, hi) = widths
        .iter()
        .fold((f32::MAX, f32::MIN), |(l, h), w| (l.min(*w), h.max(*w)));
    assert!(hi > lo + 20.0, "cells vary in width: {lo}..{hi}");
}

#[test]
fn a_justified_frame_builds_only_the_visible_cells_and_never_relays_out() {
    let mut h = justified_harness(5000);
    let before = h.state().view.stats.clone();
    h.run_steps(8);
    let s = &h.state().view.stats;
    let frames = s.frames - before.frames;
    assert!(frames > 0 && s.justified_frames > 0, "justified frames ran");
    let per_frame = (s.cells_built - before.cells_built) / frames;
    assert!(
        per_frame > 0 && per_frame < 60,
        "built {per_frame} of 5000 cells per frame"
    );
    assert_eq!(
        s.layout_builds, before.layout_builds,
        "an unchanged frame must not start a layout build (ADR-0437)"
    );
}

#[test]
fn end_scrolls_a_justified_grid_to_its_last_photo() {
    let mut h = justified_harness(1500);
    let first = h.state().session.ids()[0];
    let rect = h.get_by_label(&format!("Photo {first}")).rect();
    click_at(&mut h, rect.center());
    h.key_press(egui::Key::End);
    h.run_steps(5);
    let last = h.state().session.ids()[1499];
    assert_eq!(h.state().session.cursor(), Some(1499));
    assert!(
        h.query_by_label(&format!("Photo {last}")).is_some(),
        "End scrolled the last photo into view"
    );
    // And arrow-down then moves by a row, not by one cell.
    h.key_press(egui::Key::Home);
    h.run_steps(5);
    h.key_press(egui::Key::ArrowDown);
    h.run_steps(3);
    let c = h.state().session.cursor().unwrap();
    assert!(c > 1, "down moved to a later row, got cell {c}");
}

#[test]
fn switching_back_to_square_keeps_the_square_grid() {
    let mut h = justified_harness(200);
    h.state_mut().view.justified = false;
    h.run_steps(3);
    let cells: Vec<_> = h
        .query_all_by_label_contains("Photo ")
        .map(|n| n.rect())
        .collect();
    assert!(cells.iter().all(|r| r.size() == egui::Vec2::splat(CELL)));
}
