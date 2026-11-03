//! Lens model and ISO backfill (#547): the `{Lens}` / `{ISO}` naming tokens read `asset.lens` /
//! `asset.iso`, which ingest fills for new and changed files. Ingest skips a file whose size and
//! mtime are unchanged, so rows catalogued before schema v13 stay unread until this pass visits
//! them (`asset.exif_extra_read = 0`). A file whose EXIF can't be read stays unread and is retried
//! by the next run; a readable file that simply has no lens/ISO is recorded as "read, none".
//!
//! Lens comes from the standard EXIF `LensModel` (0xA434) only. Older bodies that predate EXIF 2.3
//! carry the lens in the Nikon MakerNote instead (`nicti-cornea`'s `find_nikon_lens_info` locates
//! it, `nicti-iris` decodes it); that fallback is not wired here and is unverified against a real
//! NEF (no fixture in this repo), so such a file reads as "no lens".

use std::path::Path;

use crate::{Asset, CatalogError, CatalogStore};

/// What a backfill run did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LensIsoReport {
    pub examined: u64,
    /// Recorded (lens and/or ISO found, or read and genuinely absent).
    pub filled: u64,
    /// The file is gone or its EXIF is unreadable: left unread.
    pub unreadable: u64,
    /// The root's folder is not there (an unmounted drive): nothing was touched.
    pub root_offline: bool,
}

/// Reads and records lens/ISO for one asset. `Ok(true)` when it was recorded.
pub fn fill_asset(
    store: &dyn CatalogStore,
    root_path: &Path,
    asset: &Asset,
) -> Result<bool, CatalogError> {
    match crate::scruff::read_exif_extra(&root_path.join(&asset.rel_path)) {
        Some((lens, iso)) => {
            store.set_exif_extra(asset.id, lens.as_deref(), iso)?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Fills every unread lens/ISO under `root_id`, `chunk` assets at a time.
pub fn backfill_root(
    store: &dyn CatalogStore,
    root_id: i64,
    chunk: usize,
) -> Result<LensIsoReport, CatalogError> {
    let mut report = LensIsoReport::default();
    let Some(root) = store.list_roots()?.into_iter().find(|r| r.id == root_id) else {
        return Ok(report);
    };
    let root_path = Path::new(&root.path);
    if !root_path.is_dir() {
        report.root_offline = true;
        return Ok(report);
    }
    let mut after = 0i64;
    loop {
        let batch = store.assets_missing_exif_extra(root_id, after, chunk.max(1))?;
        let Some(last) = batch.last() else { break };
        after = last.id;
        for asset in &batch {
            report.examined += 1;
            if fill_asset(store, root_path, asset)? {
                report.filled += 1;
            } else {
                report.unreadable += 1;
            }
        }
    }
    Ok(report)
}
