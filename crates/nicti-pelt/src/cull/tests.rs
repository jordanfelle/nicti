use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use nicti_lair::{AssetMeta, CatalogStore, NewAsset, SqliteCatalog};

use super::keys::{CullAction, Label};
use super::worker::{CatalogMeta, MetaStore};
use super::CullState;

/// A gate the fake store waits on, standing in for a catalog whose mutex an import is holding.
#[derive(Clone)]
struct Gate(Arc<(Mutex<bool>, Condvar)>);

impl Gate {
    fn open() -> Self {
        Gate(Arc::new((Mutex::new(true), Condvar::new())))
    }
    fn set(&self, open: bool) {
        *self.0 .0.lock().unwrap() = open;
        self.0 .1.notify_all();
    }
    fn wait_open(&self) {
        let mut open = self.0 .0.lock().unwrap();
        while !*open {
            open = self.0 .1.wait(open).unwrap();
        }
    }
}

struct FakeStore {
    rows: Mutex<HashMap<i64, AssetMeta>>,
    gate: Gate,
    fail_writes: AtomicBool,
    fail_reads: AtomicBool,
    reads: AtomicUsize,
    /// Every photo id `set_meta` was asked to write, in order.
    written: Mutex<Vec<i64>>,
}

impl FakeStore {
    fn with_ids(ids: impl IntoIterator<Item = i64>) -> Arc<Self> {
        Arc::new(FakeStore {
            rows: Mutex::new(ids.into_iter().map(|i| (i, AssetMeta::default())).collect()),
            gate: Gate::open(),
            fail_writes: AtomicBool::new(false),
            fail_reads: AtomicBool::new(false),
            reads: AtomicUsize::new(0),
            written: Mutex::new(Vec::new()),
        })
    }
    fn row(&self, id: i64) -> AssetMeta {
        self.rows.lock().unwrap()[&id].clone()
    }
    fn put(&self, id: i64, meta: AssetMeta) {
        self.rows.lock().unwrap().insert(id, meta);
    }
}

impl MetaStore for FakeStore {
    fn get_meta(&self, ids: &[i64]) -> Result<HashMap<i64, AssetMeta>, String> {
        self.gate.wait_open();
        self.reads.fetch_add(1, Ordering::SeqCst);
        if self.fail_reads.load(Ordering::SeqCst) {
            return Err("catalog is locked".into());
        }
        let rows = self.rows.lock().unwrap();
        Ok(ids
            .iter()
            .filter_map(|id| rows.get(id).map(|m| (*id, m.clone())))
            .collect())
    }
    fn set_meta(&self, items: &[(i64, AssetMeta)]) -> Result<(), String> {
        self.gate.wait_open();
        if self.fail_writes.load(Ordering::SeqCst) {
            return Err("disk is on fire".into());
        }
        let mut rows = self.rows.lock().unwrap();
        self.written
            .lock()
            .unwrap()
            .extend(items.iter().map(|(id, _)| *id));
        for (id, m) in items {
            if let Some(slot) = rows.get_mut(id) {
                *slot = m.clone();
            }
        }
        Ok(())
    }
}

fn state(store: &Arc<FakeStore>) -> CullState {
    CullState::new(store.clone(), || {})
}

