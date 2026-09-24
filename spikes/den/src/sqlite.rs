//! SQLite (WAL mode) backend. Keyword hierarchy is a materialized path with a covering index,
//! rather than a closure table — cheaper to maintain on write. Prefix scans (keyword, folder
//! path) use `GLOB 'prefix*'`, not `LIKE 'prefix%'`: measured here, this build's LIKE-to-index
//! transform did not trigger even with `PRAGMA case_sensitive_like` on (confirmed via `EXPLAIN
//! QUERY PLAN` — it stayed a full-table `SCAN` either way), while `GLOB`'s prefix scan is
//! unconditional and always used the index. A broad top-of-hierarchy keyword prefix (matching
//! most of the corpus) is inherently a near-full-scan regardless of engine — see #67's ADR for
//! that finding; the benchmark exercises a realistic single-leaf filter instead. Facet counts are
//! recomputed from the matching set at query time rather than trigger-maintained aggregate
//! tables, since #67's own exit criteria only requires the counts be correct and fast, not
//! incrementally maintained; a trigger-maintained version is a tiebreaker-stage follow-up if this
//! is too slow at production scale.

use crate::gen::{Asset, Flag};
use crate::workload::{FacetCounts, RangeQuery, Workload};
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};

pub struct SqliteEngine {
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
"#;

fn flag_str(f: Flag) -> &'static str {
    match f {
        Flag::None => "none",
        Flag::Pick => "pick",
        Flag::Reject => "reject",
    }
}

impl Workload for SqliteEngine {
    fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn, path: path.to_path_buf() })
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
        // No COMMIT. The transaction stays open; dropped uncommitted when the caller forgets `self`.
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

    fn faceted_filter(
        &self,
        model: Option<&str>,
        min_rating: Option<u8>,
        keyword_prefix: Option<&str>,
    ) -> anyhow::Result<FacetCounts> {
        // EXISTS + GLOB rather than a JOIN + LIKE + DISTINCT: a JOIN on asset_keywords fans out
        // one row per keyword per asset before the DISTINCT can collapse it back down, and (see
        // sqlite.rs's module doc / ADR-0008) SQLite's LIKE-to-index-range-scan transform did not
        // trigger even with `case_sensitive_like` on in this codebase's measurements — GLOB's
        // prefix scan is unconditional, not pragma-dependent, and EXISTS never materializes the
        // fanned-out join.
        let mut sql =
            String::from("SELECT a.id, a.model, a.rating FROM assets a WHERE 1=1");
        if model.is_some() {
            sql.push_str(" AND a.model = ?1");
        }
        if min_rating.is_some() {
            sql.push_str(" AND a.rating >= ?2");
        }
        if keyword_prefix.is_some() {
            sql.push_str(
                " AND EXISTS (SELECT 1 FROM asset_keywords k WHERE k.asset_id = a.id \
                 AND k.keyword GLOB ?3)",
            );
        }
        let mut stmt = self.conn.prepare(&sql)?;
        let kw_glob = keyword_prefix.map(|p| format!("{p}*"));
        let rows = stmt.query_map(
            params![model, min_rating, kw_glob],
            |row| -> rusqlite::Result<(i64, String, u8)> {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            },
        )?;

        let mut counts = FacetCounts::default();
        let mut by_model = std::collections::HashMap::new();
        let mut by_rating = std::collections::HashMap::new();
        for r in rows {
            let (_, m, rating) = r?;
            *by_model.entry(m).or_insert(0u64) += 1;
            *by_rating.entry(rating).or_insert(0u64) += 1;
            counts.total += 1;
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
            .query_map(params![limit as i64, offset as i64], |row| row.get::<_, i64>(0))?
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
            .query_map(params![format!("{keyword_prefix}*")], |row| row.get::<_, i64>(0))?
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
                params![q.min_rating, q.max_rating, q.min_iso, q.max_iso, q.date_from, q.date_to],
                |row| row.get::<_, i64>(0),
            )?
            .map(|r| r.map(|id: i64| id as u64))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    fn filename_search(&self, substr: &str) -> anyhow::Result<Vec<u64>> {
        let mut stmt = self.conn.prepare("SELECT id FROM assets WHERE filename LIKE ?1")?;
        let ids = stmt
            .query_map(params![format!("%{substr}%")], |row| row.get::<_, i64>(0))?
            .map(|r| r.map(|id: i64| id as u64))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    fn backup(&self, dest: &Path) -> anyhow::Result<()> {
        // VACUUM INTO: online, no exclusive lock on the live WAL file, no separate "optimize" step.
        self.conn.execute("VACUUM INTO ?1", params![dest.to_string_lossy()])?;
        Ok(())
    }

    fn integrity_check(&self) -> anyhow::Result<bool> {
        let result: String =
            self.conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        Ok(result == "ok")
    }
}

impl SqliteEngine {
    /// Reopens the database at the same path — used by `den crash` after a `kill -9`.
    pub fn reopen(&self) -> anyhow::Result<Self> {
        Self::open(&self.path)
    }
}
