//! `fjall` (pure-Rust, LSM-tree-based embedded KV store) backend, added for #116. Like LMDB
//! (`lmdb.rs`) and `redb` (`redb_engine.rs`), fjall has no query planner — every query pattern
//! needs its own hand-maintained secondary index, kept in sync on every write. This backend
//! reuses `redb_engine.rs`'s design almost exactly: byte-encoded composite keys
//! (`prefix || 0x00 || big-endian id`) in dedicated keyspaces, one per indexed dimension, so a
//! prefix scan is "iterate a byte range starting at the prefix." Unlike LMDB/redb, fjall's own
//! top-level API ships a native `prefix()` iterator (not just `range()`), so this backend doesn't
//! need `redb_engine.rs`'s manual "does this key still start with the prefix" loop.
//!
//! fjall 3.x's own terminology: a `Database` holds one or more `Keyspace`s (each its own physical
//! LSM-tree, analogous to LMDB's named `Database`/redb's `Table`/SQL's table); a `Snapshot`
//! (`db.snapshot()`) gives a consistent cross-keyspace read view, analogous to `heed`'s `RwTxn`/
//! `redb`'s `ReadTransaction`. This module uses one `Snapshot` per read method, mirroring
//! `lmdb.rs`/`redb_engine.rs`'s "one read-txn per call" shape, even though this spike never
//! exercises a concurrent writer (see ADR-0008's own scope note) so cross-keyspace consistency is
//! never actually put under test here.
//!
//! **Real, structural finding, not a workaround**: fjall's atomic cross-keyspace write primitive
//! (`Database::batch()` / `OwnedWriteBatch`) stages every item in a plain in-process `Vec` and
//! only touches the journal at all inside `.commit()` — unlike `heed`'s `RwTxn` or `redb`'s
//! `WriteTransaction`, which both apply writes to the store's own on-disk/mmap structures as part
//! of the transaction, before commit. This has two consequences worth being explicit about rather
//! than silently inherited from the `lmdb.rs`/`redb_engine.rs` template:
//!
//! 1. **No read-your-own-writes within one batch.** A read against a `Keyspace`/`Snapshot` only
//!    ever sees the last *committed* state — it can never see an uncommitted item already pushed
//!    into an open `OwnedWriteBatch`. `rate_burst`'s per-item read-modify-write loop below reads
//!    each asset's current row from the (uncommitted-batch-blind) keyspace before staging its
//!    update into the batch; this only produces the same answer as LMDB's/redb's open-write-txn
//!    version (which *can* read its own prior writes in the same txn) as long as no id repeats
//!    within one burst, which holds for this benchmark's inputs but would silently diverge if it
//!    didn't. A real `nicti-catalog` implementation built on fjall would need to route around
//!    this (e.g. an in-memory overlay of pending writes) if it ever needs read-your-own-writes
//!    inside one logical transaction.
//! 2. **`crash_mid_ingest`'s "leave a genuinely open, uncommitted transaction, then `mem::forget`
//!    it" methodology (the identical contract `redb_engine.rs`'s doc comment describes, and the
//!    one every prior ADR in this series measures crash-safety against) cannot leave any on-disk
//!    trace at all for fjall**, precisely because nothing reaches the journal pre-commit. This
//!    isn't a test that was skipped or weakened — it's the honest, structural answer to the same
//!    question every other engine's crash test asks: see this module's `crash_mid_ingest` and the
//!    ADR's crash-safety section for what this does and doesn't prove.
//!
//! **Backup**: fjall has no online-backup call analogous to SQLite's `VACUUM INTO`/DuckDB's
//! `EXPORT DATABASE`/LMDB's `env.copy_to_path`. Unlike `redb_engine.rs`'s bare `std::fs::copy` of
//! a single file (redb is one file), fjall's on-disk layout is a whole directory (a journal plus
//! one subfolder per keyspace), so `backup()` below recursively copies that directory while
//! holding a `db.snapshot()` open across the copy. Per fjall's own documented guarantee ("old data
//! will not be dropped until it is not referenced by any active snapshot"), this is a *stronger*
//! claim than `redb_engine.rs` could honestly make about its own bare-copy backup (that module's
//! doc comment explicitly says its held-open read transaction is "a real, if partial, safety
//! property" with no upstream guarantee behind it) — but it is still not a purpose-built,
//! coordinated-with-the-commit-boundary backup API, and this spike never exercises a concurrent
//! writer during backup() for any engine (see ADR-0008's "two honest scope limits" note), so this
//! gap doesn't affect any number measured here.
//!
//! **Background threads**: `Database::open` starts a small worker-thread pool (compaction/flush,
//! `min(cores, 4)` by default) that's joined cleanly on `Drop`. `den crash`'s `mem::forget`
//! technique skips `Drop` entirely, so — unlike every other candidate in this series, all of which
//! are either purely synchronous library calls (SQLite/DuckDB/LMDB/redb) or wrap their own async
//! runtime in a way `prepare_for_forget` can shut down cleanly first (Turso/libSQL) — fjall has no
//! public API to stop its worker pool independently of a full graceful close, so
//! `prepare_for_forget` stays the trait's default no-op here and each crash-loop iteration leaks
//! that iteration's worker threads for the rest of the test process's life. This has no bearing on
//! the crash-safety *result* itself (those threads only ever touch already-committed data via
//! background compaction/flush, and reopening replays the journal regardless of whether a
//! background flush ran), but it is a real, fjall-specific spike-tooling cost worth naming
//! explicitly, per this file's own standing rule that a clean-looking pass and a gap in the
//! methodology should never look identical to a later reader.