fn wait_until(state: &mut CullState, what: &str, mut cond: impl FnMut(&CullState) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        state.poll();
        if cond(state) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn stars(n: i64) -> AssetMeta {
    AssetMeta {
        rating: Some(n),
        ..AssetMeta::default()
    }
}

#[test]
fn ensure_loads_markers_once_and_only_the_missing_ones() {
    let store = FakeStore::with_ids(1..=3);
    store.put(2, stars(4));
    let mut cull = state(&store);
    cull.ensure([1, 2, 3]);
    cull.ensure([1, 2, 3]); // in flight: must not re-request
    cull.settle_all();
    assert_eq!(cull.meta(2), Some(&stars(4)));
    assert_eq!(cull.meta(1), Some(&AssetMeta::default()));
    cull.ensure([1, 2, 3]); // cached: nothing to do
    cull.settle_all();
    assert_eq!(
        store.reads.load(Ordering::SeqCst),
        1,
        "one read for the whole window"
    );
}

#[test]
fn mark_updates_the_cache_at_once_and_the_catalog_behind_it() {
    let store = FakeStore::with_ids(1..=2);
    let mut cull = state(&store);
    cull.ensure([1, 2]);
    cull.settle_all();

    cull.mark(vec![1, 2], CullAction::SetRating(Some(3)));
    // No poll yet: the badge is already right.
    assert_eq!(cull.meta(1), Some(&stars(3)));
    assert_eq!(cull.meta(2), Some(&stars(3)));

    cull.settle_all();
    assert_eq!(store.row(1), stars(3));
    assert_eq!(store.row(2), stars(3));
    assert_eq!(cull.meta(1), Some(&stars(3)));
}

#[test]
fn mark_returns_immediately_even_while_the_catalog_is_blocked() {
    let store = FakeStore::with_ids(1..=1);
    let mut cull = state(&store);
    cull.ensure([1]);
    cull.settle_all();

    // The catalog "mutex" is held (an import): open it again after 2 s so a regression that
    // waits on it fails the timing assertion instead of hanging the test run.
    store.gate.set(false);
    let gate = store.gate.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(2));
        gate.set(true);
    });

    let started = Instant::now();
    cull.mark(vec![1], CullAction::ToggleReject);
    cull.poll();
    let took = started.elapsed();
    assert!(
        took < Duration::from_millis(500),
        "marking waited on the catalog: {took:?}"
    );
    assert_eq!(
        cull.meta(1).unwrap().rating,
        Some(-1),
        "optimistic reject shows"
    );
    assert_eq!(store.row(1), AssetMeta::default(), "nothing written yet");

    cull.settle_all();
    assert_eq!(
        store.row(1).rating,
        Some(-1),
        "written once the catalog freed up"
    );
}

#[test]
fn rapid_marks_never_flicker_back_to_an_earlier_value() {
    let store = FakeStore::with_ids(1..=1);
    let mut cull = state(&store);
    cull.ensure([1]);
    cull.settle_all();

    store.gate.set(false);
    cull.mark(vec![1], CullAction::SetRating(Some(3)));
    cull.mark(vec![1], CullAction::SetRating(Some(4)));
    store.gate.set(true);

    // Every poll along the way, including the one that folds in the *first* write's reply, must
    // still show the latest intent.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        cull.poll();
        assert_eq!(cull.meta(1).unwrap().rating, Some(4));
        if store.row(1).rating == Some(4) && cull.pending.is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "writes never finished");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn a_failed_read_is_reported_backed_off_and_retried_later_not_forgotten() {
    let store = FakeStore::with_ids(1..=2);
    store.put(1, stars(4));
    let mut cull = state(&store);

    store.fail_reads.store(true, Ordering::SeqCst);
    cull.ensure([1, 2]);
    wait_until(&mut cull, "the read failure", |c| c.last_error().is_some());
    assert!(cull.last_error().unwrap().contains("catalog is locked"));
    assert_eq!(cull.meta(1), None);

    // While backing off, asking again (as every frame does) sends nothing.
    let reads_before = store.reads.load(Ordering::SeqCst);
    cull.ensure([1, 2]);
    cull.ensure([1, 2]);
    // The barrier is answered only after anything those calls might have queued was handled, so
    // "no reads happened" is checked against a settled writer, not a guess about timing.
    cull.barrier();
    cull.settle_all();
    assert_eq!(
        store.reads.load(Ordering::SeqCst),
        reads_before,
        "no read storm while backing off"
    );

    // Once the back-off passes and the catalog recovers, the photos load -- they were not
    // stuck in "requested" forever.
    store.fail_reads.store(false, Ordering::SeqCst);
    cull.read_backoff_until = Some(Instant::now());
    cull.ensure([1, 2]);
    cull.settle_all();
    assert_eq!(cull.meta(1), Some(&stars(4)));
    assert_eq!(cull.meta(2), Some(&AssetMeta::default()));
}

