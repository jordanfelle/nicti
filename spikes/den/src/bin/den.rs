//! CLI driver for the #67 catalog-database-engine spike. See `spikes/den`'s crate doc.

use clap::{Parser, Subcommand, ValueEnum};
use den::gen::{catalog_hash, generate_catalog, GenOptions};
use den::stats::percentiles;
use den::workload::{RangeQuery, Workload};
use std::path::PathBuf;
use std::time::Instant;

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Generates a synthetic catalog to a JSON file.
    Gen {
        #[arg(long, default_value_t = 42)]
        seed: u64,
        #[arg(long, value_enum, default_value_t = Scale::Full2m)]
        scale: Scale,
        #[arg(long, default_value = "docs/ref-10k-manifest.csv")]
        manifest: PathBuf,
        #[arg(long)]
        out: PathBuf,
        /// Runs generation twice and asserts the output hash matches, rather than writing a file.
        #[arg(long)]
        verify_determinism: bool,
    },
    /// Runs the shared query set against one engine and prints p50/p95/max per operation.
    Bench {
        #[arg(long, value_enum)]
        engine: Engine,
        #[arg(long)]
        catalog: PathBuf,
        #[arg(long, default_value = "bench-results/den")]
        out_dir: PathBuf,
        #[arg(long, default_value_t = 5)]
        runs: u32,
    },
    /// Kills a writer mid-transaction and checks the store reopens cleanly.
    Crash {
        #[arg(long, value_enum)]
        engine: Engine,
        #[arg(long, default_value_t = 20)]
        iterations: u32,
    },
    /// #103: benchmarks the two faceted-filter cache candidates (trigger-maintained SQLite table,
    /// DuckDB-backed read cache) against the same catalog + write-burst workload, plus a
    /// from-scratch correctness check for each.
    FacetBench {
        #[arg(long, value_enum)]
        variant: FacetVariant,
        #[arg(long)]
        catalog: PathBuf,
        #[arg(long, default_value = "bench-results/den")]
        out_dir: PathBuf,
        #[arg(long, default_value_t = 5)]
        runs: u32,
    },
}

#[derive(Clone, Copy, ValueEnum, Debug)]
enum FacetVariant {
    Trigger,
    DuckdbCache,
}

#[derive(Clone, Copy, ValueEnum)]
enum Scale {
    #[value(name = "600k")]
    Full600k,
    #[value(name = "2m")]
    Full2m,
}

#[derive(Clone, Copy, ValueEnum, Debug)]
enum Engine {
    #[cfg(feature = "sqlite")]
    Sqlite,
    #[cfg(feature = "duckdb")]
    Duckdb,
    #[cfg(feature = "lmdb")]
    Lmdb,
    #[cfg(feature = "turso")]
    Turso,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Gen {
            seed,
            scale,
            manifest,
            out,
            verify_determinism,
        } => cmd_gen(
            seed,
            match scale {
                Scale::Full600k => 600_000,
                Scale::Full2m => 2_000_000,
            },
            manifest,
            out,
            verify_determinism,
        ),
        Cmd::Bench {
            engine,
            catalog,
            out_dir,
            runs,
        } => cmd_bench(engine, catalog, out_dir, runs),
        Cmd::Crash { engine, iterations } => cmd_crash(engine, iterations),
        Cmd::FacetBench {
            variant,
            catalog,
            out_dir,
            runs,
        } => cmd_facet_bench(variant, catalog, out_dir, runs),
    }
}

fn cmd_gen(
    seed: u64,
    asset_count: u64,
    manifest: PathBuf,
    out: PathBuf,
    verify_determinism: bool,
) -> anyhow::Result<()> {
    let opts = GenOptions {
        seed,
        asset_count,
        folder_count: (asset_count / 30).max(1),
        manifest_path: manifest,
    };
    let assets = generate_catalog(&opts);
    let hash = catalog_hash(&assets);

    if verify_determinism {
        let assets2 = generate_catalog(&opts);
        let hash2 = catalog_hash(&assets2);
        assert_eq!(
            hash, hash2,
            "generator is not deterministic for the same seed"
        );
        println!("determinism OK: {hash}");
        return Ok(());
    }

    println!("generated {} assets, hash {hash}", assets.len());
    let file = std::fs::File::create(&out)?;
    serde_json::to_writer(std::io::BufWriter::new(file), &assets)?;
    println!("wrote {}", out.display());
    Ok(())
}

