//! RocksDB (via the `rocksdb` crate, `rust-rocksdb/rust-rocksdb`) backend, added for #115/
//! ADR-0008's follow-up. Like LMDB (`lmdb.rs`) and redb (`redb_engine.rs`), RocksDB has no query
//! planner — every query pattern needs its own hand-maintained secondary index, kept in sync on
//! every write. This backend follows `lmdb.rs`'s exact indexing shape (one column family per
//! indexed dimension, byte-encoded composite keys `prefix || 0x00 || big-endian id`, most-selective
//! index chosen per query, the rest post-filtered in application code) rather than inventing a
//! different one, per #115's own stated requirement that this be an apples-to-apples comparison,
//! not an artificially advantaged or disadvantaged one.
//!
//! Two real differences from `lmdb.rs`/`redb_engine.rs`, both upstream API shape:
//!
//! - RocksDB **does** take an OS-level `LOCK` file when opening a DB directory (a real, by-design
//!   protection against two processes/handles pointing at the same directory) — confirmed by this
//!   spike's own crash test, not assumed: see this ADR's crash-safety section for the measured
//!   result and a direct probe that pins down its scope (per-directory/leaked-fd, like
//!   redb/SQLite's own lock — **not** a process-wide open-environment table the way LMDB's is).
//!   An earlier draft of this comment claimed RocksDB had no such guard at all before that probe
//!   was run — corrected here rather than left as an untested assumption.
//! - RocksDB has **no built-in multi-key atomic transaction** unless you opt into `TransactionDB`
//!   or build a `WriteBatch` and call `db.write()` once. This backend uses a `WriteBatch` for
//!   `bulk_ingest` (one atomic multi-row commit, directly comparable to every other engine's own
//!   single-transaction bulk load), but `crash_mid_ingest` does NOT stash an open, uncommitted
//!   batch the way `redb_engine.rs` does — seeu that function's own doc comment for why, and why
//!   this is a real, structural difference in what "crash mid-write" even means for this engine,
//!   not a gap in this spike's methodology.
//!
//! `integrity_check()` here is a manual full-table deserialize scan (identical in spirit to
//! `lmdb.rs`/`redb_engine.rs`'s own), not RocksDB's own `DB::open` recovery + `ldb` repair tooling —
//! called out explicitly rather than left implicit, per this spike's standing rule that a
//! clean-looking result and a skipped check should never look identical.

use crate::gen::{Asset, Flag};
use crate::workload::{FacetCounts, RangeQuery, Workload};
use rocksdb::{ColumnFamily, IteratorMode, Options, WriteBatch, DB};
use std::path::{Path, PathBuf};

const CF_NAMES: &[&str] = &[
    "assets",
    "by_date",
    "by_folder",
    "by_keyword",
    "by_model",
    "by_rating",
];

pub struct RocksDbEngine {
    db: DB,
    path: PathBuf,
}

fn id_key(id: u64) -> [u8; 8] {
    id.to_be_bytes()
}

fn id_from_bytes(b: &[u8]) -> u64 {
    u64::from_be_bytes(b.try_into().expect("id value must be 8 bytes"))
}

