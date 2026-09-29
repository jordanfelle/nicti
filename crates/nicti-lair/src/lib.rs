//! Catalog store extension point (ADR-0019 §7/§8) and its real implementation (#22): the schema
//! ADR-0067/0103/0071/0021 settled (see `schema.rs`), a `rusqlite`-backed `CatalogStore`
//! (`sqlite.rs`), the Scruff import/ingest pipeline (`scruff.rs`) that scans a folder, fingerprints
//! and upserts each asset, and extracts its T0 grid preview at import time (ADR-0029) — carrying a
//! kitten by the scruff of its neck is how it gets moved into the catalog — and Patrol
//! (`patrol.rs`, #24), the manual "Synchronize Folder"-style sync layered on top of Scruff: after
//! Scruff's disk-side pass, Patrol walks the catalog side, flags any asset whose file has
//! disappeared, and optionally removes it, the way a cat patrols the same territory it already
//! knows. #23 (ADR-0023) adds hierarchical keywords/collections and a filter-query engine:
//! `hunt.rs` (`Filter`/`Sort`/keyset-paginated `hunt`/`facets`) and `clowder.rs`
//! (manual/smart collections — a clowder is a group of cats). #25 (ADR-0025) adds `ninelives.rs`:
//! continuous, crash-safe `VACUUM INTO` backup with integrity verification and retention — a cat
//! has nine lives, and a verified backup is a spare one for the catalog. #27 adds `larder.rs`: the
//! byte-capped, LRU-evicting, purgeable pack-file cache for T2 screen-resolution previews
//! (ADR-0029) — a cat keeps its kills in a larder.

use nicti_claw::{Module, Registry};

mod model;
pub mod schema;
mod sqlite;

pub mod carry;
pub mod clowder;
pub mod hunt;
pub mod larder;
pub mod ninelives;
pub mod patrol;
pub mod pounce_jobs;
pub mod scruff;

pub use clowder::{Collection, CollectionKind};
pub use hunt::{Cursor, FacetCounts, Filter, Page, Sort, SortDirection, SortField};
pub use model::{Asset, Keyword, MoveState, NewAsset, Preview, PreviewTier, Root, RootMove};
pub use sqlite::SqliteCatalog;

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// A filesystem-level failure (stat/read) hit during ingest, before any SQL was involved.
    #[error("I/O error: {0}")]
    Io(String),
    /// ADR-0071's flagged "two volumes, same identity -> never auto-merge" gap, partially closed:
    /// a volume upsert whose `marker_uuid` disagrees with an already-registered volume under the
    /// same `identity_key` is refused rather than silently merged into that row.
    #[error(
        "volume identity conflict for {identity_key:?}: marker_uuid mismatch (existing {existing:?}, incoming {incoming:?})"
    )]
    VolumeIdentityConflict {
        identity_key: String,
        existing: Option<String>,
        incoming: Option<String>,
    },
    /// A `hunt` call whose `Page::after` cursor variant doesn't match the query's own `Sort`
    /// field (e.g. a `Cursor::Rating` passed alongside `SortField::Captured`) -- a programmer
    /// error at the call site, not a data problem.
    #[error("cursor variant does not match the query's sort field")]
    CursorSortMismatch,
    /// A `move_keyword`/`move_collection` call whose `new_parent_id` is the node itself, or one
    /// of its own descendants — either would corrupt the tree (a keyword's materialized `path`
    /// would embed itself twice; a collection's `parent_id` chain would cycle, and
    /// `delete_collection`'s subtree walk would never terminate).
    #[error("moving under itself or a descendant would create a cycle")]
    WouldCreateCycle,
}