use crate::gen::{Asset, Flag};
use crate::workload::{FacetCounts, RangeQuery, Workload};
use fjall::{
    Database, Keyspace, KeyspaceCreateOptions, OwnedWriteBatch, PersistMode, Readable, Snapshot,
};
use std::path::{Path, PathBuf};

pub struct FjallEngine {
    db: Database,
    assets: Keyspace,
    by_date: Keyspace,
    by_folder: Keyspace,
    by_keyword: Keyspace,
    by_model: Keyspace,
    by_rating: Keyspace,
    path: PathBuf,
    /// Stashed by `crash_mid_ingest` so the caller's later `mem::forget(engine)` drops this
    /// object too — see this module's doc comment for why, unlike `redb_engine.rs`'s
    /// `pending_crash_txn`, this never actually leaves anything on disk to be interrupted.
    pending_crash_batch: Option<OwnedWriteBatch>,
}

fn id_key(id: u64) -> [u8; 8] {
    id.to_be_bytes()
}

fn composite_key(prefix: &[u8], id: u64) -> Vec<u8> {
    let mut k = Vec::with_capacity(prefix.len() + 1 + 8);
    k.extend_from_slice(prefix);
    k.push(0); // NUL separator: none of this generator's strings contain a NUL byte.
    k.extend_from_slice(&id_key(id));
    k
}

fn id_from_bytes(b: &[u8]) -> u64 {
    u64::from_be_bytes(b.try_into().expect("id value must be 8 bytes"))
}