fn load_catalog(path: &PathBuf) -> anyhow::Result<Vec<den::gen::Asset>> {
    let file = std::fs::File::open(path)?;
    Ok(serde_json::from_reader(std::io::BufReader::new(file))?)
}

/// Times `$op` (an `anyhow::Result<T>` expression) over `$runs` measured iterations, after one
/// discarded warm-up run, per `docs/benchmarks.md`'s methodology. Propagates a real error with
/// `?` instead of swallowing it — a query that fails partway through must not silently read as
/// "fast".
macro_rules! time_op {
    ($runs:expr, $op:expr) => {{
        let mut samples = Vec::with_capacity($runs as usize);
        $op?;
        for _ in 0..$runs {
            let start = Instant::now();
            $op?;
            samples.push(start.elapsed());
        }
        percentiles(&samples)
    }};
}

fn cmd_bench(engine: Engine, catalog: PathBuf, out_dir: PathBuf, runs: u32) -> anyhow::Result<()> {
    let assets = load_catalog(&catalog)?;
    std::fs::create_dir_all(&out_dir)?;
    let tmp = tempfile::tempdir()?;

    let mut results = serde_json::Map::new();
    match engine {
        #[cfg(feature = "sqlite")]
        Engine::Sqlite => bench_engine::<den::sqlite::SqliteEngine>(
            &tmp.path().join("den.sqlite3"),
            &assets,
            runs,
            &mut results,
        )?,
        #[cfg(feature = "duckdb")]
        Engine::Duckdb => bench_engine::<den::duckdb_engine::DuckDbEngine>(
            &tmp.path().join("den.duckdb"),
            &assets,
            runs,
            &mut results,
        )?,
        #[cfg(feature = "lmdb")]
        Engine::Lmdb => bench_engine::<den::lmdb::LmdbEngine>(
            &tmp.path().join("den-lmdb"),
            &assets,
            runs,
            &mut results,
        )?,
        #[cfg(feature = "turso")]
        Engine::Turso => bench_engine::<den::turso_engine::TursoEngine>(
            &tmp.path().join("den-turso.db"),
            &assets,
            runs,
            &mut results,
        )?,
    }

    let out_path = out_dir
        .join(format!("{:?}.json", engine).to_lowercase())
        .with_extension("json");
    std::fs::write(&out_path, serde_json::to_string_pretty(&results)?)?;
    println!("wrote {}", out_path.display());
    Ok(())
}

