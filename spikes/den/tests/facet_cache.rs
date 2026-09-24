//! #103: correctness tests for both facet-count-cache candidates, run against the shared
//! `Workload` query set (same pattern as `tests/cross_engine.rs`) plus each candidate's own
//! `verify_against_naive` oracle. A fast cache that isn't checked against a from-scratch
//! recomputation is exactly the "fast but wrong" failure mode #103 requires this to rule out.

#[cfg(feature = "sqlite")]
use den::gen::{generate_catalog, GenOptions, BENCH_LEAF_KEYWORD};
#[cfg(feature = "sqlite")]
use den::workload::Workload;

#[cfg(feature = "sqlite")]
fn fixture() -> Vec<den::gen::Asset> {
    generate_catalog(&GenOptions {
        seed: 7,
        asset_count: 10_000,
        folder_count: 333,
        manifest_path: "docs/ref-10k-manifest.csv".into(),
    })
}

// #113 note: both tests in this file were unconditionally compiled even though the modules they
// import are gated behind `#[cfg(feature = "sqlite")]`/`#[cfg(all(feature = "sqlite", feature =
// "duckdb"))]` in `lib.rs` — a pre-existing gap that only surfaced when trying to build/test `den`
// with the new `libsql` feature and *without* `sqlite` (unavoidable: `rusqlite`'s and `libsql`'s
// bundled SQLite C sources collide at link time if both are enabled — see `libsql_engine.rs`'s
// module doc and `bin/den.rs`'s matching fix for the full explanation). Gated here the same way.
#[cfg(feature = "sqlite")]
#[test]
fn trigger_facet_matches_naive_after_ingest_and_after_writes() {
    use den::facet_cache_trigger::TriggerFacetEngine;

    let dir = tempfile::tempdir().unwrap();
    let assets = fixture();
    let mut engine = TriggerFacetEngine::open(&dir.path().join("trigger-test.db")).unwrap();
    engine.bulk_ingest(&assets).unwrap();

    assert!(
        engine
            .verify_against_naive(Some("NIKON Z 8"), Some(3), Some(BENCH_LEAF_KEYWORD))
            .unwrap(),
        "trigger-maintained facet table disagrees with a from-scratch recomputation right after \
         bulk ingest"
    );
    // Different model/rating combinations, still narrowed by a keyword — the supported query
    // shape (see facet_cache_trigger.rs's module doc for why `keyword_prefix: None` is NOT
    // supported and is tested separately, below, as a known gap rather than skipped silently).
    assert!(
        engine
            .verify_against_naive(None, Some(3), Some(BENCH_LEAF_KEYWORD))
            .unwrap(),
        "trigger-maintained facet table disagrees with naive recomputation (model=None)"
    );
    assert!(
        engine
            .verify_against_naive(Some("NIKON D7500"), Some(0), Some(BENCH_LEAF_KEYWORD))
            .unwrap(),
        "trigger-maintained facet table disagrees with naive recomputation (a different model, \
         rating=0)"
    );

    // Known, real limitation (documented in facet_cache_trigger.rs's module doc, not a surprise):
    // with no keyword filter, `facet_counts`'s (model, rating, keyword) grain overcounts
    // multi-keyword assets and misses zero-keyword ones, so it does NOT match `sqlite.rs`'s
    // per-asset semantics. Asserting the mismatch directly, rather than omitting this case,
    // keeps this a verified gap instead of an unverified claim.
    assert!(
        !engine.verify_against_naive(None, None, None).unwrap(),
        "expected the unfiltered (no keyword_prefix) case to disagree with naive — if this now \
         passes, either the known (model,rating,keyword)-grain limitation was fixed (update this \
         test and the module docs to say so) or the fixture happens to have zero assets with \
         more than one keyword and zero with no keywords at all, which would make this a \
         false-negative test, not a real fix"
    );

    let burst: Vec<(u64, u8)> = assets.iter().take(200).map(|a| (a.id, 5)).collect();
    engine.rate_burst(&burst).unwrap();
    engine
        .tag_keyword(
            &assets.iter().take(500).map(|a| a.id).collect::<Vec<_>>(),
            "Test.Facet.Trigger",
        )
        .unwrap();
    engine.write_rating(assets[0].id, 2).unwrap();

    assert!(
        engine
            .verify_against_naive(Some("NIKON Z 8"), Some(3), Some(BENCH_LEAF_KEYWORD))
            .unwrap(),
        "trigger-maintained facet table disagrees with naive recomputation after a rating burst, \
         a keyword tag, and a single rating write"
    );
    assert!(
        engine
            .verify_against_naive(None, Some(3), Some(BENCH_LEAF_KEYWORD))
            .unwrap(),
        "trigger-maintained facet table disagrees with naive recomputation (model=None) after \
         writes"
    );
    assert!(engine.integrity_check().unwrap());
}