/// A catalog store backend. `Module` settles identity/versioning only (ADR-0019 §7); the
/// query/write methods below are this ticket's (#22) contribution, once #67's database-engine
/// choice (SQLite) landed.
pub trait CatalogStore: Module {
    /// Registers or re-confirms a volume by its identity key. Returns the same row id across
    /// repeated calls with the same `identity_key` (a drive-letter change, a reconnect) — see
    /// `CatalogError::VolumeIdentityConflict` for the one case this refuses rather than merging.
    fn upsert_volume(
        &self,
        identity_key: &str,
        label: Option<&str>,
        marker_uuid: Option<&str>,
        now_unix: i64,
    ) -> Result<i64, CatalogError>;

    /// Registers (or finds) a tracked folder under a volume.
    fn ensure_root(&self, volume_id: i64, rel_path: &str) -> Result<i64, CatalogError>;

    /// Every registered folder, in `id` order.
    fn list_roots(&self) -> Result<Vec<Root>, CatalogError>;

    /// Opens a `copying` journal row for a verified folder move of `root_id` to `dest_path`
    /// (#26). Refuses (`CatalogError::Io`) if the root doesn't exist, already has an open move,
    /// or `dest_path` is already a registered root on the same volume -- so a doomed move fails
    /// before any file is copied.
    fn begin_root_move(
        &self,
        root_id: i64,
        dest_path: &str,
        now_unix: i64,
    ) -> Result<i64, CatalogError>;

    /// The move's catalog commit, **one transaction**: re-points the root at the journal's
    /// `dest_path`, records each `(asset_id, blake3_hex)` as that asset's `content_hash`, and
    /// flips the journal to `committed`. Idempotent on an already-`committed` move.
    fn commit_root_move(&self, move_id: i64, hashes: &[(i64, String)]) -> Result<(), CatalogError>;

    /// Moves an open journal row to `state` (`Copying` <-> `Renaming`; `Committed` only via
    /// `commit_root_move`).
    fn set_root_move_state(&self, move_id: i64, state: MoveState) -> Result<(), CatalogError>;

    /// Deletes the journal row: the move is fully done (`committed` and source cleaned up) or
    /// abandoned (`copying`, destination copy discarded, catalog never re-pointed).
    fn finish_root_move(&self, move_id: i64) -> Result<(), CatalogError>;

    /// Every open journal row, for crash recovery at startup.
    fn open_root_moves(&self) -> Result<Vec<RootMove>, CatalogError>;

    fn find_asset_by_path(
        &self,
        root_id: i64,
        rel_path: &str,
    ) -> Result<Option<Asset>, CatalogError>;

    /// Reads a single asset by its own row id. `hunt`/`collection_assets` return bare ids with no
    /// other way to turn one back into a row (short of a full-root scan via
    /// `list_assets_by_root`) — #31 (loupe) needs this to resolve a navigation cursor's current
    /// id into a real asset to display.
    fn get_asset(&self, id: i64) -> Result<Option<Asset>, CatalogError>;

    /// Reads a root's own `rel_path` (which callers store as an absolute folder path under
    /// ADR-0071's placeholder-volume-identity stand-in — see `nicti-pelt::register_root`) by its
    /// row id. Combined with `Asset::rel_path`, this is what lets a caller build a real
    /// filesystem path to open (`root_path.join(&asset.rel_path)`, the same pattern
    /// `patrol::sync_root` already uses internally). `None` if the root id doesn't exist.
    fn get_root_path(&self, root_id: i64) -> Result<Option<String>, CatalogError>;

    /// Every asset sharing this fingerprint, ordered by id. Plural, not `Option<Asset>`: more
    /// than one real asset can share a fingerprint (a literal duplicate file, or a genuine
    /// collision), and a caller trying to tell a move apart from a duplicate needs to check each
    /// candidate's own old path rather than being handed an arbitrary single match that might be
    /// the wrong one (found by CodeRabbit's review).
    fn find_by_fingerprint(&self, fingerprint: &str) -> Result<Vec<Asset>, CatalogError>;

