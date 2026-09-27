//! Catalog store extension point (ADR-0019 §7/§8) and its real implementation (#22): the schema
//! ADR-0067/0103/0071/0021 settled (see `schema.rs`), a `rusqlite`-backed `CatalogStore`
//! (`sqlite.rs`), and the Scruff import/ingest pipeline (`scruff.rs`) that scans a folder, fingerprints
//! and upserts each asset, and extracts its T0 grid preview at import time (ADR-0029) — carrying a
//! kitten by the scruff of its neck is how it gets moved into the catalog.

use nicti_claw::{Module, Registry};

mod model;
pub mod schema;
mod sqlite;

pub mod scruff;

pub use model::{Asset, NewAsset, Preview, PreviewTier};
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

    fn find_asset_by_path(
        &self,
        root_id: i64,
        rel_path: &str,
    ) -> Result<Option<Asset>, CatalogError>;

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

    /// Reads the trigger-maintained `(model, rating)` facet count (ADR-0103) — used by ingest's
    /// tests to check the triggers stay consistent, and by any future facet-filtered browse view.
    fn facet_count(&self, model: Option<&str>, rating: i64) -> Result<u64, CatalogError>;
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
