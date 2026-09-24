//! Every engine must agree on the same query set against the same small (10k) fixture — the
//! whole point of the shared `Workload` trait (see `src/workload.rs`) is that a p50/p95 number
//! only means something if every backend answers the identical question.

use den::gen::{generate_catalog, GenOptions, BENCH_LEAF_KEYWORD};
use den::workload::{RangeQuery, Workload};

fn fixture() -> Vec<den::gen::Asset> {
    generate_catalog(&GenOptions {
        seed: 7,
        asset_count: 10_000,
        folder_count: 333,
        manifest_path: "docs/ref-10k-manifest.csv".into(),
    })
}

fn check<E: Workload>(dir: &tempfile::TempDir, name: &str) {
    let assets = fixture();
    let mut engine = E::open(&dir.path().join(name)).unwrap();
    engine.bulk_ingest(&assets).unwrap();

    let range = RangeQuery {
        min_rating: 3,
        max_rating: 5,
        min_iso: 100,
        max_iso: 3200,
        date_from: "2020-01-01".into(),
        date_to: "2026-12-31".into(),
    };
    let mut range_ids = engine.range_query(&range).unwrap();
    range_ids.sort_unstable();
    for id in &range_ids {
        let a = assets.iter().find(|a| a.id == *id).unwrap();
        assert!(a.rating >= 3 && a.rating <= 5, "{name}: range_query returned an out-of-range asset");
        assert!(a.iso >= 100 && a.iso <= 3200, "{name}: range_query returned an out-of-range asset");
    }
    let expected_range: usize = assets
        .iter()
        .filter(|a| a.rating >= 3 && a.rating <= 5 && a.iso >= 100 && a.iso <= 3200)
        .count();
    assert_eq!(range_ids.len(), expected_range, "{name}: range_query count mismatch");

    let mut kw_ids = engine.keyword_subtree_query(BENCH_LEAF_KEYWORD).unwrap();
    kw_ids.sort_unstable();
    let expected_kw: Vec<u64> = {
        let mut v: Vec<u64> = assets
            .iter()
            .filter(|a| a.keywords.iter().any(|k| k.starts_with(BENCH_LEAF_KEYWORD)))
            .map(|a| a.id)
            .collect();
        v.sort_unstable();
        v
    };
    assert_eq!(kw_ids, expected_kw, "{name}: keyword_subtree_query mismatch");

    engine.write_rating(assets[0].id, 5).unwrap();
    let facets =
        engine.faceted_filter(None, Some(5), Some(BENCH_LEAF_KEYWORD)).unwrap();
    assert!(
        facets.by_rating.iter().all(|(r, _)| *r == 5),
        "{name}: faceted_filter min_rating=5 returned a lower rating"
    );

    assert!(engine.integrity_check().unwrap(), "{name}: integrity_check failed on a fresh store");
}

#[cfg(feature = "sqlite")]
#[test]
fn sqlite_matches_shared_workload() {
    let dir = tempfile::tempdir().unwrap();
    check::<den::sqlite::SqliteEngine>(&dir, "sqlite-test.db");
}

#[cfg(feature = "duckdb")]
#[test]
fn duckdb_matches_shared_workload() {
    let dir = tempfile::tempdir().unwrap();
    check::<den::duckdb_engine::DuckDbEngine>(&dir, "duckdb-test.db");
}

#[cfg(feature = "lmdb")]
#[test]
fn lmdb_matches_shared_workload() {
    let dir = tempfile::tempdir().unwrap();
    check::<den::lmdb::LmdbEngine>(&dir, "lmdb-test");
}
