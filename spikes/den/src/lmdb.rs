//! LMDB (via `heed`) backend. Time-boxed per #67's own decision-rule note: unlike the SQL
//! engines, LMDB has no query planner — every query pattern needs its own hand-maintained
//! secondary index, kept in sync on every write. This backend indexes the single most selective
//! dimension per query (date, folder, keyword, model, rating) and post-filters the rest in
//! application code, which is the realistic shape of a hand-rolled LMDB catalog store, not an
//! artificially disadvantaged one. If a query pattern turns out to need more than this to meet
//! budget, that's the finding: it means LMDB needs its own real query-planning layer built on top
//! (a real engineering cost DuckDB/SQLite/Postgres don't have), which the ADR records as-is
//! rather than building out further here.

use crate::gen::{Asset, Flag};
use crate::workload::{FacetCounts, RangeQuery, Workload};
use heed::types::Bytes;
use heed::{Database, Env, EnvOpenOptions};
use std::path::{Path, PathBuf};

pub struct LmdbEngine {
    env: Env,
    assets: Database<Bytes, Bytes>,
    by_date: Database<Bytes, Bytes>,
    by_folder: Database<Bytes, Bytes>,
    by_keyword: Database<Bytes, Bytes>,
    by_model: Database<Bytes, Bytes>,
    by_rating: Database<Bytes, Bytes>,
    path: PathBuf,
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

const MAP_SIZE: usize = 16 * 1024 * 1024 * 1024; // 16 GiB address-space reservation, not disk usage.

impl Workload for LmdbEngine {
    fn open(path: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(path)?;
        let env = unsafe {
            EnvOpenOptions::new().map_size(MAP_SIZE).max_dbs(8).open(path)?
        };
        let mut wtxn = env.write_txn()?;
        let assets = env.database_options().types::<Bytes, Bytes>().name("assets").create(&mut wtxn)?;
        let by_date =
            env.database_options().types::<Bytes, Bytes>().name("by_date").create(&mut wtxn)?;
        let by_folder =
            env.database_options().types::<Bytes, Bytes>().name("by_folder").create(&mut wtxn)?;
        let by_keyword =
            env.database_options().types::<Bytes, Bytes>().name("by_keyword").create(&mut wtxn)?;
        let by_model =
            env.database_options().types::<Bytes, Bytes>().name("by_model").create(&mut wtxn)?;
        let by_rating =
            env.database_options().types::<Bytes, Bytes>().name("by_rating").create(&mut wtxn)?;
        wtxn.commit()?;
        Ok(Self { env, assets, by_date, by_folder, by_keyword, by_model, by_rating, path: path.to_path_buf() })
    }

    fn bulk_ingest(&mut self, assets: &[Asset]) -> anyhow::Result<()> {
        let mut wtxn = self.env.write_txn()?;
        for a in assets {
            let stored: StoredAsset = a.into();
            self.assets.put(&mut wtxn, &id_key(a.id), &bincode::serialize(&stored)?)?;
            self.by_date.put(
                &mut wtxn,
                &composite_key(a.capture_date.as_bytes(), a.id),
                &id_key(a.id),
            )?;
            self.by_folder.put(
                &mut wtxn,
                &composite_key(a.folder_path.as_bytes(), a.id),
                &id_key(a.id),
            )?;
            self.by_model.put(
                &mut wtxn,
                &composite_key(a.model.as_bytes(), a.id),
                &id_key(a.id),
            )?;
            self.by_rating.put(&mut wtxn, &composite_key(&[a.rating], a.id), &id_key(a.id))?;
            for kw in &a.keywords {
                self.by_keyword.put(
                    &mut wtxn,
                    &composite_key(kw.as_bytes(), a.id),
                    &id_key(a.id),
                )?;
            }
        }
        wtxn.commit()?;
        Ok(())
    }

    fn write_rating(&mut self, asset_id: u64, rating: u8) -> anyhow::Result<()> {
        let mut wtxn = self.env.write_txn()?;
        let raw = self
            .assets
            .get(&wtxn, &id_key(asset_id))?
            .ok_or_else(|| anyhow::anyhow!("asset {asset_id} not found"))?;
        let mut stored: StoredAsset = bincode::deserialize(raw)?;
        let old_rating = stored.rating;
        self.by_rating.delete(&mut wtxn, &composite_key(&[old_rating], asset_id))?;
        stored.rating = rating;
        self.assets.put(&mut wtxn, &id_key(asset_id), &bincode::serialize(&stored)?)?;
        self.by_rating.put(&mut wtxn, &composite_key(&[rating], asset_id), &id_key(asset_id))?;
        wtxn.commit()?;
        Ok(())
    }

    fn rate_burst(&mut self, updates: &[(u64, u8)]) -> anyhow::Result<()> {
        let mut wtxn = self.env.write_txn()?;
        for (asset_id, rating) in updates {
            let raw = self
                .assets
                .get(&wtxn, &id_key(*asset_id))?
                .ok_or_else(|| anyhow::anyhow!("asset {asset_id} not found"))?
                .to_vec();
            let mut stored: StoredAsset = bincode::deserialize(&raw)?;
            let old_rating = stored.rating;
            self.by_rating.delete(&mut wtxn, &composite_key(&[old_rating], *asset_id))?;
            stored.rating = *rating;
            self.assets.put(&mut wtxn, &id_key(*asset_id), &bincode::serialize(&stored)?)?;
            self.by_rating.put(&mut wtxn, &composite_key(&[*rating], *asset_id), &id_key(*asset_id))?;
        }
        wtxn.commit()?;
        Ok(())
    }

