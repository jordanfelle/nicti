//! #115's specific reason for existing: every prior catalog-engine candidate (SQLite, DuckDB,
//! LMDB, Turso, redb) was benchmarked with a single-threaded workload only. This module runs a
//! genuinely concurrent multi-writer-thread workload against both RocksDB and SQLite (`rusqlite`,
//! WAL mode) on the same hardware, same thread counts — a real side-by-side measurement of
//! write-serialization behavior, not an assumption that RocksDB's LSM/no-lock-file architecture is
//! faster here just because of its reputation.
//!
//! **Workload shape:** `n_threads` threads, each with its own DB handle/connection to the *same*
//! store, each updating its own disjoint slice of `writes_per_thread` existing rows. Disjoint key
//! ranges per thread, not shared/contended keys: this is the realistic shape of concurrent catalog
//! writers (several culling/tagging operations touching different photos at the same time), and it
//! isolates the engine's own write-path serialization from application-level lock contention on a
//! single row.
//!
//! **This is deliberately a lighter operation than `Workload::write_rating`, on both sides, not an
//! equal-weight comparison of the real catalog write** — an earlier draft of this comment claimed
//! equivalence, corrected here after a hostile review caught it. RocksDB's write here is a single
//! bare `db.put()` on the default column family (no read, no secondary-index maintenance, no
//! `WriteBatch`), versus the real `write_rating`'s read-modify-write across two column families
//! (`assets` + `by_rating`, see `rocksdb_engine.rs`). SQLite's write here is a real `UPDATE`
//! against a table with **no secondary indexes**, versus the real `assets` table's three indexes
//! (`idx_assets_rating`, `idx_assets_range`, `idx_assets_model_rating`, see `sqlite.rs`) that an
//! actual rating write maintains. Both sides are simplified in the same direction (fewer indexes,
//! no read), which keeps the *qualitative* comparison (does either engine show a
//! single-writer-serialization signature?) meaningful, but RocksDB's simplification removes
//! proportionally more work than SQLite's does — so any specific throughput multiplier this
//! benchmark reports should be read as an upper bound on RocksDB's real advantage over a faithful
//! `write_rating`-shaped concurrent write, not a precise prediction of it. See this ADR's own
//! concurrent-writer section for the full caveat.
//!
//! **RocksDB**: a plain `rocksdb::DB` (not `TransactionDB`) wrapped in `Arc`, shared across
//! threads — `rocksdb::DB` is `Send + Sync` and multi-threaded `put`/`write` is RocksDB's normal,
//! documented usage pattern (internally: each thread's write enters a queue, one thread becomes
//! the "write group leader" and batches the group's writes into one WAL append + memtable insert,
//! per RocksDB's own architecture docs) — this is exactly the "no single-writer-lock-file" design
//! #115 exists to test, not a hand-tuned special case.
//!
//! **SQLite**: `rusqlite` in WAL mode, each thread opening its *own* `Connection` to the same file
//! (the realistic multi-writer topology for an app, not one connection shared behind a mutex,
//! which would trivially and artificially serialize everything regardless of the engine). A
//! `busy_timeout` is set so a writer blocked by WAL mode's single-writer rule waits and retries
//! rather than immediately erroring — the wait shows up in that write's own recorded latency,
//! which is precisely the serialization-stall behavior this comparison exists to surface.

use rusqlite::Connection;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, serde::Serialize)]
pub struct ConcurrentBenchResult {
    pub n_threads: u32,
    pub writes_per_thread: u32,
    pub total_writes: u32,
    pub wall_time_ms: f64,
    pub aggregate_writes_per_sec: f64,
    pub latency_p50_ms: f64,
    pub latency_p95_ms: f64,
    pub latency_max_ms: f64,
    /// SQLite-only: count of `SQLITE_BUSY` retries observed (0 for RocksDB, which has no
    /// equivalent busy-wait/retry concept in its write path).
    pub busy_retries: u32,
}

fn summarize(
    n_threads: u32,
    writes_per_thread: u32,
    wall_time: Duration,
    mut all_latencies: Vec<Duration>,
    busy_retries: u32,
) -> ConcurrentBenchResult {
    all_latencies.sort_unstable();
    let total_writes = n_threads * writes_per_thread;
    let p50 = all_latencies[all_latencies.len() / 2];
    let p95 = all_latencies[(all_latencies.len() * 95 / 100).min(all_latencies.len() - 1)];
    let max = *all_latencies.last().unwrap();
    ConcurrentBenchResult {
        n_threads,
        writes_per_thread,
        total_writes,
        wall_time_ms: wall_time.as_secs_f64() * 1000.0,
        aggregate_writes_per_sec: total_writes as f64 / wall_time.as_secs_f64(),
        latency_p50_ms: p50.as_secs_f64() * 1000.0,
        latency_p95_ms: p95.as_secs_f64() * 1000.0,
        latency_max_ms: max.as_secs_f64() * 1000.0,
        busy_retries,
    }
}