fn bench_engine<E: Workload>(
    path: &std::path::Path,
    assets: &[den::gen::Asset],
    runs: u32,
    results: &mut serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<()> {
    let cold_open_start = Instant::now();
    let mut engine = E::open(path)?;
    let cold_open = cold_open_start.elapsed();
    results.insert(
        "cold_open_ms".into(),
        (cold_open.as_secs_f64() * 1000.0).into(),
    );

    let ingest_start = Instant::now();
    engine.bulk_ingest(assets)?;
    results.insert(
        "bulk_ingest_ms".into(),
        (ingest_start.elapsed().as_secs_f64() * 1000.0).into(),
    );

    let write_rating = time_op!(runs, engine.write_rating(assets[0].id, 4));
    results.insert("write_rating".into(), serde_json::to_value(write_rating)?);

    let burst: Vec<(u64, u8)> = assets.iter().take(100).map(|a| (a.id, 5)).collect();
    let rate_burst = time_op!(runs, engine.rate_burst(&burst));
    results.insert("rate_burst_100".into(), serde_json::to_value(rate_burst)?);

    // A distinct keyword per call (not the same "Bench.Tagged" 6 times): SQLite/DuckDB's
    // `tag_keyword` is a bare INSERT with no dedup, so reusing one keyword across the 1
    // discarded-warm-up + 5 measured calls would append another 10k rows *every* call, ending
    // with 60k accumulated duplicate rows and a growing index each engine measures a different
    // table size against. LMDB's equivalent index entry is a plain key overwrite (naturally
    // idempotent), so without this fix the comparison silently pitted a workload that grows every
    // iteration (SQLite/DuckDB) against a static one (LMDB) under the same "p50/p95" label.
    let tag_ids: Vec<u64> = assets.iter().take(10_000).map(|a| a.id).collect();
    let mut tag_call = 0u32;
    let tag_keyword = time_op!(runs, {
        tag_call += 1;
        engine.tag_keyword(&tag_ids, &format!("Bench.Tagged.{tag_call}"))
    });
    results.insert("tag_keyword_10k".into(), serde_json::to_value(tag_keyword)?);

    // A genuinely rare hierarchy leaf (one named event out of thousands), not a broad top-level
    // branch: matches how a real culling/filter UX narrows a selection. See
    // `gen::BENCH_LEAF_KEYWORD`'s doc comment for why the broad-branch case (which matches most
    // of the corpus) is an inherently different — near-full-scan — workload for any engine, not a
    // store-specific weakness, and isn't what this gate measures.
    let faceted_filter = time_op!(
        runs,
        engine.faceted_filter(
            Some("NIKON Z 8"),
            Some(3),
            Some(den::gen::BENCH_LEAF_KEYWORD)
        )
    );
    results.insert(
        "faceted_filter".into(),
        serde_json::to_value(faceted_filter)?,
    );

    let sort_page = time_op!(runs, engine.sort_by_date_page(0, 500));
    results.insert("sort_by_date_page".into(), serde_json::to_value(sort_page)?);

    let folder_count = time_op!(runs, engine.folder_subtree_count("NVMe/2024"));
    results.insert(
        "folder_subtree_count".into(),
        serde_json::to_value(folder_count)?,
    );

    let keyword_query = time_op!(
        runs,
        engine.keyword_subtree_query(den::gen::BENCH_LEAF_KEYWORD)
    );
    results.insert(
        "keyword_subtree_query".into(),
        serde_json::to_value(keyword_query)?,
    );

    let range = RangeQuery {
        min_rating: 3,
        max_rating: 5,
        min_iso: 100,
        max_iso: 3200,
        date_from: "2023-01-01".into(),
        date_to: "2024-12-31".into(),
    };
    let range_query = time_op!(runs, engine.range_query(&range));
    results.insert("range_query".into(), serde_json::to_value(range_query)?);

    let filename_search = time_op!(runs, engine.filename_search("00001"));
    results.insert(
        "filename_search".into(),
        serde_json::to_value(filename_search)?,
    );

    // A fresh destination per call: most engines' online-backup API refuses to write over an
    // existing file (SQLite's VACUUM INTO does), so reusing one path would silently no-op or
    // error on every call after the first.
    let mut backup_call = 0u32;
    let backup = time_op!(runs, {
        backup_call += 1;
        let dest = path.with_extension(format!("backup{backup_call}"));
        engine.backup(&dest)
    });
    results.insert("backup".into(), serde_json::to_value(backup)?);

    let integrity_ok = engine.integrity_check()?;
    results.insert("integrity_ok".into(), integrity_ok.into());

    Ok(())
}

fn cmd_facet_bench(
    variant: FacetVariant,
    catalog: PathBuf,
    out_dir: PathBuf,
    runs: u32,
) -> anyhow::Result<()> {
    let assets = load_catalog(&catalog)?;
    std::fs::create_dir_all(&out_dir)?;
    let tmp = tempfile::tempdir()?;

    let mut results = serde_json::Map::new();
    match variant {
        FacetVariant::Trigger => bench_trigger_facet(
            &tmp.path().join("den-facet-trigger.sqlite3"),
            &assets,
            runs,
            &mut results,
        )?,
        FacetVariant::DuckdbCache => bench_duckdb_facet_cache(
            &tmp.path().join("den-facet-duckdb.sqlite3"),
            &assets,
            runs,
            &mut results,
        )?,
    }

    let out_path = out_dir.join(format!("facet_{variant:?}.json").to_lowercase());
    std::fs::write(&out_path, serde_json::to_string_pretty(&results)?)?;
    println!("wrote {}", out_path.display());
    Ok(())
}

/// Candidate 1 (#103): trigger-maintained SQLite facet table. Reuses the exact query set
/// `bench_engine` runs for the primary-store comparison, plus two from-scratch correctness
/// checks (right after bulk ingest, and again after the write ops below have run) — a fast
/// answer is only worth reporting if it's also checked against the naive recomputation, per
/// #103's own required workflow.
fn bench_trigger_facet(
    path: &std::path::Path,
    assets: &[den::gen::Asset],
    runs: u32,
    results: &mut serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<()> {
    use den::facet_cache_trigger::TriggerFacetEngine;
    use den::workload::Workload;

    let cold_open_start = Instant::now();
    let mut engine = TriggerFacetEngine::open(path)?;
    results.insert(
        "cold_open_ms".into(),
        (cold_open_start.elapsed().as_secs_f64() * 1000.0).into(),
    );

    let ingest_start = Instant::now();
    engine.bulk_ingest(assets)?;
    results.insert(
        "bulk_ingest_ms".into(),
        (ingest_start.elapsed().as_secs_f64() * 1000.0).into(),
    );

    let correct_after_ingest = engine.verify_against_naive(
        Some("NIKON Z 8"),
        Some(3),
        Some(den::gen::BENCH_LEAF_KEYWORD),
    )?;
    results.insert(
        "facet_correctness_ok_after_ingest".into(),
        correct_after_ingest.into(),
    );

    // The same per-write ops `bench_engine` times for every primary-store candidate, so the
    // trigger-maintenance overhead added to each is directly comparable to the plain-SQLite
    // baseline (`den bench --engine sqlite`) run in the same session.
    let write_rating = time_op!(runs, engine.write_rating(assets[0].id, 4));
    results.insert("write_rating".into(), serde_json::to_value(write_rating)?);

    let burst: Vec<(u64, u8)> = assets.iter().take(100).map(|a| (a.id, 5)).collect();
    let rate_burst = time_op!(runs, engine.rate_burst(&burst));
    results.insert("rate_burst_100".into(), serde_json::to_value(rate_burst)?);

    let tag_ids: Vec<u64> = assets.iter().take(10_000).map(|a| a.id).collect();
    let mut tag_call = 0u32;
    let tag_keyword = time_op!(runs, {
        tag_call += 1;
        engine.tag_keyword(&tag_ids, &format!("Bench.Tagged.{tag_call}"))
    });
    results.insert("tag_keyword_10k".into(), serde_json::to_value(tag_keyword)?);

    let faceted_filter = time_op!(
        runs,
        engine.faceted_filter(
            Some("NIKON Z 8"),
            Some(3),
            Some(den::gen::BENCH_LEAF_KEYWORD)
        )
    );
    results.insert(
        "faceted_filter".into(),
        serde_json::to_value(faceted_filter)?,
    );

    // Re-verify after the write burst above (100 rating changes + a 10k-row keyword tag): the
    // trigger-maintained table must stay correct under writes, not just at initial ingest.
    let correct_after_writes = engine.verify_against_naive(
        Some("NIKON Z 8"),
        Some(3),
        Some(den::gen::BENCH_LEAF_KEYWORD),
    )?;
    results.insert(
        "facet_correctness_ok_after_writes".into(),
        correct_after_writes.into(),
    );

    let integrity_ok = engine.integrity_check()?;
    results.insert("integrity_ok".into(), integrity_ok.into());

    Ok(())
}

/// Candidate 2 (#103): DuckDB-backed read-side facet cache. Writes go only to SQLite (measured
/// identically to plain `sqlite.rs`, since it *is* `sqlite.rs`'s schema underneath); the cache is
/// refreshed explicitly, and its cost is reported as its own distinct metric rather than folded
/// into any write op's latency. Also demonstrates staleness concretely: a specific asset known to
/// belong to the benchmarked facet is rated *without* a refresh, and the cache is checked against
/// the naive recomputation both before and after the following `refresh()` call.
fn bench_duckdb_facet_cache(
    path: &std::path::Path,
    assets: &[den::gen::Asset],
    runs: u32,
    results: &mut serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<()> {
    use den::facet_cache_duckdb::DuckFacetCacheEngine;
    use den::workload::Workload;

    let cold_open_start = Instant::now();
    let mut engine = DuckFacetCacheEngine::open(path)?;
    results.insert(
        "cold_open_ms".into(),
        (cold_open_start.elapsed().as_secs_f64() * 1000.0).into(),
    );

    let ingest_start = Instant::now();
    engine.bulk_ingest(assets)?;
    results.insert(
        "bulk_ingest_ms".into(),
        (ingest_start.elapsed().as_secs_f64() * 1000.0).into(),
    );

    let refresh_after_ingest = engine.refresh()?;
    results.insert(
        "cache_refresh_after_bulk_ingest_ms".into(),
        (refresh_after_ingest.as_secs_f64() * 1000.0).into(),
    );

    let correct_after_ingest = engine.verify_against_naive(
        Some("NIKON Z 8"),
        Some(3),
        Some(den::gen::BENCH_LEAF_KEYWORD),
    )?;
    results.insert(
        "facet_correctness_ok_after_ingest".into(),
        correct_after_ingest.into(),
    );

    let faceted_filter = time_op!(
        runs,
        engine.faceted_filter(
            Some("NIKON Z 8"),
            Some(3),
            Some(den::gen::BENCH_LEAF_KEYWORD)
        )
    );
    results.insert(
        "faceted_filter".into(),
        serde_json::to_value(faceted_filter)?,
    );

    // Same write ops (and same generic ids) as the trigger candidate/`bench_engine`, so
    // point-update cost is directly comparable — these go straight to SQLite, untouched by the
    // DuckDB cache, so this number should look identical to plain `sqlite.rs`.
    let write_rating = time_op!(runs, engine.write_rating(assets[0].id, 4));
    results.insert("write_rating".into(), serde_json::to_value(write_rating)?);

    let burst: Vec<(u64, u8)> = assets.iter().take(100).map(|a| (a.id, 5)).collect();
    let rate_burst = time_op!(runs, engine.rate_burst(&burst));
    results.insert("rate_burst_100".into(), serde_json::to_value(rate_burst)?);

    let tag_ids: Vec<u64> = assets.iter().take(10_000).map(|a| a.id).collect();
    let mut tag_call = 0u32;
    let tag_keyword = time_op!(runs, {
        tag_call += 1;
        engine.tag_keyword(&tag_ids, &format!("Bench.Tagged.{tag_call}"))
    });
    results.insert("tag_keyword_10k".into(), serde_json::to_value(tag_keyword)?);

    // A concrete staleness demonstration, not just a theoretical risk: find a real asset that
    // belongs to the exact facet under benchmark (NIKON Z 8, this leaf keyword) but currently
    // falls below the rating threshold, and push it above threshold *without* calling refresh().
    // If the cache were being read naively, this would silently under-count by exactly one.
    let stale_demo_id = assets.iter().find_map(|a| {
        if a.model == "NIKON Z 8"
            && a.rating < 3
            && a.keywords.iter().any(|k| k == den::gen::BENCH_LEAF_KEYWORD)
        {
            Some(a.id)
        } else {
            None
        }
    });
    if let Some(id) = stale_demo_id {
        engine.write_rating(id, 5)?;
        // Expected to be `false`: this is the demonstration that the cache is stale between
        // refreshes, not a bug in `verify_against_naive` — the naive side sees the just-written
        // rating immediately (it reads SQLite directly), the cached side doesn't until the next
        // `refresh()` below.
        let correctness_while_stale = engine.verify_against_naive(
            Some("NIKON Z 8"),
            Some(3),
            Some(den::gen::BENCH_LEAF_KEYWORD),
        )?;
        results.insert(
            "facet_correctness_ok_while_stale".into(),
            correctness_while_stale.into(),
        );
        results.insert("stale_demo_ran".into(), true.into());
    } else {
        // No asset in this catalog happened to match all three conditions (possible at small
        // scales/seeds) — reported explicitly rather than silently skipped.
        results.insert("stale_demo_ran".into(), false.into());
    }

    let refresh_after_burst = engine.refresh()?;
    results.insert(
        "cache_refresh_after_burst_ms".into(),
        (refresh_after_burst.as_secs_f64() * 1000.0).into(),
    );

    let correct_after_refresh = engine.verify_against_naive(
        Some("NIKON Z 8"),
        Some(3),
        Some(den::gen::BENCH_LEAF_KEYWORD),
    )?;
    results.insert(
        "facet_correctness_ok_after_refresh".into(),
        correct_after_refresh.into(),
    );

    let faceted_filter_after_refresh = time_op!(
        runs,
        engine.faceted_filter(
            Some("NIKON Z 8"),
            Some(3),
            Some(den::gen::BENCH_LEAF_KEYWORD)
        )
    );
    results.insert(
        "faceted_filter_after_refresh".into(),
        serde_json::to_value(faceted_filter_after_refresh)?,
    );

    let integrity_ok = engine.integrity_check()?;
    results.insert("integrity_ok".into(), integrity_ok.into());

    Ok(())
}

fn cmd_crash(engine: Engine, iterations: u32) -> anyhow::Result<()> {
    // Real cross-process kill -9 needs a helper binary fork; documented in the ADR as a follow-up
    // if this in-process approximation isn't convincing enough for the crash-safety gate.
    // `crash_mid_ingest` (not `bulk_ingest`) leaves an open, uncommitted transaction for SQLite/
    // DuckDB before `mem::forget` drops the handle without ever calling COMMIT or ROLLBACK,
    // matching what an OS-level SIGKILL mid-transaction leaves behind — an earlier version of
    // this test called `bulk_ingest` (which commits internally) here, so it only ever exercised
    // reopening after an already-fully-committed write, not a genuinely interrupted one.
    let tmp = tempfile::tempdir()?;
    let failures = match engine {
        #[cfg(feature = "sqlite")]
        Engine::Sqlite => {
            crash_loop::<den::sqlite::SqliteEngine>(tmp.path(), "sqlite3", iterations)?
        }
        #[cfg(feature = "duckdb")]
        Engine::Duckdb => {
            crash_loop::<den::duckdb_engine::DuckDbEngine>(tmp.path(), "duckdb", iterations)?
        }
        #[cfg(feature = "lmdb")]
        Engine::Lmdb => crash_loop::<den::lmdb::LmdbEngine>(tmp.path(), "lmdb", iterations)?,
        #[cfg(feature = "turso")]
        Engine::Turso => {
            crash_loop::<den::turso_engine::TursoEngine>(tmp.path(), "turso.db", iterations)?
        }
    };
    println!("{engine:?}: {failures}/{iterations} crash-reopen failures");
    Ok(())
}

fn crash_loop<E: Workload>(
    dir: &std::path::Path,
    ext: &str,
    iterations: u32,
) -> anyhow::Result<u32> {
    let mut failures = 0u32;
    for i in 0..iterations {
        // A fresh store path per iteration, not one file reused 20 times: with `crash_mid_ingest`
        // now leaving a genuinely *uncommitted* transaction for SQLite/DuckDB (see cmd_crash's
        // doc comment), the forgotten handle's file descriptor is never closed for the rest of
        // this process's lifetime — unlike a real crash, where the OS releases every lock a dead
        // process held. Reusing one path surfaced exactly that gap as a spurious "database is
        // locked" error on SQLite's second iteration, which is a leaked-fd artifact of staying in
        // one process for 20 "crashes," not a finding about crash safety. A fresh path per
        // iteration sidesteps it and is the more correct design anyway: independent trials, not
        // one file accumulating 20 rounds of abandoned state.
        //
        // This does NOT change LMDB's own already-documented result (see the ADR's hard-gate-3
        // finding): `heed`/`liblmdb`'s open-environment guard rejects reopening *any* path once a
        // handle to it has been forgotten, even a path that's only ever been opened once before —
        // it isn't specifically about reusing a path across trials, so a fresh path per iteration
        // doesn't help LMDB the way it does the other two. Confirmed here (not just asserted):
        // every LMDB iteration fails at its own within-iteration reopen, not just iteration 2+.
        let path = dir.join(format!("den-crash-{i}.{ext}"));
        {
            let mut e = E::open(&path)?;
            let assets = den::gen::generate_catalog(&den::gen::GenOptions {
                seed: i as u64,
                asset_count: 1000,
                folder_count: 10,
                manifest_path: "docs/ref-10k-manifest.csv".into(),
            });
            e.crash_mid_ingest(&assets)?;
            e.prepare_for_forget();
            std::mem::forget(e);
        }
        match E::open(&path).and_then(|e| e.integrity_check()) {
            Ok(true) => {}
            Ok(false) => failures += 1,
            Err(e) => {
                eprintln!("iteration {i} reopen/integrity-check failed: {e:#}");
                failures += 1;
            }
        }
    }
    Ok(failures)
}
