//! Preview-cache format candidates for #29: SQLite BLOBs in a separate `previews.db`, an
//! append-only pack file + a SQLite offset index, and a plain file-per-preview baseline (the
//! shape LRC's own `Previews.lrdata` uses). All three implement `CacheFormat` so `tier-bench`
//! (`tier_bench.rs`) measures them uniformly: populate throughput, on-disk bytes, and
//! random-order read latency.
//!
//! Not production code -- like the rest of `spikes/sniff`, this exists to produce the numbers
//! `docs/adr/0017-preview-tier-strategy.md` cites, not to be built on directly.

use std::io;
use std::path::{Path, PathBuf};

/// One cached preview's identity: an asset id (this spike uses the ref-10k numeric id) plus a
/// tier tag ("t0", "t2", ...). Real ingest would key on the asset's actual catalog id.
#[derive(Debug, Clone, Copy)]
pub struct CacheKey {
    pub asset_id: u32,
    pub tier: &'static str,
}

pub trait CacheFormat {
    fn put(&mut self, key: CacheKey, bytes: &[u8]) -> io::Result<()>;
    fn get(&mut self, key: CacheKey) -> io::Result<Option<Vec<u8>>>;
    /// Total on-disk footprint of everything written so far.
    fn disk_bytes(&self) -> io::Result<u64>;
    /// Wraps a bulk-populate pass so a SQLite-backed format can batch it into one transaction
    /// instead of paying an implicit commit (and WAL fsync) per `put` -- a real, well-known
    /// SQLite cost, not a fundamental one, so this stays a fair per-format comparison rather than
    /// silently handicapping the SQLite candidate. `FileCache` has no such cost, so its default
    /// no-op implementation is correct as-is.
    fn begin_batch(&mut self) -> io::Result<()> {
        Ok(())
    }
    fn commit_batch(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A) SQLite BLOBs in a separate `previews.db`. WAL mode (matches ADR-0008's catalog-DB choice,
/// consistency across this repo's SQLite usage), one `previews` table keyed on `(asset_id, tier)`.
pub struct SqliteBlobCache {
    conn: rusqlite::Connection,
    db_path: PathBuf,
}

impl SqliteBlobCache {
    pub fn open(db_path: &Path) -> rusqlite::Result<Self> {
        let conn = rusqlite::Connection::open(db_path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS previews (
                asset_id INTEGER NOT NULL,
                tier TEXT NOT NULL,
                data BLOB NOT NULL,
                PRIMARY KEY (asset_id, tier)
            )",
            [],
        )?;
        Ok(SqliteBlobCache {
            conn,
            db_path: db_path.to_path_buf(),
        })
    }
}

impl CacheFormat for SqliteBlobCache {
    fn put(&mut self, key: CacheKey, bytes: &[u8]) -> io::Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO previews (asset_id, tier, data) VALUES (?1, ?2, ?3)",
                rusqlite::params![key.asset_id, key.tier, bytes],
            )
            .map_err(io::Error::other)?;
        Ok(())
    }

    fn get(&mut self, key: CacheKey) -> io::Result<Option<Vec<u8>>> {
        self.conn
            .query_row(
                "SELECT data FROM previews WHERE asset_id = ?1 AND tier = ?2",
                rusqlite::params![key.asset_id, key.tier],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .map(Some)
            .or_else(|e| {
                if matches!(e, rusqlite::Error::QueryReturnedNoRows) {
                    Ok(None)
                } else {
                    Err(io::Error::other(e))
                }
            })
    }

    fn disk_bytes(&self) -> io::Result<u64> {
        // Force a full checkpoint back into the main db file first -- otherwise this measures
        // WAL mode's transient (not steady-state) overhead: a single big `begin_batch`/
        // `commit_batch` transaction leaves everything in `-wal` until the next checkpoint,
        // which nothing has triggered yet at the point a caller wants this number.
        self.conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .map_err(io::Error::other)?;
        disk_bytes_for_prefix(&self.db_path)
    }

    fn begin_batch(&mut self) -> io::Result<()> {
        self.conn.execute_batch("BEGIN").map_err(io::Error::other)
    }

    fn commit_batch(&mut self) -> io::Result<()> {
        self.conn.execute_batch("COMMIT").map_err(io::Error::other)
    }
}

/// B) Append-only pack segments (`pack.bin`) + a SQLite offset/len index (`pack_index.db`).
/// Candidate for large T2 blobs where SQLite's own internal-vs-external-BLOB storage crossover
/// (~100 KB at the default 4 KB page size) starts to matter. No eviction/compaction is simulated
/// here (the plan's own "simulated LRU eviction/compaction pass" is a named follow-up, not
/// implemented in this pass -- flagged in the ADR, not silently skipped).
pub struct PackCache {
    pack: std::fs::File,
    pack_path: PathBuf,
    index_db_path: PathBuf,
    index: rusqlite::Connection,
    write_offset: u64,
}

impl PackCache {
    pub fn open(
        pack_path: &Path,
        index_db_path: &Path,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let pack = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(pack_path)?;
        let write_offset = pack.metadata()?.len();
        let index = rusqlite::Connection::open(index_db_path)?;
        index.execute(
            "CREATE TABLE IF NOT EXISTS pack_index (
                asset_id INTEGER NOT NULL,
                tier TEXT NOT NULL,
                offset INTEGER NOT NULL,
                len INTEGER NOT NULL,
                PRIMARY KEY (asset_id, tier)
            )",
            [],
        )?;
        Ok(PackCache {
            pack,
            pack_path: pack_path.to_path_buf(),
            index_db_path: index_db_path.to_path_buf(),
            index,
            write_offset,
        })
    }
}

