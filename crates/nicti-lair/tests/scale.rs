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
