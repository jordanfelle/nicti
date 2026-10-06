//! Larder (#27, ADR-0029): the bounded on-disk cache for T2 screen-resolution previews — a cat
//! keeps its kills in a larder and only holds as much as it can carry. T0 stays in the catalog's
//! own `preview` table (small, persistent, one per asset); T1/T3 are RAM-only. T2 is the tier
//! ADR-0029 measured as too large for SQLite BLOBs (~1.2 MB each), so it lives in an append-only
//! pack file with a SQLite offset index, which this module bounds with a byte cap and an LRU
//! eviction policy, and exposes manual purge for.
//!
//! Layout under the cache directory: `larder.db` (index + metadata) and `pack-<gen>.bin` (payload
//! bytes). Eviction and purge only drop index rows, so the pack file accumulates dead bytes;
//! [`Larder::compact`] rewrites the live entries into a fresh `pack-<gen+1>.bin` and switches over
//! by committing the new offsets and generation in *one* SQLite transaction, then deleting the old
//! file — a crash at any point leaves either the old generation or the new one fully consistent
//! (never index rows pointing into the wrong file), and nothing needs to rename over an open
//! file, which Windows would refuse.
//!
//! Everything here is regenerable from the source RAW files, so the cache favours cheap
//! self-healing over durability: every payload carries a blake3 checksum in its index row, a
//! failed read or checksum mismatch just drops that entry and reports a miss, and opening
//! discards any row that points past the end of the pack file (a crash before the OS flushed the
//! tail).

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};

use crate::CatalogError;

/// Tiers this cache persists. Only T2 today (ADR-0029); the tier is part of the key so archival
/// tiers (#64/#72) can share the same store later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LarderTier {
    T2,
    /// A screen-size render of the photo *with its develop edits* (#145). Kept beside the camera
    /// `T2` (the primary key is `(asset_id, tier)`) so a stale camera preview survives until the
    /// render lands.
    Rendered,
}

impl LarderTier {
    pub fn as_str(self) -> &'static str {
        match self {
            LarderTier::T2 => "t2",
            LarderTier::Rendered => "r2",
        }
    }
}

/// What a content-addressed (keyed) payload is (#353). The kind is part of the key so other
/// derived blobs can share the store later without colliding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LarderKind {
    /// A baked AI mask alpha, keyed by `ai_bake_key` (ADR-0044's disk tier for mask alpha).
    AiAlpha,
}

impl LarderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            LarderKind::AiAlpha => "alpha",
        }
    }
}

/// A keyed payload's identity. `key` is the content address (for an alpha, the 32 bytes of the
/// bake key); `asset_id` only scopes `purge_asset`, it is not part of the key.
#[derive(Debug, Clone, Copy)]
pub struct LarderKeyed<'a> {
    pub kind: LarderKind,
    pub asset_id: i64,
    pub key: &'a [u8],
}

/// One cached payload's identity: ADR-0029's `(asset_id, tier, render_hash)` cache key. A stored
/// entry whose `render_hash` differs from the one asked for is stale and treated as a miss.
/// `render_hash` is ADR-0021's blake3 edit-document hash, or a fixed sentinel (e.g. `"embedded"`)
/// for camera-JPEG-derived tiers.
#[derive(Debug, Clone, Copy)]
pub struct LarderKey<'a> {
    pub asset_id: i64,
    pub tier: LarderTier,
    pub render_hash: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LarderConfig {
    /// Cap on live payload bytes (not the pack file's footprint, which also holds dead bytes
    /// awaiting compaction — see `compact_min_dead_bytes`).
    pub cap_bytes: u64,
    /// Compaction runs automatically once dead bytes both exceed this floor *and* exceed the live
    /// bytes, so the pack file stays under roughly `2 * cap_bytes + floor` without rewriting the
    /// whole file on every small eviction.
    pub compact_min_dead_bytes: u64,
}

impl Default for LarderConfig {
    fn default() -> Self {
        // ADR-0029: a 3000-5000-image active folder at ~1.2 MB/T2 is ~3.6-6 GB, so 8 GiB covers
        // the largest measured folders comfortably. The user-facing cap setting is a UI concern.
        Self {
            cap_bytes: 8 * 1024 * 1024 * 1024,
            compact_min_dead_bytes: 256 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LarderStats {
    pub entry_count: u64,
    /// Bytes of payloads still indexed.
    pub live_bytes: u64,
    /// Actual size of the pack file (`live_bytes` plus dead bytes awaiting compaction).
    pub file_bytes: u64,
    pub cap_bytes: u64,
}

pub struct Larder {
    dir: PathBuf,
    cfg: LarderConfig,
    conn: Connection,
    pack: File,
    generation: i64,
    file_len: u64,
    live_bytes: u64,
    next_seq: i64,
    /// Whether `put`/`set_cap` compact inline once past the dead-byte threshold. On by default;
    /// a caller that would rather run [`Larder::compact`] as its own background job (#301: a
    /// Pounce job, so a multi-GiB rewrite never stalls whichever thread happened to trigger it)
    /// turns it off with [`Larder::set_auto_compact`] and polls [`Larder::compaction_due`].
    auto_compact: bool,
    /// Held for the life of the `Larder`: the in-memory counters assume a single owner.
    _lock: File,
}

/// Live payload bytes across both index tables (the `(asset, tier)` entries and the keyed ones).
const LIVE_BYTES_SQL: &str = "SELECT COALESCE((SELECT SUM(len) FROM entry), 0)
     + COALESCE((SELECT SUM(len) FROM keyed_entry), 0)";

fn io_err(e: io::Error) -> CatalogError {
    CatalogError::Io(e.to_string())
}

fn pack_path(dir: &Path, generation: i64) -> PathBuf {
    dir.join(format!("pack-{generation}.bin"))
}

fn open_pack(path: &Path) -> io::Result<File> {
    // Not append mode: writes go to an explicit offset so `file_len` stays authoritative, and
    // `set_len` (which Windows refuses on an append-only handle) can truncate a torn write.
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
}

fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.write_all_at(buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut total = 0usize;
        while total < buf.len() {
            let n = file.seek_write(&buf[total..], offset + total as u64)?;
            if n == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            total += n;
        }
        Ok(())
    }
}

fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.read_exact_at(buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut total = 0usize;
        while total < buf.len() {
            let n = file.seek_read(&mut buf[total..], offset + total as u64)?;
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            total += n;
        }
        Ok(())
    }
}

impl Larder {
    pub fn open(dir: &Path, cfg: LarderConfig) -> Result<Self, CatalogError> {
        std::fs::create_dir_all(dir).map_err(io_err)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join("larder.lock"))
            .map_err(io_err)?;
        lock.try_lock().map_err(|e| match e {
            std::fs::TryLockError::WouldBlock => {
                CatalogError::Io("larder directory is already open in another instance".into())
            }
            std::fs::TryLockError::Error(e) => io_err(e),
        })?;
        let conn = Connection::open(dir.join("larder.db"))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // Everything here is regenerable, so don't pay a WAL fsync per commit (every `get` bumps
        // recency); the checksum + open-time checks handle whatever a power loss leaves behind.
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS entry (
                 asset_id    INTEGER NOT NULL,
                 tier        TEXT NOT NULL,
                 render_hash TEXT NOT NULL,
                 offset      INTEGER NOT NULL,
                 len         INTEGER NOT NULL,
                 checksum    BLOB NOT NULL,
                 seq         INTEGER NOT NULL,
                 PRIMARY KEY (asset_id, tier)
             );
             CREATE INDEX IF NOT EXISTS idx_entry_seq ON entry(seq);
             CREATE TABLE IF NOT EXISTS keyed_entry (
                 key      BLOB NOT NULL,
                 kind     TEXT NOT NULL,
                 asset_id INTEGER NOT NULL,
                 offset   INTEGER NOT NULL,
                 len      INTEGER NOT NULL,
                 checksum BLOB NOT NULL,
                 seq      INTEGER NOT NULL,
                 PRIMARY KEY (kind, key)
             );
             CREATE INDEX IF NOT EXISTS idx_keyed_seq ON keyed_entry(seq);
             CREATE INDEX IF NOT EXISTS idx_keyed_asset ON keyed_entry(asset_id);",
        )?;
        let meta = |key: &str| -> Result<Option<i64>, rusqlite::Error> {
            conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
                .optional()
        };
        let generation = meta("generation")?.unwrap_or(0);
        let next_seq = meta("next_seq")?.unwrap_or(0);

