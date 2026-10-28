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

/// Preferred codec for reads; `read` falls back to the other, so a folder stays browsable
/// whichever codec it was exported in (#393).
const READ_PREF: SidecarCodec = SidecarCodec::Jpeg;

/// Writes `asset`'s catalog T0 thumbnail as a sidecar next to its RAW in `codec` (AVIF falls back
/// to JPEG for an ICC-tagged T0, see [`sidecar::export`]). `Ok(false)` if the catalog
/// holds none (nothing to export).
pub fn export_asset_sidecar(
    store: &dyn CatalogStore,
    root_path: &Path,
    asset: &Asset,
    codec: SidecarCodec,
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
    sidecar::export(&raw, &pv, codec).map_err(|e| format!("{}: {e}", raw.display()))?;
    Ok(true)
}

/// What [`settle_root`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SettleReport {
    /// Archived root: catalog blobs dropped because their sidecar is on disk.
    pub blobs_dropped: u64,
    /// Active root: sidecars folded back into the catalog and deleted.
    pub sidecars_ingested: u64,
    /// Archived root: a sidecar rewritten from a catalog blob that had been re-tagged with the
    /// RAW's rotation (#561) while the sidecar still had the old untagged bytes (the blob is then
    /// dropped as usual); without this the byte-different pair would stay `kept` forever.
    pub sidecars_refreshed: u64,
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
    let side = sidecar::read_with_codec(&raw, READ_PREF);
    let blob = store.get_preview(asset.id, PreviewTier::T0)?;
    if archived {
        match (side, blob) {
            // Only drop a blob its sidecar reproduces; anything else is kept and the read path
            // (catalog first) keeps serving it. A JPEG sidecar must match byte for byte; an AVIF
            // one is a re-encode, so it must be a whole, parseable file of the blob's pixel size.
            (Some((codec, side)), Some(blob)) if sidecar_matches(codec, &side, &blob) => {
                store.clear_preview(asset.id, PreviewTier::T0)?;
                report.blobs_dropped += 1;
            }
            // The blob was re-tagged with the RAW's rotation (righting, #561) and the JPEG sidecar
            // is its untagged original: refresh the sidecar from the blob, then drop the blob.
            (Some((SidecarCodec::Jpeg, side)), Some(blob)) if is_retag_of(&side, &blob) => {
                sidecar::export(&raw, &blob, SidecarCodec::Jpeg)
                    .map_err(|e| CatalogError::Io(format!("{}: {e}", raw.display())))?;
                store.clear_preview(asset.id, PreviewTier::T0)?;
                report.sidecars_refreshed += 1;
                report.blobs_dropped += 1;
            }
            (_, Some(_)) => report.kept += 1,
            _ => {}
        }
    } else if let Some((codec, side)) = side {
        // The catalog's T0 is always the RAW's own embedded JPEG (ADR-0029): an AVIF sidecar is
        // never ingested, the original is re-extracted instead. If that fails the sidecar stays.
        let restored = match (blob.is_some(), codec) {
            (true, _) => true,
            (false, SidecarCodec::Jpeg) => {
                store.put_preview(asset.id, PreviewTier::T0, &side)?;
                true
            }
            (false, SidecarCodec::Avif) => match crate::scruff::extract_t0_preview(&raw) {
                Some(pv) => {
                    store.put_preview(asset.id, PreviewTier::T0, &pv)?;
                    true
                }
                None => false,
            },
        };
        if restored {
            sidecar::remove(&raw);
            report.sidecars_ingested += 1;
        } else {
            report.kept += 1;
        }
    }
    Ok(())
}

/// `blob` is `side` plus the EXIF Orientation segment righting adds (and nothing else), and `side`
/// has no Orientation of its own: the sidecar is the stale, untagged original of the blob.
fn is_retag_of(side: &Preview, blob: &Preview) -> bool {
    use crate::scruff::{jpeg_orientation, tag_orientation};
    let Some(o) = jpeg_orientation(&blob.bytes) else {
        return false;
    };
    jpeg_orientation(&side.bytes).is_none() && tag_orientation(side.bytes.clone(), o) == blob.bytes
}

fn sidecar_matches(codec: SidecarCodec, side: &Preview, blob: &Preview) -> bool {
    match codec {
        SidecarCodec::Jpeg => side.bytes == blob.bytes,
        SidecarCodec::Avif => sidecar::avif_matches_jpeg(&side.bytes, &blob.bytes),
    }
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
        if let Some(p) = sidecar::read(&raw, READ_PREF) {
            return Ok(Some(p));
        }
        return store.get_preview(asset.id, PreviewTier::T0);
    }
    if let Some(p) = store.get_preview(asset.id, PreviewTier::T0)? {
        return Ok(Some(p));
    }
    Ok(sidecar::read(&raw, READ_PREF))
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
        READ_PREF,
    ))
}

/// The RAW's path for `asset_id` (catalog lookups only, no disk access), so a caller can hand the
/// sidecar read -- which touches a possibly offline archive drive -- to a worker thread.
pub fn raw_path_for(
    store: &dyn CatalogStore,
    asset_id: i64,
) -> Result<Option<std::path::PathBuf>, CatalogError> {
    let Some(asset) = store.get_asset(asset_id)? else {
        return Ok(None);
    };
    Ok(store
        .get_root_path(asset.root_id)?
        .map(|root| Path::new(&root).join(&asset.rel_path)))
}

/// Reads the sidecar for `raw` (the disk half of [`load_t0_by_id`]).
pub fn read_sidecar(raw: &Path) -> Option<Preview> {
    sidecar::read(raw, READ_PREF)
}
