//! The shared query set every engine backend runs, so p50/p95 numbers are comparable across
//! engines rather than each engine grading its own homework. Every backend implements this
//! trait synchronously (async engines block on their own runtime internally) so `den bench` can
//! drive them all through one code path.

use crate::gen::Asset;
use std::path::Path;

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct FacetCounts {
    pub by_model: Vec<(String, u64)>,
    pub by_rating: Vec<(u8, u64)>,
    pub by_keyword: Vec<(String, u64)>,
    pub total: u64,
}

#[derive(Debug, Clone)]
pub struct RangeQuery {
    pub min_rating: u8,
    pub max_rating: u8,
    pub min_iso: u32,
    pub max_iso: u32,
    pub date_from: String,
    pub date_to: String,
}

/// One backend under test. Every method is a single query pattern from #67's exit criteria.
pub trait Workload {
    /// Opens (or creates) the store at `path`. Measuring this call IS the cold-open gate.
    fn open(path: &Path) -> anyhow::Result<Self>
    where
        Self: Sized;

    fn bulk_ingest(&mut self, assets: &[Asset]) -> anyhow::Result<()>;

    /// A single rating write — the point-update-latency gate.
    fn write_rating(&mut self, asset_id: u64, rating: u8) -> anyhow::Result<()>;

    /// A rapid rate-and-advance burst (rate-while-culling), measured as one unit.
    fn rate_burst(&mut self, updates: &[(u64, u8)]) -> anyhow::Result<()>;

    fn tag_keyword(&mut self, asset_ids: &[u64], keyword: &str) -> anyhow::Result<()>;

    /// Faceted filter + live facet counts for the matching set.
    fn faceted_filter(
        &self,
        model: Option<&str>,
        min_rating: Option<u8>,
        keyword_prefix: Option<&str>,
    ) -> anyhow::Result<FacetCounts>;

    /// Sort by capture date, return one page.
    fn sort_by_date_page(&self, offset: u64, limit: u64) -> anyhow::Result<Vec<u64>>;

    fn folder_subtree_count(&self, folder_prefix: &str) -> anyhow::Result<u64>;

    fn keyword_subtree_query(&self, keyword_prefix: &str) -> anyhow::Result<Vec<u64>>;

    fn range_query(&self, q: &RangeQuery) -> anyhow::Result<Vec<u64>>;

    fn filename_search(&self, substr: &str) -> anyhow::Result<Vec<u64>>;

    /// Backs up the live store to `dest` without an exclusive lock or an "optimize" pass.
    fn backup(&self, dest: &Path) -> anyhow::Result<()>;

    /// Verifies the store isn't corrupted. Used both as a sanity check and after `den crash`.
    fn integrity_check(&self) -> anyhow::Result<bool>;
}