    /// Upserts on `(root_id, rel_path)`: a brand-new path inserts a fresh asset (plus its master
    /// `edit_variant`, an empty `EditDocument`); a path that already exists updates its stat/
    /// fingerprint/EXIF fields in place instead of inserting a duplicate. Returns the asset id
    /// either way.
    ///
    /// `t0_preview` is written (or, if `None`, cleared) in the same transaction as the asset row
    /// and its master variant — not a separate `put_preview`/`clear_preview` call afterward. Found
    /// by CodeRabbit's review: two separate calls left a window where a crash between them could
    /// commit the asset row but never write (or clear) its preview, leaving it silently out of
    /// sync with the file it was extracted from until the next rescan.
    fn insert_asset(
        &self,
        root_id: i64,
        asset: &NewAsset,
        t0_preview: Option<&Preview>,
    ) -> Result<i64, CatalogError>;

    /// Re-points an existing asset at a new `(root_id, rel_path)` — the persistence step a
    /// fingerprint match under a different path needs (a file moved rather than newly imported).
    /// Also refreshes `size_bytes`/`mtime_unix` to the moved file's own stat, so a move that
    /// changed mtime (a cross-filesystem move, some backup tools) doesn't leave the row stuck
    /// with stale stat fields that would make every future scan misdetect it as changed and
    /// needlessly reprocess it.
    fn relink_asset(
        &self,
        asset_id: i64,
        new_root_id: i64,
        new_rel_path: &str,
        new_rel_path_fold: &str,
        size_bytes: u64,
        mtime_unix: i64,
    ) -> Result<(), CatalogError>;

    fn put_preview(
        &self,
        asset_id: i64,
        tier: PreviewTier,
        preview: &Preview,
    ) -> Result<(), CatalogError>;

    fn get_preview(
        &self,
        asset_id: i64,
        tier: PreviewTier,
    ) -> Result<Option<Preview>, CatalogError>;

    /// Removes a stored preview, if one exists. Ingest calls this on a rescan whose file no
    /// longer yields an extractable preview (an in-place edit that removed the embedded JPEG, a
    /// truncated/corrupted rewrite) — a no-op the rest of the time (nothing stored, nothing to
    /// remove), so it's safe to call unconditionally rather than only when a stale preview is
    /// suspected.
    fn clear_preview(&self, asset_id: i64, tier: PreviewTier) -> Result<(), CatalogError>;

    /// Reads the trigger-maintained `(model, rating)` facet count (ADR-0103), summed across every
    /// online volume — used by ingest's tests to check the triggers stay consistent, and by any
    /// future facet-filtered browse view. `rating: None` reads the unrated bucket; `Some(-1)`
    /// reads reject; `Some(0..=5)` reads a star rating.
    fn facet_count(&self, model: Option<&str>, rating: Option<i64>) -> Result<u64, CatalogError>;

    /// Sets (or clears, with `None`) the rating on every listed asset in one statement — so a
    /// culling `rate_burst` across a multi-selection is one commit, not one per asset. A no-op on
    /// an empty slice. `rating` must be `None`, `-1` (reject), or `0..=5` (star rating) — anything
    /// else fails the schema's own `CHECK` constraint.
    fn set_rating(&self, asset_ids: &[i64], rating: Option<i64>) -> Result<(), CatalogError>;

    /// Sets (or clears, with `None`) the pick flag on every listed asset in one statement. A no-op
    /// on an empty slice. `flag` must be `None` or `1` (pick) — anything else fails the schema's
    /// own `CHECK` constraint.
    fn set_flag(&self, asset_ids: &[i64], flag: Option<i64>) -> Result<(), CatalogError>;

    /// Sets (or clears, with `None`) the free-text label on every listed asset in one statement. A
    /// no-op on an empty slice.
    fn set_label(&self, asset_ids: &[i64], label: Option<&str>) -> Result<(), CatalogError>;

    /// Every asset registered under this root, in `id` order. `patrol::sync_root` (#24) uses this
    /// to find rows whose file it needs to check for, since ingest only ever walks the disk and
    /// has no way to notice a path that used to be there and now isn't.
    fn list_assets_by_root(&self, root_id: i64) -> Result<Vec<Asset>, CatalogError>;

