//! #103 candidate 2: a DuckDB-backed read-side facet-count cache. SQLite (`sqlite::SqliteEngine`,
//! unmodified schema) stays the single source of truth for every write and every other query;
//! this module owns a *second*, separate DuckDB file holding only one materialized table —
//! `facet_counts(model, rating, keyword, cnt)` — that `faceted_filter` reads instead of scanning
//! `assets` from scratch. This is deliberately narrow, per #103's own scope: not "replace SQLite,"
//! not "DuckDB as a general OLAP sidecar" (ADR-0008 already declined that when nothing measured
//! needed it) — a targeted materialized view for exactly one query shape.
//!
//! **Refresh strategy: full rebuild, not incremental — stated honestly, not glossed over.**
//! Incremental refresh (only re-aggregating rows changed since the last refresh) needs some way
//! to identify "changed since last time" in the source of truth: a change-log table, a
//! last-modified timestamp column, or equivalent. SQLite's schema here has none of those today,
//! and adding one would mean giving `assets`/`asset_keywords` the same per-write bookkeeping this
//! candidate is trying to avoid by *not* using triggers — at which point the trigger-maintained
//! candidate (`facet_cache_trigger.rs`) is strictly simpler for the same cost. So `refresh()` here
//! always does a full `GROUP BY` rebuild, executed *inside* SQLite (so only the small aggregated
//! result — bounded by distinct (model, rating, keyword) combinations, not by row count — crosses
//! the process boundary into DuckDB) rather than streaming every raw asset/keyword row across.
//! This is the real, if currently non-gating, cost this module measures explicitly: refresh time
//! as a function of catalog size, both right after a bulk ingest and after a write-rate burst.
//!
//! **Staleness is a real, named risk of this design, not just discussed in the abstract:**
//! `faceted_filter` before the first `refresh()` call returns an error rather than silently
//! reading an empty/stale cache — see `refreshed`'s doc comment. After a write burst and before
//! the next `refresh()`, the cache's answer is stale by construction; `verify_against_naive` (see
//! below) is how this module tests that the *refreshed* answer is correct, not that the cache is
//! always live.
//!
//! **Known, real scope limitation, shared with the trigger candidate (see
//! `facet_cache_trigger.rs`'s doc comment for the full account):** `facet_counts`'s grain is
//! `(model, rating, keyword)`, so `SUM(cnt)` with `keyword_prefix: None` overcounts multi-keyword
//! assets and misses zero-keyword ones — it only matches `sqlite.rs`'s per-asset semantics when
//! `keyword_prefix` narrows to a specific value, exactly the shape #103 scopes this cache to.
//! `tests/facet_cache.rs` asserts this mismatch directly rather than leaving it implicit.

use crate::sqlite::SqliteEngine;
use crate::workload::{FacetCounts, RangeQuery, Workload};
use duckdb::{params, Connection as DuckConn};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub struct DuckFacetCacheEngine {
    sqlite: SqliteEngine,
    duck: DuckConn,
    duck_path: PathBuf,
    /// Set the first time `refresh()` succeeds. `faceted_filter` refuses to read from an
    /// never-refreshed cache rather than silently returning an empty/zero result — a fast, wrong
    /// answer is worse than a clear error (see the mistakes-already-made list in #103's own task
    /// description: a fast-but-wrong cache is worse than the problem it's solving).
    refreshed: bool,
}