#[test]
fn a_failed_write_drops_the_guess_and_reports_it() {
    let store = FakeStore::with_ids(1..=1);
    let mut cull = state(&store);
    cull.ensure([1]);
    cull.settle_all();

    store.fail_writes.store(true, Ordering::SeqCst);
    cull.mark(vec![1], CullAction::SetRating(Some(5)));
    assert_eq!(cull.meta(1), Some(&stars(5)), "optimistic first");

    wait_until(&mut cull, "the failure to surface", |c| {
        c.last_error().is_some()
    });
    assert!(cull.last_error().unwrap().contains("disk is on fire"));
    assert_eq!(store.row(1), AssetMeta::default(), "nothing was written");
    assert_eq!(cull.meta(1), None, "the wrong guess is gone, to be re-read");
    cull.clear_error();
    assert!(cull.last_error().is_none());
}

#[test]
fn undo_and_redo_restore_each_photos_own_previous_markers() {
    let store = FakeStore::with_ids(1..=3);
    store.put(1, stars(1));
    store.put(2, stars(5));
    let mut cull = state(&store);
    cull.ensure([1, 2, 3]);
    cull.settle_all();

    cull.mark(vec![1, 2, 3], CullAction::SetRating(Some(3)));
    cull.settle_all();
    assert!([1, 2, 3].iter().all(|i| store.row(*i) == stars(3)));

    cull.undo();
    wait_until(&mut cull, "undo", |c| c.meta(2) == Some(&stars(5)));
    assert_eq!(
        store.row(1),
        stars(1),
        "each photo gets its OWN old value back"
    );
    assert_eq!(store.row(2), stars(5));
    assert_eq!(store.row(3), AssetMeta::default());
    assert_eq!(cull.meta(3), Some(&AssetMeta::default()));

    cull.redo();
    wait_until(&mut cull, "redo", |c| c.meta(2) == Some(&stars(3)));
    assert_eq!(store.row(1), stars(3));
    assert_eq!(store.row(2), stars(3));
}

#[test]
fn undo_with_nothing_to_undo_is_a_no_op() {
    let store = FakeStore::with_ids(1..=1);
    let mut cull = state(&store);
    cull.undo();
    cull.redo();
    cull.poll();
    assert!(cull.last_error().is_none());
}

#[test]
fn a_mark_that_changes_nothing_leaves_no_undo_entry() {
    let store = FakeStore::with_ids(1..=1);
    let mut cull = state(&store);
    cull.ensure([1]);
    cull.settle_all();

    cull.mark(vec![1], CullAction::SetRating(Some(3))); // a real change
    cull.settle_all();
    cull.mark(vec![1], CullAction::SetRating(Some(3))); // changes nothing
    cull.settle_all();
    assert_eq!(
        store.written.lock().unwrap().len(),
        1,
        "a no-op mark writes nothing to the catalog"
    );

    // One undo must take back the REAL mark. If the no-op had pushed its own entry, this undo
    // would only "restore" 3 -> 3 and the photo would still be rated.
    cull.undo();
    wait_until(&mut cull, "the undo", |c| {
        c.meta(1) == Some(&AssetMeta::default())
    });
    assert_eq!(store.row(1), AssetMeta::default());
}

#[test]
fn toggles_decide_from_the_real_markers_even_for_uncached_targets() {
    let store = FakeStore::with_ids(1..=3);
    store.put(
        1,
        AssetMeta {
            flag: Some(1),
            ..AssetMeta::default()
        },
    );
    store.put(
        2,
        AssetMeta {
            flag: Some(1),
            ..AssetMeta::default()
        },
    );
    store.put(
        3,
        AssetMeta {
            flag: Some(1),
            ..AssetMeta::default()
        },
    );
    let mut cull = state(&store);
    // Nothing cached (a select-all beyond the visible window): no optimistic guess is made, the
    // worker decides from what is really in the catalog -- all picked, so this un-picks.
    cull.mark(vec![1, 2, 3], CullAction::TogglePick);
    assert_eq!(cull.meta(1), None);
    wait_until(&mut cull, "the worker's answer", |c| c.meta(3).is_some());
    for i in 1..=3 {
        assert_eq!(store.row(i).flag, None);
        assert_eq!(cull.meta(i).unwrap().flag, None);
    }
}