    /// Sets or clears an asset's `missing_since` (#24). `Some(now_unix)` flags it as missing as of
    /// that time; `None` marks it present again (a file that reappeared at its cataloged path).
    /// Does not touch any other column — a `relink_asset` call clears this independently, since a
    /// relink means the row's path changed, not just that the old path is present again.
    fn set_asset_missing(
        &self,
        asset_id: i64,
        missing_since: Option<i64>,
    ) -> Result<(), CatalogError>;

    /// Deletes an asset row entirely, cascading to its previews and edit history. Only called by
    /// `patrol::sync_root` (#24) when a sync runs with `remove_missing: true` against a file
    /// that's still gone — never by ingest, and never for an asset under a root that failed to
    /// resolve on disk (ADR-0071's offline-volume case is a separate path from this).
    fn remove_asset(&self, asset_id: i64) -> Result<(), CatalogError>;

    /// Creates a new keyword under `parent_id` (`None` = top-level). Fails (a `UNIQUE` constraint
    /// violation surfaced as `CatalogError::Sqlite`) if a sibling already has the same name,
    /// case-insensitively.
    fn create_keyword(&self, parent_id: Option<i64>, name: &str) -> Result<i64, CatalogError>;

    /// Renames a keyword in place. Never touches `path` — this scheme's id-based materialized
    /// path is exactly what makes a rename cheap: no descendant's `path` needs rewriting, unlike
    /// a name-based path scheme.
    fn rename_keyword(&self, keyword_id: i64, new_name: &str) -> Result<(), CatalogError>;

    /// Moves a keyword (and its whole subtree) under a new parent (`None` = top-level),
    /// rewriting `path` for the keyword and every descendant. Rare relative to tagging/rename, so
    /// this is the one keyword operation that touches more than a single row.
    fn move_keyword(&self, keyword_id: i64, new_parent_id: Option<i64>)
        -> Result<(), CatalogError>;

    /// Deletes a keyword and its entire subtree, along with every `asset_keyword` link to any of
    /// them — never a partial delete that would leave an orphaned child keyword or a dangling
    /// tag reference.
    fn delete_keyword(&self, keyword_id: i64) -> Result<(), CatalogError>;

    /// Tags every listed asset with `keyword_id` in one statement. Re-tagging an asset that
    /// already carries this keyword is a no-op (`asset_keyword`'s own primary key absorbs the
    /// duplicate), not an error.
    fn tag(&self, asset_ids: &[i64], keyword_id: i64) -> Result<(), CatalogError>;

    /// Removes `keyword_id` from every listed asset in one statement. A no-op for any asset that
    /// didn't carry it.
    fn untag(&self, asset_ids: &[i64], keyword_id: i64) -> Result<(), CatalogError>;

    /// Every keyword directly tagged on this asset (not its ancestors) — the filter bar's own
    /// subtree-inclusive matching is a query concern (#242), not this method's.
    fn keywords_for(&self, asset_id: i64) -> Result<Vec<Keyword>, CatalogError>;

    /// Resolves a `/`-free path of plain names (e.g. `["Events", "Named", "birthday-2026"]`) to
    /// the keyword at that exact position in the tree, if one exists. Deliberately takes a
    /// caller-split `&[&str]` rather than committing to one separator style itself — XMP's
    /// `lr:hierarchicalSubject` uses `|`, LRC's `AgLibraryKeyword.genealogy` uses `/`-joined
    /// ancestor ids, and this repo's own den-spike precedent used `.` — each caller splits its own
    /// format and hands this method plain segments.
    fn keyword_by_path(&self, segments: &[&str]) -> Result<Option<Keyword>, CatalogError>;

    /// Runs a `Filter`/`Sort` query, keyset-paginated (`Page::after` echoes the last row's own
    /// sort-key + id back in, never an `OFFSET` — O(n) at 2M rows per ADR-0067). Always scoped to
    /// online volumes only, the same as `facet_count`.
    fn hunt(&self, filter: &Filter, sort: Sort, page: &Page) -> Result<Vec<i64>, CatalogError>;