const DUCK_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS facet_counts (
    model VARCHAR NOT NULL,
    rating UTINYINT NOT NULL,
    keyword VARCHAR NOT NULL,
    cnt BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_facet_model_keyword ON facet_counts(model, keyword);
"#;

impl Workload for DuckFacetCacheEngine {
    fn open(path: &Path) -> anyhow::Result<Self> {
        let sqlite = SqliteEngine::open(path)?;
        let duck_path = path.with_extension("facet-cache.duckdb");
        // A fresh DuckDB file every open, matching every other engine's `open()` semantics in
        // this spike (a stale leftover cache file from a previous run must never be mistaken for
        // a freshly-refreshed one).
        if duck_path.exists() {
            std::fs::remove_file(&duck_path)?;
        }
        let duck = DuckConn::open(&duck_path)?;
        duck.execute_batch(DUCK_SCHEMA)?;
        Ok(Self {
            sqlite,
            duck,
            duck_path,
            refreshed: false,
        })
    }

    fn bulk_ingest(&mut self, assets: &[crate::gen::Asset]) -> anyhow::Result<()> {
        self.sqlite.bulk_ingest(assets)
    }

    fn crash_mid_ingest(&mut self, assets: &[crate::gen::Asset]) -> anyhow::Result<()> {
        self.sqlite.crash_mid_ingest(assets)
    }

    fn write_rating(&mut self, asset_id: u64, rating: u8) -> anyhow::Result<()> {
        // Writes go straight to SQLite only — the cache is untouched until the next explicit
        // `refresh()`. This is the whole point of the design (writes stay as fast as plain
        // SQLite) and the whole risk (the cache goes stale the instant this returns).
        self.sqlite.write_rating(asset_id, rating)
    }

    fn rate_burst(&mut self, updates: &[(u64, u8)]) -> anyhow::Result<()> {
        self.sqlite.rate_burst(updates)
    }

    fn tag_keyword(&mut self, asset_ids: &[u64], keyword: &str) -> anyhow::Result<()> {
        self.sqlite.tag_keyword(asset_ids, keyword)
    }

    fn faceted_filter(
        &self,
        model: Option<&str>,
        min_rating: Option<u8>,
        keyword_prefix: Option<&str>,
    ) -> anyhow::Result<FacetCounts> {
        if !self.refreshed {
            anyhow::bail!(
                "facet cache has never been refreshed — call refresh() before faceted_filter(); \
                 returning an empty/stale answer silently would be exactly the fast-but-wrong \
                 failure mode this module is required to avoid"
            );
        }
        // DuckDB's binder needs the bound-value count to match the placeholders textually present
        // (see duckdb_engine.rs's own note on this) — numbered sequentially per clause actually
        // appended, same pattern as duckdb_engine.rs's faceted_filter.
        let mut sql = String::from("SELECT model, rating, SUM(cnt) FROM facet_counts WHERE 1=1");
        let mut bound: Vec<Box<dyn duckdb::ToSql>> = Vec::new();
        if let Some(m) = model {
            bound.push(Box::new(m.to_string()));
            sql.push_str(&format!(" AND model = ?{}", bound.len()));
        }
        if let Some(r) = min_rating {
            bound.push(Box::new(r));
            sql.push_str(&format!(" AND rating >= ?{}", bound.len()));
        }
        if let Some(kw) = keyword_prefix {
            bound.push(Box::new(format!("{kw}%")));
            sql.push_str(&format!(" AND keyword LIKE ?{}", bound.len()));
        }
        sql.push_str(" GROUP BY model, rating");

        let mut stmt = self.duck.prepare(&sql)?;
        let param_refs: Vec<&dyn duckdb::ToSql> = bound.iter().map(|p| p.as_ref()).collect();
        let rows = stmt.query_map(param_refs.as_slice(), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u8>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;

        let mut counts = FacetCounts::default();
        let mut by_model = std::collections::HashMap::new();
        let mut by_rating = std::collections::HashMap::new();
        for r in rows {
            let (m, rating, cnt) = r?;
            let cnt = cnt as u64;
            *by_model.entry(m).or_insert(0u64) += cnt;
            *by_rating.entry(rating).or_insert(0u64) += cnt;
            counts.total += cnt;
        }
        counts.by_model = by_model.into_iter().collect();
        counts.by_rating = by_rating.into_iter().collect();
        Ok(counts)
    }

    fn sort_by_date_page(&self, offset: u64, limit: u64) -> anyhow::Result<Vec<u64>> {
        self.sqlite.sort_by_date_page(offset, limit)
    }

    fn folder_subtree_count(&self, folder_prefix: &str) -> anyhow::Result<u64> {
        self.sqlite.folder_subtree_count(folder_prefix)
    }

    fn keyword_subtree_query(&self, keyword_prefix: &str) -> anyhow::Result<Vec<u64>> {
        self.sqlite.keyword_subtree_query(keyword_prefix)
    }

    fn range_query(&self, q: &RangeQuery) -> anyhow::Result<Vec<u64>> {
        self.sqlite.range_query(q)
    }

    fn filename_search(&self, substr: &str) -> anyhow::Result<Vec<u64>> {
        self.sqlite.filename_search(substr)
    }

    fn backup(&self, dest: &Path) -> anyhow::Result<()> {
        // Only SQLite (the source of truth) is backed up — the DuckDB cache is disposable,
        // rebuildable from SQLite alone by construction, so it is deliberately not part of the
        // backup surface (a second thing to back up and restore in lockstep would itself be a
        // consistency surface, exactly what #103 asks this candidate's cost to be measured
        // against).
        self.sqlite.backup(dest)
    }

    fn integrity_check(&self) -> anyhow::Result<bool> {
        self.sqlite.integrity_check()
    }
}

impl DuckFacetCacheEngine {
    /// Full-rebuild refresh: aggregates `(model, rating, keyword) -> COUNT(*)` inside SQLite
    /// (indexed join + group-by, so only the aggregated result crosses into DuckDB, not every raw
    /// row) and reloads DuckDB's `facet_counts` table from that result. Returns how long the
    /// refresh itself took, so the benchmark can report it as an explicit, separate cost rather
    /// than folding it invisibly into whatever op happened to trigger it.
    pub fn refresh(&mut self) -> anyhow::Result<Duration> {
        let start = Instant::now();
        let conn = self.sqlite.connection();
        let mut stmt = conn.prepare(
            "SELECT a.model, a.rating, k.keyword, COUNT(*) \
             FROM assets a JOIN asset_keywords k ON k.asset_id = a.id \
             GROUP BY a.model, a.rating, k.keyword",
        )?;
        let rows: Vec<(String, u8, String, i64)> = stmt
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);

        let tx = self.duck.transaction()?;
        tx.execute("DELETE FROM facet_counts", [])?;
        {
            let mut appender = tx.appender("facet_counts")?;
            for (model, rating, keyword, cnt) in &rows {
                appender.append_row(params![model, *rating, keyword, *cnt])?;
            }
        }
        tx.commit()?;
        self.refreshed = true;
        Ok(start.elapsed())
    }

    /// Correctness oracle, mirroring `TriggerFacetEngine::verify_against_naive`: recomputes the
    /// same query from scratch against SQLite (bypassing the DuckDB cache entirely) and compares
    /// against the cache's own (post-refresh) answer.
    pub fn verify_against_naive(
        &self,
        model: Option<&str>,
        min_rating: Option<u8>,
        keyword_prefix: Option<&str>,
    ) -> anyhow::Result<bool> {
        let cached = self.faceted_filter(model, min_rating, keyword_prefix)?;
        let naive = SqliteEngine::naive_faceted_filter(
            self.sqlite.connection(),
            model,
            min_rating,
            keyword_prefix,
        )?;

        let mut cached_by_rating = cached.by_rating.clone();
        let mut naive_by_rating = naive.by_rating.clone();
        cached_by_rating.sort_unstable();
        naive_by_rating.sort_unstable();
        let mut cached_by_model = cached.by_model.clone();
        let mut naive_by_model = naive.by_model.clone();
        cached_by_model.sort_unstable();
        naive_by_model.sort_unstable();

        Ok(cached.total == naive.total
            && cached_by_rating == naive_by_rating
            && cached_by_model == naive_by_model)
    }

    /// Path of the (gitignored, tempdir-scoped in every real caller) DuckDB cache file, exposed
    /// for the benchmark harness to report file size / for tests to inspect directly.
    pub fn duck_cache_path(&self) -> &Path {
        &self.duck_path
    }
}
