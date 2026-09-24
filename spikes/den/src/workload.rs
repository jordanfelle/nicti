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

    /// Called by `den crash` immediately before `mem::forget`ing the engine — a hook for engines
    /// that own a background thread pool of their own (e.g. a per-engine-instance async runtime)
    /// to shut that pool down cleanly first, so a leaked *harness* thread is never conflated with
    /// the engine's own on-disk crash-safety. No-op by default (SQLite/DuckDB/LMDB are plain
    /// synchronous library calls with no threads of their own to leak).
    ///
    /// Added while investigating a real finding in the Turso backend's crash test (every reopen
    /// after `mem::forget` failed with "database is locked", even after 5 seconds of retries) —
    /// the story turned out to be genuinely unresolved, not a clean fix, and is worth reading in
    /// full before assuming this method is a general-purpose solution: a hostile re-review read
    /// `turso_core`'s actual locking code and found the lock is a POSIX `fcntl` advisory lock tied
    /// to the database connection's own file descriptor (released by closing it, not by anything
    /// runtime-related), which predicts that shutting down *only* the runtime here should change
    /// nothing. A direct follow-up experiment partially contradicted that: a minimal reproduction
    /// (a bare table, no indexes, a few dozen rows) *did* reopen successfully after this method's
    /// fix was applied, but the same fix applied to the real, full-schema `crash_mid_ingest`
    /// workload still failed identically. Neither explanation — leaked runtime thread, or fd-level
    /// lock independent of the runtime — fully accounts for both results. See
    /// `docs/adr/0009-turso-database-evaluation.md`'s crash-safety row for the complete account;
    /// short version: this in-process technique could not produce a trustworthy verdict on
    /// Turso's real crash-safety either way, and a real fork+exec+SIGKILL harness is the only way
    /// to actually resolve it. This method is kept because it's still the right hygiene for *any*
    /// future async engine's crash test (never let a leaked harness thread pool masquerade as the
    /// property under test), not because it's confirmed to have fixed Turso's case specifically.
    fn prepare_for_forget(&mut self) {}

    /// Writes half of `assets` inside an open transaction and returns **without committing** —
    /// used only by `den crash`'s mid-write simulation. The caller `mem::forget`s the engine
    /// immediately after this returns, so neither `COMMIT` nor `ROLLBACK` ever runs, approximating
    /// what an OS-level `SIGKILL` mid-transaction would leave behind. This is a distinct method
    /// from `bulk_ingest` (which commits internally) specifically because the first version of
    /// `den crash` called `bulk_ingest` before forgetting the handle — meaning it only ever tested
    /// reopening after an *already fully committed* write, not a real interrupted one.
    fn crash_mid_ingest(&mut self, assets: &[Asset]) -> anyhow::Result<()>;

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
