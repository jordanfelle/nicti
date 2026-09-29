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
}

impl LarderTier {
    pub fn as_str(self) -> &'static str {
        match self {
            LarderTier::T2 => "t2",
        }
    }
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
    /// Held for the life of the `Larder`: the in-memory counters assume a single owner.
    _lock: File,
}

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
             CREATE INDEX IF NOT EXISTS idx_entry_seq ON entry(seq);",
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
        let live_bytes: i64 =
            conn.query_row("SELECT COALESCE(SUM(len), 0) FROM entry", [], |r| r.get(0))?;

        let mut larder = Self {
            dir: dir.to_path_buf(),
            cfg,
            conn,
            pack,
            generation,
            file_len,
            live_bytes: live_bytes as u64,
            next_seq,
            _lock: lock,
        };
        // A smaller cap than the previous session's takes effect immediately.
        larder.evict_to_fit(0)?;
        Ok(larder)
    }

    pub fn stats(&self) -> Result<LarderStats, CatalogError> {
        let entry_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM entry", [], |r| r.get(0))?;
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
        if bytes.len() as u64 > self.cfg.cap_bytes {
            return Ok(false);
        }
        self.conn.execute_batch("BEGIN")?;
        let stored = self.put_in_tx(key, bytes);
        match stored {
            Ok(()) => self.conn.execute_batch("COMMIT")?,
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

    fn put_in_tx(&mut self, key: LarderKey<'_>, bytes: &[u8]) -> Result<(), CatalogError> {
        let len = bytes.len() as u64;
        self.remove_entry(key.asset_id, key.tier.as_str())?;
        self.evict_to_fit(len)?;

        let offset = self.file_len;
        if let Err(e) = write_all_at(&self.pack, bytes, offset) {
            // Drop any torn tail so the next write can't leave a gap or overlap.
            let _ = self.pack.set_len(offset);
            return Err(io_err(e));
        }
        self.file_len += len;
        let seq = self.bump_seq()?;
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
            .query_row("SELECT COALESCE(SUM(len), 0) FROM entry", [], |r| {
                r.get::<_, i64>(0)
            })
        {
            self.live_bytes = live as u64;
        }
        if let Ok(meta) = self.pack.metadata() {
            self.file_len = meta.len();
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

    /// Manual purge: drops every cached payload for one asset (e.g. after its source file was
    /// replaced). Returns the live bytes freed.
    pub fn purge_asset(&mut self, asset_id: i64) -> Result<u64, CatalogError> {
        self.purge_where("asset_id = ?1", asset_id.into())
    }

    /// Manual purge: drops every payload of one tier. Returns the live bytes freed.
    pub fn purge_tier(&mut self, tier: LarderTier) -> Result<u64, CatalogError> {
        self.purge_where("tier = ?1", tier.as_str().to_string().into())
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
        let mut moved = Vec::with_capacity(rows.len());
        let mut new_len = 0u64;
        for (asset_id, tier, offset, len) in rows {
            let mut buf = vec![0u8; len as usize];
            read_exact_at(&self.pack, &mut buf, offset as u64).map_err(io_err)?;
            write_all_at(&new_pack, &buf, new_len).map_err(io_err)?;
            moved.push((asset_id, tier, new_len as i64));
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

    fn compact_if_worthwhile(&mut self) -> Result<(), CatalogError> {
        let dead = self.file_len.saturating_sub(self.live_bytes);
        if dead > self.cfg.compact_min_dead_bytes && dead > self.live_bytes {
            self.compact()?;
        }
        Ok(())
    }

    fn purge_where(
        &mut self,
        predicate: &str,
        arg: rusqlite::types::Value,
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
        // A manual purge is asked to give space back, so reclaim it now instead of waiting for
        // the dead-byte floor. Best-effort: the rows are already gone either way.
        if freed > 0 {
            let _ = self.compact();
        }
        Ok(freed as u64)
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
            let victim: Option<(i64, String)> = self
                .conn
                .query_row(
                    "SELECT asset_id, tier FROM entry ORDER BY seq LIMIT 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let Some((asset_id, tier)) = victim else {
                break;
            };
            self.remove_entry(asset_id, &tier)?;
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
    fn a_second_instance_on_the_same_dir_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let _first = Larder::open(dir.path(), cfg(100)).unwrap();
        assert!(Larder::open(dir.path(), cfg(100)).is_err());
    }

    #[test]
    fn manual_purge_of_one_asset_reclaims_disk_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Larder::open(dir.path(), cfg(1000)).unwrap();
        l.put(key(1), &[1; 40]).unwrap();
        l.put(key(2), &[2; 40]).unwrap();
        l.purge_asset(1).unwrap();
        assert_eq!(l.stats().unwrap().file_bytes, 40);
        assert_eq!(l.get(key(2)).unwrap(), Some(vec![2; 40]));
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
}
