//! Manual scale check for #30's grid snapshot: 1M assets in a file-backed catalog. `#[ignore]`d
//! because seeding takes a while and wall-clock assertions don't belong in CI (see #314 for what
//! those do to a loaded runner) -- run it by hand and read the printed timings:
//!
//! ```text
//! cargo test -p nicti-lair --release --test scale -- --ignored --nocapture
//! ```
//!
//! The budgets asserted are deliberately loose (an order of magnitude over what a normal machine
//! does) so this only fails on a real regression, e.g. a sort falling back to a temp B-tree.

use std::time::{Duration, Instant};

use nicti_lair::{
    CatalogStore, Filter, Preview, PreviewTier, Sort, SortDirection, SortField, SqliteCatalog,
};

const ASSETS: usize = 1_000_000;

fn time<T>(what: &str, f: impl FnOnce() -> T) -> (T, Duration) {
    let start = Instant::now();
    let out = f();
    let took = start.elapsed();
    println!("{what}: {took:.2?}");
    (out, took)
}

#[test]
#[ignore = "seeds 1M rows; run manually with --ignored --nocapture"]
fn grid_snapshot_and_thumbnail_reads_at_one_million_assets() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteCatalog::open(&dir.path().join("catalog.db")).unwrap();
    let volume = store.upsert_volume("scale", None, None, 0).unwrap();
    let root = store.ensure_root(volume, "/photos").unwrap();

    // 64 real-sized (~138 KB, ADR-0029's measured T0) previews, enough for one thumbnail batch.
    let preview = Preview {
        width: Some(640),
        height: Some(424),
        bytes: vec![0xAB; 138 * 1024],
    };
    time("seed 1M assets", || {
        store.bulk_seed(root, ASSETS, Some((&preview, 64))).unwrap()
    });

    for field in [
        SortField::Captured,
        SortField::Imported,
        SortField::Filename,
        SortField::Rating,
    ] {
        for direction in [SortDirection::Asc, SortDirection::Desc] {
            let sort = Sort { field, direction };
            for (label, filter) in [
                ("all roots", Filter::default()),
                (
                    "one root",
                    Filter {
                        root_id: Some(root),
                        ..Default::default()
                    },
                ),
            ] {
                let (ids, took) = time(
                    &format!("hunt_ids {field:?} {direction:?} ({label})"),
                    || store.hunt_ids(&filter, sort).unwrap(),
                );
                assert_eq!(ids.len(), ASSETS);
                assert!(
                    took < Duration::from_secs(10),
                    "{field:?} {direction:?} ({label}) took {took:?}"
                );
            }
        }
    }

    // First grid page, the ADR-0067 "sort by date + first page < 100 ms" gate, via keyset paging.
    let sort = Sort {
        field: SortField::Captured,
        direction: SortDirection::Asc,
    };
    let (page, took) = time("first hunt page (limit 200)", || {
        store
            .hunt(
                &Filter::default(),
                sort,
                &nicti_lair::Page {
                    after: None,
                    limit: 200,
                },
            )
            .unwrap()
    });
    assert_eq!(page.len(), 200);
    assert!(
        took < Duration::from_millis(500),
        "first page took {took:?}"
    );

    let ids = store.hunt_ids(&Filter::default(), sort).unwrap();
    // The first-seeded 64 assets are the ones with previews: ids 1..=64 in insertion order.
    let batch: Vec<i64> = (1..=64).collect();
    let (previews, took) = time("get_previews (64 x ~138 KB)", || {
        store.get_previews(&batch, PreviewTier::T0).unwrap()
    });
    assert_eq!(previews.len(), 64);
    assert!(
        took < Duration::from_millis(500),
        "batch read took {took:?}"
    );

    println!(
        "snapshot memory: {} ids x 8 B = {:.1} MB",
        ids.len(),
        ids.len() as f64 * 8.0 / 1e6
    );
}