    fn tag_keyword(&mut self, asset_ids: &[u64], keyword: &str) -> anyhow::Result<()> {
        let mut wtxn = self.env.write_txn()?;
        for id in asset_ids {
            self.by_keyword.put(&mut wtxn, &composite_key(keyword.as_bytes(), *id), &id_key(*id))?;
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
        let rtxn = self.env.read_txn()?;
        // Start from whichever index is most selective: a keyword prefix if given, else model.
        let mut candidate_ids: Vec<u64> = if let Some(kw) = keyword_prefix {
            self.by_keyword
                .prefix_iter(&rtxn, kw.as_bytes())?
                .map(|r| r.map(|(_, v)| u64::from_be_bytes(v.try_into().unwrap())))
                .collect::<Result<Vec<_>, _>>()?
        } else if let Some(m) = model {
            self.by_model
                .prefix_iter(&rtxn, m.as_bytes())?
                .map(|r| r.map(|(_, v)| u64::from_be_bytes(v.try_into().unwrap())))
                .collect::<Result<Vec<_>, _>>()?
        } else {
            self.assets
                .iter(&rtxn)?
                .map(|r| r.map(|(k, _)| u64::from_be_bytes(k.try_into().unwrap())))
                .collect::<Result<Vec<_>, _>>()?
        };
        candidate_ids.sort_unstable();
        candidate_ids.dedup();

        let mut counts = FacetCounts::default();
        let mut by_model = std::collections::HashMap::new();
        let mut by_rating = std::collections::HashMap::new();
        for id in candidate_ids {
            let raw = match self.assets.get(&rtxn, &id_key(id))? {
                Some(r) => r,
                None => continue,
            };
            let stored: StoredAsset = bincode::deserialize(raw)?;
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
        let rtxn = self.env.read_txn()?;
        // rev_iter walks the cursor backwards directly (newest date first) rather than
        // materializing and reversing every row on every call — the first version of this method
        // did that and it dominated the whole benchmark (13ms vs sub-millisecond for every other
        // op at 600k), a real cost that would only get worse at 2M and at the sort's actual
        // production use (repeated re-paging while scrolling the library grid).
        let ids: Vec<u64> = self
            .by_date
            .rev_iter(&rtxn)?
            .skip(offset as usize)
            .take(limit as usize)
            .map(|r| r.map(|(_, v)| u64::from_be_bytes(v.try_into().unwrap())))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    fn folder_subtree_count(&self, folder_prefix: &str) -> anyhow::Result<u64> {
        let rtxn = self.env.read_txn()?;
        let count = self.by_folder.prefix_iter(&rtxn, folder_prefix.as_bytes())?.count() as u64;
        Ok(count)
    }

    fn keyword_subtree_query(&self, keyword_prefix: &str) -> anyhow::Result<Vec<u64>> {
        let rtxn = self.env.read_txn()?;
        let mut ids: Vec<u64> = self
            .by_keyword
            .prefix_iter(&rtxn, keyword_prefix.as_bytes())?
            .map(|r| r.map(|(_, v)| u64::from_be_bytes(v.try_into().unwrap())))
            .collect::<Result<Vec<_>, _>>()?;
        ids.sort_unstable();
        ids.dedup();
        Ok(ids)
    }

    fn range_query(&self, q: &RangeQuery) -> anyhow::Result<Vec<u64>> {
        let rtxn = self.env.read_txn()?;
        // Indexed on rating alone (the most selective of the three dimensions given the
        // generator's 2-10% keep rate); iso/date are post-filtered in application code — see this
        // module's doc comment for why that's the honest shape of a hand-rolled KV index, not an
        // artificial handicap.
        let lo = composite_key(&[q.min_rating], 0);
        let hi = composite_key(&[q.max_rating], u64::MAX);
        let bounds = (std::ops::Bound::Included(lo.as_slice()), std::ops::Bound::Included(hi.as_slice()));
        let mut out = Vec::new();
        for r in self.by_rating.range(&rtxn, &bounds)? {
            let (_, v) = r?;
            let id = u64::from_be_bytes(v.try_into().unwrap());
            let raw = match self.assets.get(&rtxn, &id_key(id))? {
                Some(r) => r,
                None => continue,
            };
            let stored: StoredAsset = bincode::deserialize(raw)?;
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
        let rtxn = self.env.read_txn()?;
        let mut out = Vec::new();
        for r in self.assets.iter(&rtxn)? {
            let (k, v) = r?;
            let stored: StoredAsset = bincode::deserialize(v)?;
            if stored.filename.contains(substr) {
                out.push(u64::from_be_bytes(k.try_into().unwrap()));
            }
        }
        Ok(out)
    }

    fn backup(&self, dest: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(dest)?;
        self.env.copy_to_path(dest.join("data.mdb"), heed::CompactionOption::Enabled)?;
        Ok(())
    }

    fn integrity_check(&self) -> anyhow::Result<bool> {
        let rtxn = self.env.read_txn()?;
        for r in self.assets.iter(&rtxn)? {
            let (_, v) = r?;
            let _: StoredAsset = bincode::deserialize(v)?;
        }
        Ok(true)
    }
}

impl LmdbEngine {
    pub fn reopen(&self) -> anyhow::Result<Self> {
        Self::open(&self.path)
    }
}
