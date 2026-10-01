//! Tiered thumbnail storage (#72, ADR-0072): while a folder is under active edit its T0 grid
//! thumbnails live in the catalog; once it sits on an archive drive they live as `.thumb.jpg`
//! sidecars next to the RAWs instead, so the archive can be browsed with no central-cache read.
//!
//! The functions here are the idempotent pieces [`crate::carry::Carry`] and the app compose:
//! export before an archive move, [`settle_root`] after any move (and at startup), and
//! [`load_t0`] on the read path. Every step falls back rather than deletes, so no crash window
//! leaves a folder without thumbnails: a catalog blob is only dropped once its sidecar is on
//! disk, and a sidecar is only removed once its blob is back in the catalog.

use std::path::Path;

use crate::thumb_sidecar::{self as sidecar, SidecarCodec};
use crate::{Asset, CatalogError, CatalogStore, Preview, PreviewTier};

const CODEC: SidecarCodec = SidecarCodec::Jpeg;

/// Writes `asset`'s catalog T0 thumbnail as a sidecar next to its RAW. `Ok(false)` if the catalog
/// holds none (nothing to export).
pub fn export_asset_sidecar(
    store: &dyn CatalogStore,
    root_path: &Path,
    asset: &Asset,
) -> Result<bool, String> {
    let Some(pv) = store
        .get_preview(asset.id, PreviewTier::T0)
        .map_err(|e| e.to_string())?
    else {
        return Ok(false);
    };
    let raw = root_path.join(&asset.rel_path);
    if !raw.is_file() {
        // Gone from disk since the last sync: nothing to sit next to, and not this move's problem.
        return Ok(false);
    }
    sidecar::export(&raw, &pv, CODEC).map_err(|e| format!("{}: {e}", raw.display()))?;
    Ok(true)
}

/// What [`settle_root`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SettleReport {
    /// Archived root: catalog blobs dropped because their sidecar is on disk.
    pub blobs_dropped: u64,
    /// Active root: sidecars folded back into the catalog and deleted.
    pub sidecars_ingested: u64,
    /// Assets left as they were because the other tier isn't there yet (sidecar missing on an
    /// archived root, e.g. the drive is unmounted).
    pub kept: u64,
}

/// Brings one root's thumbnails in line with its `archived` flag. Idempotent; safe to re-run
/// after a crash or on every start.
pub fn settle_root(store: &dyn CatalogStore, root_id: i64) -> Result<SettleReport, CatalogError> {
    let mut report = SettleReport::default();
    let Some(root) = store.list_roots()?.into_iter().find(|r| r.id == root_id) else {
        return Ok(report);
    };
    for asset in store.list_assets_by_root(root_id)? {
        settle_asset(
            store,
            Path::new(&root.path),
            root.archived,
            &asset,
            &mut report,
        )?;
    }
    Ok(report)
}

/// [`settle_root`]'s per-asset step, so a caller can spread it over job chunks.
pub fn settle_asset(
    store: &dyn CatalogStore,
    root_path: &Path,
    archived: bool,
    asset: &Asset,
    report: &mut SettleReport,
) -> Result<(), CatalogError> {
    let raw = root_path.join(&asset.rel_path);
    let side = sidecar::read(&raw, CODEC);
    let blob = store.get_preview(asset.id, PreviewTier::T0)?;
    if archived {
        match (side, blob) {
            // Only drop a blob its sidecar exactly reproduces; anything else is kept and the
            // read path (catalog first) keeps serving it.
            (Some(side), Some(blob)) if side.bytes == blob.bytes => {
                store.clear_preview(asset.id, PreviewTier::T0)?;
                report.blobs_dropped += 1;
            }
            (_, Some(_)) => report.kept += 1,
            _ => {}
        }
    } else if let Some(side) = side {
        if blob.is_none() {
            store.put_preview(asset.id, PreviewTier::T0, &side)?;
        }
        sidecar::remove(&raw, CODEC);
        report.sidecars_ingested += 1;
    }
    Ok(())
}

/// The T0 thumbnail for `asset`, from whichever tier the folder is in: sidecar first on an
/// archived root, catalog first on an active one, the other as the fallback either way.
pub fn load_t0(
    store: &dyn CatalogStore,
    root_path: &Path,
    archived: bool,
    asset: &Asset,
) -> Result<Option<Preview>, CatalogError> {
    let raw = root_path.join(&asset.rel_path);
    if archived {
        if let Some(p) = sidecar::read(&raw, CODEC) {
            return Ok(Some(p));
        }
        return store.get_preview(asset.id, PreviewTier::T0);
    }
    if let Some(p) = store.get_preview(asset.id, PreviewTier::T0)? {
        return Ok(Some(p));
    }
    Ok(sidecar::read(&raw, CODEC))
}

/// [`load_t0`] for a caller that only has an asset id (the grid, cull and loupe readers).
/// Catalog first, then the sidecar: an archived folder has no catalog blobs once it has settled
/// (see [`settle_root`]), so its reads land on the sidecar; a folder mid-transition is served
/// from whichever tier still holds the bytes.
pub fn load_t0_by_id(
    store: &dyn CatalogStore,
    asset_id: i64,
) -> Result<Option<Preview>, CatalogError> {
    if let Some(p) = store.get_preview(asset_id, PreviewTier::T0)? {
        return Ok(Some(p));
    }
    let Some(asset) = store.get_asset(asset_id)? else {
        return Ok(None);
    };
    let Some(root) = store.get_root_path(asset.root_id)? else {
        return Ok(None);
    };
    Ok(sidecar::read(
        &Path::new(&root).join(&asset.rel_path),
        CODEC,
    ))
}