#[test]
fn forgetting_deleted_photos_removes_them_from_undo() {
    let store = FakeStore::with_ids(1..=2);
    let mut cull = state(&store);
    cull.ensure([1, 2]);
    cull.settle_all();
    cull.mark(vec![1, 2], CullAction::ToggleLabel(Label::Red));
    cull.settle_all();

    // Photo 1 is deleted. Undo must not even ATTEMPT to write it (a real catalog would ignore
    // the missing row, which is why this checks the attempts, not the resulting rows).
    store.written.lock().unwrap().clear();
    store.rows.lock().unwrap().remove(&1);
    cull.forget(&[1]);
    assert_eq!(cull.meta(1), None);
    cull.undo();
    wait_until(&mut cull, "undo", |c| {
        c.meta(2) == Some(&AssetMeta::default())
    });
    assert_eq!(
        *store.written.lock().unwrap(),
        vec![2],
        "only photo 2 was restored"
    );
}

#[test]
fn a_stale_read_reply_does_not_overwrite_a_pending_edit() {
    let store = FakeStore::with_ids(1..=1);
    let mut cull = state(&store);
    store.gate.set(false);
    cull.ensure([1]); // read queued behind the closed gate
    cull.mark(vec![1], CullAction::SetRating(Some(2))); // not cached: no optimistic value
    store.gate.set(true);
    cull.settle_all();
    wait_until(&mut cull, "the write", |c| {
        c.meta(1).is_some_and(|m| m.rating == Some(2))
    });
    assert_eq!(store.row(1), stars(2));
}

#[test]
fn works_end_to_end_against_the_real_catalog() {
    let catalog = Arc::new(SqliteCatalog::open_in_memory().unwrap());
    let volume = catalog.upsert_volume("v", None, None, 0).unwrap();
    let root = catalog.ensure_root(volume, "").unwrap();
    let ids: Vec<i64> = (0..3)
        .map(|i| {
            catalog
                .insert_asset(
                    root,
                    &NewAsset {
                        rel_path: format!("{i}.NEF"),
                        rel_path_fold: format!("{i}.nef"),
                        size_bytes: 1,
                        mtime_unix: 0,
                        fingerprint: None,
                        natural_key: None,
                        make: None,
                        model: None,
                        captured_at: None,
                        width: None,
                        height: None,
                        imported_at: 0,
                    },
                    None,
                )
                .unwrap()
        })
        .collect();
    let dyn_store: Arc<dyn CatalogStore + Send + Sync> = catalog.clone();
    let mut cull = CullState::new(Arc::new(CatalogMeta(dyn_store)), || {});

    cull.ensure(ids.iter().copied());
    cull.settle_all();
    cull.mark(ids.clone(), CullAction::ToggleReject);
    cull.mark(vec![ids[0]], CullAction::ToggleLabel(Label::Blue));
    cull.settle_all();

    let meta = catalog.get_meta(&ids).unwrap();
    assert!(meta.values().all(|m| m.rating == Some(-1)));
    assert_eq!(meta[&ids[0]].label.as_deref(), Some("Blue"));
    assert_eq!(meta[&ids[1]].label, None);
}

// ---- the whole flow, composed from the real pieces (no window needed) -----------------------
//
// `PeltApp` itself needs a GPU and an eframe context, so it can't be built here. These tests wire
// the same parts it wires -- key polling, `CullState`, `GridSession`, the marker filter, the
// delete flow -- against a real catalog and real files, so the behaviour the user relies on is
// exercised end to end even though the top-level `PeltApp` glue is not.

mod flow {
    use std::path::PathBuf;

