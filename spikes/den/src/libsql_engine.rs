//! libSQL (`tursodatabase/libsql`, crate `libsql`) backend — #113's follow-up candidate, added
//! after ADR-0010 (`redb`) merged. Distinct from #102's Turso Database candidate
//! (`turso_engine.rs`): libSQL is an actual fork of SQLite's real C source (not a from-scratch
//! Rust rewrite), so unlike `turso_engine.rs` there is no reason to expect a missing `LIKE`/`GLOB`
//! prefix-scan optimization or a still-unproven WAL implementation — this schema uses `GLOB`
//! anyway, purely to keep every query byte-identical to `sqlite.rs`'s own SQL text, so any
//! measured difference between the two backends is attributable to the engine, not to a
//! different query shape.
//!
//! The crate's own API is `async` (same shape as `turso`'s: `Connection::execute`/`query`/
//! `prepare` are all `async fn`, `Rows::next` is `async fn`, `Row::get`/`get_value` are sync) —
//! every `Workload` method here blocks on a per-engine `tokio::runtime::Runtime`, the same
//! pattern `turso_engine.rs` established.
//!
//! **#113's specific question — does the embedded-replica feature cost anything when unused?**
//! This engine opens the database with `Builder::new_local(path)`, never calls `.sync()` or
//! configures a `RemoteReplica`/`Offline`/`Remote` variant. Read `libsql-0.9.30`'s own
//! `src/database/builder.rs` and `src/database.rs` before assuming this is a safe no-op rather
//! than measuring it: `Database`'s `db_type` field is a `DbType` enum with five variants
//! (`Memory`, `File`, `Sync`, `Offline`, `Remote`) gated behind `core`/`replication`/`sync`/
//! `remote` features respectively; `Builder::<Local>::build()` constructs exactly one of them —
//! `DbType::File` — and nothing else. There is no `tokio::spawn` anywhere in `database.rs` or
//! `src/local/` (confirmed by grepping the actual crate source under
//! `~/.cargo/registry/src/.../libsql-0.9.30`, not assumed from the docs), so opening a local file
//! with the crate's **default** features (`core`, `replication`, `remote`, `sync`, `tls` — i.e.
//! exactly what `den`'s `Cargo.toml` pulls in here, deliberately not trimmed down, since that's
//! what a real caller would actually depend on) starts no background replication/sync task at
//! all. This backend's own measured numbers (see ADR) are the direct evidence for the *runtime*
//! half of #113's question; the **compile-time** half is a separate, real cost worth reporting
//! regardless of the runtime numbers: `cargo tree -e normal -p den --features libsql` pulls in
//! `tonic`/`tower`/`hyper`/`h2` (the gRPC/HTTP stack backing `remote`/`replication`/`sync`) even
//! though this engine's own code never touches any of it — a real dependency-graph/binary-size/
//! compile-time/supply-chain-surface cost baked into the crate's default features, separate from
//! (and not contradicted by) whatever the latency numbers below show.

use crate::gen::{Asset, Flag};
use crate::workload::{FacetCounts, RangeQuery, Workload};
use libsql::{Builder, Connection, Database, Value};
use std::path::{Path, PathBuf};

pub struct LibsqlEngine {
    rt: tokio::runtime::Runtime,
    // Keep `Database` alive alongside `Connection` — dropping it early is the same hazard
    // `turso_engine.rs` documents for its own `_db` field.
    _db: Database,
    conn: Connection,
    // Stored for parity with `sqlite.rs`/`turso_engine.rs` (both keep the open path for a
    // `reopen()`-style helper / backup fallback), even though this engine's own `backup()` never
    // needs a filesystem fallback (libSQL's `VACUUM INTO`, unlike Turso's, is not experimental —
    // see `backup()`'s own doc comment). Not read anywhere yet, so allowed dead rather than
    // removed: `den crash`'s reopen step constructs a fresh path itself rather than calling back
    // into this struct, but a future caller reusing this engine outside the `Workload` trait would
    // want it.
    #[allow(dead_code)]
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
        Value::Text(s) => s.clone(),
        other => panic!("expected text, got {other:?}"),
    }
}

