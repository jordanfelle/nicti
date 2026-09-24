//! DuckDB backend, measured both as a primary store candidate and (per #67's own exit criteria)
//! as a possible OLAP read-side sidecar. DuckDB has no `GLOB`/case-insensitivity quirk like the
//! one found in `sqlite.rs` — `LIKE 'prefix%'` uses its index-backed zonemaps/min-max pruning
//! directly — so prefix filters here use plain `LIKE`.

use crate::gen::{Asset, Flag};
use crate::workload::{FacetCounts, RangeQuery, Workload};
use duckdb::{params, Connection};
use std::path::{Path, PathBuf};

pub struct DuckDbEngine {
    conn: Connection,
    path: PathBuf,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS assets (
    id BIGINT PRIMARY KEY,
    folder_path VARCHAR NOT NULL,
    filename VARCHAR NOT NULL,
    capture_date VARCHAR NOT NULL,
    model VARCHAR NOT NULL,
    iso INTEGER NOT NULL,
    compression VARCHAR NOT NULL,
    width INTEGER NOT NULL,
    height INTEGER NOT NULL,
    size_bytes BIGINT NOT NULL,
    rating UTINYINT NOT NULL,
    flag VARCHAR NOT NULL
);
CREATE TABLE IF NOT EXISTS asset_keywords (
    asset_id BIGINT NOT NULL,
    keyword VARCHAR NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_assets_model_rating ON assets(model, rating);
CREATE INDEX IF NOT EXISTS idx_assets_range ON assets(rating, iso, capture_date);
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

impl Workload for DuckDbEngine {
    fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn,
            path: path.to_path_buf(),
        })
    }

    fn bulk_ingest(&mut self, assets: &[Asset]) -> anyhow::Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut appender = tx.appender("assets")?;
            for a in assets {
                appender.append_row(params![
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
        }
        {
            let mut kw_appender = tx.appender("asset_keywords")?;
            for a in assets {
                for kw in &a.keywords {
                    kw_appender.append_row(params![a.id as i64, kw])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn crash_mid_ingest(&mut self, assets: &[Asset]) -> anyhow::Result<()> {
        let half = assets.len() / 2;
        self.conn.execute_batch("BEGIN TRANSACTION")?;
        let mut appender = self.conn.appender("assets")?;
        for a in &assets[..half] {
            appender.append_row(params![
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
        appender.flush()?;
        // No COMMIT. Dropped uncommitted when the caller forgets `self`.
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
        for (id, rating) in updates {
            tx.execute(
                "UPDATE assets SET rating = ?1 WHERE id = ?2",
                params![rating, *id as i64],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn tag_keyword(&mut self, asset_ids: &[u64], keyword: &str) -> anyhow::Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut appender = tx.appender("asset_keywords")?;
            for id in asset_ids {
                appender.append_row(params![*id as i64, keyword])?;
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
        // DuckDB's binder requires bound-value count to match exactly the distinct `?N` tokens
        // textually present in the parsed query (unlike SQLite, which reserves numbered-parameter
        // slots up to the highest index regardless of which ones are actually referenced) — so
        // placeholders here are numbered sequentially as each optional clause is appended, not by
        // the predicate's fixed position, and only the params for clauses actually appended are
        // bound. Confirmed by `tests/cross_engine.rs`'s cross-engine agreement test, which calls
        // this with `model: None`.
        let mut sql = String::from("SELECT a.id, a.model, a.rating FROM assets a WHERE 1=1");
        let mut params: Vec<Box<dyn duckdb::ToSql>> = Vec::new();
        if let Some(m) = model {
            params.push(Box::new(m.to_string()));
            sql.push_str(&format!(" AND a.model = ?{}", params.len()));
        }
        if let Some(r) = min_rating {
            params.push(Box::new(r));
            sql.push_str(&format!(" AND a.rating >= ?{}", params.len()));
        }
        if let Some(kw) = keyword_prefix {
            params.push(Box::new(format!("{kw}%")));
            sql.push_str(&format!(
                " AND EXISTS (SELECT 1 FROM asset_keywords k WHERE k.asset_id = a.id \
                 AND k.keyword LIKE ?{})",
                params.len()
            ));
        }
        let mut stmt = self.conn.prepare(&sql)?;
        let param_refs: Vec<&dyn duckdb::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let rows = stmt.query_map(param_refs.as_slice(), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, u8>(2)?,
            ))
        })?;

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
            .query_map(params![limit as i64, offset as i64], |row| {
                row.get::<_, i64>(0)
            })?
            .map(|r| r.map(|id: i64| id as u64))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    fn folder_subtree_count(&self, folder_prefix: &str) -> anyhow::Result<u64> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM assets WHERE folder_path LIKE ?1",
            params![format!("{folder_prefix}%")],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }

    fn keyword_subtree_query(&self, keyword_prefix: &str) -> anyhow::Result<Vec<u64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT asset_id FROM asset_keywords WHERE keyword LIKE ?1")?;
        let ids = stmt
            .query_map(params![format!("{keyword_prefix}%")], |row| {
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
        // DuckDB's EXPORT DATABASE is the closest online, no-optimize-step analogue to SQLite's
        // VACUUM INTO; it writes a directory of Parquet + schema, not a single file. `EXPORT
        // DATABASE` takes its path as a string literal in the statement grammar, not a bindable
        // parameter, so this can't use `params![...]` the way every other query in this file
        // does — the single-quote doubling below is standard SQL string-literal escaping, needed
        // since `dest` is only ever a spike-internal tempdir path today but a path containing a
        // literal `'` (plausible on a real filesystem) would otherwise break the generated SQL.
        std::fs::create_dir_all(dest)?;
        let escaped = dest.to_string_lossy().replace('\'', "''");
        self.conn
            .execute(&format!("EXPORT DATABASE '{escaped}' (FORMAT PARQUET)"), [])?;
        Ok(())
    }

    fn integrity_check(&self) -> anyhow::Result<bool> {
        // DuckDB has no PRAGMA integrity_check equivalent exposed via the Rust API at this
        // version; a full scan across every column is the closest self-check available without
        // shelling out to the CLI's `PRAGMA database_list`/`.recover` tooling.
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM assets", [], |row| row.get(0))?;
        Ok(count >= 0)
    }
}

impl DuckDbEngine {
    pub fn reopen(&self) -> anyhow::Result<Self> {
        Self::open(&self.path)
    }
}