    use egui::{Event, Key, Modifiers};
    use nicti_lair::shred::{DeleteMode, Trasher};
    use nicti_lair::{Filter, Sort, SortDirection, SortField};
    use nicti_pounce::Pounce;

    use super::*;
    use crate::cull::delete::DeleteFlow;
    use crate::cull::input::{self, KeyCommand};
    use crate::cull::should_advance;
    use crate::filter_bar::{FilterBar, FlagChoice, LabelChoice, RatingChoice};
    use crate::grid::GridSession;

    struct FakeBin {
        bin: PathBuf,
    }
    impl Trasher for FakeBin {
        fn trash_all(&self, paths: &[PathBuf]) -> Result<(), String> {
            for p in paths {
                std::fs::rename(p, self.bin.join(p.file_name().unwrap()))
                    .map_err(|e| e.to_string())?;
            }
            Ok(())
        }
    }

    struct Rig {
        dir: tempfile::TempDir,
        catalog: Arc<SqliteCatalog>,
        ids: Vec<i64>,
        cull: CullState,
        session: GridSession,
        ctx: egui::Context,
        pounce: Pounce,
    }

    fn sort() -> Sort {
        Sort {
            field: SortField::Filename,
            direction: SortDirection::Asc,
        }
    }

    impl Rig {
        /// `n` photos with real RAW-named files, all shown in the grid.
        fn new(n: usize) -> Rig {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("shoot");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::create_dir_all(dir.path().join("bin")).unwrap();
            let catalog = Arc::new(SqliteCatalog::open_in_memory().unwrap());
            let volume = catalog.upsert_volume("v", None, None, 0).unwrap();
            let root_id = catalog
                .ensure_root(volume, &root.to_string_lossy())
                .unwrap();
            let ids: Vec<i64> = (0..n)
                .map(|i| {
                    let name = format!("IMG_{i:02}.NEF");
                    std::fs::write(root.join(&name), b"raw").unwrap();
                    catalog
                        .insert_asset(
                            root_id,
                            &NewAsset {
                                rel_path: name.clone(),
                                rel_path_fold: name.to_lowercase(),
                                size_bytes: 3,
                                mtime_unix: 0,
                                fingerprint: None,
                                natural_key: None,
                                make: None,
                                model: None,
                                captured_at: None,
                                width: None,
                                height: None,
                                imported_at: 0,
                            },
                            None,
                        )
                        .unwrap()
                })
                .collect();
            let dyn_store: Arc<dyn CatalogStore + Send + Sync> = catalog.clone();
            let cull = CullState::new(Arc::new(CatalogMeta(dyn_store.clone())), || {});
            let ctx = egui::Context::default();
            let pounce = Pounce::new(0, 2, 2, || {});
            let mut session = GridSession::new(dyn_store, 1 << 20);
            session.set_query(Filter::default(), sort(), &pounce);
            let mut rig = Rig {
                dir,
                catalog,
                ids,
                cull,
                session,
                ctx,
                pounce,
            };
            rig.wait_snapshot();
            rig
        }

        fn wait_snapshot(&mut self) {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                self.session.poll(&self.ctx, &self.pounce);
                if self.session.is_loaded() && !self.session.is_loading() {
                    return;
                }
                assert!(Instant::now() < deadline, "grid snapshot never landed");
                std::thread::sleep(Duration::from_millis(2));
            }
        }

        /// Shows what the real Library filter bar would for these three controls.
        fn show(&mut self, bar: FilterBar) {
            self.session
                .set_query(bar.to_filter(None), sort(), &self.pounce);
            self.wait_snapshot();
        }

        fn mark(&mut self, ids: &[i64], action: CullAction) {
            self.cull.ensure(ids.iter().copied());
            self.cull.settle_all();
            self.cull.mark(ids.to_vec(), action);
            self.cull.settle_all();
        }

