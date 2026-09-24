//! Turso Database (`tursodatabase/turso`, crate `turso`) backend — #102's follow-up candidate,
//! added after ADR-0008 merged. Pure-Rust, matching ADR-0001's own stated preference (memory
//! safety) unlike SQLite/DuckDB/LMDB, which are all C/C++ cores wrapped by a Rust binding.
//!
//! Two things confirmed during #102's own research, before writing this file, that shape the
//! implementation below:
//! - **No `LIKE`/`GLOB` prefix-scan-to-index-range-scan optimization exists yet** (confirmed via
//!   `tursodatabase/turso#8990`, an open optimizer PR whose own description says this). Unlike
//!   `sqlite.rs`, where switching `LIKE` to `GLOB` fixed a real index-usage bug, there is no
//!   equivalent fix available here — every prefix-scan query (folder, keyword) is expected to be
//!   a full scan regardless of operator. This is measured, not assumed away, in the benchmark.
//! - The crate's own API is `async`, not `rusqlite`-shaped, so every `Workload` method (which is
//!   sync, to keep one shared trait across all four engines) blocks on a per-engine
//!   `tokio::runtime::Runtime` rather than an async trait.

use crate::gen::{Asset, Flag};
use crate::workload::{FacetCounts, RangeQuery, Workload};
use std::path::{Path, PathBuf};
use turso::{Builder, Connection, Database, Value};

pub struct TursoEngine {
    rt: tokio::runtime::Runtime,
    // Keep `Database` alive alongside `Connection` — dropping it would close the file.
    _db: Database,
    conn: Connection,
    path: PathBuf,
}

const SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS assets (
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
    )",
    "CREATE TABLE IF NOT EXISTS asset_keywords (
        asset_id INTEGER NOT NULL,
        keyword TEXT NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS idx_assets_date ON assets(capture_date)",
    "CREATE INDEX IF NOT EXISTS idx_assets_folder ON assets(folder_path)",
    "CREATE INDEX IF NOT EXISTS idx_assets_model_rating ON assets(model, rating)",
    "CREATE INDEX IF NOT EXISTS idx_assets_range ON assets(rating, iso, capture_date)",
    "CREATE INDEX IF NOT EXISTS idx_assets_filename ON assets(filename)",
    "CREATE INDEX IF NOT EXISTS idx_keywords_asset ON asset_keywords(asset_id)",
    "CREATE INDEX IF NOT EXISTS idx_keywords_kw ON asset_keywords(keyword)",
];

fn flag_str(f: Flag) -> &'static str {
    match f {
        Flag::None => "none",
        Flag::Pick => "pick",
        Flag::Reject => "reject",
    }
}

fn row_i64(v: &Value) -> i64 {
    match v {
        Value::Integer(i) => *i,
        other => panic!("expected integer, got {other:?}"),
    }
}

fn row_text(v: &Value) -> String {
    match v {
        Value::Text(s) => s.to_string(),
        other => panic!("expected text, got {other:?}"),
    }
}

impl TursoEngine {
    fn ingest_range(&self, conn: &Connection, assets: &[Asset]) -> anyhow::Result<()> {
        self.rt.block_on(async {
            for a in assets {
                conn.execute(
                    "INSERT INTO assets (id, folder_path, filename, capture_date, model, iso, \
                     compression, width, height, size_bytes, rating, flag) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                    (
                        a.id as i64,
                        a.folder_path.as_str(),
                        a.filename.as_str(),
                        a.capture_date.as_str(),
                        a.model.as_str(),
                        a.iso as i64,
                        a.compression.as_str(),
                        a.width as i64,
                        a.height as i64,
                        a.size_bytes as i64,
                        a.rating as i64,
                        flag_str(a.flag),
                    ),
                )
                .await?;
                for kw in &a.keywords {
                    conn.execute(
                        "INSERT INTO asset_keywords (asset_id, keyword) VALUES (?1, ?2)",
                        (a.id as i64, kw.as_str()),
                    )
                    .await?;
                }
            }
            Ok::<_, anyhow::Error>(())
        })
    }
}