/// #32's marker filters (rating / flag / label) over 1M assets. `unrated`, `unflagged` and
/// `no_label` have no index (a `NULL` test can't use the sort expression indexes), so this is the
/// measurement of what a filter change costs next to an unfiltered snapshot. `bulk_seed` already
/// gives the synthetic library a spread of ratings/flags/labels; ~5% more are re-marked on top so
/// "rejected" and "label Red" are non-trivial. Read the printed numbers (result sizes included);
/// the assertions are loose regression tripwires only.
///
/// Measured 2026-09-29 (release, WSL2, 1M assets, Filename sort): every filter -- these and the
/// pre-existing rating/flag/label ones -- takes ~1.5-2.0 s, the same as the unfiltered snapshot
/// (2.0 s): the sort-order index scan dominates, not the predicate. `hunt_ids` runs on a Pounce
/// job so the UI never blocks on it. Above `docs/benchmarks.md`'s < 100 ms library budget at this
/// size; that gap predates #32 and is not closed by it.
#[test]
#[ignore = "seeds 1M rows; run manually with --ignored --nocapture"]
fn marker_filters_at_one_million_assets() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteCatalog::open(&dir.path().join("catalog.db")).unwrap();
    let volume = store.upsert_volume("scale", None, None, 0).unwrap();
    let root = store.ensure_root(volume, "/photos").unwrap();
    time("seed 1M assets", || {
        store.bulk_seed(root, ASSETS, None).unwrap()
    });

    let sort = Sort {
        field: SortField::Filename,
        direction: SortDirection::Asc,
    };
    let all = store.hunt_ids(&Filter::default(), sort).unwrap();
    // Re-mark ~5% of the library, spread out: stars 1..=5 rotating, every 3rd of those picked,
    // every 4th labelled, every 20th of the marked rejected instead.
    let marked: Vec<i64> = all.iter().copied().step_by(20).collect();
    time("mark ~5% (set_meta)", || {
        let items: Vec<(i64, nicti_lair::AssetMeta)> = marked
            .iter()
            .enumerate()
            .map(|(n, id)| {
                (
                    *id,
                    nicti_lair::AssetMeta {
                        rating: Some(if n % 20 == 0 { -1 } else { (n % 5) as i64 + 1 }),
                        flag: (n % 3 == 0 && n % 20 != 0).then_some(1),
                        label: (n % 4 == 0).then(|| "Red".to_string()),
                    },
                )
            })
            .collect();
        store.set_meta(&items).unwrap();
    });

    let cases: Vec<(&str, Filter)> = vec![
        ("baseline (no marker filter)", Filter::default()),
        (
            "unrated",
            Filter {
                unrated: true,
                ..Default::default()
            },
        ),
        (
            "exactly 1 star",
            Filter {
                rating_min: Some(1),
                rating_max: Some(1),
                ..Default::default()
            },
        ),
        (
            "rejected",
            Filter {
                rating_min: Some(-1),
                rating_max: Some(-1),
                ..Default::default()
            },
        ),
        (
            "picked",
            Filter {
                flag: Some(1),
                ..Default::default()
            },
        ),
        (
            "unflagged",
            Filter {
                unflagged: true,
                ..Default::default()
            },
        ),
        (
            "label Red",
            Filter {
                label: Some("Red".into()),
                ..Default::default()
            },
        ),
        (
            "no label",
            Filter {
                no_label: true,
                ..Default::default()
            },
        ),
        (
            "unrated + unflagged + no label",
            Filter {
                unrated: true,
                unflagged: true,
                no_label: true,
                ..Default::default()
            },
        ),
    ];
    for (label, filter) in &cases {
        let (ids, took) = time(&format!("hunt_ids {label}"), || {
            store.hunt_ids(filter, sort).unwrap()
        });
        println!("    -> {} photos", ids.len());
        assert!(took < Duration::from_secs(10), "{label} took {took:?}");
    }
    // A count-only read (no id list): what "how many match" costs.
    let unrated = Filter {
        unrated: true,
        ..Default::default()
    };
    let expected = store.hunt_ids(&unrated, sort).unwrap().len();
    let (n, took) = time("hunt_count unrated", || store.hunt_count(&unrated).unwrap());
    assert_eq!(n as usize, expected, "count and id list must agree");
    assert!(took < Duration::from_secs(10));
}