        /// One headless frame with `events`, handled the way `PeltApp::handle_cull_keys` does for
        /// the Library view.
        fn keys(&mut self, events: Vec<Event>) {
            let input = egui::RawInput {
                events,
                ..Default::default()
            };
            let mut commands = Vec::new();
            let ctx = self.ctx.clone();
            let output = ctx.run_ui(input, |_ui| commands = input::poll(&ctx));
            output.drop_without_applying_deltas();
            for command in commands {
                match command {
                    KeyCommand::Mark {
                        action,
                        invert_advance,
                    } => {
                        let targets = self.session.target_ids();
                        if targets.is_empty() {
                            continue;
                        }
                        let advance = should_advance(true, invert_advance, targets.len());
                        self.cull.ensure(targets.iter().copied());
                        self.cull.settle_all();
                        self.cull.mark(targets, action);
                        if advance {
                            self.session.advance_after_mark();
                        }
                    }
                    KeyCommand::Undo => self.cull.undo(),
                    KeyCommand::Redo => self.cull.redo(),
                    _ => {}
                }
            }
            // The writer is strictly ordered: once its barrier is answered, every mark, undo and
            // redo sent above has landed. (No sleeping: a loaded CI runner can't flake this.)
            self.cull.barrier();
            self.cull.settle_all();
        }

        fn rating(&self, id: i64) -> Option<i64> {
            self.catalog.get_meta(&[id]).unwrap()[&id].rating
        }

        /// "Select all, then Delete" through the flow, Recycle Bin mode.
        fn delete_selection(&mut self) -> Vec<i64> {
            self.session.select_all();
            let ids = self.session.target_ids();
            let mut flow = DeleteFlow::new();
            let dyn_store: Arc<dyn CatalogStore + Send + Sync> = self.catalog.clone();
            flow.submit_with(
                dyn_store,
                None,
                DeleteMode::RecycleBin,
                ids.clone(),
                &self.pounce,
                Box::new(FakeBin {
                    bin: self.dir.path().join("bin"),
                }),
            );
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if flow.poll().finished {
                    return ids;
                }
                assert!(Instant::now() < deadline, "delete never finished");
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        fn in_bin(&self, i: usize) -> bool {
            self.dir.path().join(format!("bin/IMG_{i:02}.NEF")).exists()
        }

        fn on_disk(&self, i: usize) -> bool {
            self.dir
                .path()
                .join(format!("shoot/IMG_{i:02}.NEF"))
                .exists()
        }
    }

    fn tap(key: Key, mods: Modifiers) -> Vec<Event> {
        [true, false]
            .into_iter()
            .map(|pressed| Event::Key {
                key,
                physical_key: Some(key),
                pressed,
                repeat: false,
                modifiers: mods,
            })
            .collect()
    }

    #[test]
    fn pressing_3_rates_the_photo_and_advances_and_shift_3_does_not() {
        let mut rig = Rig::new(4);
        rig.session.click(0);

        rig.keys(tap(Key::Num3, Modifiers::NONE));
        assert_eq!(rig.rating(rig.ids[0]), Some(3), "the first photo is rated");
        assert_eq!(
            rig.session.cursor_id(),
            Some(rig.ids[1]),
            "and the cursor moved on"
        );

        rig.keys(tap(Key::Num5, Modifiers::SHIFT));
        assert_eq!(rig.rating(rig.ids[1]), Some(5));
        assert_eq!(
            rig.session.cursor_id(),
            Some(rig.ids[1]),
            "Shift inverts auto-advance for that press"
        );
    }

    #[test]
    fn ctrl_z_takes_back_a_mark_and_ctrl_shift_z_reapplies_it() {
        let mut rig = Rig::new(3);
        rig.session.click(0);
        rig.keys(tap(Key::X, Modifiers::NONE)); // reject photo 0, advance to photo 1
        assert_eq!(rig.rating(rig.ids[0]), Some(-1));

        rig.keys(tap(Key::Z, Modifiers::COMMAND));
        assert_eq!(rig.rating(rig.ids[0]), None, "the reject is undone");
        rig.keys(tap(Key::Z, Modifiers::COMMAND | Modifiers::SHIFT));
        assert_eq!(rig.rating(rig.ids[0]), Some(-1), "and redone");
    }

