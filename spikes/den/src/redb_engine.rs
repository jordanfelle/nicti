//! `redb` (pure-Rust embedded KV store) backend, added for #106/ADR-0010. Like LMDB (`lmdb.rs`),
//! redb has no query planner — every query pattern needs its own hand-maintained secondary index,
//! kept in sync on every write. This backend reuses `lmdb.rs`'s design pattern almost exactly:
//! byte-encoded composite keys (`prefix || 0x00 || big-endian id`) in dedicated tables, one per
//! indexed dimension, so a prefix scan is just "iterate a byte range starting at the prefix and
//! stop at the first key that no longer starts with it." `redb`'s `Key` trait is implemented for
//! `&[u8]` using ordinary byte-lexicographic comparison, which is exactly what `composite_key`
//! assumes (`tests/cross_engine.rs`'s `redb_matches_shared_workload` is what actually confirms
//! this, not just this doc comment's assertion).
//!
//! Two real differences from `lmdb.rs`'s own module, both upstream API shape, not something this
//! backend works around:
//!
//! - `redb::WriteTransaction` is **not** lifetime-bound to `Database` (its own docs say so
//!   explicitly: "the returned transaction ... keeps \[the Database\] open if the Database is
//!   dropped"). That means, unlike `heed`'s `RwTxn<'_>` (which borrows `&Env` and can't be stashed
//!   across a method return without becoming self-referential — see `lmdb.rs::crash_mid_ingest`'s
//!   own comment on why it had to fall back to a plain commit), a `redb` engine *can* hold a
//!   genuinely open, uncommitted `WriteTransaction` as a struct field across `crash_mid_ingest`'s
//!   return. This backend does exactly that (`pending_crash_txn`), which makes `redb`'s
//!   `crash_mid_ingest` a faithful "half a batch, never committed, never rolled back" simulation —
//!   the same shape SQLite/DuckDB already get, not the compromise LMDB had to accept.
//! - `redb` has no built-in online-backup call analogous to SQLite's `VACUUM INTO`, DuckDB's
//!   `EXPORT DATABASE`, or LMDB's `env.copy_to_path`. `backup()` below is a plain `std::fs::copy`
//!   of the single database file, with a read transaction held open across the copy so `redb`'s
//!   own MVCC page allocator can't reclaim the pages behind the snapshot the copy is reading (a
//!   real, if partial, safety property) — but this does **not** give the same crash-consistency
//!   guarantee as the other three engines' dedicated backup APIs against a *concurrent* writer,
//!   because a bare `fs::copy` isn't coordinated with `redb`'s own commit boundary the way a
//!   purpose-built backup routine would be. This spike never exercises a concurrent writer for any
//!   engine (see ADR-0008's own "two honest scope limits" note), so this gap doesn't affect the
//!   numbers measured here, but it is a real, additional gap specific to `redb` and is called out
//!   in ADR-0010 rather than left to look like a like-for-like backup call.
//!
//! `integrity_check()` here is a manual full-table deserialize scan (identical in spirit to
//! `lmdb.rs`'s own), not a call to `redb`'s own `Database::check_integrity` — that method takes
//! `&mut self` and attempts a repair, which doesn't fit this trait's `&self` signature without
//! wrapping `Database` in interior mutability (`RefCell`/`Mutex`) purely for this one call, not
//! attempted in this pass. This means, like DuckDB's shallow `integrity_check` (see ADR-0008), the
//! ✅ this backend earns on the crash-safety hard gate is *this* check (real per-row deserialize
//! validation, stronger than DuckDB's `SELECT COUNT(*)` probe but weaker than redb's own available
//! page-level repair scan) — called out explicitly rather than left implicit, per this spike's
//! standing rule that a clean-looking result and a skipped check should never look identical.

use crate::gen::{Asset, Flag};
use crate::workload::{FacetCounts, RangeQuery, Workload};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition, WriteTransaction};
use std::path::{Path, PathBuf};

const ASSETS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("assets");
const BY_DATE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("by_date");
const BY_FOLDER: TableDefinition<&[u8], &[u8]> = TableDefinition::new("by_folder");
const BY_KEYWORD: TableDefinition<&[u8], &[u8]> = TableDefinition::new("by_keyword");
const BY_MODEL: TableDefinition<&[u8], &[u8]> = TableDefinition::new("by_model");
const BY_RATING: TableDefinition<&[u8], &[u8]> = TableDefinition::new("by_rating");

