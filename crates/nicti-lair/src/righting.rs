//! Righting: backfill the EXIF Orientation onto T0 thumbnails ingested before #476 (#561).
//!
//! Since #476/#560 a new ingest tags a RAW's T0 JPEG with the RAW's IFD0 Orientation (the embedded
//! preview carries none of its own), so rotated photos show upright. But `ingest_one` skips a file
//! whose size and mtime are unchanged, so T0s already in a catalog -- and the `.thumb.jpg` sidecars
//! made from them -- stay sideways until the file changes. This module finds those assets (a RAW
//! whose IFD0 Orientation is 2-8 and whose T0 has none) and tags the **existing bytes** in place: no
//! RAW preview re-extraction, the same pixels, just the EXIF segment [`tag_orientation`] adds at
//! ingest. It is idempotent (a tagged T0 is left alone), so a crash or a re-run only does what is
//! left, and it brings the catalog blob and the sidecar into line together:
//!
//! - a catalog T0 blob without the tag is rewritten tagged;
//! - a JPEG sidecar without the tag is rewritten tagged (atomically, [`sidecar::export`]);
//! - an AVIF sidecar cannot carry the tag (AVIF encode refuses a rotated T0, #476), so it is
//!   replaced by a JPEG sidecar of the corrected T0.
//!
//! JPEG assets are skipped: their own EXIF travels with the file. The trigger (a startup or Library
//! action and a once-per-catalog marker) is the caller's; [`righting_asset`] is the per-asset step so
//! it can be spread over job chunks, [`backfill_root`] the whole-root loop.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use nicti_cornea::embedded::{FileSource, Walker};

use crate::scruff::{extract_t0_preview, is_jpeg, jpeg_orientation, tag_orientation};
use crate::thumb_sidecar::{self as sidecar, SidecarCodec};
use crate::{Asset, CatalogError, CatalogStore, Preview, PreviewTier};

/// What a righting run did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RightingReport {
    pub examined: u64,
    /// Catalog T0 blobs rewritten with the tag.
    pub fixed_blobs: u64,
    /// Sidecars rewritten (a tagged JPEG, or an AVIF replaced by a JPEG).
    pub fixed_sidecars: u64,
    /// Needs rotation but every thumbnail it has already carries the tag (a T0 with its own
    /// Orientation, even a different one, is left alone, exactly as ingest does).
    pub already_right: u64,
    /// JPEG assets, and RAWs with no Orientation or Orientation 1.
    pub no_rotation: u64,
    /// Has no catalog T0 and no sidecar (nothing to fix).
    pub no_thumbnail: u64,
    /// The RAW is gone from disk since the last sync.
    pub missing_file: u64,
    /// The RAW on disk no longer matches the catalog row (size or mtime): its T0 is from older
    /// content, so tagging it with the new file's rotation would be wrong. The next ingest/sync
    /// re-extracts (and tags) it; run righting again afterwards.
    pub stale_file: u64,
    /// The catalog blob changed between reading and writing it (an ingest replaced it): left alone.
    pub raced: u64,
    /// The root's folder is not there (an unmounted drive): nothing was examined or touched, and
    /// every asset would otherwise look like a deleted file.
    pub root_offline: bool,
    /// Things that could not be fixed, with why. Never silently dropped.
    pub failed: Vec<(PathBuf, String)>,
}

impl RightingReport {
    /// Whether a caller may record "this root has been righted" (a once-per-catalog marker): the
    /// root was reachable and nothing was left undone. Deleted files (`missing_file`) and RAWs
    /// that need no rotation are not leftovers.
    pub fn can_mark_done(&self) -> bool {
        !self.root_offline && self.failed.is_empty() && self.stale_file == 0 && self.raced == 0
    }
}

/// The RAW's IFD0 Orientation when it is a rotation (2-8). `Ok(None)` for none, 1, or an odd tag.
fn raw_rotation(raw: &Path) -> Result<Option<u8>, String> {
    let source = FileSource::open(raw).map_err(|e| format!("opening: {e:?}"))?;
    let mut walker = Walker::new(source).map_err(|e| format!("reading TIFF header: {e:?}"))?;
    Ok(walker
        .ifd0_orientation()
        .ok()
        .flatten()
        .filter(|o| (2..=8).contains(o)))
}

/// The file's mtime the way ingest records it (whole seconds since the epoch).
fn mtime_unix(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64)
}