        // A crash mid-compaction can leave the not-yet-adopted (or already-superseded) pack file
        // behind; only the generation the index names is real.
        let keep = format!("pack-{generation}.bin");
        for entry in std::fs::read_dir(dir).map_err(io_err)? {
            let entry = entry.map_err(io_err)?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("pack-") && name.ends_with(".bin") && name != keep {
                let _ = std::fs::remove_file(entry.path());
            }
        }

        let pack = open_pack(&pack_path(dir, generation)).map_err(io_err)?;
        let file_len = pack.metadata().map_err(io_err)?.len();
        conn.execute(
            "DELETE FROM entry WHERE offset < 0 OR len < 0 OR offset + len > ?1",
            [file_len as i64],
        )?;
        conn.execute(
            "DELETE FROM keyed_entry WHERE offset < 0 OR len < 0 OR offset + len > ?1",
            [file_len as i64],
        )?;
        let live_bytes: i64 = conn.query_row(LIVE_BYTES_SQL, [], |r| r.get(0))?;

        let mut larder = Self {
            dir: dir.to_path_buf(),
            cfg,
            conn,
            pack,
            generation,
            file_len,
            live_bytes: live_bytes as u64,
            next_seq,
            auto_compact: true,
            _lock: lock,
        };
        // A smaller cap than the previous session's takes effect immediately.
        larder.evict_to_fit(0)?;
        Ok(larder)
    }

    pub fn stats(&self) -> Result<LarderStats, CatalogError> {
        let entry_count: i64 = self.conn.query_row(
            "SELECT (SELECT COUNT(*) FROM entry) + (SELECT COUNT(*) FROM keyed_entry)",
            [],
            |r| r.get(0),
        )?;
        Ok(LarderStats {
            entry_count: entry_count as u64,
            live_bytes: self.live_bytes,
            file_bytes: self.file_len,
            cap_bytes: self.cfg.cap_bytes,
        })
    }

    /// Changes the byte cap, evicting least-recently-used entries at once if it shrank.
    pub fn set_cap(&mut self, cap_bytes: u64) -> Result<(), CatalogError> {
        self.cfg.cap_bytes = cap_bytes;
        self.evict_to_fit(0)?;
        self.compact_if_worthwhile()
    }

    /// Stores `bytes` for `key`, replacing any previous entry for the same `(asset, tier)` and
    /// evicting least-recently-used entries until it fits under the cap. Returns `false` (storing
    /// nothing, and leaving any existing entry alone) when the payload alone exceeds the whole
    /// cap — evicting everything for something that would itself be evicted next is worse than a
    /// miss.
    pub fn put(&mut self, key: LarderKey<'_>, bytes: &[u8]) -> Result<bool, CatalogError> {
        self.put_tx(bytes, |l| l.put_in_tx(key, bytes))
    }

    /// Stores `bytes` under a content-addressed key (#353), replacing any previous entry for the
    /// same `(kind, key)`. Unlike [`Larder::put`] there can be many per asset, and no stale case:
    /// the key already names the content. Shares the pack file, the byte cap and the
    /// recency order with the `(asset, tier)` entries. Same `false`-when-over-the-cap contract.
    pub fn put_keyed(&mut self, key: LarderKeyed<'_>, bytes: &[u8]) -> Result<bool, CatalogError> {
        self.put_tx(bytes, |l| l.put_keyed_in_tx(key, bytes))
    }

    fn put_tx(
        &mut self,
        bytes: &[u8],
        body: impl FnOnce(&mut Self) -> Result<(), CatalogError>,
    ) -> Result<bool, CatalogError> {
        if bytes.len() as u64 > self.cfg.cap_bytes {
            return Ok(false);
        }
        self.conn.execute_batch("BEGIN")?;
        let stored = body(self);
        match stored {
            Ok(()) => {
                if let Err(e) = self.conn.execute_batch("COMMIT") {
                    // A failed COMMIT can leave the transaction open; close it and resync.
                    let _ = self.conn.execute_batch("ROLLBACK");
                    self.resync_counters();
                    return Err(e.into());
                }
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                // The in-memory counters may have moved past what the rollback kept.
                self.resync_counters();
                return Err(e);
            }
        }
        // Compaction is best-effort housekeeping: the entry is already stored, so a failure here
        // (disk full, ...) must not turn a successful put into an error. It retries next time.
        let _ = self.compact_if_worthwhile();
        Ok(true)
    }

    /// Appends `bytes` to the pack and takes the next recency `seq`; the caller inserts the index
    /// row. Evicts first so the payload fits under the cap.
    fn append_payload(&mut self, bytes: &[u8]) -> Result<(u64, i64), CatalogError> {
        let len = bytes.len() as u64;
        self.evict_to_fit(len)?;
        let offset = self.file_len;
        if let Err(e) = write_all_at(&self.pack, bytes, offset) {
            // Drop any torn tail so the next write can't leave a gap or overlap.
            let _ = self.pack.set_len(offset);
            return Err(io_err(e));
        }
        self.file_len += len;
        let seq = self.bump_seq()?;
        Ok((offset, seq))
    }

    fn put_keyed_in_tx(&mut self, key: LarderKeyed<'_>, bytes: &[u8]) -> Result<(), CatalogError> {
        self.remove_keyed(key.kind.as_str(), key.key)?;
        let (offset, seq) = self.append_payload(bytes)?;
        let len = bytes.len() as u64;
        self.conn.execute(
            "INSERT INTO keyed_entry (key, kind, asset_id, offset, len, checksum, seq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                key.key,
                key.kind.as_str(),
                key.asset_id,
                offset as i64,
                len as i64,
                blake3::hash(bytes).as_bytes().as_slice(),
                seq,
            ],
        )?;
        self.live_bytes += len;
        Ok(())
    }

    fn put_in_tx(&mut self, key: LarderKey<'_>, bytes: &[u8]) -> Result<(), CatalogError> {
        let len = bytes.len() as u64;
        self.remove_entry(key.asset_id, key.tier.as_str())?;
        let (offset, seq) = self.append_payload(bytes)?;
        self.conn.execute(
            "INSERT INTO entry (asset_id, tier, render_hash, offset, len, checksum, seq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                key.asset_id,
                key.tier.as_str(),
                key.render_hash,
                offset as i64,
                len as i64,
                blake3::hash(bytes).as_bytes().as_slice(),
                seq,
            ],
        )?;
        self.live_bytes += len;
        Ok(())
    }

    /// Recomputes the counters from ground truth after a failed transaction.
    fn resync_counters(&mut self) {
        if let Ok(live) = self
            .conn
            .query_row(LIVE_BYTES_SQL, [], |r| r.get::<_, i64>(0))
        {
            self.live_bytes = live as u64;
        }
        if let Ok(meta) = self.pack.metadata() {
            self.file_len = meta.len();
        }
        // A rolled-back `bump_seq` also rolled back its persisted high-water mark; resume from
        // the committed one (always ahead of every committed seq) so a restart can't resume below
        // rows written after this point.
        if let Ok(mark) = self.conn.query_row(
            "SELECT COALESCE((SELECT value FROM meta WHERE key = 'next_seq'), 0)",
            [],
            |r| r.get::<_, i64>(0),
        ) {
            self.next_seq = mark;
        }
    }

    /// Returns the cached payload and marks it most-recently-used. A stored entry with a
    /// different `render_hash` is stale: it's dropped and reported as a miss. An unreadable or
    /// checksum-failing entry is dropped and reported as a miss too — the caller regenerates it.
    pub fn get(&mut self, key: LarderKey<'_>) -> Result<Option<Vec<u8>>, CatalogError> {
        let tier = key.tier.as_str();
        let row: Option<(String, i64, i64, Vec<u8>)> = self
            .conn
            .query_row(
                "SELECT render_hash, offset, len, checksum FROM entry
                 WHERE asset_id = ?1 AND tier = ?2",
                params![key.asset_id, tier],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((render_hash, offset, len, checksum)) = row else {
            return Ok(None);
        };
        if render_hash != key.render_hash {
            self.remove_entry(key.asset_id, tier)?;
            return Ok(None);
        }
        let mut buf = vec![0u8; len as usize];
        let intact = read_exact_at(&self.pack, &mut buf, offset as u64).is_ok()
            && blake3::hash(&buf).as_bytes().as_slice() == checksum.as_slice();
        if !intact {
            self.remove_entry(key.asset_id, tier)?;
            return Ok(None);
        }
        let seq = self.bump_seq()?;
        self.conn.execute(
            "UPDATE entry SET seq = ?3 WHERE asset_id = ?1 AND tier = ?2",
            params![key.asset_id, tier, seq],
        )?;
        Ok(Some(buf))
    }

    /// Returns whatever payload is stored for `(asset_id, tier)` together with the `render_hash` it
    /// was stored under, **without** dropping it when that hash is out of date -- the
    /// stale-while-revalidate read (#145): the caller compares the hash itself and may still show
    /// an older render while a new one is made. An unreadable or checksum-failing entry is dropped
    /// and reported as a miss, as in [`Larder::get`]. Marks the entry most-recently-used.
    pub fn get_latest(
        &mut self,
        asset_id: i64,
        tier: LarderTier,
    ) -> Result<Option<(String, Vec<u8>)>, CatalogError> {
        let tier = tier.as_str();
        let row: Option<(String, i64, i64, Vec<u8>)> = self
            .conn
            .query_row(
                "SELECT render_hash, offset, len, checksum FROM entry
                 WHERE asset_id = ?1 AND tier = ?2",
                params![asset_id, tier],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((render_hash, offset, len, checksum)) = row else {
            return Ok(None);
        };
        let mut buf = vec![0u8; len as usize];
        let intact = read_exact_at(&self.pack, &mut buf, offset as u64).is_ok()
            && blake3::hash(&buf).as_bytes().as_slice() == checksum.as_slice();
        if !intact {
            self.remove_entry(asset_id, tier)?;
            return Ok(None);
        }
        let seq = self.bump_seq()?;
        self.conn.execute(
            "UPDATE entry SET seq = ?3 WHERE asset_id = ?1 AND tier = ?2",
            params![asset_id, tier, seq],
        )?;
        Ok(Some((render_hash, buf)))
    }

    /// The `render_hash` stored for `(asset_id, tier)`, if any -- a cheap index-only lookup (no
    /// payload read, no recency bump) for "is a current render already cached?" checks.
    pub fn stored_hash(
        &self,
        asset_id: i64,
        tier: LarderTier,
    ) -> Result<Option<String>, CatalogError> {
        Ok(self
            .conn
            .query_row(
                "SELECT render_hash FROM entry WHERE asset_id = ?1 AND tier = ?2",
                params![asset_id, tier.as_str()],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn contains(&self, key: LarderKey<'_>) -> Result<bool, CatalogError> {
        let stored: Option<String> = self
            .conn
            .query_row(
                "SELECT render_hash FROM entry WHERE asset_id = ?1 AND tier = ?2",
                params![key.asset_id, key.tier.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        Ok(stored.as_deref() == Some(key.render_hash))
    }

    /// Returns the payload stored under a content-addressed key and marks it most-recently-used.
    /// An unreadable or checksum-failing entry is dropped and reported as a miss.
    pub fn get_keyed(&mut self, key: LarderKeyed<'_>) -> Result<Option<Vec<u8>>, CatalogError> {
        let row: Option<(i64, i64, Vec<u8>)> = self
            .conn
            .query_row(
                "SELECT offset, len, checksum FROM keyed_entry WHERE kind = ?1 AND key = ?2",
                params![key.kind.as_str(), key.key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((offset, len, checksum)) = row else {
            return Ok(None);
        };
        let mut buf = vec![0u8; len as usize];
        let intact = read_exact_at(&self.pack, &mut buf, offset as u64).is_ok()
            && blake3::hash(&buf).as_bytes().as_slice() == checksum.as_slice();
        if !intact {
            self.remove_keyed(key.kind.as_str(), key.key)?;
            return Ok(None);
        }
        let seq = self.bump_seq()?;
        self.conn.execute(
            "UPDATE keyed_entry SET seq = ?3 WHERE kind = ?1 AND key = ?2",
            params![key.kind.as_str(), key.key, seq],
        )?;
        Ok(Some(buf))
    }

    /// Drops the entry under `key`, if any. For a caller that read a payload which passed the
    /// checksum but that it cannot interpret (written by another build): left in place it would
    /// also make [`Larder::contains_keyed`] true forever and keep a replacement from being stored.
    pub fn forget_keyed(&mut self, key: LarderKeyed<'_>) -> Result<(), CatalogError> {
        self.remove_keyed(key.kind.as_str(), key.key)
    }

    /// Whether a payload is indexed under `key` -- index-only, no read, no recency bump.
    pub fn contains_keyed(&self, key: LarderKeyed<'_>) -> Result<bool, CatalogError> {
        let found: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM keyed_entry WHERE kind = ?1 AND key = ?2",
                params![key.kind.as_str(), key.key],
                |r| r.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    /// Manual purge: drops every cached payload for one asset (e.g. after its source file was
    /// replaced), keyed ones included. Returns the live bytes freed. Disk space is reclaimed by
    /// the next compaction (automatic past the dead-byte threshold, or call [`Larder::compact`]
    /// after a batch).
    pub fn purge_asset(&mut self, asset_id: i64) -> Result<u64, CatalogError> {
        let keyed = self.purge_keyed_where("asset_id = ?1", asset_id.into())?;
        Ok(keyed + self.purge_where("asset_id = ?1", asset_id.into(), false)?)
    }

    /// Manual purge: drops every keyed payload of one kind. Returns the live bytes freed.
    pub fn purge_kind(&mut self, kind: LarderKind) -> Result<u64, CatalogError> {
        let freed = self.purge_keyed_where("kind = ?1", kind.as_str().to_string().into())?;
        if freed > 0 {
            let _ = self.compact();
        }
        Ok(freed)
    }

    /// Manual purge: drops every payload of one tier. Returns the live bytes freed.
    pub fn purge_tier(&mut self, tier: LarderTier) -> Result<u64, CatalogError> {
        self.purge_where("tier = ?1", tier.as_str().to_string().into(), true)
    }

    /// Manual purge: empties the cache and reclaims its disk space immediately (by starting a
    /// fresh pack generation instead of waiting for compaction). Returns the live bytes freed.
    pub fn purge_all(&mut self) -> Result<u64, CatalogError> {
        let freed = self.live_bytes;
        let new_gen = self.generation + 1;
        let _ = std::fs::remove_file(pack_path(&self.dir, new_gen));
        let new_pack = open_pack(&pack_path(&self.dir, new_gen)).map_err(io_err)?;
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM entry", [])?;
        tx.execute("DELETE FROM keyed_entry", [])?;
        tx.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('generation', ?1)",
            [new_gen],
        )?;
        tx.commit()?;
        let old = std::mem::replace(&mut self.pack, new_pack);
        drop(old);
        let _ = std::fs::remove_file(pack_path(&self.dir, self.generation));
        self.generation = new_gen;
        self.file_len = 0;
        self.live_bytes = 0;
        Ok(freed)
    }

    /// Rewrites live payloads into a fresh pack generation, dropping dead bytes. Safe to call at
    /// any time; a no-op reduction if nothing is dead.
    pub fn compact(&mut self) -> Result<(), CatalogError> {
        let new_gen = self.generation + 1;
        let new_path = pack_path(&self.dir, new_gen);
        let result = self.compact_into(new_gen, &new_path);
        if result.is_err() {
            let _ = std::fs::remove_file(&new_path);
        }
        result
    }

    fn compact_into(&mut self, new_gen: i64, new_path: &Path) -> Result<(), CatalogError> {
        // `append` and `truncate` can't be combined, so clear any stale file at this path first.
        let _ = std::fs::remove_file(new_path);
        let new_pack = open_pack(new_path).map_err(io_err)?;
        let rows: Vec<(i64, String, i64, i64)> = {
            let mut stmt = self
                .conn
                .prepare("SELECT asset_id, tier, offset, len FROM entry ORDER BY offset")?;
            let mapped =
                stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
            mapped.collect::<Result<_, _>>()?
        };
        let keyed_rows: Vec<(Vec<u8>, String, i64, i64)> = {
            let mut stmt = self
                .conn
                .prepare("SELECT key, kind, offset, len FROM keyed_entry ORDER BY offset")?;
            let mapped =
                stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
            mapped.collect::<Result<_, _>>()?
        };
        let mut moved = Vec::with_capacity(rows.len());
        let mut moved_keyed = Vec::with_capacity(keyed_rows.len());
        let mut new_len = 0u64;
        for (asset_id, tier, offset, len) in rows {
            let mut buf = vec![0u8; len as usize];
            read_exact_at(&self.pack, &mut buf, offset as u64).map_err(io_err)?;
            write_all_at(&new_pack, &buf, new_len).map_err(io_err)?;
            moved.push((asset_id, tier, new_len as i64));
            new_len += len as u64;
        }
        for (key, kind, offset, len) in keyed_rows {
            let mut buf = vec![0u8; len as usize];
            read_exact_at(&self.pack, &mut buf, offset as u64).map_err(io_err)?;
            write_all_at(&new_pack, &buf, new_len).map_err(io_err)?;
            moved_keyed.push((key, kind, new_len as i64));
            new_len += len as u64;
        }
        new_pack.sync_all().map_err(io_err)?;

        let tx = self.conn.transaction()?;
        for (asset_id, tier, new_offset) in &moved {
            tx.execute(
                "UPDATE entry SET offset = ?3 WHERE asset_id = ?1 AND tier = ?2",
                params![asset_id, tier, new_offset],
            )?;
        }
        for (key, kind, new_offset) in &moved_keyed {
            tx.execute(
                "UPDATE keyed_entry SET offset = ?3 WHERE key = ?1 AND kind = ?2",
                params![key, kind, new_offset],
            )?;
        }
        tx.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('generation', ?1)",
            [new_gen],
        )?;
        tx.commit()?;

        let old_gen = self.generation;
        let old = std::mem::replace(&mut self.pack, new_pack);
        drop(old);
        let _ = std::fs::remove_file(pack_path(&self.dir, old_gen));
        self.generation = new_gen;
        self.file_len = new_len;
        Ok(())
    }

    /// Repairs the Larder after a panic unwound through one of its methods while a caller held it
    /// (a poisoned `Mutex`): rolls back any transaction the panic left open -- otherwise every
    /// later `put` fails at `BEGIN` -- and resyncs the in-memory counters from ground truth.
    /// Harmless when nothing is wrong (`ROLLBACK` with no open transaction is ignored).
    pub fn recover_after_panic(&mut self) {
        let _ = self.conn.execute_batch("ROLLBACK");
        self.resync_counters();
    }

    /// Whether dead bytes have passed the same threshold `put` auto-compacts at.
    pub fn compaction_due(&self) -> bool {
        let dead = self.file_len.saturating_sub(self.live_bytes);
        dead > self.cfg.compact_min_dead_bytes && dead > self.live_bytes
    }

    /// Turns inline auto-compaction on or off (on by default). With it off, nothing compacts
    /// until the caller runs [`Larder::compact`] itself, typically when [`Larder::compaction_due`]
    /// says so.
    pub fn set_auto_compact(&mut self, enabled: bool) {
        self.auto_compact = enabled;
    }

    fn compact_if_worthwhile(&mut self) -> Result<(), CatalogError> {
        if self.auto_compact && self.compaction_due() {
            self.compact()?;
        }
        Ok(())
    }

    fn purge_where(
        &mut self,
        predicate: &str,
        arg: rusqlite::types::Value,
        reclaim_now: bool,
    ) -> Result<u64, CatalogError> {
        let freed: i64 = self.conn.query_row(
            &format!("SELECT COALESCE(SUM(len), 0) FROM entry WHERE {predicate}"),
            params![arg],
            |r| r.get(0),
        )?;
        self.conn.execute(
            &format!("DELETE FROM entry WHERE {predicate}"),
            params![arg],
        )?;
        self.live_bytes = self.live_bytes.saturating_sub(freed as u64);
        // Compaction rewrites every live entry, so a bulk purge (a whole tier) reclaims now, while
        // a per-asset purge only compacts once dead bytes cross the usual threshold -- purging
        // many assets in a row must not rewrite the pack each time (call `compact` after a batch
        // to reclaim immediately). Best-effort: the rows are already gone either way.
        if reclaim_now && freed > 0 {
            let _ = self.compact();
        } else {
            let _ = self.compact_if_worthwhile();
        }
        Ok(freed as u64)
    }

    /// Like [`Larder::purge_where`] but for `keyed_entry`; never compacts (the callers decide).
    fn purge_keyed_where(
        &mut self,
        predicate: &str,
        arg: rusqlite::types::Value,
    ) -> Result<u64, CatalogError> {
        let freed: i64 = self.conn.query_row(
            &format!("SELECT COALESCE(SUM(len), 0) FROM keyed_entry WHERE {predicate}"),
            params![arg],
            |r| r.get(0),
        )?;
        self.conn.execute(
            &format!("DELETE FROM keyed_entry WHERE {predicate}"),
            params![arg],
        )?;
        self.live_bytes = self.live_bytes.saturating_sub(freed as u64);
        Ok(freed as u64)
    }

    fn remove_keyed(&mut self, kind: &str, key: &[u8]) -> Result<(), CatalogError> {
        let len: Option<i64> = self
            .conn
            .query_row(
                "SELECT len FROM keyed_entry WHERE kind = ?1 AND key = ?2",
                params![kind, key],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(len) = len {
            self.conn.execute(
                "DELETE FROM keyed_entry WHERE kind = ?1 AND key = ?2",
                params![kind, key],
            )?;
            self.live_bytes = self.live_bytes.saturating_sub(len as u64);
        }
        Ok(())
    }

    fn remove_entry(&mut self, asset_id: i64, tier: &str) -> Result<(), CatalogError> {
        let len: Option<i64> = self
            .conn
            .query_row(
                "SELECT len FROM entry WHERE asset_id = ?1 AND tier = ?2",
                params![asset_id, tier],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(len) = len {
            self.conn.execute(
                "DELETE FROM entry WHERE asset_id = ?1 AND tier = ?2",
                params![asset_id, tier],
            )?;
            self.live_bytes = self.live_bytes.saturating_sub(len as u64);
        }
        Ok(())
    }

    /// Evicts least-recently-used entries until `incoming` more bytes would fit under the cap.
    fn evict_to_fit(&mut self, incoming: u64) -> Result<(), CatalogError> {
        while self.live_bytes + incoming > self.cfg.cap_bytes {
            // Oldest `seq` across both index tables; `seq` is one shared counter, so it orders
            // them against each other.
            let tiered: Option<(i64, i64, String)> = self
                .conn
                .query_row(
                    "SELECT seq, asset_id, tier FROM entry ORDER BY seq LIMIT 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            let keyed: Option<(i64, Vec<u8>, String)> = self
                .conn
                .query_row(
                    "SELECT seq, key, kind FROM keyed_entry ORDER BY seq LIMIT 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            match (tiered, keyed) {
                (None, None) => break,
                (Some((_, asset_id, tier)), None) => self.remove_entry(asset_id, &tier)?,
                (Some((a, asset_id, tier)), Some((b, ..))) if a <= b => {
                    self.remove_entry(asset_id, &tier)?
                }
                (_, Some((_, key, kind))) => self.remove_keyed(&kind, &key)?,
            }
        }
        Ok(())
    }

    fn bump_seq(&mut self) -> Result<i64, CatalogError> {
        let seq = self.next_seq;
        // Persisted so recency order survives a restart, without a write per bump: store a
        // high-water mark 256 ahead every 256 bumps and resume from it on open (a restart can
        // only ever *under*-order the last <256 touches, never reorder older ones after newer).
        // Written before advancing, so a failed write can't leave `next_seq` past a stale mark.
        if seq % 256 == 0 {
            self.conn.execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES ('next_seq', ?1)",
                [seq + 256],
            )?;
        }
        self.next_seq += 1;
        Ok(seq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(asset_id: i64) -> LarderKey<'static> {
        LarderKey {
            asset_id,
            tier: LarderTier::T2,
            render_hash: "embedded",
        }
    }

    fn cfg(cap: u64) -> LarderConfig {
        LarderConfig {
            cap_bytes: cap,
            compact_min_dead_bytes: u64::MAX,
        }
    }

    #[test]
    fn put_then_get_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Larder::open(dir.path(), cfg(1000)).unwrap();
        assert!(l.put(key(1), b"hello").unwrap());
        assert_eq!(l.get(key(1)).unwrap().as_deref(), Some(&b"hello"[..]));
        assert_eq!(l.get(key(2)).unwrap(), None);
    }

    #[test]
    fn evicts_least_recently_used_not_least_recently_inserted() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Larder::open(dir.path(), cfg(30)).unwrap();
        l.put(key(1), &[1; 10]).unwrap();
        l.put(key(2), &[2; 10]).unwrap();
        l.put(key(3), &[3; 10]).unwrap();
        // Touch 1 so 2 becomes the LRU victim.
        assert!(l.get(key(1)).unwrap().is_some());
        l.put(key(4), &[4; 10]).unwrap();
        assert!(l.get(key(2)).unwrap().is_none(), "2 was the LRU entry");
        assert!(l.get(key(1)).unwrap().is_some());
        assert!(l.get(key(3)).unwrap().is_some());
        assert!(l.get(key(4)).unwrap().is_some());
        assert!(l.stats().unwrap().live_bytes <= 30);
    }

    #[test]
    fn payload_larger_than_cap_is_rejected_without_evicting() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Larder::open(dir.path(), cfg(20)).unwrap();
        l.put(key(1), &[1; 10]).unwrap();
        assert!(!l.put(key(2), &[2; 21]).unwrap());
        assert!(l.get(key(1)).unwrap().is_some());
    }

    #[test]
    fn replacing_an_entry_does_not_double_count_live_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Larder::open(dir.path(), cfg(100)).unwrap();
        l.put(key(1), &[1; 40]).unwrap();
        l.put(key(1), &[2; 40]).unwrap();
        let s = l.stats().unwrap();
        assert_eq!((s.entry_count, s.live_bytes), (1, 40));
        assert_eq!(l.get(key(1)).unwrap(), Some(vec![2; 40]));
    }

    #[test]
    fn stale_render_hash_is_a_miss_and_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Larder::open(dir.path(), cfg(100)).unwrap();
        l.put(key(1), b"old").unwrap();
        let newer = LarderKey {
            render_hash: "abc123",
            ..key(1)
        };
        assert!(!l.contains(newer).unwrap());
        assert_eq!(l.get(newer).unwrap(), None);
        assert_eq!(l.stats().unwrap().entry_count, 0);
    }

    fn rendered(asset_id: i64, hash: &'static str) -> LarderKey<'static> {
        LarderKey {
            asset_id,
            tier: LarderTier::Rendered,
            render_hash: hash,
        }
    }

    #[test]
    fn a_rendered_entry_and_the_camera_t2_coexist_for_one_asset() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Larder::open(dir.path(), cfg(100)).unwrap();
        l.put(key(1), b"camera").unwrap();
        l.put(rendered(1, "r:a"), b"edited").unwrap();
        assert_eq!(l.get(key(1)).unwrap().as_deref(), Some(&b"camera"[..]));
        assert_eq!(
            l.get(rendered(1, "r:a")).unwrap().as_deref(),
            Some(&b"edited"[..])
        );
        // Asking for a *newer* render drops only the rendered entry, never the camera one.
        assert_eq!(l.get(rendered(1, "r:b")).unwrap(), None);
        assert_eq!(l.get(key(1)).unwrap().as_deref(), Some(&b"camera"[..]));
    }

    #[test]
    fn get_latest_returns_a_stale_entry_without_dropping_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Larder::open(dir.path(), cfg(100)).unwrap();
        l.put(rendered(1, "r:old"), b"old").unwrap();
        let (hash, bytes) = l.get_latest(1, LarderTier::Rendered).unwrap().unwrap();
        assert_eq!((hash.as_str(), bytes.as_slice()), ("r:old", &b"old"[..]));
        // Still there afterwards, and replaceable by the newer render.
        assert_eq!(
            l.stored_hash(1, LarderTier::Rendered).unwrap().as_deref(),
            Some("r:old")
        );
        l.put(rendered(1, "r:new"), b"new").unwrap();
        let (hash, _) = l.get_latest(1, LarderTier::Rendered).unwrap().unwrap();
        assert_eq!(hash, "r:new");
        assert_eq!(l.stats().unwrap().entry_count, 1);
        assert_eq!(l.get_latest(2, LarderTier::Rendered).unwrap(), None);
    }

    #[test]
    fn get_latest_drops_a_corrupted_payload() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Larder::open(dir.path(), cfg(100)).unwrap();
        l.put(rendered(1, "r:a"), &[7; 16]).unwrap();
        drop(l);
        let pack = pack_path(dir.path(), 0);
        let mut bytes = std::fs::read(&pack).unwrap();
        bytes[3] ^= 0xff;
        std::fs::write(&pack, bytes).unwrap();
        let mut l = Larder::open(dir.path(), cfg(100)).unwrap();
        assert_eq!(l.get_latest(1, LarderTier::Rendered).unwrap(), None);
        assert_eq!(l.stats().unwrap().entry_count, 0);
    }

    #[test]
    fn purge_tier_rendered_leaves_the_camera_t2() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Larder::open(dir.path(), cfg(100)).unwrap();
        l.put(key(1), &[1; 10]).unwrap();
        l.put(rendered(1, "r:a"), &[2; 10]).unwrap();
        assert_eq!(l.purge_tier(LarderTier::Rendered).unwrap(), 10);
        assert!(l.get(key(1)).unwrap().is_some());
        assert_eq!(l.stored_hash(1, LarderTier::Rendered).unwrap(), None);
    }

    #[test]
    fn corrupted_payload_is_dropped_as_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Larder::open(dir.path(), cfg(100)).unwrap();
        l.put(key(1), &[7; 16]).unwrap();
        drop(l);
        let pack = pack_path(dir.path(), 0);
        let mut bytes = std::fs::read(&pack).unwrap();
        bytes[3] ^= 0xff;
        std::fs::write(&pack, bytes).unwrap();
        let mut l = Larder::open(dir.path(), cfg(100)).unwrap();
        assert_eq!(l.get(key(1)).unwrap(), None);
        assert_eq!(l.stats().unwrap().live_bytes, 0);
    }

    #[test]
    fn open_discards_entries_pointing_past_a_truncated_pack() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Larder::open(dir.path(), cfg(100)).unwrap();
        l.put(key(1), &[1; 10]).unwrap();
        l.put(key(2), &[2; 10]).unwrap();
        drop(l);
        let f = OpenOptions::new()
            .write(true)
            .open(pack_path(dir.path(), 0))
            .unwrap();
        f.set_len(15).unwrap(); // entry 2 (bytes 10..20) lost its tail
        drop(f);
        let mut l = Larder::open(dir.path(), cfg(100)).unwrap();
        assert!(l.get(key(1)).unwrap().is_some());
        assert!(l.get(key(2)).unwrap().is_none());
        assert_eq!(l.stats().unwrap().live_bytes, 10);
    }

    #[test]
    fn survives_reopen_and_keeps_recency_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Larder::open(dir.path(), cfg(30)).unwrap();
        l.put(key(1), &[1; 10]).unwrap();
        l.put(key(2), &[2; 10]).unwrap();
        l.put(key(3), &[3; 10]).unwrap();
        l.get(key(1)).unwrap();
        drop(l);
        let mut l = Larder::open(dir.path(), cfg(30)).unwrap();
        l.put(key(4), &[4; 10]).unwrap();
        assert!(
            l.get(key(2)).unwrap().is_none(),
            "2 stayed the LRU across reopen"
        );
        assert!(l.get(key(1)).unwrap().is_some());
    }

    #[test]
    fn shrinking_the_cap_on_open_evicts_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Larder::open(dir.path(), cfg(100)).unwrap();
        for id in 1..=5 {
            l.put(key(id), &[id as u8; 20]).unwrap();
        }
        drop(l);
        let l = Larder::open(dir.path(), cfg(45)).unwrap();
        assert_eq!(l.stats().unwrap().live_bytes, 40);
    }

    #[test]
    fn purge_asset_tier_and_all_report_freed_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Larder::open(dir.path(), cfg(1000)).unwrap();
        l.put(key(1), &[1; 10]).unwrap();
        l.put(key(2), &[2; 20]).unwrap();
        l.put(key(3), &[3; 30]).unwrap();
        assert_eq!(l.purge_asset(2).unwrap(), 20);
        assert!(l.get(key(2)).unwrap().is_none());
        assert_eq!(l.purge_tier(LarderTier::T2).unwrap(), 40);
        assert_eq!(l.stats().unwrap().entry_count, 0);
        l.put(key(4), &[4; 10]).unwrap();
        assert_eq!(l.purge_all().unwrap(), 10);
        let s = l.stats().unwrap();
        assert_eq!((s.entry_count, s.live_bytes, s.file_bytes), (0, 0, 0));
        assert!(l.put(key(5), b"after purge").unwrap());
        assert_eq!(l.get(key(5)).unwrap().as_deref(), Some(&b"after purge"[..]));
    }

    #[test]
    fn compaction_reclaims_dead_bytes_and_keeps_payloads_readable() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Larder::open(dir.path(), cfg(1000)).unwrap();
        for id in 1..=4 {
            l.put(key(id), &[id as u8; 25]).unwrap();
        }
        // Replacing an entry leaves its old bytes dead in the pack.
        l.put(key(1), &[11; 25]).unwrap();
        l.put(key(3), &[33; 25]).unwrap();
        assert_eq!(l.stats().unwrap().file_bytes, 150);
        l.compact().unwrap();
        assert_eq!(l.stats().unwrap().file_bytes, 100);
        assert_eq!(l.get(key(1)).unwrap(), Some(vec![11; 25]));
        assert_eq!(l.get(key(2)).unwrap(), Some(vec![2; 25]));
        assert_eq!(l.get(key(3)).unwrap(), Some(vec![33; 25]));
        assert_eq!(l.get(key(4)).unwrap(), Some(vec![4; 25]));
        assert!(!pack_path(dir.path(), 0).exists(), "old generation deleted");
        // ...and it all still holds after a reopen onto the new generation.
        drop(l);
        let mut l = Larder::open(dir.path(), cfg(1000)).unwrap();
        assert_eq!(l.get(key(4)).unwrap(), Some(vec![4; 25]));
    }

    #[test]
    fn auto_compaction_bounds_the_pack_file_under_churn() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Larder::open(
            dir.path(),
            LarderConfig {
                cap_bytes: 100,
                compact_min_dead_bytes: 50,
            },
        )
        .unwrap();
        for round in 0..200u32 {
            l.put(key((round % 7) as i64), &[round as u8; 20]).unwrap();
        }
        let s = l.stats().unwrap();
        assert!(s.live_bytes <= 100);
        // dead <= max(floor, live) after every put, plus at most one uncompacted payload.
        assert!(
            s.file_bytes <= 100 + 100 + 20,
            "pack file grew unbounded: {} bytes",
            s.file_bytes
        );
        assert!(l.generation > 0, "churn must have triggered compaction");
    }

    #[test]
    fn recover_after_panic_rolls_back_a_wedged_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Larder::open(dir.path(), cfg(1000)).unwrap();
        assert!(l.put(key(1), b"before").unwrap());
        // What a panic inside `put` between BEGIN and COMMIT leaves behind.
        l.conn.execute_batch("BEGIN").unwrap();
        assert!(l.put(key(2), b"wedged").is_err());

        l.recover_after_panic();
        assert!(l.put(key(2), b"after").unwrap());
        assert_eq!(l.get(key(1)).unwrap().unwrap(), b"before");
        assert_eq!(l.get(key(2)).unwrap().unwrap(), b"after");
    }

    #[test]
    fn disabling_auto_compact_defers_compaction_to_an_explicit_call() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Larder::open(
            dir.path(),
            LarderConfig {
                cap_bytes: 100,
                compact_min_dead_bytes: 50,
            },
        )
        .unwrap();
        l.set_auto_compact(false);
        assert!(!l.compaction_due());
        for round in 0..200u32 {
            l.put(key((round % 7) as i64), &[round as u8; 20]).unwrap();
        }
        assert_eq!(
            l.generation, 0,
            "no compaction may run while auto-compact is off"
        );
        assert!(
            l.compaction_due(),
            "churn must have pushed dead bytes past the threshold"
        );

        l.compact().unwrap();
        assert!(!l.compaction_due());
        assert_eq!(l.stats().unwrap().file_bytes, l.stats().unwrap().live_bytes);
        assert!(
            l.get(key(6)).unwrap().is_some(),
            "live payloads survive the explicit compact"
        );
    }

    #[test]
    fn recency_after_reopen_puts_new_entries_after_old_ones() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Larder::open(dir.path(), cfg(30)).unwrap();
        l.put(key(1), &[1; 10]).unwrap();
        l.put(key(2), &[2; 10]).unwrap();
        drop(l);
        let mut l = Larder::open(dir.path(), cfg(30)).unwrap();
        let max_seq: i64 = l
            .conn
            .query_row("SELECT MAX(seq) FROM entry", [], |r| r.get(0))
            .unwrap();
        assert!(
            l.next_seq > max_seq,
            "next_seq must resume past every stored seq"
        );
        l.put(key(3), &[3; 10]).unwrap();
        l.put(key(4), &[4; 10]).unwrap(); // evicts 1, the oldest -- not the fresh 3
        assert!(l.get(key(1)).unwrap().is_none());
        assert!(l.get(key(2)).unwrap().is_some());
        assert!(l.get(key(3)).unwrap().is_some());
    }

    #[test]
    fn resync_after_a_rolled_back_put_restores_next_seq_to_the_committed_mark() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Larder::open(dir.path(), cfg(1000)).unwrap();
        l.put(key(1), &[1; 10]).unwrap();
        let committed_mark = l.next_seq.max(256); // seq 0 persisted a mark of 256
        l.conn.execute_batch("BEGIN").unwrap();
        l.next_seq = 256; // next bump crosses a boundary and writes mark 512 inside the tx
        l.bump_seq().unwrap();
        l.conn.execute_batch("ROLLBACK").unwrap();
        l.resync_counters();
        assert_eq!(l.next_seq, committed_mark);
        drop(l);
        let l = Larder::open(dir.path(), cfg(1000)).unwrap();
        let max_seq: i64 = l
            .conn
            .query_row("SELECT MAX(seq) FROM entry", [], |r| r.get(0))
            .unwrap();
        assert!(l.next_seq > max_seq);
    }

    #[test]
    fn a_second_instance_on_the_same_dir_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let _first = Larder::open(dir.path(), cfg(100)).unwrap();
        assert!(Larder::open(dir.path(), cfg(100)).is_err());
    }

    #[test]
    fn purging_one_asset_leaves_dead_bytes_until_compact() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Larder::open(dir.path(), cfg(1000)).unwrap();
        l.put(key(1), &[1; 40]).unwrap();
        l.put(key(2), &[2; 40]).unwrap();
        l.purge_asset(1).unwrap();
        assert_eq!(l.stats().unwrap().file_bytes, 80, "no rewrite per purge");
        l.compact().unwrap();
        assert_eq!(l.stats().unwrap().file_bytes, 40);
        assert_eq!(l.get(key(2)).unwrap(), Some(vec![2; 40]));
    }

    #[test]
    fn purge_tier_reclaims_disk_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Larder::open(dir.path(), cfg(1000)).unwrap();
        l.put(key(1), &[1; 40]).unwrap();
        l.put(key(2), &[2; 40]).unwrap();
        l.purge_tier(LarderTier::T2).unwrap();
        assert_eq!(l.stats().unwrap().file_bytes, 0);
    }

    #[test]
    fn purging_an_asset_compacts_once_dead_bytes_cross_the_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Larder::open(
            dir.path(),
            LarderConfig {
                cap_bytes: 1000,
                compact_min_dead_bytes: 10,
            },
        )
        .unwrap();
        l.put(key(1), &[1; 90]).unwrap();
        l.put(key(2), &[2; 10]).unwrap();
        l.purge_asset(1).unwrap(); // dead 90 > floor 10 and > live 10
        assert_eq!(l.stats().unwrap().file_bytes, 10);
        assert_eq!(l.get(key(2)).unwrap(), Some(vec![2; 10]));
    }

    #[test]
    fn a_put_after_a_torn_write_lands_at_the_offset_it_records() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Larder::open(dir.path(), cfg(1000)).unwrap();
        l.put(key(1), &[1; 10]).unwrap();
        // Simulate a torn write: bytes on disk past `file_len` that no index row covers.
        write_all_at(&l.pack, &[9; 7], l.file_len).unwrap();
        l.pack.set_len(l.file_len).unwrap();
        l.put(key(2), &[2; 10]).unwrap();
        assert_eq!(l.get(key(2)).unwrap(), Some(vec![2; 10]));
        assert_eq!(l.get(key(1)).unwrap(), Some(vec![1; 10]));
    }

    #[test]
    fn open_removes_orphaned_pack_generations() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Larder::open(dir.path(), cfg(100)).unwrap();
        l.put(key(1), b"x").unwrap();
        drop(l);
        std::fs::write(
            pack_path(dir.path(), 7),
            b"orphan from a crashed compaction",
        )
        .unwrap();
        let mut l = Larder::open(dir.path(), cfg(100)).unwrap();
        assert!(!pack_path(dir.path(), 7).exists());
        assert!(l.get(key(1)).unwrap().is_some());
    }

    fn kkey<'a>(asset_id: i64, key: &'a [u8]) -> LarderKeyed<'a> {
        LarderKeyed {
            kind: LarderKind::AiAlpha,
            asset_id,
            key,
        }
    }

    #[test]
    fn keyed_put_get_round_trips_and_many_keys_share_one_asset() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Larder::open(dir.path(), cfg(1000)).unwrap();
        assert!(l.put_keyed(kkey(1, b"a"), b"alpha-a").unwrap());
        assert!(l.put_keyed(kkey(1, b"b"), b"alpha-b").unwrap());
        assert!(
            l.put(key(1), b"t2").unwrap(),
            "the (asset, tier) entry coexists"
        );
        assert_eq!(
            l.get_keyed(kkey(1, b"a")).unwrap().as_deref(),
            Some(&b"alpha-a"[..])
        );
        assert_eq!(
            l.get_keyed(kkey(1, b"b")).unwrap().as_deref(),
            Some(&b"alpha-b"[..])
        );
        assert_eq!(l.get_keyed(kkey(1, b"c")).unwrap(), None);
        assert!(l.contains_keyed(kkey(1, b"a")).unwrap());
        assert!(!l.contains_keyed(kkey(1, b"c")).unwrap());
        assert_eq!(l.get(key(1)).unwrap().as_deref(), Some(&b"t2"[..]));
        let stats = l.stats().unwrap();
        assert_eq!(stats.entry_count, 3);
        assert_eq!(stats.live_bytes, (7 + 7 + 2) as u64);
    }

    #[test]
    fn re_putting_a_keyed_entry_replaces_it_without_leaking_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Larder::open(dir.path(), cfg(1000)).unwrap();
        l.put_keyed(kkey(1, b"a"), &[1; 10]).unwrap();
        l.put_keyed(kkey(1, b"a"), &[2; 4]).unwrap();
        assert_eq!(l.get_keyed(kkey(1, b"a")).unwrap(), Some(vec![2; 4]));
        let stats = l.stats().unwrap();
        assert_eq!((stats.entry_count, stats.live_bytes), (1, 4));
    }

    #[test]
    fn eviction_orders_keyed_and_tiered_entries_against_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Larder::open(dir.path(), cfg(30)).unwrap();
        l.put(key(1), &[1; 10]).unwrap(); // oldest
        l.put_keyed(kkey(2, b"k"), &[2; 10]).unwrap();
        l.put(key(3), &[3; 10]).unwrap();
        // Touch the oldest tiered entry so the keyed one becomes the LRU victim.
        assert!(l.get(key(1)).unwrap().is_some());
        l.put(key(4), &[4; 10]).unwrap();
        assert!(
            l.get_keyed(kkey(2, b"k")).unwrap().is_none(),
            "keyed was the LRU entry"
        );
        assert!(l.get(key(1)).unwrap().is_some());
        assert!(l.get(key(3)).unwrap().is_some());
        // And the other way round: a keyed put evicts the oldest tiered entry.
        l.put_keyed(kkey(5, b"n"), &[5; 10]).unwrap();
        assert!(l.get(key(4)).unwrap().is_none(), "4 was the LRU entry");
        assert!(l.stats().unwrap().live_bytes <= 30);
    }

    #[test]
    fn a_keyed_payload_larger_than_the_cap_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Larder::open(dir.path(), cfg(8)).unwrap();
        assert!(!l.put_keyed(kkey(1, b"a"), &[0; 9]).unwrap());
        assert_eq!(l.stats().unwrap().entry_count, 0);
    }

    #[test]
    fn compaction_keeps_keyed_entries_readable() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Larder::open(dir.path(), cfg(1000)).unwrap();
        l.put_keyed(kkey(1, b"a"), &[1; 50]).unwrap();
        l.put(key(2), &[2; 50]).unwrap();
        l.put_keyed(kkey(3, b"b"), &[3; 50]).unwrap();
        l.purge_asset(2).unwrap(); // leaves a dead hole in the middle
        l.compact().unwrap();
        assert_eq!(l.stats().unwrap().file_bytes, 100);
        assert_eq!(l.get_keyed(kkey(1, b"a")).unwrap(), Some(vec![1; 50]));
        assert_eq!(l.get_keyed(kkey(3, b"b")).unwrap(), Some(vec![3; 50]));
    }

    #[test]
    fn purge_asset_drops_keyed_rows_and_purge_kind_only_keyed() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Larder::open(dir.path(), cfg(1000)).unwrap();
        l.put_keyed(kkey(1, b"a"), &[1; 10]).unwrap();
        l.put_keyed(kkey(2, b"b"), &[2; 10]).unwrap();
        l.put(key(1), &[9; 5]).unwrap();
        assert_eq!(l.purge_asset(1).unwrap(), 15);
        assert!(!l.contains_keyed(kkey(1, b"a")).unwrap());
        assert!(l.get(key(1)).unwrap().is_none());
        assert!(l.contains_keyed(kkey(2, b"b")).unwrap());
        l.put(key(3), &[3; 5]).unwrap();
        assert_eq!(l.purge_kind(LarderKind::AiAlpha).unwrap(), 10);
        assert!(
            l.get(key(3)).unwrap().is_some(),
            "tiered entries survive purge_kind"
        );
        assert_eq!(l.stats().unwrap().live_bytes, 5);
    }

    #[test]
    fn forget_keyed_drops_one_entry_and_frees_its_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Larder::open(dir.path(), cfg(1000)).unwrap();
        l.put_keyed(kkey(1, b"a"), &[1; 10]).unwrap();
        l.put_keyed(kkey(1, b"b"), &[2; 6]).unwrap();
        l.forget_keyed(kkey(1, b"a")).unwrap();
        l.forget_keyed(kkey(1, b"never-stored")).unwrap(); // a no-op, not an error
        assert!(!l.contains_keyed(kkey(1, b"a")).unwrap());
        assert!(l.contains_keyed(kkey(1, b"b")).unwrap());
        assert_eq!(l.stats().unwrap().live_bytes, 6);
    }

    #[test]
    fn purge_all_clears_keyed_entries_too() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Larder::open(dir.path(), cfg(1000)).unwrap();
        l.put_keyed(kkey(1, b"a"), &[1; 10]).unwrap();
        assert_eq!(l.purge_all().unwrap(), 10);
        assert!(!l.contains_keyed(kkey(1, b"a")).unwrap());
        assert_eq!(l.stats().unwrap().entry_count, 0);
    }

    #[test]
    fn a_corrupt_keyed_payload_is_a_miss_and_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Larder::open(dir.path(), cfg(1000)).unwrap();
        l.put_keyed(kkey(1, b"a"), &[7; 10]).unwrap();
        write_all_at(&l.pack, &[0xFF; 10], 0).unwrap();
        assert_eq!(l.get_keyed(kkey(1, b"a")).unwrap(), None);
        assert!(!l.contains_keyed(kkey(1, b"a")).unwrap());
        assert_eq!(l.stats().unwrap().live_bytes, 0);
    }

    #[test]
    fn keyed_entries_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Larder::open(dir.path(), cfg(1000)).unwrap();
        l.put_keyed(kkey(1, b"a"), &[4; 12]).unwrap();
        drop(l);
        let mut l = Larder::open(dir.path(), cfg(1000)).unwrap();
        assert_eq!(l.get_keyed(kkey(1, b"a")).unwrap(), Some(vec![4; 12]));
        assert_eq!(l.stats().unwrap().live_bytes, 12);
    }
}
