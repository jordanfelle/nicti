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
    /// The whole-folder `fs::rename` fast path is in flight or just landed: the folder is at
    /// the source, the destination, or (if the source path was since recreated) both -- recovery
    /// must never delete either side on its own.
    Renaming,
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

/// The three culling markers on an asset (#32): star rating, pick flag, and colour label. A unit
/// so the culling UI can snapshot, restore and diff all three at once (`CatalogStore::get_meta`
/// / `set_meta`), instead of round-tripping whole [`Asset`] rows. Same value domains as the
/// matching `Asset` fields.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AssetMeta {
    /// `None` = unrated, `Some(-1)` = reject, `Some(0..=5)` = star rating.
    pub rating: Option<i64>,
    /// `None` = unflagged, `Some(1)` = pick.
    pub flag: Option<i64>,
    pub label: Option<String>,
}

/// Journal state of one asset in an in-flight delete (#32).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteState {
    /// Recorded before any file was touched: the RAW (and its sidecar) may or may not be in the
    /// Recycle Bin yet.
    Pending,
    /// The files are in the Recycle Bin; only the catalog row removal remains.
    Trashed,
}

/// What nicti last saw and wrote in an asset's XMP sidecar (#60, `asset_sidecar` table). Hashes are
/// raw BLAKE3 bytes (32 each); `None` means "never seen"/"never written".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SidecarState {
    pub path: String,
    pub last_seen_hash: Option<Vec<u8>>,
    pub last_seen_mtime_ms: Option<i64>,
    pub last_written_hash: Option<Vec<u8>>,
    /// When the catalog's markers last diverged from the sidecar unwritten; `None` = in sync.
    pub catalog_dirty_since_ms: Option<i64>,
    /// Both sides changed within the ambiguity window; neither was overwritten.
    pub needs_review: bool,
}

/// Everything an LRC import keeps about one source image beyond what nicti models (#62, the
/// `lrc_provenance` table, schema v10). `develop_text` is the verbatim Lua so a later translator
/// can re-run without the `.lrcat`; `untranslated` is the list of develop keys the import saw but
/// could not map to a nicti stage.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LrcProvenance {
    /// `Adobe_images.id_global` -- the idempotency key for re-runs.
    pub image_global: String,
    /// `Adobe_images.id_local`.
    pub image_local: i64,
    pub import_hash: Option<String>,
    pub process_version: Option<String>,
    pub develop_text: Option<String>,
    pub has_masks: Option<bool>,
    pub has_ai_masks: Option<bool>,
    pub has_big_data: Option<bool>,
    pub iptc_caption: Option<String>,
    pub iptc_copyright: Option<String>,
    pub lrc_rating: Option<f64>,
    pub lrc_pick: Option<f64>,
    pub untranslated: Vec<String>,
}

/// One LRC image to apply in [`crate::CatalogStore::apply_lrc_chunk`].
#[derive(Debug, Clone)]
pub struct LrcItem {
    pub asset_id: i64,
    /// `None` for a virtual copy: nicti's markers are per asset, so only the master image's
    /// rating/flag/label are applied (the copy's own go to provenance only).
    pub meta: Option<AssetMeta>,
    /// `None` = the asset's master edit variant; `Some(name)` = a virtual copy's own variant
    /// (created on first apply, found again by provenance on a re-run).
    pub variant_name: Option<String>,
    /// The translated edit document. `None` leaves the variant's document untouched.
    pub document: Option<nicti_pawprint::EditDocument>,
    pub provenance: LrcProvenance,
}

/// What [`crate::CatalogStore::apply_lrc_chunk`] did, for the import report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LrcChunkOutcome {
    pub docs_written: u64,
    /// A re-run found the variant's document no longer matches what the import last wrote (or, on
    /// a first import, the master already carries edits) and left it alone.
    pub kept_local_docs: u64,
    pub meta_applied: u64,
    /// Same guard for rating/flag/label.
    pub kept_local_meta: u64,
    pub variants_created: u64,
    /// Assets whose rating/flag/label actually changed -- the caller marks these catalog-dirty so
    /// the XMP sidecar sync does not revert them.
    pub meta_changed_assets: Vec<i64>,
}

/// One open `delete_item` journal row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteItem {
    pub asset_id: i64,
    /// Absolute path of the RAW file, as it was when the delete began (the row itself may be gone
    /// by recovery time, so the path is journaled, not re-derived).
    pub abs_path: String,
    pub state: DeleteState,
}