pub struct RedbEngine {
    db: Database,
    path: PathBuf,
    /// Stashed by `crash_mid_ingest` so the caller's later `mem::forget(engine)` leaks a genuinely
    /// open, uncommitted transaction — see this module's doc comment.
    pending_crash_txn: Option<WriteTransaction>,
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

/// Inserts one asset's row plus every secondary-index entry into already-open tables. Shared by
/// `bulk_ingest` and `crash_mid_ingest` so the two can't drift.
#[allow(clippy::too_many_arguments)]
fn insert_asset(
    assets_t: &mut redb::Table<&[u8], &[u8]>,
    by_date: &mut redb::Table<&[u8], &[u8]>,
    by_folder: &mut redb::Table<&[u8], &[u8]>,
    by_keyword: &mut redb::Table<&[u8], &[u8]>,
    by_model: &mut redb::Table<&[u8], &[u8]>,
    by_rating: &mut redb::Table<&[u8], &[u8]>,
    a: &Asset,
) -> anyhow::Result<()> {
    let stored: StoredAsset = a.into();
    let bytes = bincode::serialize(&stored)?;
    let idk = id_key(a.id);
    assets_t.insert(idk.as_slice(), bytes.as_slice())?;
    by_date.insert(
        composite_key(a.capture_date.as_bytes(), a.id).as_slice(),
        idk.as_slice(),
    )?;
    by_folder.insert(
        composite_key(a.folder_path.as_bytes(), a.id).as_slice(),
        idk.as_slice(),
    )?;
    by_model.insert(
        composite_key(a.model.as_bytes(), a.id).as_slice(),
        idk.as_slice(),
    )?;
    by_rating.insert(composite_key(&[a.rating], a.id).as_slice(), idk.as_slice())?;
    for kw in &a.keywords {
        by_keyword.insert(
            composite_key(kw.as_bytes(), a.id).as_slice(),
            idk.as_slice(),
        )?;
    }
    Ok(())
}

impl Workload for RedbEngine {
    fn open(path: &Path) -> anyhow::Result<Self> {
        let db = Database::create(path)?;
        // Create every table up front (mirrors lmdb.rs's `open()`, which creates all 6 named
        // databases in one write txn) so a read-only caller never hits redb's "table does not
        // exist" error against a table nothing has written to yet.
        let wtxn = db.begin_write()?;
        {
            let _ = wtxn.open_table(ASSETS)?;
            let _ = wtxn.open_table(BY_DATE)?;
            let _ = wtxn.open_table(BY_FOLDER)?;
            let _ = wtxn.open_table(BY_KEYWORD)?;
            let _ = wtxn.open_table(BY_MODEL)?;
            let _ = wtxn.open_table(BY_RATING)?;
        }
        wtxn.commit()?;
        Ok(Self {
            db,
            path: path.to_path_buf(),
            pending_crash_txn: None,
        })
    }

    fn bulk_ingest(&mut self, assets: &[Asset]) -> anyhow::Result<()> {
        let wtxn = self.db.begin_write()?;
        {
            let mut assets_t = wtxn.open_table(ASSETS)?;
            let mut by_date = wtxn.open_table(BY_DATE)?;
            let mut by_folder = wtxn.open_table(BY_FOLDER)?;
            let mut by_keyword = wtxn.open_table(BY_KEYWORD)?;
            let mut by_model = wtxn.open_table(BY_MODEL)?;
            let mut by_rating = wtxn.open_table(BY_RATING)?;
            for a in assets {
                insert_asset(
                    &mut assets_t,
                    &mut by_date,
                    &mut by_folder,
                    &mut by_keyword,
                    &mut by_model,
                    &mut by_rating,
                    a,
                )?;
            }
        }
        wtxn.commit()?;
        Ok(())
    }

    fn crash_mid_ingest(&mut self, assets: &[Asset]) -> anyhow::Result<()> {
        let half = assets.len() / 2;
        let wtxn = self.db.begin_write()?;
        {
            let mut assets_t = wtxn.open_table(ASSETS)?;
            let mut by_date = wtxn.open_table(BY_DATE)?;
            let mut by_folder = wtxn.open_table(BY_FOLDER)?;
            let mut by_keyword = wtxn.open_table(BY_KEYWORD)?;
            let mut by_model = wtxn.open_table(BY_MODEL)?;
            let mut by_rating = wtxn.open_table(BY_RATING)?;
            for a in &assets[..half] {
                insert_asset(
                    &mut assets_t,
                    &mut by_date,
                    &mut by_folder,
                    &mut by_keyword,
                    &mut by_model,
                    &mut by_rating,
                    a,
                )?;
            }
        }
        // Deliberately never committed or aborted: stashed so the caller's later
        // `mem::forget(engine)` leaks this transaction exactly as an OS-level SIGKILL
        // mid-transaction would — see this module's doc comment for why redb's `WriteTransaction`
        // (unlike heed's) can be stored this way at all.
        self.pending_crash_txn = Some(wtxn);
        Ok(())
    }

