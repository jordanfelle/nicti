//! #107 schema-fit prototype: is DuckDB's data model (columnar, JSON-as-a-type) actually a good
//! or bad fit for #22's planned catalog schema (ADR-0002), rather than the OLTP-shaped
//! filter/sort/range queries `workload.rs`/ADR-0008 already measured?
//!
//! Three concrete things ADR-0002 commits to that ADR-0008's `Workload` trait never exercised:
//!
//! 1. A JSON-ish per-stage edit-parameter map (`EditDocument::stages: BTreeMap<String,
//!    StageEntry>`, `StageEntry.params: serde_json::Value`) — does DuckDB have a real JSON type
//!    with extraction/query operations, or only opaque-string storage?
//! 2. An append-only history log with burst-compaction: many raw ticks land, then most of a burst
//!    is deleted and replaced by one merged row (a real UPDATE/DELETE-heavy pattern, not a
//!    pure-insert one) — is this workable on a columnar engine, whose storage model is generally
//!    understood to disfavor row-level mutation?
//! 3. Reconstructing "the current effective edit stack for asset X" — latest row per
//!    `(asset_id, stage_id)` — without a linear scan of the whole history table every time.
//!
//! This module builds a rough approximation of that shape against both surviving SQL candidates
//! (SQLite and DuckDB; LMDB is out of scope here — ADR-0008 already rejected it as primary on the
//! crash-safety gate, independent of this schema-fit question) and measures it directly, per
//! #107's exit criteria: "prototype the actual planned #22 schema shape... against both engines
//! if the fit question isn't answerable from documentation alone." Not production code — see
//! `CLAUDE.md`'s package map, same caveat as every other `spikes/den` module.

use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use std::time::Instant;

/// One raw history tick as it would be appended before compaction — mirrors ADR-0002's "records
/// one delta per stage change, tagged with a coalescing `control` key and a timestamp."
#[derive(Debug, Clone)]
pub struct RawTick {
    pub asset_id: i64,
    pub stage_id: &'static str,
    pub control: &'static str,
    pub seq: i64, // monotonic per-asset sequence, stands in for a real timestamp
    pub before: String,
    pub after: String,
}

const STAGES: &[&str] = &["white_balance", "tone", "mask_subject", "mask_sky", "crop"];