/// Populates `n_rows` pre-existing rows (ids `0..n_rows`) so the concurrent phase writes real
/// updates, not inserts into empty space — matching what `write_rating` measures elsewhere.
#[cfg(feature = "rocksdb")]
pub fn run_concurrent_writers_rocksdb(
    path: &Path,
    n_rows: u64,
    n_threads: u32,
    writes_per_thread: u32,
) -> anyhow::Result<ConcurrentBenchResult> {
    use rocksdb::{Options, WriteBatch, DB};

    let mut opts = Options::default();
    opts.create_if_missing(true);
    let db = Arc::new(DB::open(&opts, path)?);

    // Setup phase (single-threaded, not timed): seed n_rows rows.
    {
        let mut batch = WriteBatch::default();
        for id in 0..n_rows {
            batch.put(id.to_be_bytes(), 0u8.to_be_bytes());
        }
        db.write(batch)?;
    }

    let rows_per_thread = (n_rows / n_threads as u64).max(1);
    let start = Instant::now();
    let handles: Vec<_> = (0..n_threads)
        .map(|t| {
            let db = Arc::clone(&db);
            std::thread::spawn(move || -> anyhow::Result<Vec<Duration>> {
                let mut latencies = Vec::with_capacity(writes_per_thread as usize);
                let base_id = t as u64 * rows_per_thread;
                for i in 0..writes_per_thread as u64 {
                    let id = base_id + (i % rows_per_thread);
                    let write_start = Instant::now();
                    db.put(id.to_be_bytes(), (i as u8).to_be_bytes())?;
                    latencies.push(write_start.elapsed());
                }
                Ok(latencies)
            })
        })
        .collect();

    let mut all_latencies = Vec::new();
    for h in handles {
        all_latencies.extend(h.join().expect("writer thread panicked")?);
    }
    let wall_time = start.elapsed();

    Ok(summarize(
        n_threads,
        writes_per_thread,
        wall_time,
        all_latencies,
        0,
    ))
}

/// Same workload shape as `run_concurrent_writers_rocksdb`, against SQLite/WAL. Each thread opens
/// its own connection to the same file — the realistic multi-writer topology, not a shared
/// connection behind a mutex.
#[cfg(feature = "sqlite")]
pub fn run_concurrent_writers_sqlite(
    path: &Path,
    n_rows: u64,
    n_threads: u32,
    writes_per_thread: u32,
) -> anyhow::Result<ConcurrentBenchResult> {
    // Setup phase (single-threaded, not timed).
    {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch("CREATE TABLE IF NOT EXISTS rows (id INTEGER PRIMARY KEY, v INTEGER)")?;
        let tx = conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare("INSERT INTO rows (id, v) VALUES (?1, 0)")?;
            for id in 0..n_rows {
                stmt.execute([id as i64])?;
            }
        }
        tx.commit()?;
    }

    let rows_per_thread = (n_rows / n_threads as u64).max(1);
    let busy_retries = Arc::new(AtomicU32::new(0));
    let start = Instant::now();
    let handles: Vec<_> = (0..n_threads)
        .map(|t| {
            let path = path.to_path_buf();
            let busy_retries = Arc::clone(&busy_retries);
            std::thread::spawn(move || -> anyhow::Result<Vec<Duration>> {
                let conn = Connection::open(&path)?;
                conn.pragma_update(None, "journal_mode", "WAL")?;
                // 5s busy timeout: a writer blocked behind WAL mode's single-writer rule waits and
                // retries via SQLite's own busy-handler rather than erroring immediately — the
                // wait is what shows up as elevated latency below, which is the point.
                conn.busy_timeout(Duration::from_secs(5))?;
                let mut stmt = conn.prepare("UPDATE rows SET v = ?1 WHERE id = ?2")?;
                let mut latencies = Vec::with_capacity(writes_per_thread as usize);
                let base_id = t as u64 * rows_per_thread;
                for i in 0..writes_per_thread as u64 {
                    let id = base_id + (i % rows_per_thread);
                    let write_start = Instant::now();
                    loop {
                        match stmt.execute(rusqlite::params![i as i64, id as i64]) {
                            Ok(_) => break,
                            Err(rusqlite::Error::SqliteFailure(e, _))
                                if e.code == rusqlite::ErrorCode::DatabaseBusy =>
                            {
                                busy_retries.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }
                            Err(e) => return Err(e.into()),
                        }
                    }
                    latencies.push(write_start.elapsed());
                }
                Ok(latencies)
            })
        })
        .collect();

    let mut all_latencies = Vec::new();
    for h in handles {
        all_latencies.extend(h.join().expect("writer thread panicked")?);
    }
    let wall_time = start.elapsed();

    Ok(summarize(
        n_threads,
        writes_per_thread,
        wall_time,
        all_latencies,
        busy_retries.load(Ordering::Relaxed),
    ))
}
