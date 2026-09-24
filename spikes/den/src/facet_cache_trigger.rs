//! #103 candidate 1: a SQLite-native, trigger-maintained facet-count table. No new dependency, no
//! second store — SQLite triggers on `assets`/`asset_keywords` keep a denormalized
//! `facet_counts(model, rating, keyword) -> cnt` table in sync as rows are inserted, rated, or
//! (for completeness/hygiene, even though this benchmark never calls it) keyword-deleted.
//! `faceted_filter` then reads that small aggregate table instead of scanning the matching set of
//! `assets` rows from scratch — see `sqlite.rs`'s own module doc, which names this exact
//! trigger-maintained approach as the "tiebreaker-stage follow-up" ADR-0008 left unattempted.
//!
//! Schema is otherwise identical to `sqlite.rs` (same tables/indexes/pragmas) so this is an
//! apples-to-apples comparison, not a differently-tuned SQLite. Every op other than
//! `faceted_filter` behaves exactly like `sqlite.rs`'s implementation (it *is* the same SQL),
//! except that `assets`/`asset_keywords` writes now also fire the maintenance triggers below.
//!
//! **Correctness, precisely:** `TriggerFacetEngine::verify_against_naive` recomputes facet counts
//! from scratch (via `sqlite::SqliteEngine::naive_faceted_filter`, the same query `sqlite.rs`
//! itself uses) and compares against the trigger-maintained table's answer for the same query.
//! This is checked in `tests/facet_cache.rs`, not just asserted in this module's own doc comment.
//!
//! **Known, real scope limitation — found by that same test suite, not just theorized:** because
//! `facet_counts`'s grain is `(model, rating, keyword)`, `SUM(cnt)` with `keyword_prefix: None`
//! does **not** equal the true count of distinct matching assets — an asset with 2 keywords is
//! counted twice, an asset with 0 keywords isn't counted at all. This only matches `sqlite.rs`'s
//! own (per-asset) semantics when `keyword_prefix` narrows to a *specific* value, which is exactly
//! the query shape #103 scopes this cache to ("serves *only* faceted-filter-with-counts", the
//! keyword-narrowed benchmark case) and the only shape this candidate is claimed to support. A
//! `keyword_prefix: None` call is intentionally left uncorrected rather than silently patched
//! around — see `tests/facet_cache.rs`'s dedicated test asserting this mismatch, so it stays a
//! documented, verified gap instead of a latent surprise for whoever wires this into #22.

use crate::gen::{Asset, Flag};
use crate::sqlite::SqliteEngine;
use crate::workload::{FacetCounts, RangeQuery, Workload};
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};