    #[test]
    fn a_multi_selection_is_marked_as_one_undoable_action() {
        let mut rig = Rig::new(5);
        rig.mark(&[rig.ids[1]], CullAction::SetRating(Some(4)));
        rig.session.click(0);
        rig.session.select_range_to(2); // photos 0..=2, with photo 1 already at 4 stars

        rig.keys(tap(Key::Num2, Modifiers::NONE));
        assert!(rig.ids[..3].iter().all(|id| rig.rating(*id) == Some(2)));
        assert_eq!(
            rig.session.cursor_id(),
            Some(rig.ids[2]),
            "no auto-advance for a multi-selection"
        );

        rig.keys(tap(Key::Z, Modifiers::COMMAND));
        assert_eq!(rig.rating(rig.ids[0]), None);
        assert_eq!(
            rig.rating(rig.ids[1]),
            Some(4),
            "photo 1 gets ITS old value back"
        );
        assert_eq!(rig.rating(rig.ids[2]), None);
    }

    #[test]
    fn filter_by_one_star_then_select_all_and_delete_removes_only_those() {
        let mut rig = Rig::new(6);
        let (ones, others) = (vec![rig.ids[1], rig.ids[4]], [rig.ids[0], rig.ids[2]]);
        rig.mark(&ones, CullAction::SetRating(Some(1)));
        rig.mark(&others, CullAction::SetRating(Some(5)));

        rig.show(FilterBar::with_markers(
            RatingChoice::Exactly(1),
            FlagChoice::Any,
            LabelChoice::Any,
        ));
        assert_eq!(
            rig.session.len(),
            2,
            "the filter shows only the one-star photos"
        );
        let deleted = rig.delete_selection();

        assert_eq!(deleted, ones);
        assert_eq!(rig.catalog.asset_count().unwrap(), 4);
        assert!(rig.in_bin(1) && rig.in_bin(4));
        assert!(rig.on_disk(0) && rig.on_disk(2) && rig.on_disk(3) && rig.on_disk(5));
    }

    #[test]
    fn filter_by_reject_then_select_all_and_delete_removes_only_the_rejects() {
        let mut rig = Rig::new(5);
        rig.mark(&[rig.ids[0], rig.ids[3]], CullAction::ToggleReject);
        rig.mark(&[rig.ids[2]], CullAction::TogglePick);

        rig.show(FilterBar::with_markers(
            RatingChoice::Rejected,
            FlagChoice::Any,
            LabelChoice::Any,
        ));
        assert_eq!(rig.session.len(), 2);
        let deleted = rig.delete_selection();

        assert_eq!(deleted, vec![rig.ids[0], rig.ids[3]]);
        assert!(rig.in_bin(0) && rig.in_bin(3));
        assert!(
            rig.on_disk(1) && rig.on_disk(2) && rig.on_disk(4),
            "the pick survives"
        );
    }

    #[test]
    fn filter_by_a_colour_label_then_select_all_and_delete_removes_only_those() {
        let mut rig = Rig::new(5);
        rig.mark(
            &[rig.ids[1], rig.ids[2]],
            CullAction::ToggleLabel(Label::Red),
        );
        rig.mark(&[rig.ids[3]], CullAction::ToggleLabel(Label::Blue));

        rig.show(FilterBar::with_markers(
            RatingChoice::Any,
            FlagChoice::Any,
            LabelChoice::Is("Red".into()),
        ));
        assert_eq!(rig.session.len(), 2);
        let deleted = rig.delete_selection();

        assert_eq!(deleted, vec![rig.ids[1], rig.ids[2]]);
        assert!(rig.in_bin(1) && rig.in_bin(2));
        assert!(rig.on_disk(3), "the blue-labelled photo is not touched");
        assert_eq!(rig.catalog.asset_count().unwrap(), 3);
    }