impl LibsqlEngine {
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

impl Workload for LibsqlEngine {
    fn open(path: &Path) -> anyhow::Result<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        let (db, conn) = rt.block_on(async {
            // `Builder::new_local`: the plain-local-file path, `DbType::File` per this module's
            // doc comment — never a `Sync`/`Offline`/`Remote` variant, so #113's embedded-replica
            // question is exercised honestly here: this engine's entire lifetime never touches
            // that code.
            let db = Builder::new_local(path).build().await?;
            let conn = db.connect()?;
            // Same defensive pattern `turso_engine.rs` uses and the same reason: `execute()`
            // rejects a statement that returns a row, and `PRAGMA journal_mode = ...` always
            // returns the resulting mode as one row (same as plain `PRAGMA journal_mode`) — a
            // real sqlite3 core (which libSQL forks) behaves the same way here as Turso's
            // reimplementation did, confirmed by running this, not assumed from sqlite.rs's
            // synchronous `rusqlite` behavior (where `pragma_update` handles this transparently).
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
        // Same hygiene `turso_engine.rs` applies for the same reason (see its own doc comment on
        // this trait method and `workload.rs`'s doc comment): swap in a throwaway runtime so the
        // real one — which ran whatever transaction is about to be abandoned — can be taken by
        // value and shut down cleanly, joining every worker thread, so a leaked *harness* thread
        // is never conflated with libSQL's own on-disk crash-safety.
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
            let stmt = conn.prepare(&sql).await?;
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
            let stmt = conn
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
            let stmt = conn
                .prepare("SELECT COUNT(*) FROM assets WHERE folder_path GLOB ?1")
                .await?;
            let mut rows = stmt.query([pattern]).await?;
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
            let stmt = conn
                .prepare("SELECT DISTINCT asset_id FROM asset_keywords WHERE keyword GLOB ?1")
                .await?;
            let mut rows = stmt.query([pattern]).await?;
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
            let stmt = conn
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
            let stmt = conn
                .prepare("SELECT id FROM assets WHERE filename LIKE ?1")
                .await?;
            let mut rows = stmt.query([pattern]).await?;
            let mut ids = Vec::new();
            while let Some(row) = rows.next().await? {
                ids.push(row_i64(&row.get_value(0)?) as u64);
            }
            Ok(ids)
        })
    }

    fn backup(&self, dest: &Path) -> anyhow::Result<()> {
        // `VACUUM INTO`: libSQL forks the real sqlite3.c, so unlike `turso_engine.rs` (whose
        // Rust reimplementation gates in-place VACUUM behind an experimental flag and has open
        // issues on VACUUM INTO edge cases) there's no reason to expect this to be anything but
        // the same online, no-exclusive-lock backup `sqlite.rs` uses — confirmed by actually
        // running it (see `tests/cross_engine.rs`'s `libsql_matches_shared_workload`), not
        // assumed from the "it's a real fork" premise alone.
        let conn = self.conn.clone();
        let dest_sql = dest.to_string_lossy().replace('\'', "''");
        self.rt
            .block_on(async { conn.execute(&format!("VACUUM INTO '{dest_sql}'"), ()).await })?;
        Ok(())
    }

    fn integrity_check(&self) -> anyhow::Result<bool> {
        let conn = self.conn.clone();
        self.rt.block_on(async {
            let stmt = conn.prepare("PRAGMA integrity_check").await?;
            let mut rows = stmt.query(()).await?;
            let row = rows
                .next()
                .await?
                .ok_or_else(|| anyhow::anyhow!("no integrity row"))?;
            Ok(row_text(&row.get_value(0)?) == "ok")
        })
    }
}