pub struct TriggerFacetEngine {
    conn: Connection,
    path: PathBuf,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS assets (
    id INTEGER PRIMARY KEY,
    folder_path TEXT NOT NULL,
    filename TEXT NOT NULL,
    capture_date TEXT NOT NULL,
    model TEXT NOT NULL,
    iso INTEGER NOT NULL,
    compression TEXT NOT NULL,
    width INTEGER NOT NULL,
    height INTEGER NOT NULL,
    size_bytes INTEGER NOT NULL,
    rating INTEGER NOT NULL,
    flag TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS asset_keywords (
    asset_id INTEGER NOT NULL,
    keyword TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_assets_date ON assets(capture_date);
CREATE INDEX IF NOT EXISTS idx_assets_folder ON assets(folder_path);
CREATE INDEX IF NOT EXISTS idx_assets_model ON assets(model);
CREATE INDEX IF NOT EXISTS idx_assets_rating ON assets(rating);
CREATE INDEX IF NOT EXISTS idx_assets_iso ON assets(iso);
CREATE INDEX IF NOT EXISTS idx_assets_range ON assets(rating, iso, capture_date);
CREATE INDEX IF NOT EXISTS idx_assets_model_rating ON assets(model, rating);
CREATE INDEX IF NOT EXISTS idx_assets_filename ON assets(filename);
CREATE INDEX IF NOT EXISTS idx_keywords_asset ON asset_keywords(asset_id);
CREATE INDEX IF NOT EXISTS idx_keywords_kw ON asset_keywords(keyword);

-- The trigger-maintained facet-count aggregate. Grain is (model, rating, keyword): one row per
-- distinct combination that actually occurs, not one row per asset. A faceted_filter narrowed to
-- one specific keyword leaf (the realistic case this benchmark measures, see gen.rs's
-- BENCH_LEAF_KEYWORD doc) only ever touches the handful of rows for that one keyword value across
-- ratings/models, regardless of how many assets carry it — that's the whole speed win.
CREATE TABLE IF NOT EXISTS facet_counts (
    model TEXT NOT NULL,
    rating INTEGER NOT NULL,
    keyword TEXT NOT NULL,
    cnt INTEGER NOT NULL,
    PRIMARY KEY (model, rating, keyword)
);
CREATE INDEX IF NOT EXISTS idx_facet_counts_model_keyword ON facet_counts(model, keyword);

-- Maintenance trigger 1: a new asset-keyword row. Fires on both `bulk_ingest` (asset row is
-- always inserted before its keyword rows in the same transaction, so the join sees the correct
-- model/rating already) and `tag_keyword`.
CREATE TRIGGER IF NOT EXISTS trg_facet_kw_insert AFTER INSERT ON asset_keywords
BEGIN
    INSERT INTO facet_counts (model, rating, keyword, cnt)
    SELECT a.model, a.rating, NEW.keyword, 1
    FROM assets a WHERE a.id = NEW.asset_id
    ON CONFLICT(model, rating, keyword) DO UPDATE SET cnt = cnt + 1;
END;

-- Maintenance trigger 2: a keyword removed from an asset. Not exercised by this benchmark (no op
-- deletes a keyword) but included for correctness completeness — an incomplete trigger set would
-- be a correctness bug waiting to happen the first time a real delete path is added.
CREATE TRIGGER IF NOT EXISTS trg_facet_kw_delete AFTER DELETE ON asset_keywords
BEGIN
    UPDATE facet_counts SET cnt = cnt - 1
    WHERE model = (SELECT model FROM assets WHERE id = OLD.asset_id)
      AND rating = (SELECT rating FROM assets WHERE id = OLD.asset_id)
      AND keyword = OLD.keyword;
    DELETE FROM facet_counts
    WHERE cnt <= 0
      AND model = (SELECT model FROM assets WHERE id = OLD.asset_id)
      AND rating = (SELECT rating FROM assets WHERE id = OLD.asset_id)
      AND keyword = OLD.keyword;
END;

-- Maintenance trigger 3: a rating change (write_rating, rate_burst). Moves every keyword this
-- asset already has from its old (model, old_rating) bucket to the new one. Bounded by the
-- asset's own keyword count (0-4 in this generator), not by catalog size — this is the per-write
-- cost that must clear the <=5ms/<=16ms point-update gates from ADR-0008.
CREATE TRIGGER IF NOT EXISTS trg_facet_rating_update AFTER UPDATE OF rating ON assets
WHEN OLD.rating IS NOT NEW.rating
BEGIN
    UPDATE facet_counts SET cnt = cnt - 1
    WHERE model = OLD.model AND rating = OLD.rating
      AND keyword IN (SELECT keyword FROM asset_keywords WHERE asset_id = NEW.id);
    DELETE FROM facet_counts
    WHERE cnt <= 0 AND model = OLD.model AND rating = OLD.rating
      AND keyword IN (SELECT keyword FROM asset_keywords WHERE asset_id = NEW.id);

    INSERT INTO facet_counts (model, rating, keyword, cnt)
    SELECT NEW.model, NEW.rating, keyword, 1
    FROM asset_keywords WHERE asset_id = NEW.id
    ON CONFLICT(model, rating, keyword) DO UPDATE SET cnt = cnt + 1;
END;
"#;

fn flag_str(f: Flag) -> &'static str {
    match f {
        Flag::None => "none",
        Flag::Pick => "pick",
        Flag::Reject => "reject",
    }
}

impl Workload for TriggerFacetEngine {
    fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn,
            path: path.to_path_buf(),
        })
    }

    fn bulk_ingest(&mut self, assets: &[Asset]) -> anyhow::Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO assets (id, folder_path, filename, capture_date, model, iso, \
                 compression, width, height, size_bytes, rating, flag) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            )?;
            let mut kw_stmt =
                tx.prepare("INSERT INTO asset_keywords (asset_id, keyword) VALUES (?1, ?2)")?;
            for a in assets {
                stmt.execute(params![
                    a.id as i64,
                    a.folder_path,
                    a.filename,
                    a.capture_date,
                    a.model,
                    a.iso,
                    a.compression,
                    a.width,
                    a.height,
                    a.size_bytes as i64,
                    a.rating,
                    flag_str(a.flag),
                ])?;
                for kw in &a.keywords {
                    kw_stmt.execute(params![a.id as i64, kw])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn crash_mid_ingest(&mut self, assets: &[Asset]) -> anyhow::Result<()> {
        let half = assets.len() / 2;
        self.conn.execute_batch("BEGIN")?;
        let mut stmt = self.conn.prepare(
            "INSERT INTO assets (id, folder_path, filename, capture_date, model, iso, \
             compression, width, height, size_bytes, rating, flag) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
        )?;
        for a in &assets[..half] {
            stmt.execute(params![
                a.id as i64,
                a.folder_path,
                a.filename,
                a.capture_date,
                a.model,
                a.iso,
                a.compression,
                a.width,
                a.height,
                a.size_bytes as i64,
                a.rating,
                flag_str(a.flag),
            ])?;
        }
        Ok(())
    }

    fn write_rating(&mut self, asset_id: u64, rating: u8) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE assets SET rating = ?1 WHERE id = ?2",
            params![rating, asset_id as i64],
        )?;
        Ok(())
    }

    fn rate_burst(&mut self, updates: &[(u64, u8)]) -> anyhow::Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare("UPDATE assets SET rating = ?1 WHERE id = ?2")?;
            for (id, rating) in updates {
                let id = *id as i64;
                stmt.execute(params![rating, id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn tag_keyword(&mut self, asset_ids: &[u64], keyword: &str) -> anyhow::Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt =
                tx.prepare("INSERT INTO asset_keywords (asset_id, keyword) VALUES (?1, ?2)")?;
            for id in asset_ids {
                stmt.execute(params![*id as i64, keyword])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Reads the trigger-maintained `facet_counts` aggregate instead of scanning `assets` — the
    /// entire point of this candidate. Placeholders are built dynamically, one `?N` per clause
    /// actually appended, and only that many values are bound — a fixed `?1`/`?2`/`?3` literal
    /// with a fixed 3-value `params![...]` looked fine until a real caller (this module's own
    /// test suite, checking the *unfiltered* facet count) passed all three filters as `None`: with
    /// zero clauses appended, the SQL has zero declared placeholders, but a fixed 3-value
    /// `params!` literal still supplied three — `rusqlite` rejected that outright ("Wrong number
    /// of parameters passed to query"), rather than silently tolerating it. See
    /// `sqlite.rs::naive_faceted_filter`'s doc comment for the full account of this bug, found and
    /// fixed while writing #103's tests.
    fn faceted_filter(
        &self,
        model: Option<&str>,
        min_rating: Option<u8>,
        keyword_prefix: Option<&str>,
    ) -> anyhow::Result<FacetCounts> {
        let mut sql = String::from("SELECT model, rating, SUM(cnt) FROM facet_counts WHERE 1=1");
        let mut bound: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(m) = model {
            bound.push(Box::new(m.to_string()));
            sql.push_str(&format!(" AND model = ?{}", bound.len()));
        }
        if let Some(r) = min_rating {
            bound.push(Box::new(r));
            sql.push_str(&format!(" AND rating >= ?{}", bound.len()));
        }
        if let Some(kw) = keyword_prefix {
            bound.push(Box::new(format!("{kw}*")));
            sql.push_str(&format!(" AND keyword GLOB ?{}", bound.len()));
        }
        sql.push_str(" GROUP BY model, rating");

        let mut stmt = self.conn.prepare(&sql)?;
        let param_refs: Vec<&dyn rusqlite::ToSql> = bound.iter().map(|p| p.as_ref()).collect();
        let rows = stmt.query_map(
            param_refs.as_slice(),
            |row| -> rusqlite::Result<(String, u8, i64)> {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            },
        )?;

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
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM assets ORDER BY capture_date DESC LIMIT ?1 OFFSET ?2")?;
        let ids = stmt
            .query_map(params![limit as i64, offset as i64], |row| {
                row.get::<_, i64>(0)
            })?
            .map(|r| r.map(|id: i64| id as u64))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    fn folder_subtree_count(&self, folder_prefix: &str) -> anyhow::Result<u64> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM assets WHERE folder_path GLOB ?1",
            params![format!("{folder_prefix}*")],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }

    fn keyword_subtree_query(&self, keyword_prefix: &str) -> anyhow::Result<Vec<u64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT asset_id FROM asset_keywords WHERE keyword GLOB ?1")?;
        let ids = stmt
            .query_map(params![format!("{keyword_prefix}*")], |row| {
                row.get::<_, i64>(0)
            })?
            .map(|r| r.map(|id: i64| id as u64))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    fn range_query(&self, q: &RangeQuery) -> anyhow::Result<Vec<u64>> {
        let mut stmt = self.conn.prepare(
            "SELECT id FROM assets WHERE rating BETWEEN ?1 AND ?2 AND iso BETWEEN ?3 AND ?4 \
             AND capture_date BETWEEN ?5 AND ?6",
        )?;
        let ids = stmt
            .query_map(
                params![
                    q.min_rating,
                    q.max_rating,
                    q.min_iso,
                    q.max_iso,
                    q.date_from,
                    q.date_to
                ],
                |row| row.get::<_, i64>(0),
            )?
            .map(|r| r.map(|id: i64| id as u64))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    fn filename_search(&self, substr: &str) -> anyhow::Result<Vec<u64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM assets WHERE filename LIKE ?1")?;
        let ids = stmt
            .query_map(params![format!("%{substr}%")], |row| row.get::<_, i64>(0))?
            .map(|r| r.map(|id: i64| id as u64))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    fn backup(&self, dest: &Path) -> anyhow::Result<()> {
        self.conn
            .execute("VACUUM INTO ?1", params![dest.to_string_lossy()])?;
        Ok(())
    }

    fn integrity_check(&self) -> anyhow::Result<bool> {
        let result: String = self
            .conn
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        Ok(result == "ok")
    }
}

impl TriggerFacetEngine {
    pub fn reopen(&self) -> anyhow::Result<Self> {
        Self::open(&self.path)
    }

    /// Correctness oracle: recomputes the same query from scratch (bypassing `facet_counts`
    /// entirely) and compares against this engine's own (trigger-fed) answer. Returns `Ok(true)`
    /// only if every `(model, rating)` bucket total and the grand total agree exactly.
    pub fn verify_against_naive(
        &self,
        model: Option<&str>,
        min_rating: Option<u8>,
        keyword_prefix: Option<&str>,
    ) -> anyhow::Result<bool> {
        let fast = self.faceted_filter(model, min_rating, keyword_prefix)?;
        let naive =
            SqliteEngine::naive_faceted_filter(&self.conn, model, min_rating, keyword_prefix)?;

        let mut fast_by_rating = fast.by_rating.clone();
        let mut naive_by_rating = naive.by_rating.clone();
        fast_by_rating.sort_unstable();
        naive_by_rating.sort_unstable();
        let mut fast_by_model = fast.by_model.clone();
        let mut naive_by_model = naive.by_model.clone();
        fast_by_model.sort_unstable();
        naive_by_model.sort_unstable();

        Ok(fast.total == naive.total
            && fast_by_rating == naive_by_rating
            && fast_by_model == naive_by_model)
    }
}