    #[test]
    fn a_combined_filter_narrows_with_and_and_a_mixed_workflow_works() {
        // Stars AND a label AND unflagged: three different markers, all honoured at once.
        let mut rig = Rig::new(6);
        rig.mark(
            &[rig.ids[0], rig.ids[1], rig.ids[2]],
            CullAction::SetRating(Some(2)),
        );
        rig.mark(
            &[rig.ids[1], rig.ids[2], rig.ids[5]],
            CullAction::ToggleLabel(Label::Yellow),
        );
        rig.mark(&[rig.ids[2]], CullAction::TogglePick);

        rig.show(FilterBar::with_markers(
            RatingChoice::Exactly(2),
            FlagChoice::Unflagged,
            LabelChoice::Is("Yellow".into()),
        ));
        assert_eq!(
            rig.session.ids(),
            &[rig.ids[1]],
            "only photo 1 has all three"
        );
        rig.delete_selection();
        assert!(rig.in_bin(1));
        assert!(rig.on_disk(0) && rig.on_disk(2) && rig.on_disk(5));
    }

    #[test]
    fn the_unrated_filter_finds_what_is_left_after_a_pass() {
        let mut rig = Rig::new(4);
        rig.mark(&[rig.ids[0], rig.ids[2]], CullAction::SetRating(Some(3)));
        rig.show(FilterBar::with_markers(
            RatingChoice::Unrated,
            FlagChoice::Any,
            LabelChoice::Any,
        ));
        assert_eq!(rig.session.ids(), &[rig.ids[1], rig.ids[3]]);
    }

    #[test]
    fn marking_inside_a_filter_leaves_the_list_frozen_until_the_filter_changes() {
        let mut rig = Rig::new(4);
        rig.show(FilterBar::with_markers(
            RatingChoice::Unrated,
            FlagChoice::Any,
            LabelChoice::Any,
        ));
        assert_eq!(rig.session.len(), 4);
        rig.session.click(0);
        rig.keys(tap(Key::Num4, Modifiers::NONE));
        // Photo 0 no longer matches "Unrated", yet nothing shifted under the cursor.
        assert_eq!(
            rig.session.len(),
            4,
            "the list is frozen, not reloaded by a mark"
        );
        assert_eq!(rig.session.cursor_id(), Some(rig.ids[1]));
    }
}

struct PanicStore;
impl MetaStore for PanicStore {
    fn get_meta(&self, _ids: &[i64]) -> Result<HashMap<i64, AssetMeta>, String> {
        panic!("the catalog mutex was poisoned");
    }
    fn set_meta(&self, _items: &[(i64, AssetMeta)]) -> Result<(), String> {
        panic!("the catalog mutex was poisoned");
    }
}

#[test]
fn a_dead_writer_is_reported_instead_of_silently_dropping_marks() {
    let mut cull = CullState::new(Arc::new(PanicStore), || {});
    cull.mark(vec![1], CullAction::SetRating(Some(3)));
    wait_until(&mut cull, "the dead writer to be noticed", |c| {
        c.last_error().is_some()
    });
    assert!(
        cull.last_error().unwrap().contains("stopped"),
        "{:?}",
        cull.last_error()
    );
    // And it does not hang or panic the UI thread if the user keeps marking.
    cull.mark(vec![1], CullAction::SetRating(Some(4)));
    cull.poll();
}

#[test]
fn invalidate_forgets_cached_markers_so_a_reused_id_is_re_read() {
    let store = FakeStore::with_ids(1..=1);
    store.put(1, stars(4));
    let mut cull = state(&store);
    cull.ensure([1]);
    cull.settle_all();
    assert_eq!(cull.meta(1), Some(&stars(4)));

    // The photo is deleted and a new one reuses id 1 (an import after a delete).
    store.put(1, stars(1));
    cull.invalidate();
    assert_eq!(cull.meta(1), None);
    cull.ensure([1]);
    cull.settle_all();
    assert_eq!(
        cull.meta(1),
        Some(&stars(1)),
        "the new photo's own markers, not the old ghost"
    );
}
