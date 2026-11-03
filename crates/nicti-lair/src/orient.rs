//! Orientation of cataloged files (#604).
//!
//! The justified grid lays photos out by oriented aspect ratio, but `asset.width/height` are the
//! stored (pre-rotation) size. `asset.orientation` holds the EXIF Orientation, 1-8 (1 = upright or
//! no tag), `NULL` until read. Ingest fills it for new and changed files ([`file_orientation`]);
//! [`backfill_root`] fills rows ingested before the column existed, since ingest skips files whose
//! size and mtime are unchanged. Both are idempotent and tolerate unreadable files (the value stays
//! `NULL` and a later pass retries).

use std::io::Read;
use std::path::Path;

use nicti_cornea::embedded::{FileSource, Walker};

use crate::scruff::{is_jpeg, jpeg_orientation};
use crate::{Asset, CatalogError, CatalogStore};

/// How much of a JPEG is read to find its EXIF APP1 segment (it sits right after SOI).
const JPEG_HEAD_BYTES: u64 = 128 * 1024;

/// The EXIF Orientation of the file at `path`, 1-8 (1 for upright, untagged or out-of-range), or
/// `None` when the file can't be read at all.
pub fn file_orientation(path: &Path) -> Option<u8> {
    let raw = if is_jpeg(path) {
        let mut head = Vec::new();
        std::fs::File::open(path)
            .ok()?
            .take(JPEG_HEAD_BYTES)
            .read_to_end(&mut head)
            .ok()?;
        jpeg_orientation(&head)
    } else {
        let mut walker = Walker::new(FileSource::open(path).ok()?).ok()?;
        walker.ifd0_orientation().ok().flatten()
    };
    Some(raw.filter(|o| (2..=8).contains(o)).unwrap_or(1))
}

/// What a backfill run did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OrientReport {
    pub examined: u64,
    pub filled: u64,
    /// The file is gone or unreadable: left `NULL`.
    pub unreadable: u64,
    /// The root's folder is not there (an unmounted drive): nothing was touched.
    pub root_offline: bool,
}

/// Reads and records the orientation of one asset. `Ok(true)` when it was filled.
pub fn orient_asset(
    store: &dyn CatalogStore,
    root_path: &Path,
    asset: &Asset,
) -> Result<bool, CatalogError> {
    let path = root_path.join(&asset.rel_path);
    match file_orientation(&path) {
        Some(o) => {
            store.set_orientation(asset.id, o)?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Fills every NULL orientation under `root_id`, `chunk` assets at a time (the caller's job can
/// call this with a small chunk and `after_id` itself; this is the whole-root loop).
pub fn backfill_root(
    store: &dyn CatalogStore,
    root_id: i64,
    chunk: usize,
) -> Result<OrientReport, CatalogError> {
    let mut report = OrientReport::default();
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
        let batch = store.assets_missing_orientation(root_id, after, chunk.max(1))?;
        let Some(last) = batch.last() else { break };
        after = last.id;
        for asset in &batch {
            report.examined += 1;
            if orient_asset(store, root_path, asset)? {
                report.filled += 1;
            } else {
                report.unreadable += 1;
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unreadable_file_has_no_orientation() {
        assert_eq!(file_orientation(Path::new("/nonexistent/x.jpg")), None);
        assert_eq!(file_orientation(Path::new("/nonexistent/x.nef")), None);
    }

    #[test]
    fn a_jpeg_without_exif_is_upright() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.jpg");
        let img = image::RgbImage::from_pixel(4, 4, image::Rgb([1, 2, 3]));
        img.save(&p).unwrap();
        assert_eq!(file_orientation(&p), Some(1));
    }

    #[test]
    fn a_tagged_jpeg_reports_its_orientation() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.jpg");
        let img = image::RgbImage::from_pixel(4, 4, image::Rgb([1, 2, 3]));
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut bytes)
            .encode(img.as_raw(), 4, 4, image::ExtendedColorType::Rgb8)
            .unwrap();
        std::fs::write(&p, crate::scruff::tag_orientation(bytes, 6)).unwrap();
        assert_eq!(file_orientation(&p), Some(6));
    }
}