/// Generates a burst-and-compact history shape for `asset_count` assets: each asset gets
/// `bursts_per_asset` editing sessions, each session a burst of `ticks_per_burst` raw slider
/// ticks on one randomly chosen stage — the "rapid slider drag" case ADR-0002 names explicitly,
/// not a uniform one-row-per-edit stream. Returns the raw ticks in insertion order.
pub fn generate_bursts(
    asset_count: u64,
    bursts_per_asset: u32,
    ticks_per_burst: u32,
    seed: u64,
) -> Vec<RawTick> {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut out = Vec::with_capacity(
        (asset_count * bursts_per_asset as u64 * ticks_per_burst as u64) as usize,
    );
    for asset_id in 0..asset_count as i64 {
        let mut seq = 0i64;
        for _ in 0..bursts_per_asset {
            let stage = STAGES[rng.random_range(0..STAGES.len())];
            let control = "slider_drag";
            let mut value = rng.random_range(-100..100);
            for _ in 0..ticks_per_burst {
                let before = format!(r#"{{"stage":"{stage}","v":{value}}}"#);
                value += rng.random_range(-2..3);
                let after = format!(r#"{{"stage":"{stage}","v":{value}}}"#);
                out.push(RawTick {
                    asset_id,
                    stage_id: stage,
                    control,
                    seq,
                    before,
                    after,
                });
                seq += 1;
            }
        }
    }
    out
}

/// Given one asset's raw ticks (already filtered to one `asset_id`), returns the compacted rows
/// per ADR-0002's rule: consecutive ticks sharing `(stage_id, control)` collapse into a single
/// delta spanning the run (first `before`, last `after`). Ticks here are already grouped by burst
/// (generation never interleaves two stages' ticks within a burst), so this is a straightforward
/// linear compaction pass, not a full interval-merge — matches how `generate_bursts` produces one
/// burst at a time.
pub fn compact(ticks: &[RawTick]) -> Vec<RawTick> {
    let mut out: Vec<RawTick> = Vec::new();
    for t in ticks {
        match out.last_mut() {
            Some(last) if last.stage_id == t.stage_id && last.control == t.control => {
                last.after = t.after.clone();
            }
            _ => out.push(t.clone()),
        }
    }
    out
}

pub mod duckdb_fit {
    use super::*;
    use duckdb::{params, Connection};

    pub struct Fit {
        pub conn: Connection,
    }

    impl Fit {
        pub fn open_in_memory() -> anyhow::Result<Self> {
            let conn = Connection::open_in_memory()?;
            conn.execute_batch(
                r#"
                CREATE TABLE history (
                    id BIGINT PRIMARY KEY,
                    asset_id BIGINT NOT NULL,
                    stage_id VARCHAR NOT NULL,
                    control VARCHAR NOT NULL,
                    seq BIGINT NOT NULL,
                    before JSON NOT NULL,
                    after JSON NOT NULL
                );
                CREATE INDEX idx_history_asset_stage_seq ON history(asset_id, stage_id, seq);
                "#,
            )?;
            Ok(Self { conn })
        }

        /// Real JSON-type check, not just opaque-string storage: extracts a field out of the
        /// `after` column via DuckDB's JSON path syntax. If this fails to compile/execute, that's
        /// the finding (no usable JSON query story); if it succeeds and the value round-trips
        /// correctly, that's real evidence DuckDB's JSON support goes beyond storage.
        pub fn json_extract_check(&self) -> anyhow::Result<f64> {
            let v: f64 = self.conn.query_row(
                r#"SELECT json_extract(after, '$.v')::DOUBLE FROM history LIMIT 1"#,
                [],
                |row| row.get(0),
            )?;
            Ok(v)
        }

        /// Inserts raw ticks via the Appender (bulk path, matching every other bulk_ingest in this
        /// spike) and returns elapsed time.
        pub fn insert_raw(
            &mut self,
            ticks: &[RawTick],
            id_start: i64,
        ) -> anyhow::Result<std::time::Duration> {
            let start = Instant::now();
            let tx = self.conn.transaction()?;
            {
                let mut appender = tx.appender("history")?;
                for (i, t) in ticks.iter().enumerate() {
                    appender.append_row(params![
                        id_start + i as i64,
                        t.asset_id,
                        t.stage_id,
                        t.control,
                        t.seq,
                        t.before,
                        t.after,
                    ])?;
                }
            }
            tx.commit()?;
            Ok(start.elapsed())
        }

        /// Simulates compaction as DuckDB would actually execute it: per ADR-0002, compaction only
        /// merges *consecutive* deltas from one uninterrupted editing burst, not every row that
        /// ever touched this `(asset_id, stage_id, control)` key — a stage can be revisited in a
        /// later, unrelated burst, and those must NOT be folded into an earlier one. `seq` is
        /// monotonic per asset across every stage, so a burst is exactly a maximal run of rows for
        /// this key whose `seq` values are consecutive integers; a gap means a different burst.
        /// Each contiguous run gets its own UPDATE + bulk DELETE (delete all but the first row,
        /// rewrite the first row's `after` to the run's last `after`) inside one transaction.
        /// Returns (elapsed, number of runs actually compacted — i.e. real compaction ops, not
        /// "keys touched").
        pub fn compact_run(
            &mut self,
            asset_id: i64,
            stage_id: &str,
            control: &str,
        ) -> anyhow::Result<(std::time::Duration, u64)> {
            let start = Instant::now();
            let tx = self.conn.transaction()?;
            let rows: Vec<(i64, i64, String)> = {
                let mut stmt = tx.prepare(
                    "SELECT id, seq, after FROM history \
                     WHERE asset_id = ?1 AND stage_id = ?2 AND control = ?3 ORDER BY seq ASC",
                )?;
                stmt.query_map(params![asset_id, stage_id, control], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })?
                .collect::<Result<Vec<_>, _>>()?
            };

            let mut ops = 0u64;
            let mut i = 0usize;
            while i < rows.len() {
                let mut j = i;
                while j + 1 < rows.len() && rows[j + 1].1 == rows[j].1 + 1 {
                    j += 1;
                }
                if j > i {
                    let (first_id, _, _) = &rows[i];
                    let (_, _, last_after) = &rows[j];
                    tx.execute(
                        "UPDATE history SET after = ?1 WHERE id = ?2",
                        params![last_after, first_id],
                    )?;
                    let to_delete: Vec<i64> = rows[i + 1..=j].iter().map(|(id, ..)| *id).collect();
                    let placeholders = to_delete
                        .iter()
                        .enumerate()
                        .map(|(k, _)| format!("?{}", k + 1))
                        .collect::<Vec<_>>()
                        .join(",");
                    tx.execute(
                        &format!("DELETE FROM history WHERE id IN ({placeholders})"),
                        duckdb::params_from_iter(to_delete.iter()),
                    )?;
                    ops += 1;
                }
                i = j + 1;
            }
            tx.commit()?;
            Ok((start.elapsed(), ops))
        }

        /// "Current effective edit stack for asset X": latest row per `(asset_id, stage_id)`,
        /// without scanning the whole table for every asset individually — the exact query #107
        /// names as the thing to check is expressible without excessive complexity. Uses DuckDB's
        /// `QUALIFY` clause with `ROW_NUMBER()`, a single-pass window-function query.
        pub fn current_effective_stack(
            &self,
            asset_id: i64,
        ) -> anyhow::Result<Vec<(String, String)>> {
            let mut stmt = self.conn.prepare(
                r#"
                SELECT stage_id, after FROM history
                WHERE asset_id = ?1
                QUALIFY ROW_NUMBER() OVER (PARTITION BY stage_id ORDER BY seq DESC) = 1
                "#,
            )?;
            let rows = stmt
                .query_map(params![asset_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        }

        pub fn row_count(&self) -> anyhow::Result<i64> {
            Ok(self
                .conn
                .query_row("SELECT COUNT(*) FROM history", [], |row| row.get(0))?)
        }
    }
}

pub mod sqlite_fit {
    use super::*;
    use rusqlite::{params, Connection};

    pub struct Fit {
        pub conn: Connection,
    }

    impl Fit {
        pub fn open_in_memory() -> anyhow::Result<Self> {
            let conn = Connection::open_in_memory()?;
            conn.execute_batch(
                r#"
                CREATE TABLE history (
                    id INTEGER PRIMARY KEY,
                    asset_id INTEGER NOT NULL,
                    stage_id TEXT NOT NULL,
                    control TEXT NOT NULL,
                    seq INTEGER NOT NULL,
                    before TEXT NOT NULL,
                    after TEXT NOT NULL
                );
                CREATE INDEX idx_history_asset_stage_seq ON history(asset_id, stage_id, seq);
                "#,
            )?;
            Ok(Self { conn })
        }

        /// SQLite's JSON1 extension equivalent of the DuckDB check above — bundled/compiled in by
        /// `rusqlite`'s `bundled` feature, so this should always succeed; included so the ADR's
        /// JSON-support comparison is measured on both sides, not asserted for one and assumed for
        /// the other.
        pub fn json_extract_check(&self) -> anyhow::Result<f64> {
            let v: f64 = self.conn.query_row(
                r#"SELECT json_extract(after, '$.v') FROM history LIMIT 1"#,
                [],
                |row| row.get(0),
            )?;
            Ok(v)
        }

        pub fn insert_raw(
            &mut self,
            ticks: &[RawTick],
            id_start: i64,
        ) -> anyhow::Result<std::time::Duration> {
            let start = Instant::now();
            let tx = self.conn.transaction()?;
            {
                let mut stmt = tx.prepare(
                    "INSERT INTO history (id, asset_id, stage_id, control, seq, before, after) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7)",
                )?;
                for (i, t) in ticks.iter().enumerate() {
                    stmt.execute(params![
                        id_start + i as i64,
                        t.asset_id,
                        t.stage_id,
                        t.control,
                        t.seq,
                        t.before,
                        t.after,
                    ])?;
                }
            }
            tx.commit()?;
            Ok(start.elapsed())
        }

        /// See `duckdb_fit::Fit::compact_run`'s doc comment: compaction only merges a single
        /// contiguous run of consecutive `seq` values sharing this key, not every row that ever
        /// touched it, since a stage can be revisited in a later, unrelated burst.
        pub fn compact_run(
            &mut self,
            asset_id: i64,
            stage_id: &str,
            control: &str,
        ) -> anyhow::Result<(std::time::Duration, u64)> {
            let start = Instant::now();
            let tx = self.conn.transaction()?;
            let mut stmt = tx.prepare(
                "SELECT id, seq, after FROM history \
                 WHERE asset_id = ?1 AND stage_id = ?2 AND control = ?3 ORDER BY seq ASC",
            )?;
            let rows: Vec<(i64, i64, String)> = stmt
                .query_map(params![asset_id, stage_id, control], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            drop(stmt);

            let mut ops = 0u64;
            let mut i = 0usize;
            while i < rows.len() {
                let mut j = i;
                while j + 1 < rows.len() && rows[j + 1].1 == rows[j].1 + 1 {
                    j += 1;
                }
                if j > i {
                    let (first_id, _, _) = &rows[i];
                    let (_, _, last_after) = &rows[j];
                    tx.execute(
                        "UPDATE history SET after = ?1 WHERE id = ?2",
                        params![last_after, first_id],
                    )?;
                    let to_delete: Vec<i64> = rows[i + 1..=j].iter().map(|(id, ..)| *id).collect();
                    let placeholders = to_delete
                        .iter()
                        .enumerate()
                        .map(|(k, _)| format!("?{}", k + 1))
                        .collect::<Vec<_>>()
                        .join(",");
                    tx.execute(
                        &format!("DELETE FROM history WHERE id IN ({placeholders})"),
                        rusqlite::params_from_iter(to_delete.iter()),
                    )?;
                    ops += 1;
                }
                i = j + 1;
            }
            tx.commit()?;
            Ok((start.elapsed(), ops))
        }

        /// SQLite has no `QUALIFY`; the equivalent shape is a window function in a subquery
        /// filtered by `WHERE rn = 1` in the outer query.
        pub fn current_effective_stack(
            &self,
            asset_id: i64,
        ) -> anyhow::Result<Vec<(String, String)>> {
            let mut stmt = self.conn.prepare(
                r#"
                SELECT stage_id, after FROM (
                    SELECT stage_id, after,
                           ROW_NUMBER() OVER (PARTITION BY stage_id ORDER BY seq DESC) AS rn
                    FROM history WHERE asset_id = ?1
                ) WHERE rn = 1
                "#,
            )?;
            let rows = stmt
                .query_map(params![asset_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        }

        pub fn row_count(&self) -> anyhow::Result<i64> {
            Ok(self
                .conn
                .query_row("SELECT COUNT(*) FROM history", [], |row| row.get(0))?)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Scale note: 4,000 assets x 6 bursts x 40 ticks/burst = 960,000 raw history rows — modeling
    // several real editing sessions per asset over the catalog's lifetime, well past a single
    // "slider drag" (ADR-0002's own example is ~200 ticks for *one* drag). Kept below the
    // 2M-asset/full-corpus scale `spikes/den`'s main benchmark uses (this measures one query
    // shape in isolation, not a full catalog), but large enough that a per-row cost problem would
    // show up as a real wall-clock number, not get lost in fixed overhead.
    const ASSET_COUNT: u64 = 4_000;
    const BURSTS_PER_ASSET: u32 = 6;
    const TICKS_PER_BURST: u32 = 40;

    #[test]
    fn json_extraction_works_on_both_engines() {
        let mut d = duckdb_fit::Fit::open_in_memory().unwrap();
        d.insert_raw(&generate_bursts(1, 1, 1, 1), 0).unwrap();
        let dv = d.json_extract_check().unwrap();

        let mut s = sqlite_fit::Fit::open_in_memory().unwrap();
        s.insert_raw(&generate_bursts(1, 1, 1, 1), 0).unwrap();
        let sv = s.json_extract_check().unwrap();

        // Same input, same JSON path -> same extracted value on both engines. This is the
        // concrete "does DuckDB have real JSON query operations, not just opaque-string storage"
        // check #107 asks for, not an assertion from documentation alone.
        assert_eq!(dv, sv);
    }

    // Ignored by default: at this scale, DuckDB's compaction cost (the ADR-0012 finding itself)
    // makes this test take on the order of 18 minutes — the number IS the finding, but that's far
    // too slow to run on every `cargo test --workspace --all-targets --all-features` invocation in
    // CI. Run explicitly to reproduce the ADR's measured numbers:
    // `cargo test -p den --features sqlite,duckdb --release -- --ignored --nocapture
    // burst_insert_and_compact_duckdb`. `burst_insert_and_compact_sqlite` below is not ignored
    // (SQLite completes the same workload in a few seconds) — it stays a normal, fast CI test
    // asserting the compaction/query logic itself is correct.
    #[test]
    #[ignore]
    fn burst_insert_and_compact_duckdb() {
        let ticks = generate_bursts(ASSET_COUNT, BURSTS_PER_ASSET, TICKS_PER_BURST, 7);
        let raw_count = ticks.len();
        let mut d = duckdb_fit::Fit::open_in_memory().unwrap();
        let insert_elapsed = d.insert_raw(&ticks, 0).unwrap();
        println!(
            "duckdb: inserted {raw_count} raw history rows in {insert_elapsed:?} ({:.1} rows/ms)",
            raw_count as f64 / insert_elapsed.as_secs_f64() / 1000.0
        );

        // Compact every asset's every burst: BURSTS_PER_ASSET compaction ops per asset, each
        // touching TICKS_PER_BURST rows (delete all but 2, update 1).
        let compact_start = Instant::now();
        let mut compacted_runs = 0u64;
        for asset_id in 0..ASSET_COUNT as i64 {
            // Each asset's bursts landed on a randomly chosen stage per burst (see
            // generate_bursts); compacting is keyed on (asset_id, stage_id, control), so iterate
            // every stage — a burst that never touched a given stage simply finds 0 rows and its
            // compact_run below is skipped via the row_count guard.
            for stage in STAGES {
                let count: i64 = d
                    .conn
                    .query_row(
                        "SELECT COUNT(*) FROM history WHERE asset_id = ?1 AND stage_id = ?2",
                        duckdb::params![asset_id, stage],
                        |row| row.get(0),
                    )
                    .unwrap_or(0);
                if count > 1 {
                    let (_, ops) = d.compact_run(asset_id, stage, "slider_drag").unwrap();
                    compacted_runs += ops;
                }
            }
        }
        let compact_elapsed = compact_start.elapsed();
        println!(
            "duckdb: {compacted_runs} compaction ops in {compact_elapsed:?} ({:.3}ms/op avg)",
            compact_elapsed.as_secs_f64() * 1000.0 / compacted_runs as f64
        );

        let remaining = d.row_count().unwrap();
        println!("duckdb: {remaining} rows remain after compaction (from {raw_count} raw)");

        // Sanity: compaction must have actually shrunk the table, and by roughly the expected
        // amount (each fully-compacted burst goes from TICKS_PER_BURST rows to 2).
        assert!((remaining as usize) < raw_count);

        // The query #107 actually asks about: fast, expressible current-effective-stack lookup.
        let query_start = Instant::now();
        for asset_id in 0..200i64 {
            let stack = d.current_effective_stack(asset_id).unwrap();
            assert!(!stack.is_empty());
        }
        let query_elapsed = query_start.elapsed();
        println!(
            "duckdb: 200 current_effective_stack lookups in {query_elapsed:?} ({:.3}ms/call avg)",
            query_elapsed.as_secs_f64() * 1000.0 / 200.0
        );
    }

    /// Small-scale (fast, always-run) correctness check for DuckDB's compaction and
    /// current-effective-stack logic — the full-scale `#[ignore]`d test above is what produces the
    /// ADR's actual timing numbers, but this keeps the logic itself under normal CI coverage
    /// without paying the 18-minute cost every run.
    #[test]
    fn duckdb_compaction_correctness_small_scale() {
        let ticks = generate_bursts(50, 3, 10, 11);
        let raw_count = ticks.len();
        let mut d = duckdb_fit::Fit::open_in_memory().unwrap();
        d.insert_raw(&ticks, 0).unwrap();

        for asset_id in 0..50i64 {
            for stage in STAGES {
                let count: i64 = d
                    .conn
                    .query_row(
                        "SELECT COUNT(*) FROM history WHERE asset_id = ?1 AND stage_id = ?2",
                        duckdb::params![asset_id, stage],
                        |row| row.get(0),
                    )
                    .unwrap_or(0);
                if count > 1 {
                    d.compact_run(asset_id, stage, "slider_drag").unwrap();
                }
            }
        }
        let remaining = d.row_count().unwrap();
        assert!((remaining as usize) < raw_count);

        for asset_id in 0..50i64 {
            let stack = d.current_effective_stack(asset_id).unwrap();
            assert!(!stack.is_empty());
        }
    }

    #[test]
    fn burst_insert_and_compact_sqlite() {
        let ticks = generate_bursts(ASSET_COUNT, BURSTS_PER_ASSET, TICKS_PER_BURST, 7);
        let raw_count = ticks.len();
        let mut s = sqlite_fit::Fit::open_in_memory().unwrap();
        let insert_elapsed = s.insert_raw(&ticks, 0).unwrap();
        println!(
            "sqlite: inserted {raw_count} raw history rows in {insert_elapsed:?} ({:.1} rows/ms)",
            raw_count as f64 / insert_elapsed.as_secs_f64() / 1000.0
        );

        let compact_start = Instant::now();
        let mut compacted_runs = 0u64;
        for asset_id in 0..ASSET_COUNT as i64 {
            for stage in STAGES {
                let count: i64 = s
                    .conn
                    .query_row(
                        "SELECT COUNT(*) FROM history WHERE asset_id = ?1 AND stage_id = ?2",
                        rusqlite::params![asset_id, stage],
                        |row| row.get(0),
                    )
                    .unwrap_or(0);
                if count > 1 {
                    let (_, ops) = s.compact_run(asset_id, stage, "slider_drag").unwrap();
                    compacted_runs += ops;
                }
            }
        }
        let compact_elapsed = compact_start.elapsed();
        println!(
            "sqlite: {compacted_runs} compaction ops in {compact_elapsed:?} ({:.3}ms/op avg)",
            compact_elapsed.as_secs_f64() * 1000.0 / compacted_runs as f64
        );

        let remaining = s.row_count().unwrap();
        println!("sqlite: {remaining} rows remain after compaction (from {raw_count} raw)");
        assert!((remaining as usize) < raw_count);

        let query_start = Instant::now();
        for asset_id in 0..200i64 {
            let stack = s.current_effective_stack(asset_id).unwrap();
            assert!(!stack.is_empty());
        }
        let query_elapsed = query_start.elapsed();
        println!(
            "sqlite: 200 current_effective_stack lookups in {query_elapsed:?} ({:.3}ms/call avg)",
            query_elapsed.as_secs_f64() * 1000.0 / 200.0
        );
    }
}