fn flag_str(f: Flag) -> &'static str {
    match f {
        Flag::None => "none",
        Flag::Pick => "pick",
        Flag::Reject => "reject",
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct StoredAsset {
    folder_path: String,
    filename: String,
    capture_date: String,
    model: String,
    iso: u32,
    rating: u8,
    flag: String,
}

impl From<&Asset> for StoredAsset {
    fn from(a: &Asset) -> Self {
        Self {
            folder_path: a.folder_path.clone(),
            filename: a.filename.clone(),
            capture_date: a.capture_date.clone(),
            model: a.model.clone(),
            iso: a.iso,
            rating: a.rating,
            flag: flag_str(a.flag).to_string(),
        }
    }
}

/// Stages one asset's row plus every secondary-index entry into an open batch. Shared by
/// `bulk_ingest` and `crash_mid_ingest` so the two can't drift, same reasoning as
/// `redb_engine.rs::insert_asset`.
#[allow(clippy::too_many_arguments)]
fn stage_asset(
    batch: &mut OwnedWriteBatch,
    assets: &Keyspace,
    by_date: &Keyspace,
    by_folder: &Keyspace,
    by_keyword: &Keyspace,
    by_model: &Keyspace,
    by_rating: &Keyspace,
    a: &Asset,
) -> anyhow::Result<()> {
    let stored: StoredAsset = a.into();
    let bytes = bincode::serialize(&stored)?;
    let idk = id_key(a.id);
    batch.insert(assets, idk.as_slice(), bytes.as_slice());
    batch.insert(
        by_date,
        composite_key(a.capture_date.as_bytes(), a.id).as_slice(),
        idk.as_slice(),
    );
    batch.insert(
        by_folder,
        composite_key(a.folder_path.as_bytes(), a.id).as_slice(),
        idk.as_slice(),
    );
    batch.insert(
        by_model,
        composite_key(a.model.as_bytes(), a.id).as_slice(),
        idk.as_slice(),
    );
    batch.insert(
        by_rating,
        composite_key(&[a.rating], a.id).as_slice(),
        idk.as_slice(),
    );
    for kw in &a.keywords {
        batch.insert(
            by_keyword,
            composite_key(kw.as_bytes(), a.id).as_slice(),
            idk.as_slice(),
        );
    }
    Ok(())
}

/// Collects every id under `prefix` in `keyspace`, using fjall's own native prefix iterator
/// (unlike `redb_engine.rs::prefix_ids_owned`, no manual "does this key still start with the
/// prefix" check is needed — see this module's doc comment).
fn prefix_ids(snap: &Snapshot, keyspace: &Keyspace, prefix: &[u8]) -> anyhow::Result<Vec<u64>> {
    let mut out = Vec::new();
    for guard in snap.prefix(keyspace, prefix) {
        let (_, v) = guard.into_inner()?;
        out.push(id_from_bytes(v.as_ref()));
    }
    Ok(out)
}

impl Workload for FjallEngine {
    fn open(path: &Path) -> anyhow::Result<Self> {
        let db = Database::builder(path).open()?;
        let assets = db.keyspace("assets", KeyspaceCreateOptions::default)?;
        let by_date = db.keyspace("by_date", KeyspaceCreateOptions::default)?;
        let by_folder = db.keyspace("by_folder", KeyspaceCreateOptions::default)?;
        let by_keyword = db.keyspace("by_keyword", KeyspaceCreateOptions::default)?;
        let by_model = db.keyspace("by_model", KeyspaceCreateOptions::default)?;
        let by_rating = db.keyspace("by_rating", KeyspaceCreateOptions::default)?;
        Ok(Self {
            db,
            assets,
            by_date,
            by_folder,
            by_keyword,
            by_model,
            by_rating,
            path: path.to_path_buf(),
            pending_crash_batch: None,
        })
    }

    fn bulk_ingest(&mut self, assets: &[Asset]) -> anyhow::Result<()> {
        let mut batch = self.db.batch();
        for a in assets {
            stage_asset(
                &mut batch,
                &self.assets,
                &self.by_date,
                &self.by_folder,
                &self.by_keyword,
                &self.by_model,
                &self.by_rating,
                a,
            )?;
        }
        batch.commit()?;
        Ok(())
    }

    fn crash_mid_ingest(&mut self, assets: &[Asset]) -> anyhow::Result<()> {
        let half = assets.len() / 2;
        let mut batch = self.db.batch();
        for a in &assets[..half] {
            stage_asset(
                &mut batch,
                &self.assets,
                &self.by_date,
                &self.by_folder,
                &self.by_keyword,
                &self.by_model,
                &self.by_rating,
                a,
            )?;
        }
        // Deliberately never committed — see this module's doc comment for why, unlike every
        // other engine in this series, this leaves *nothing* on disk to be interrupted at all
        // (fjall's write batch stages purely in-process memory until `.commit()`).
        self.pending_crash_batch = Some(batch);
        Ok(())
    }

    fn write_rating(&mut self, asset_id: u64, rating: u8) -> anyhow::Result<()> {
        let idk = id_key(asset_id);
        let raw = self
            .assets
            .get(idk.as_slice())?
            .ok_or_else(|| anyhow::anyhow!("asset {asset_id} not found"))?;
        let mut stored: StoredAsset = bincode::deserialize(raw.as_ref())?;
        let old_rating = stored.rating;
        stored.rating = rating;
        let bytes = bincode::serialize(&stored)?;

        let mut batch = self.db.batch();
        batch.remove(
            &self.by_rating,
            composite_key(&[old_rating], asset_id).as_slice(),
        );
        batch.insert(&self.assets, idk.as_slice(), bytes.as_slice());
        batch.insert(
            &self.by_rating,
            composite_key(&[rating], asset_id).as_slice(),
            idk.as_slice(),
        );
        batch.commit()?;
        Ok(())
    }

    fn rate_burst(&mut self, updates: &[(u64, u8)]) -> anyhow::Result<()> {
        // Each read below sees only the last *committed* state, not any earlier update already
        // staged into `batch` this same call. If `updates` repeats an asset_id, that would leave a
        // stale `by_rating` entry for an intermediate rating (found by CodeRabbit's CLI review,
        // same class of bug already fixed in rocksdb_engine.rs's rate_burst). Tracks each asset's
        // rating as staged so far *within this batch* to avoid it, matching that fix.
        let mut batch = self.db.batch();
        let mut staged_rating: std::collections::HashMap<u64, u8> =
            std::collections::HashMap::new();
        for (asset_id, rating) in updates {
            let idk = id_key(*asset_id);
            let raw = self
                .assets
                .get(idk.as_slice())?
                .ok_or_else(|| anyhow::anyhow!("asset {asset_id} not found"))?;
            let mut stored: StoredAsset = bincode::deserialize(raw.as_ref())?;
            let old_rating = staged_rating
                .get(asset_id)
                .copied()
                .unwrap_or(stored.rating);
            stored.rating = *rating;
            let bytes = bincode::serialize(&stored)?;

            batch.remove(
                &self.by_rating,
                composite_key(&[old_rating], *asset_id).as_slice(),
            );
            batch.insert(&self.assets, idk.as_slice(), bytes.as_slice());
            batch.insert(
                &self.by_rating,
                composite_key(&[*rating], *asset_id).as_slice(),
                idk.as_slice(),
            );
            staged_rating.insert(*asset_id, *rating);
        }
        batch.commit()?;
        Ok(())
    }

    fn tag_keyword(&mut self, asset_ids: &[u64], keyword: &str) -> anyhow::Result<()> {
        let mut batch = self.db.batch();
        for id in asset_ids {
            batch.insert(
                &self.by_keyword,
                composite_key(keyword.as_bytes(), *id).as_slice(),
                id_key(*id).as_slice(),
            );
        }
        batch.commit()?;
        Ok(())
    }

    fn faceted_filter(
        &self,
        model: Option<&str>,
        min_rating: Option<u8>,
        keyword_prefix: Option<&str>,
    ) -> anyhow::Result<FacetCounts> {
        let snap = self.db.snapshot();

        // Start from whichever index is most selective: a keyword prefix if given, else model —
        // same choice `lmdb.rs`/`redb_engine.rs::faceted_filter` make, for the same reason.
        let mut candidate_ids: Vec<u64> = if let Some(kw) = keyword_prefix {
            prefix_ids(&snap, &self.by_keyword, kw.as_bytes())?
        } else if let Some(m) = model {
            prefix_ids(&snap, &self.by_model, m.as_bytes())?
        } else {
            let mut ids = Vec::new();
            for guard in snap.iter(&self.assets) {
                let (k, _) = guard.into_inner()?;
                ids.push(id_from_bytes(k.as_ref()));
            }
            ids
        };
        candidate_ids.sort_unstable();
        candidate_ids.dedup();

        let mut counts = FacetCounts::default();
        let mut by_model = std::collections::HashMap::new();
        let mut by_rating = std::collections::HashMap::new();
        for id in candidate_ids {
            let raw = match snap.get(&self.assets, id_key(id).as_slice())? {
                Some(r) => r,
                None => continue,
            };
            let stored: StoredAsset = bincode::deserialize(raw.as_ref())?;
            if let Some(m) = model {
                if stored.model != m {
                    continue;
                }
            }
            if let Some(min_r) = min_rating {
                if stored.rating < min_r {
                    continue;
                }
            }
            *by_model.entry(stored.model.clone()).or_insert(0u64) += 1;
            *by_rating.entry(stored.rating).or_insert(0u64) += 1;
            counts.total += 1;
        }
        counts.by_model = by_model.into_iter().collect();
        counts.by_rating = by_rating.into_iter().collect();
        Ok(counts)
    }

    fn sort_by_date_page(&self, offset: u64, limit: u64) -> anyhow::Result<Vec<u64>> {
        let snap = self.db.snapshot();
        // Reverse the range cursor directly (newest date first) rather than materializing and
        // reversing every row — same lesson `lmdb.rs`/`redb_engine.rs::sort_by_date_page`'s own
        // comments document: doing it the naive way dominated the whole benchmark at 600k.
        let mut ids = Vec::with_capacity(limit as usize);
        for guard in snap
            .range::<&[u8], _>(&self.by_date, ..)
            .rev()
            .skip(offset as usize)
            .take(limit as usize)
        {
            let (_, v) = guard.into_inner()?;
            ids.push(id_from_bytes(v.as_ref()));
        }
        Ok(ids)
    }

    fn folder_subtree_count(&self, folder_prefix: &str) -> anyhow::Result<u64> {
        let snap = self.db.snapshot();
        Ok(prefix_ids(&snap, &self.by_folder, folder_prefix.as_bytes())?.len() as u64)
    }

    fn keyword_subtree_query(&self, keyword_prefix: &str) -> anyhow::Result<Vec<u64>> {
        let snap = self.db.snapshot();
        let mut ids = prefix_ids(&snap, &self.by_keyword, keyword_prefix.as_bytes())?;
        ids.sort_unstable();
        ids.dedup();
        Ok(ids)
    }

    fn range_query(&self, q: &RangeQuery) -> anyhow::Result<Vec<u64>> {
        let snap = self.db.snapshot();
        // Indexed on rating alone (the most selective of the three dimensions given the
        // generator's 2-10% keep rate); iso/date are post-filtered in application code — same
        // honest shape as `lmdb.rs`/`redb_engine.rs::range_query`, for the same reason.
        let lo = composite_key(&[q.min_rating], 0);
        let hi = composite_key(&[q.max_rating], u64::MAX);
        let mut out = Vec::new();
        for guard in snap.range::<&[u8], _>(&self.by_rating, lo.as_slice()..=hi.as_slice()) {
            let (_, v) = guard.into_inner()?;
            let id = id_from_bytes(v.as_ref());
            let raw = match snap.get(&self.assets, id_key(id).as_slice())? {
                Some(r) => r,
                None => continue,
            };
            let stored: StoredAsset = bincode::deserialize(raw.as_ref())?;
            if stored.iso >= q.min_iso
                && stored.iso <= q.max_iso
                && stored.capture_date.as_str() >= q.date_from.as_str()
                && stored.capture_date.as_str() <= q.date_to.as_str()
            {
                out.push(id);
            }
        }
        Ok(out)
    }

    fn filename_search(&self, substr: &str) -> anyhow::Result<Vec<u64>> {
        let snap = self.db.snapshot();
        let mut out = Vec::new();
        for guard in snap.iter(&self.assets) {
            let (k, v) = guard.into_inner()?;
            let stored: StoredAsset = bincode::deserialize(v.as_ref())?;
            if stored.filename.contains(substr) {
                out.push(id_from_bytes(k.as_ref()));
            }
        }
        Ok(out)
    }

    fn backup(&self, dest: &Path) -> anyhow::Result<()> {
        // Reject a dest that is the source directory itself or nested inside it -- copying a
        // directory into itself would recurse indefinitely / corrupt the source. Not reachable by
        // this crate's own benchmark call site (always a sibling path), but a real hazard for any
        // other caller of this spike code, found by CodeRabbit's CLI review.
        let src_canon = self.path.canonicalize()?;
        if dest.exists() {
            let dest_canon = dest.canonicalize()?;
            anyhow::ensure!(
                dest_canon != src_canon && !dest_canon.starts_with(&src_canon),
                "backup destination {dest_canon:?} must not be the source directory or nested inside it"
            );
        } else if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
            let parent_canon = parent.canonicalize()?;
            anyhow::ensure!(
                parent_canon != src_canon && !parent_canon.starts_with(&src_canon),
                "backup destination {dest:?} must not be nested inside the source directory"
            );
        }

        std::fs::create_dir_all(dest)?;
        // Pins the current state so segment/journal files this snapshot depends on aren't
        // reclaimed mid-copy — see this module's doc comment for why this is a real, documented
        // fjall guarantee, stronger than `redb_engine.rs`'s equivalent caveat, but still not a
        // purpose-built, commit-boundary-coordinated backup call.
        let _snapshot = self.db.snapshot();
        // Flush the in-memory-buffered journal to disk *before* the raw file copy below -- without
        // this, recently committed writes that haven't reached disk yet would be silently missing
        // from the backup, since `copy_dir_recursive` only sees what's already on the filesystem.
        // Found by CodeRabbit's CLI review; `snapshot()` alone pins existing on-disk state, it
        // doesn't force pending journal data to disk.
        self.db.persist(PersistMode::SyncAll)?;
        copy_dir_recursive(&self.path, dest)?;
        Ok(())
    }

    fn integrity_check(&self) -> anyhow::Result<bool> {
        for guard in self.assets.iter() {
            let (_, v) = guard.into_inner()?;
            let _: StoredAsset = bincode::deserialize(v.as_ref())?;
        }
        Ok(true)
    }
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let target = dst.join(entry.file_name());
        if path.is_dir() {
            std::fs::create_dir_all(&target)?;
            copy_dir_recursive(&path, &target)?;
        } else {
            std::fs::copy(&path, &target)?;
        }
    }
    Ok(())
}

impl FjallEngine {
    pub fn reopen(&self) -> anyhow::Result<Self> {
        Self::open(&self.path)
    }
}