impl Workload for TursoEngine {
    fn open(path: &Path) -> anyhow::Result<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        let (db, conn) = rt.block_on(async {
            let db = Builder::new_local(&path.to_string_lossy()).build().await?;
            let conn = db.connect()?;
            // Match sqlite.rs's pragmas exactly for a fair comparison: journal_mode=wal is
            // already Turso's own default (confirmed via a throwaway probe), but its default
            // synchronous is FULL (2), not sqlite.rs's explicit NORMAL (1) -- set both explicitly
            // so neither engine gets a stricter-by-default durability setting than the other.
            // `execute()` (unlike `query()`) rejects a statement that returns a row, and
            // `PRAGMA journal_mode = ...` always returns the resulting mode as one row (same as
            // plain `PRAGMA journal_mode`) — caught by actually running this, not assumed.
            conn.prepare("PRAGMA journal_mode = WAL")
                .await?
                .query(())
                .await?
                .next()
                .await?;
            conn.execute("PRAGMA synchronous = NORMAL", ()).await?;
            for stmt in SCHEMA {
                conn.execute(stmt, ()).await?;
            }
            Ok::<_, anyhow::Error>((db, conn))
        })?;
        Ok(Self {
            rt,
            _db: db,
            conn,
            path: path.to_path_buf(),
        })
    }

    fn prepare_for_forget(&mut self) {
        // Swap in a throwaway runtime so the *real* one (which actually ran whatever transaction
        // is about to be abandoned) can be taken by value and shut down cleanly, joining every
        // worker thread. This does NOT fully resolve this engine's own crash-safety question --
        // see workload.rs's doc comment on this trait method, and ADR-0009's crash-safety row, for
        // the complete (genuinely inconclusive) story. The throwaway runtime never runs anything,
        // so dropping it normally right after (when `self` itself is forgotten) is harmless.
        let old_rt = std::mem::replace(
            &mut self.rt,
            tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("throwaway runtime"),
        );
        old_rt.shutdown_timeout(std::time::Duration::from_secs(5));
    }

    fn bulk_ingest(&mut self, assets: &[Asset]) -> anyhow::Result<()> {
        let conn = self.conn.clone();
        self.rt
            .block_on(async { conn.execute("BEGIN", ()).await })?;
        // Propagate an ingest failure immediately, before attempting COMMIT — an earlier version
        // of this method captured the result but committed unconditionally, so a real failure
        // partway through ingest surfaced as a confusing, unrelated "cannot commit - no
        // transaction is active" error instead of whatever actually went wrong.
        self.ingest_range(&self.conn.clone(), assets)?;
        let conn = self.conn.clone();
        self.rt
            .block_on(async { conn.execute("COMMIT", ()).await })?;
        Ok(())
    }

    fn crash_mid_ingest(&mut self, assets: &[Asset]) -> anyhow::Result<()> {
        let half = assets.len() / 2;
        let conn = self.conn.clone();
        self.rt
            .block_on(async { conn.execute("BEGIN", ()).await })?;
        self.ingest_range(&self.conn.clone(), &assets[..half])?;
        // No COMMIT — the point of this method, see workload.rs's doc comment.
        Ok(())
    }

    fn write_rating(&mut self, asset_id: u64, rating: u8) -> anyhow::Result<()> {
        let conn = self.conn.clone();
        self.rt.block_on(async {
            conn.execute(
                "UPDATE assets SET rating = ?1 WHERE id = ?2",
                (rating as i64, asset_id as i64),
            )
            .await
        })?;
        Ok(())
    }

    fn rate_burst(&mut self, updates: &[(u64, u8)]) -> anyhow::Result<()> {
        let conn = self.conn.clone();
        self.rt.block_on(async {
            conn.execute("BEGIN", ()).await?;
            for (id, rating) in updates {
                conn.execute(
                    "UPDATE assets SET rating = ?1 WHERE id = ?2",
                    (*rating as i64, *id as i64),
                )
                .await?;
            }
            conn.execute("COMMIT", ()).await?;
            Ok::<_, anyhow::Error>(())
        })
    }

    fn tag_keyword(&mut self, asset_ids: &[u64], keyword: &str) -> anyhow::Result<()> {
        let conn = self.conn.clone();
        let keyword = keyword.to_string();
        self.rt.block_on(async {
            conn.execute("BEGIN", ()).await?;
            for id in asset_ids {
                conn.execute(
                    "INSERT INTO asset_keywords (asset_id, keyword) VALUES (?1, ?2)",
                    (*id as i64, keyword.as_str()),
                )
                .await?;
            }
            conn.execute("COMMIT", ()).await?;
            Ok::<_, anyhow::Error>(())
        })
    }

    fn faceted_filter(
        &self,
        model: Option<&str>,
        min_rating: Option<u8>,
        keyword_prefix: Option<&str>,
    ) -> anyhow::Result<FacetCounts> {
        let conn = self.conn.clone();
        let model = model.map(str::to_string);
        let min_rating = min_rating.map(|r| r as i64);
        let kw_glob = keyword_prefix.map(|p| format!("{p}*"));
        self.rt.block_on(async {
            let mut sql = String::from("SELECT a.id, a.model, a.rating FROM assets a WHERE 1=1");
            let mut params: Vec<Value> = Vec::new();
            if let Some(m) = &model {
                params.push(Value::Text(m.clone()));
                sql.push_str(&format!(" AND a.model = ?{}", params.len()));
            }
            if let Some(r) = min_rating {
                params.push(Value::Integer(r));
                sql.push_str(&format!(" AND a.rating >= ?{}", params.len()));
            }
            if let Some(kw) = &kw_glob {
                params.push(Value::Text(kw.clone()));
                sql.push_str(&format!(
                    " AND EXISTS (SELECT 1 FROM asset_keywords k WHERE k.asset_id = a.id \
                     AND k.keyword GLOB ?{})",
                    params.len()
                ));
            }
            let mut stmt = conn.prepare(&sql).await?;
            let mut rows = stmt.query(params).await?;
            let mut counts = FacetCounts::default();
            let mut by_model = std::collections::HashMap::new();
            let mut by_rating = std::collections::HashMap::new();
            while let Some(row) = rows.next().await? {
                let m = row_text(&row.get_value(1)?);
                let rating = row_i64(&row.get_value(2)?) as u8;
                *by_model.entry(m).or_insert(0u64) += 1;
                *by_rating.entry(rating).or_insert(0u64) += 1;
                counts.total += 1;
            }
            counts.by_model = by_model.into_iter().collect();
            counts.by_rating = by_rating.into_iter().collect();
            Ok(counts)
        })
    }

    fn sort_by_date_page(&self, offset: u64, limit: u64) -> anyhow::Result<Vec<u64>> {
        let conn = self.conn.clone();
        self.rt.block_on(async {
            let mut stmt = conn
                .prepare("SELECT id FROM assets ORDER BY capture_date DESC LIMIT ?1 OFFSET ?2")
                .await?;
            let mut rows = stmt.query((limit as i64, offset as i64)).await?;
            let mut ids = Vec::new();
            while let Some(row) = rows.next().await? {
                ids.push(row_i64(&row.get_value(0)?) as u64);
            }
            Ok(ids)
        })
    }

    fn folder_subtree_count(&self, folder_prefix: &str) -> anyhow::Result<u64> {
        let conn = self.conn.clone();
        let pattern = format!("{folder_prefix}*");
        self.rt.block_on(async {
            let mut stmt = conn
                .prepare("SELECT COUNT(*) FROM assets WHERE folder_path GLOB ?1")
                .await?;
            let mut rows = stmt.query((pattern,)).await?;
            let row = rows
                .next()
                .await?
                .ok_or_else(|| anyhow::anyhow!("no count row"))?;
            Ok(row_i64(&row.get_value(0)?) as u64)
        })
    }

    fn keyword_subtree_query(&self, keyword_prefix: &str) -> anyhow::Result<Vec<u64>> {
        let conn = self.conn.clone();
        let pattern = format!("{keyword_prefix}*");
        self.rt.block_on(async {
            let mut stmt = conn
                .prepare("SELECT DISTINCT asset_id FROM asset_keywords WHERE keyword GLOB ?1")
                .await?;
            let mut rows = stmt.query((pattern,)).await?;
            let mut ids = Vec::new();
            while let Some(row) = rows.next().await? {
                ids.push(row_i64(&row.get_value(0)?) as u64);
            }
            Ok(ids)
        })
    }

    fn range_query(&self, q: &RangeQuery) -> anyhow::Result<Vec<u64>> {
        let conn = self.conn.clone();
        let params = (
            q.min_rating as i64,
            q.max_rating as i64,
            q.min_iso as i64,
            q.max_iso as i64,
            q.date_from.clone(),
            q.date_to.clone(),
        );
        self.rt.block_on(async {
            let mut stmt = conn
                .prepare(
                    "SELECT id FROM assets WHERE rating BETWEEN ?1 AND ?2 AND iso BETWEEN ?3 \
                     AND ?4 AND capture_date BETWEEN ?5 AND ?6",
                )
                .await?;
            let mut rows = stmt.query(params).await?;
            let mut ids = Vec::new();
            while let Some(row) = rows.next().await? {
                ids.push(row_i64(&row.get_value(0)?) as u64);
            }
            Ok(ids)
        })
    }

    fn filename_search(&self, substr: &str) -> anyhow::Result<Vec<u64>> {
        let conn = self.conn.clone();
        let pattern = format!("%{substr}%");
        self.rt.block_on(async {
            let mut stmt = conn
                .prepare("SELECT id FROM assets WHERE filename LIKE ?1")
                .await?;
            let mut rows = stmt.query((pattern,)).await?;
            let mut ids = Vec::new();
            while let Some(row) = rows.next().await? {
                ids.push(row_i64(&row.get_value(0)?) as u64);
            }
            Ok(ids)
        })
    }

    fn backup(&self, dest: &Path) -> anyhow::Result<()> {
        // VACUUM INTO is still experimental upstream (tursodatabase/turso's own 0.6 release notes
        // gate in-place VACUUM behind --experimental-vacuum, with open issues on VACUUM INTO edge
        // cases) — tried first, falling back to a plain file copy (safe here specifically because
        // the benchmark's own backup calls are serial, never run against a concurrent writer; see
        // ADR-0008's own equivalent caveat for the other three engines) if it errors.
        let conn = self.conn.clone();
        let dest_sql = dest.to_string_lossy().replace('\'', "''");
        let vacuum_result = self
            .rt
            .block_on(async { conn.execute(&format!("VACUUM INTO '{dest_sql}'"), ()).await });
        if vacuum_result.is_err() {
            std::fs::copy(&self.path, dest)?;
        }
        Ok(())
    }

    fn integrity_check(&self) -> anyhow::Result<bool> {
        let conn = self.conn.clone();
        self.rt.block_on(async {
            let mut stmt = conn.prepare("PRAGMA integrity_check").await?;
            let mut rows = stmt.query(()).await?;
            let row = rows
                .next()
                .await?
                .ok_or_else(|| anyhow::anyhow!("no integrity row"))?;
            Ok(row_text(&row.get_value(0)?) == "ok")
        })
    }
}