impl CacheFormat for PackCache {
    fn put(&mut self, key: CacheKey, bytes: &[u8]) -> io::Result<()> {
        use std::io::Write;
        let offset = self.write_offset;
        self.pack.write_all(bytes)?;
        self.write_offset += bytes.len() as u64;
        self.index
            .execute(
                "INSERT OR REPLACE INTO pack_index (asset_id, tier, offset, len) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![key.asset_id, key.tier, offset as i64, bytes.len() as i64],
            )
            .map_err(io::Error::other)?;
        Ok(())
    }

    fn get(&mut self, key: CacheKey) -> io::Result<Option<Vec<u8>>> {
        let row: Option<(i64, i64)> = self
            .index
            .query_row(
                "SELECT offset, len FROM pack_index WHERE asset_id = ?1 AND tier = ?2",
                rusqlite::params![key.asset_id, key.tier],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map(Some)
            .or_else(|e| {
                if matches!(e, rusqlite::Error::QueryReturnedNoRows) {
                    Ok(None)
                } else {
                    Err(io::Error::other(e))
                }
            })?;
        let Some((offset, len)) = row else {
            return Ok(None);
        };
        let (offset, len) = (offset as u64, len as u64);
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            let mut buf = vec![0u8; len as usize];
            self.pack.read_exact_at(&mut buf, offset)?;
            Ok(Some(buf))
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileExt;
            let mut buf = vec![0u8; len as usize];
            let mut total = 0usize;
            while total < buf.len() {
                let n = self
                    .pack
                    .seek_read(&mut buf[total..], offset + total as u64)?;
                if n == 0 {
                    break;
                }
                total += n;
            }
            Ok(Some(buf))
        }
    }

    fn disk_bytes(&self) -> io::Result<u64> {
        let pack_len = std::fs::metadata(&self.pack_path)?.len();
        let index_len = disk_bytes_for_prefix(&self.index_db_path)?;
        Ok(pack_len + index_len)
    }

    fn begin_batch(&mut self) -> io::Result<()> {
        self.index.execute_batch("BEGIN").map_err(io::Error::other)
    }

    fn commit_batch(&mut self) -> io::Result<()> {
        self.index.execute_batch("COMMIT").map_err(io::Error::other)
    }
}

/// C) File-per-preview baseline, matching LRC's `Previews.lrdata`-style layout: one file per
/// `(asset_id, tier)` under `root`.
pub struct FileCache {
    root: PathBuf,
}

impl FileCache {
    pub fn open(root: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(root)?;
        Ok(FileCache {
            root: root.to_path_buf(),
        })
    }

    fn path_for(&self, key: CacheKey) -> PathBuf {
        self.root.join(format!("{}_{}.bin", key.asset_id, key.tier))
    }
}

impl CacheFormat for FileCache {
    fn put(&mut self, key: CacheKey, bytes: &[u8]) -> io::Result<()> {
        std::fs::write(self.path_for(key), bytes)
    }

    fn get(&mut self, key: CacheKey) -> io::Result<Option<Vec<u8>>> {
        match std::fs::read(self.path_for(key)) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn disk_bytes(&self) -> io::Result<u64> {
        let mut total = 0u64;
        for entry in std::fs::read_dir(&self.root)? {
            total += entry?.metadata()?.len();
        }
        Ok(total)
    }
}

/// SQLite's WAL mode leaves the footprint spread across `<path>`, `<path>-wal`, `<path>-shm`;
/// sum whichever of those exist rather than just the main file (which under-reports mid-session).
fn disk_bytes_for_prefix(db_path: &Path) -> io::Result<u64> {
    let mut total = 0u64;
    for suffix in ["", "-wal", "-shm"] {
        let p = PathBuf::from(format!("{}{}", db_path.display(), suffix));
        if let Ok(meta) = std::fs::metadata(&p) {
            total += meta.len();
        }
    }
    Ok(total)
}

#[derive(Debug, Clone, Copy, clap::ValueEnum, PartialEq, Eq)]
pub enum Format {
    Sqlite,
    Pack,
    File,
}

// The populate + random-order-read benchmark loop over these three formats lives in
// `tier_bench.rs` (it also needs to decode each read, which is outside this module's job of
// storing/retrieving opaque bytes) -- this module stays scoped to the `CacheFormat` backends
// themselves plus the `Format` selector `main.rs`'s CLI and `tier_bench::run` share.