fn composite_key(prefix: &[u8], id: u64) -> Vec<u8> {
    let mut k = Vec::with_capacity(prefix.len() + 1 + 8);
    k.extend_from_slice(prefix);
    k.push(0); // NUL separator: none of this generator's strings contain a NUL byte.
    k.extend_from_slice(&id_key(id));
    k
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

impl RocksDbEngine {
    fn cf(&self, name: &str) -> &ColumnFamily {
        self.db
            .cf_handle(name)
            .unwrap_or_else(|| panic!("column family {name} missing"))
    }

    /// Appends one asset's row plus every secondary-index entry to `batch`. Shared by
    /// `bulk_ingest` and `crash_mid_ingest` so the two indexing strategies can't drift apart.
    fn append_asset(&self, batch: &mut WriteBatch, a: &Asset) -> anyhow::Result<()> {
        let stored: StoredAsset = a.into();
        let bytes = bincode::serialize(&stored)?;
        let idk = id_key(a.id);
        batch.put_cf(self.cf("assets"), idk, &bytes);
        batch.put_cf(
            self.cf("by_date"),
            composite_key(a.capture_date.as_bytes(), a.id),
            idk,
        );
        batch.put_cf(
            self.cf("by_folder"),
            composite_key(a.folder_path.as_bytes(), a.id),
            idk,
        );
        batch.put_cf(
            self.cf("by_model"),
            composite_key(a.model.as_bytes(), a.id),
            idk,
        );
        batch.put_cf(self.cf("by_rating"), composite_key(&[a.rating], a.id), idk);
        for kw in &a.keywords {
            batch.put_cf(
                self.cf("by_keyword"),
                composite_key(kw.as_bytes(), a.id),
                idk,
            );
        }
        Ok(())
    }

    /// Prefix scan: seeks to `prefix` and iterates forward, stopping at the first key that no
    /// longer starts with it. RocksDB's default (bytewise) comparator makes this correct without
    /// any `prefix_extractor`/bloom-filter configuration — same assumption `lmdb.rs`/
    /// `redb_engine.rs` make about their own stores' default key ordering, confirmed by this
    /// module's `tests/cross_engine.rs::rocksdb_matches_shared_workload` entry, not just asserted.
    fn prefix_ids(&self, cf_name: &str, prefix: &[u8]) -> anyhow::Result<Vec<u64>> {
        let cf = self.cf(cf_name);
        let mut out = Vec::new();
        let iter = self
            .db
            .iterator_cf(cf, IteratorMode::From(prefix, rocksdb::Direction::Forward));
        for item in iter {
            let (k, v) = item?;
            if !k.starts_with(prefix) {
                break;
            }
            out.push(id_from_bytes(&v));
        }
        Ok(out)
    }

    pub fn reopen(&self) -> anyhow::Result<Self> {
        Self::open(&self.path)
    }
}

impl Workload for RocksDbEngine {
    fn open(path: &Path) -> anyhow::Result<Self> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);
        let db = DB::open_cf(&opts, path, CF_NAMES)?;
        Ok(Self {
            db,
            path: path.to_path_buf(),
        })
    }

    fn bulk_ingest(&mut self, assets: &[Asset]) -> anyhow::Result<()> {
        // One WriteBatch for the whole call, written atomically in a single `db.write()` — directly
        // comparable to every other engine's own single-transaction bulk load (`lmdb.rs`'s one
        // `write_txn`, `redb_engine.rs`'s one `WriteTransaction`, SQLite/DuckDB's one `tx`).
        let mut batch = WriteBatch::default();
        for a in assets {
            self.append_asset(&mut batch, a)?;
        }
        self.db.write(batch)?;
        Ok(())
    }

    fn crash_mid_ingest(&mut self, assets: &[Asset]) -> anyhow::Result<()> {
        // Structurally different from `redb_engine.rs`'s `crash_mid_ingest`, and worth reading in
        // full before assuming this is a weaker test: RocksDB has no multi-key atomic transaction
        // outside an explicit `WriteBatch` + `db.write()` call, so there is no "genuinely open,
        // uncommitted transaction" object to stash and forget the way redb's `WriteTransaction` (or
        // SQLite/DuckDB's own open transaction) can be. The faithful RocksDB analog of "a process
        // is SIGKILLed midway through a bulk-load loop" is: some prefix of the loop's individual
        // writes already reached the WAL and are durable, and the rest simply never got issued —
        // not a half-committed multi-row transaction rolled back on recovery. This backend commits
        // each of the first half's assets as its own small atomic per-asset WriteBatch (row +
        // every secondary-index entry for that one asset, still atomic *per asset*, just not across
        // assets) and returns *before* touching the second half at all.
        //
        // This distinction ended up **not** mattering for the actual crash-safety gate result
        // (same lesson `redb_engine.rs`'s own doc comment draws for its own crash test): the
        // reopen step itself fails first, before the half-committed-vs-fully-committed question is
        // ever reached — see this ADR's crash-safety section. RocksDB opens a `LOCK` file on the
        // DB directory and holds it for the handle's lifetime; `mem::forget` skips the `Drop` that
        // would release it, so the caller's later reopen of this *same* path fails with "lock hold
        // by current process." Confirmed path-scoped, not a process-wide guard like LMDB's
        // (ADR-0008's hard-gate-3 finding): opening a **different**, never-before-touched path in
        // the same process, after forgetting this one, succeeds cleanly.
        let half = assets.len() / 2;
        for a in &assets[..half] {
            let mut batch = WriteBatch::default();
            self.append_asset(&mut batch, a)?;
            self.db.write(batch)?;
        }
        Ok(())
    }

    fn write_rating(&mut self, asset_id: u64, rating: u8) -> anyhow::Result<()> {
        let assets_cf = self.cf("assets");
        let raw = self
            .db
            .get_cf(assets_cf, id_key(asset_id))?
            .ok_or_else(|| anyhow::anyhow!("asset {asset_id} not found"))?;
        let mut stored: StoredAsset = bincode::deserialize(&raw)?;
        let old_rating = stored.rating;
        stored.rating = rating;

        let mut batch = WriteBatch::default();
        batch.delete_cf(self.cf("by_rating"), composite_key(&[old_rating], asset_id));
        batch.put_cf(assets_cf, id_key(asset_id), bincode::serialize(&stored)?);
        batch.put_cf(
            self.cf("by_rating"),
            composite_key(&[rating], asset_id),
            id_key(asset_id),
        );
        self.db.write(batch)?;
        Ok(())
    }

    fn rate_burst(&mut self, updates: &[(u64, u8)]) -> anyhow::Result<()> {
        let assets_cf = self.cf("assets");
        let by_rating_cf = self.cf("by_rating");
        let mut batch = WriteBatch::default();
        // Tracks each asset's rating as staged so far *within this batch*, not just on disk --
        // if `updates` repeats an asset_id (e.g. (5, 2) then (5, 3) in the same burst), the
        // second iteration must delete the by_rating[2] entry the first iteration just staged,
        // not re-read the stale on-disk rating (which hasn't been committed yet) and delete the
        // wrong key, which would leave a stale index entry pointing at an intermediate rating a
        // range_query could then incorrectly return. Found by adversarial review.
        let mut staged_rating: std::collections::HashMap<u64, u8> =
            std::collections::HashMap::new();
        for (asset_id, rating) in updates {
            let raw = self
                .db
                .get_cf(assets_cf, id_key(*asset_id))?
                .ok_or_else(|| anyhow::anyhow!("asset {asset_id} not found"))?;
            let mut stored: StoredAsset = bincode::deserialize(&raw)?;
            // The rating to delete from the index is whatever this asset's rating was staged to
            // by an earlier entry in *this same batch*, if any -- not the on-disk value, which
            // still reflects the pre-batch state until `self.db.write(batch)` below commits.
            let old_rating = staged_rating
                .get(asset_id)
                .copied()
                .unwrap_or(stored.rating);
            batch.delete_cf(by_rating_cf, composite_key(&[old_rating], *asset_id));
            stored.rating = *rating;
            batch.put_cf(assets_cf, id_key(*asset_id), bincode::serialize(&stored)?);
            batch.put_cf(
                by_rating_cf,
                composite_key(&[*rating], *asset_id),
                id_key(*asset_id),
            );
            staged_rating.insert(*asset_id, *rating);
        }
        self.db.write(batch)?;
        Ok(())
    }

    fn tag_keyword(&mut self, asset_ids: &[u64], keyword: &str) -> anyhow::Result<()> {
        let by_keyword_cf = self.cf("by_keyword");
        let mut batch = WriteBatch::default();
        for id in asset_ids {
            batch.put_cf(
                by_keyword_cf,
                composite_key(keyword.as_bytes(), *id),
                id_key(*id),
            );
        }
        self.db.write(batch)?;
        Ok(())
    }

    fn faceted_filter(
        &self,
        model: Option<&str>,
        min_rating: Option<u8>,
        keyword_prefix: Option<&str>,
    ) -> anyhow::Result<FacetCounts> {
        let assets_cf = self.cf("assets");
        // Same choice `lmdb.rs`/`redb_engine.rs::faceted_filter` make: start from whichever index
        // is most selective (keyword prefix, else model, else a full scan), for the same reason.
        let mut candidate_ids: Vec<u64> = if let Some(kw) = keyword_prefix {
            self.prefix_ids("by_keyword", kw.as_bytes())?
        } else if let Some(m) = model {
            self.prefix_ids("by_model", m.as_bytes())?
        } else {
            let mut ids = Vec::new();
            for item in self.db.iterator_cf(assets_cf, IteratorMode::Start) {
                let (k, _) = item?;
                ids.push(id_from_bytes(&k));
            }
            ids
        };
        candidate_ids.sort_unstable();
        candidate_ids.dedup();

        let mut counts = FacetCounts::default();
        let mut by_model = std::collections::HashMap::new();
        let mut by_rating = std::collections::HashMap::new();
        for id in candidate_ids {
            let raw = match self.db.get_cf(assets_cf, id_key(id))? {
                Some(r) => r,
                None => continue,
            };
            let stored: StoredAsset = bincode::deserialize(&raw)?;
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
        let cf = self.cf("by_date");
        // Iterate the cursor backwards directly (newest date first), same lesson `lmdb.rs`'s and
        // `redb_engine.rs`'s own `sort_by_date_page` document: materializing and reversing every
        // row dominated the whole benchmark the first time either of those modules tried it.
        let ids: Vec<u64> = self
            .db
            .iterator_cf(cf, IteratorMode::End)
            .skip(offset as usize)
            .take(limit as usize)
            .map(|r| r.map(|(_, v)| id_from_bytes(&v)))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    fn folder_subtree_count(&self, folder_prefix: &str) -> anyhow::Result<u64> {
        Ok(self
            .prefix_ids("by_folder", folder_prefix.as_bytes())?
            .len() as u64)
    }

    fn keyword_subtree_query(&self, keyword_prefix: &str) -> anyhow::Result<Vec<u64>> {
        let mut ids = self.prefix_ids("by_keyword", keyword_prefix.as_bytes())?;
        ids.sort_unstable();
        ids.dedup();
        Ok(ids)
    }

    fn range_query(&self, q: &RangeQuery) -> anyhow::Result<Vec<u64>> {
        let by_rating_cf = self.cf("by_rating");
        let assets_cf = self.cf("assets");
        // Indexed on rating alone (the most selective of the three dimensions given the
        // generator's 2-10% keep rate); iso/date are post-filtered in application code — same
        // honest shape as `lmdb.rs`/`redb_engine.rs::range_query`, for the same reason.
        let lo = composite_key(&[q.min_rating], 0);
        let hi = composite_key(&[q.max_rating], u64::MAX);
        let mut out = Vec::new();
        let iter = self.db.iterator_cf(
            by_rating_cf,
            IteratorMode::From(&lo, rocksdb::Direction::Forward),
        );
        for item in iter {
            let (k, v) = item?;
            if k.as_ref() > hi.as_slice() {
                break;
            }
            let id = id_from_bytes(&v);
            let raw = match self.db.get_cf(assets_cf, id_key(id))? {
                Some(r) => r,
                None => continue,
            };
            let stored: StoredAsset = bincode::deserialize(&raw)?;
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
        let cf = self.cf("assets");
        let mut out = Vec::new();
        for item in self.db.iterator_cf(cf, IteratorMode::Start) {
            let (k, v) = item?;
            let stored: StoredAsset = bincode::deserialize(&v)?;
            if stored.filename.contains(substr) {
                out.push(id_from_bytes(&k));
            }
        }
        Ok(out)
    }

    fn backup(&self, dest: &Path) -> anyhow::Result<()> {
        // RocksDB's `Checkpoint` API: an online, no-exclusive-lock, no-"optimize"-step consistent
        // snapshot (hard-links unchanged SST files, copies only the small live WAL/manifest state)
        // — a first-class purpose-built mechanism, unlike `redb_engine.rs`'s own admitted fallback
        // to a bare `fs::copy` (redb has no equivalent API). Directly analogous to SQLite's
        // `VACUUM INTO` / DuckDB's `EXPORT DATABASE` / LMDB's `env.copy_to_path`.
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let checkpoint = rocksdb::checkpoint::Checkpoint::new(&self.db)?;
        checkpoint.create_checkpoint(dest)?;
        Ok(())
    }

    fn integrity_check(&self) -> anyhow::Result<bool> {
        let cf = self.cf("assets");
        for item in self.db.iterator_cf(cf, IteratorMode::Start) {
            let (_, v) = item?;
            let _: StoredAsset = bincode::deserialize(&v)?;
        }
        Ok(true)
    }
}