    /// The total row count a `hunt` call with this `Filter` would match, ignoring `Page`.
    fn hunt_count(&self, filter: &Filter) -> Result<u64, CatalogError>;

    /// Facet breakdowns (by model/rating/flag) for a `Filter`. An unfiltered `Filter` (the
    /// `Default`) reads the trigger-maintained `facet_counts` cache for its model/rating facet
    /// (the case ADR-0103 optimized); any narrowing filter computes a live, exact `GROUP BY` over
    /// the narrowed set instead, since the cache's `(volume_id, model, rating)` grain can't answer
    /// a keyword- or date-narrowed facet count on its own.
    fn facets(&self, filter: &Filter) -> Result<FacetCounts, CatalogError>;

    /// Looks up a single collection by id, `None` if it doesn't exist.
    fn collection(&self, collection_id: i64) -> Result<Option<Collection>, CatalogError>;

    /// Creates a new collection under `parent_id` (`None` = top-level). A `Smart` collection
    /// starts with no rule set (`collection_filter` then returns `None` until `set_smart_rule`
    /// is called); a `Manual` collection starts empty.
    fn create_collection(
        &self,
        parent_id: Option<i64>,
        name: &str,
        kind: CollectionKind,
    ) -> Result<i64, CatalogError>;

    fn rename_collection(&self, collection_id: i64, new_name: &str) -> Result<(), CatalogError>;

    /// Moves a collection (and its subtree, if any) under a new parent. Unlike `move_keyword`,
    /// nothing else needs rewriting — collections aren't looked up by path, only by id.
    fn move_collection(
        &self,
        collection_id: i64,
        new_parent_id: Option<i64>,
    ) -> Result<(), CatalogError>;

    /// Deletes a collection and its subtree. For a `Manual` collection this also deletes its
    /// `collection_asset` membership rows — the assets themselves are untouched, only their
    /// membership in this collection.
    fn delete_collection(&self, collection_id: i64) -> Result<(), CatalogError>;

    /// Appends every listed asset to a `Manual` collection, in the given order, after whatever's
    /// already there. A no-op on an asset already a member (its existing position is untouched,
    /// not moved to the end).
    fn add_to_collection(&self, collection_id: i64, asset_ids: &[i64]) -> Result<(), CatalogError>;

    fn remove_from_collection(
        &self,
        collection_id: i64,
        asset_ids: &[i64],
    ) -> Result<(), CatalogError>;

    /// Every asset in a `Manual` collection, in position order.
    fn collection_assets(&self, collection_id: i64) -> Result<Vec<i64>, CatalogError>;

    /// Sets (replacing any prior one) the saved `Filter` a `Smart` collection resolves to.
    fn set_smart_rule(&self, collection_id: i64, filter: &Filter) -> Result<(), CatalogError>;

    /// The saved `Filter` for a `Smart` collection — `None` if it has no rule set yet (or if
    /// `collection_id` names a `Manual` collection, which has no rule at all).
    fn collection_filter(&self, collection_id: i64) -> Result<Option<Filter>, CatalogError>;
}

/// Registry of catalog store modules, keyed by namespaced id.
pub type StoreRegistry = Registry<dyn CatalogStore>;

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_claw::Descriptor;
    use std::sync::Arc;

    #[test]
    fn sqlite_catalog_registers_and_resolves_as_trait_object() {
        let mut registry: StoreRegistry = Registry::new();
        registry
            .register(
                Descriptor {
                    id: "nicti.catalog.sqlite",
                    schema_version: 1,
                },
                || Arc::new(SqliteCatalog::open_in_memory().expect("in-memory catalog opens")),
            )
            .expect("registration should succeed");

        let resolved = registry
            .get("nicti.catalog.sqlite")
            .expect("sqlite catalog store is registered");
        assert_eq!(resolved.id(), "nicti.catalog.sqlite");
    }
}