    fn write_rating(&mut self, asset_id: u64, rating: u8) -> anyhow::Result<()> {
        let wtxn = self.db.begin_write()?;
        {
            let mut assets_t = wtxn.open_table(ASSETS)?;
            let mut by_rating = wtxn.open_table(BY_RATING)?;
            let raw = assets_t
                .get(id_key(asset_id).as_slice())?
                .ok_or_else(|| anyhow::anyhow!("asset {asset_id} not found"))?
                .value()
                .to_vec();
            let mut stored: StoredAsset = bincode::deserialize(&raw)?;
            let old_rating = stored.rating;
            by_rating.remove(composite_key(&[old_rating], asset_id).as_slice())?;
            stored.rating = rating;
            assets_t.insert(
                id_key(asset_id).as_slice(),
                bincode::serialize(&stored)?.as_slice(),
            )?;
            by_rating.insert(
                composite_key(&[rating], asset_id).as_slice(),
                id_key(asset_id).as_slice(),
            )?;
        }
        wtxn.commit()?;
        Ok(())
    }

    fn rate_burst(&mut self, updates: &[(u64, u8)]) -> anyhow::Result<()> {
        let wtxn = self.db.begin_write()?;
        {
            let mut assets_t = wtxn.open_table(ASSETS)?;
            let mut by_rating = wtxn.open_table(BY_RATING)?;
            for (asset_id, rating) in updates {
                let raw = assets_t
                    .get(id_key(*asset_id).as_slice())?
                    .ok_or_else(|| anyhow::anyhow!("asset {asset_id} not found"))?
                    .value()
                    .to_vec();
                let mut stored: StoredAsset = bincode::deserialize(&raw)?;
                let old_rating = stored.rating;
                by_rating.remove(composite_key(&[old_rating], *asset_id).as_slice())?;
                stored.rating = *rating;
                assets_t.insert(
                    id_key(*asset_id).as_slice(),
                    bincode::serialize(&stored)?.as_slice(),
                )?;
                by_rating.insert(
                    composite_key(&[*rating], *asset_id).as_slice(),
                    id_key(*asset_id).as_slice(),
                )?;
            }
        }
        wtxn.commit()?;
        Ok(())
    }

    fn tag_keyword(&mut self, asset_ids: &[u64], keyword: &str) -> anyhow::Result<()> {
        let wtxn = self.db.begin_write()?;
        {
            let mut by_keyword = wtxn.open_table(BY_KEYWORD)?;
            for id in asset_ids {
                by_keyword.insert(
                    composite_key(keyword.as_bytes(), *id).as_slice(),
                    id_key(*id).as_slice(),
                )?;
            }
        }
        wtxn.commit()?;
        Ok(())
    }

