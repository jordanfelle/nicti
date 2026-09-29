//! Row types the `CatalogStore` trait's methods pass in and out — kept separate from `sqlite.rs`
//! so a future non-SQLite backend (DuckDB, ADR-0067's named fallback) can implement the same
//! trait against the same shapes.

/// One asset row, as read back from the catalog.
#[derive(Debug, Clone, PartialEq)]
pub struct Asset {
    pub id: i64,
    pub root_id: i64,
    pub rel_path: String,
    pub rel_path_fold: String,
    pub size_bytes: u64,
    pub mtime_unix: i64,
    pub fingerprint: Option<String>,
    pub natural_key: Option<String>,
    pub make: Option<String>,
    pub model: Option<String>,
    pub captured_at: Option<String>,
    /// `None` = unrated, `Some(-1)` = reject, `Some(0..=5)` = star rating (ADR-0059/0061).
    pub rating: Option<i64>,
    /// `None` = unflagged, `Some(1)` = pick.
    pub flag: Option<i64>,
    pub label: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub imported_at: i64,
    /// Set (to a unix timestamp) when a sync (#24's `patrol::sync_root`) walked this asset's root
    /// and found its file gone from disk. Cleared back to `None` if the file reappears at the same
    /// path on a later sync, or if `relink_asset` re-points this row to a moved file. `None` means
    /// present — ingest never sets this field itself, only `patrol` reads/writes it.
    pub missing_since: Option<i64>,
}

/// A new (or re-scanned) asset, as ingest builds it. Passed to
/// [`crate::CatalogStore::insert_asset`], which upserts on `(root_id, rel_path)` — a re-scan of a
/// path whose stat/fingerprint/EXIF changed updates the existing row in place rather than
/// inserting a duplicate.
#[derive(Debug, Clone)]
pub struct NewAsset {
    pub rel_path: String,
    pub rel_path_fold: String,
    pub size_bytes: u64,
    pub mtime_unix: i64,
    pub fingerprint: Option<String>,
    pub natural_key: Option<String>,
    pub make: Option<String>,
    pub model: Option<String>,
    pub captured_at: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub imported_at: i64,
}

/// Preview cache tiers a catalog store can hold. Only T0 (the grid preview, ADR-0029) is written
/// at import time — T1-T3 are a render-pipeline concern, not catalog storage, but the column
/// already carries a tier discriminator so a later ticket can add them without a schema change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewTier {
    T0,
}

impl PreviewTier {
    pub fn as_str(self) -> &'static str {
        match self {
            PreviewTier::T0 => "t0",
        }
    }
}

/// A stored preview's bytes plus the declared dimensions from the source IFD (not decoded from
/// the JPEG itself).
#[derive(Debug, Clone, PartialEq)]
pub struct Preview {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub bytes: Vec<u8>,
}

/// One node in the hierarchical keyword tree (#23). `path` is an id-based materialized path
/// (`/1/5/12/`, one segment per ancestor id including this keyword's own, each `/`-terminated) --
/// ids never change on rename, unlike a name-based path, and the trailing separator on every
/// segment means a subtree `GLOB` prefix scan (`path GLOB '/1/5/*'`) can't also match an
/// unrelated sibling whose id happens to share a numeric prefix (e.g. `/1/50/` under a naive
/// name-based scheme without the separator).
#[derive(Debug, Clone, PartialEq)]
pub struct Keyword {
    pub id: i64,
    pub parent_id: Option<i64>,
    pub name: String,
    pub path: String,
}

/// A registered folder (`root` row). `path` is the root's `rel_path` -- today an absolute path,
/// since every root sits under the shell's placeholder volume (see `nicti-pelt`'s
/// `register_root`).
#[derive(Debug, Clone, PartialEq)]
pub struct Root {
    pub id: i64,
    pub volume_id: i64,
    pub path: String,
    pub archived: bool,
}

/// Journal state of an in-flight verified folder move (#26, ADR-0026).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveState {
    /// Files are being copied/verified; the catalog still points at the source.
    Copying,
    /// The catalog already points at the destination; only source cleanup remains.
    Committed,
}

/// One open `root_move` journal row.
#[derive(Debug, Clone, PartialEq)]
pub struct RootMove {
    pub id: i64,
    pub root_id: i64,
    pub src_path: String,
    pub dest_path: String,
    pub state: MoveState,
}