#[cfg(all(feature = "sqlite", feature = "duckdb"))]
#[test]
fn duckdb_cache_matches_naive_after_refresh_and_is_stale_before_it() {
    use den::facet_cache_duckdb::DuckFacetCacheEngine;

    let dir = tempfile::tempdir().unwrap();
    let assets = fixture();
    let mut engine = DuckFacetCacheEngine::open(&dir.path().join("duckdb-cache-test.db")).unwrap();
    engine.bulk_ingest(&assets).unwrap();

    // Reading before any refresh() must fail loudly, not return an empty/zero answer.
    assert!(
        engine
            .faceted_filter(Some("NIKON Z 8"), Some(3), Some(BENCH_LEAF_KEYWORD))
            .is_err(),
        "faceted_filter should refuse to answer before the cache has ever been refreshed"
    );

    engine.refresh().unwrap();
    assert!(
        engine
            .verify_against_naive(Some("NIKON Z 8"), Some(3), Some(BENCH_LEAF_KEYWORD))
            .unwrap(),
        "DuckDB cache disagrees with a from-scratch SQLite recomputation right after refresh()"
    );
    assert!(
        engine
            .verify_against_naive(Some("NIKON D7500"), Some(0), Some(BENCH_LEAF_KEYWORD))
            .unwrap(),
        "DuckDB cache disagrees with naive recomputation (a different model, rating=0)"
    );
    // Known, real limitation shared with the trigger candidate (see facet_cache_duckdb.rs's
    // module doc): with no keyword filter, the (model, rating, keyword)-grain cache does not
    // match SQLite's per-asset semantics. Asserted directly, not omitted.
    assert!(
        !engine.verify_against_naive(None, None, None).unwrap(),
        "expected the unfiltered (no keyword_prefix) case to disagree with naive — see the same \
         note in the trigger test above"
    );

    // Find a real asset in this fixture's exact benchmarked facet, currently below threshold.
    let demo_id = assets
        .iter()
        .find(|a| {
            a.model == "NIKON Z 8"
                && a.rating < 3
                && a.keywords.iter().any(|k| k == BENCH_LEAF_KEYWORD)
        })
        .map(|a| a.id)
        .expect("fixture should contain at least one NIKON Z 8 / low-rating / leaf-keyword asset");

    engine.write_rating(demo_id, 5).unwrap();
    // The whole point of this design: writing to SQLite does NOT keep the cache honest. This is
    // the risk #103 requires to be measured, not glossed over — asserting it's actually stale
    // here (not merely claimed in a doc comment) is exactly the discipline ADR-0009's Turso
    // walk-back was missing.
    assert!(
        !engine
            .verify_against_naive(Some("NIKON Z 8"), Some(3), Some(BENCH_LEAF_KEYWORD))
            .unwrap(),
        "expected the cache to be stale immediately after an unrefreshed write — if this \
         passes, either the demo write didn't change the matching set or refresh() ran \
         implicitly somewhere, either of which would be a real bug to investigate"
    );

    engine.refresh().unwrap();
    assert!(
        engine
            .verify_against_naive(Some("NIKON Z 8"), Some(3), Some(BENCH_LEAF_KEYWORD))
            .unwrap(),
        "DuckDB cache disagrees with naive recomputation after refresh() following a write"
    );
    assert!(engine.integrity_check().unwrap());
}