    fn faceted_filter(
        &self,
        model: Option<&str>,
        min_rating: Option<u8>,
        keyword_prefix: Option<&str>,
    ) -> anyhow::Result<FacetCounts> {
        let rtxn = self.db.begin_read()?;
        let assets_t = rtxn.open_table(ASSETS)?;

        // Start from whichever index is most selective: a keyword prefix if given, else model —
        // same choice `lmdb.rs::faceted_filter` makes, for the same reason.
        let mut candidate_ids: Vec<u64> = if let Some(kw) = keyword_prefix {
            let t = rtxn.open_table(BY_KEYWORD)?;
            prefix_ids_owned(&t, kw.as_bytes())?
        } else if let Some(m) = model {
            let t = rtxn.open_table(BY_MODEL)?;
            prefix_ids_owned(&t, m.as_bytes())?
        } else {
            let mut ids = Vec::new();
            for r in assets_t.iter()? {
                let (k, _) = r?;
                ids.push(id_from_bytes(k.value()));
            }
            ids
        };
        candidate_ids.sort_unstable();
        candidate_ids.dedup();

        let mut counts = FacetCounts::default();
        let mut by_model = std::collections::HashMap::new();
        let mut by_rating = std::collections::HashMap::new();
        for id in candidate_ids {
            let raw = match assets_t.get(id_key(id).as_slice())? {
                Some(r) => r.value().to_vec(),
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
        let rtxn = self.db.begin_read()?;
        let t = rtxn.open_table(BY_DATE)?;
        // Reverse the range cursor directly (newest date first) rather than materializing and
        // reversing every row — same lesson `lmdb.rs::sort_by_date_page`'s own comment documents:
        // doing it the naive way dominated the whole benchmark at 600k.
        let ids: Vec<u64> = t
            .range::<&[u8]>(..)?
            .rev()
            .skip(offset as usize)
            .take(limit as usize)
            .map(|r| r.map(|(_, v)| id_from_bytes(v.value())))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    fn folder_subtree_count(&self, folder_prefix: &str) -> anyhow::Result<u64> {
        let rtxn = self.db.begin_read()?;
        let t = rtxn.open_table(BY_FOLDER)?;
        Ok(prefix_ids_owned(&t, folder_prefix.as_bytes())?.len() as u64)
    }

    fn keyword_subtree_query(&self, keyword_prefix: &str) -> anyhow::Result<Vec<u64>> {
        let rtxn = self.db.begin_read()?;
        let t = rtxn.open_table(BY_KEYWORD)?;
        let mut ids = prefix_ids_owned(&t, keyword_prefix.as_bytes())?;
        ids.sort_unstable();
        ids.dedup();
        Ok(ids)
    }

    fn range_query(&self, q: &RangeQuery) -> anyhow::Result<Vec<u64>> {
        let rtxn = self.db.begin_read()?;
        let by_rating = rtxn.open_table(BY_RATING)?;
        let assets_t = rtxn.open_table(ASSETS)?;
        // Indexed on rating alone (the most selective of the three dimensions given the
        // generator's 2-10% keep rate); iso/date are post-filtered in application code — same
        // honest shape as `lmdb.rs::range_query`, for the same reason (see that module's doc
        // comment).
        let lo = composite_key(&[q.min_rating], 0);
        let hi = composite_key(&[q.max_rating], u64::MAX);
        let mut out = Vec::new();
        for r in by_rating.range::<&[u8]>(lo.as_slice()..=hi.as_slice())? {
            let (_, v) = r?;
            let id = id_from_bytes(v.value());
            let raw = match assets_t.get(id_key(id).as_slice())? {
                Some(r) => r.value().to_vec(),
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
        let rtxn = self.db.begin_read()?;
        let t = rtxn.open_table(ASSETS)?;
        let mut out = Vec::new();
        for r in t.iter()? {
            let (k, v) = r?;
            let stored: StoredAsset = bincode::deserialize(v.value())?;
            if stored.filename.contains(substr) {
                out.push(id_from_bytes(k.value()));
            }
        }
        Ok(out)
    }

    fn backup(&self, dest: &Path) -> anyhow::Result<()> {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Pin the current MVCC snapshot for the duration of the copy so redb's own page allocator
        // can't reclaim pages the copy is still reading — see this module's doc comment for why
        // this is weaker than the other three engines' dedicated backup APIs (no coordination with
        // a concurrent writer, never exercised in this spike regardless).
        let _read_txn = self.db.begin_read()?;
        std::fs::copy(&self.path, dest)?;
        Ok(())
    }

    fn integrity_check(&self) -> anyhow::Result<bool> {
        let rtxn = self.db.begin_read()?;
        let t = rtxn.open_table(ASSETS)?;
        for r in t.iter()? {
            let (_, v) = r?;
            let _: StoredAsset = bincode::deserialize(v.value())?;
        }
        Ok(true)
    }
}

/// Same as `prefix_ids` above but against a `ReadOnlyTable` (redb's read-transaction table type
/// has a different concrete type than the write-transaction `Table`, and `ReadableTable`'s
/// `range()` return type differs enough between them that one generic helper isn't worth fighting
/// the type system for in a spike — this duplication is intentional, not an oversight).
fn prefix_ids_owned(
    table: &redb::ReadOnlyTable<&[u8], &[u8]>,
    prefix: &[u8],
) -> anyhow::Result<Vec<u64>> {
    let mut out = Vec::new();
    for r in table.range(prefix..)? {
        let (k, v) = r?;
        if !k.value().starts_with(prefix) {
            break;
        }
        out.push(id_from_bytes(v.value()));
    }
    Ok(out)
}

impl RedbEngine {
    pub fn reopen(&self) -> anyhow::Result<Self> {
        Self::open(&self.path)
    }
}