/// One asset's righting step. Store errors propagate; a problem with the file itself is recorded
/// in `report.failed` instead, so one bad file never aborts a root. Run it serialised with ingest
/// of the same root (the blob is re-checked just before it is replaced, which narrows but cannot
/// close a race with a concurrent ingest).
pub fn righting_asset(
    store: &dyn CatalogStore,
    root_path: &Path,
    asset: &Asset,
    report: &mut RightingReport,
) -> Result<(), CatalogError> {
    report.examined += 1;
    let rel = Path::new(&asset.rel_path);
    if is_jpeg(rel) {
        report.no_rotation += 1;
        return Ok(());
    }
    let raw = root_path.join(rel);
    let Ok(meta) = std::fs::metadata(&raw) else {
        report.missing_file += 1;
        return Ok(());
    };
    if !meta.is_file() {
        report.missing_file += 1;
        return Ok(());
    }
    // The catalog's T0 is of the content ingest saw; if the file changed since, its rotation is not
    // this thumbnail's.
    if meta.len() != asset.size_bytes || mtime_unix(&meta) != asset.mtime_unix {
        report.stale_file += 1;
        return Ok(());
    }
    let o = match raw_rotation(&raw) {
        Ok(Some(o)) => o,
        Ok(None) => {
            report.no_rotation += 1;
            return Ok(());
        }
        Err(why) => {
            report.failed.push((raw, why));
            return Ok(());
        }
    };

    let blob = store.get_preview(asset.id, PreviewTier::T0)?;
    let side = sidecar::read_with_codec(&raw, SidecarCodec::Jpeg);
    if blob.is_none() && side.is_none() {
        report.no_thumbnail += 1;
        return Ok(());
    }

    let failed_before = report.failed.len();
    let mut changed = false;
    // 1. The catalog blob. Its corrected bytes are also the source for an AVIF sidecar's
    // replacement, but only when they really are a JPEG.
    let mut corrected: Option<Preview> = None;
    if let Some(b) = blob {
        let t = tag_orientation(b.bytes.clone(), o);
        if t == b.bytes && jpeg_orientation(&b.bytes).is_none() {
            report
                .failed
                .push((raw.clone(), "catalog T0 is not a taggable JPEG".into()));
        } else {
            let fixed = Preview {
                width: b.width,
                height: b.height,
                bytes: t,
            };
            if fixed.bytes != b.bytes {
                let now = store.get_preview(asset.id, PreviewTier::T0)?;
                if now.as_ref().map(|p| &p.bytes) != Some(&b.bytes) {
                    report.raced += 1;
                    return Ok(());
                }
                store.put_preview(asset.id, PreviewTier::T0, &fixed)?;
                report.fixed_blobs += 1;
                changed = true;
            }
            corrected = Some(fixed);
        }
    }
    // 2. The sidecar, in step with the blob.
    if let Some((codec, s)) = side {
        let replacement = match codec {
            SidecarCodec::Jpeg => {
                let t = tag_orientation(s.bytes.clone(), o);
                if t == s.bytes {
                    if jpeg_orientation(&s.bytes).is_none() {
                        report
                            .failed
                            .push((raw.clone(), "sidecar is not a taggable JPEG".into()));
                    }
                    None
                } else {
                    Some(Preview {
                        width: s.width,
                        height: s.height,
                        bytes: t,
                    })
                }
            }
            // An AVIF can't hold the tag: a JPEG of the corrected T0 replaces it (`export` removes
            // the AVIF; the folder's codec becomes JPEG). With no usable catalog blob (archived),
            // the RAW's own preview is re-extracted, which is tagged at extraction.
            SidecarCodec::Avif => corrected
                .clone()
                .or_else(|| extract_t0_preview(&raw))
                .or_else(|| {
                    report.failed.push((
                        raw.clone(),
                        "an AVIF sidecar cannot carry the rotation and no JPEG T0 was available"
                            .into(),
                    ));
                    None
                }),
        };
        if let Some(p) = replacement {
            match sidecar::export(&raw, &p, SidecarCodec::Jpeg) {
                Ok(_) => {
                    report.fixed_sidecars += 1;
                    changed = true;
                }
                Err(e) => report
                    .failed
                    .push((raw.clone(), format!("writing sidecar: {e}"))),
            }
        }
    }
    if !changed && report.failed.len() == failed_before {
        report.already_right += 1;
    }
    Ok(())
}

/// [`righting_asset`] over every asset of `root_id`. An unreachable root (an unmounted drive) is
/// reported as `root_offline` and nothing is touched, so a caller never marks it done.
pub fn backfill_root(
    store: &dyn CatalogStore,
    root_id: i64,
) -> Result<RightingReport, CatalogError> {
    let mut report = RightingReport::default();
    let Some(root) = store.list_roots()?.into_iter().find(|r| r.id == root_id) else {
        return Ok(report);
    };
    if !Path::new(&root.path).is_dir() {
        report.root_offline = true;
        return Ok(report);
    }
    for asset in store.list_assets_by_root(root_id)? {
        righting_asset(store, Path::new(&root.path), &asset, &mut report)?;
    }
    Ok(report)
}
