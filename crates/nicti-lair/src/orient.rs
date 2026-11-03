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
const JPEG_HEAD_BYTES: u64 = 1024 * 1024;

/// True when the JPEG segment walk reaches the start of scan (SOS) inside `head`, i.e. every header
/// segment (where EXIF lives) was fully read. False when `head` ends first (a truncated read).
fn headers_fully_read(head: &[u8]) -> bool {
    if head.get(..2) != Some(&[0xFF, 0xD8]) {
        return false;
    }
    let mut pos = 2usize;
    loop {
        if head.get(pos) != Some(&0xFF) {
            return false;
        }
        // Fill bytes: any number of 0xFF may precede the marker code.
        while head.get(pos + 1) == Some(&0xFF) {
            pos += 1;
        }
        let Some(&marker) = head.get(pos + 1) else {
            return false;
        };
        match marker {
            0xDA => return true,
            0x01 | 0xD0..=0xD9 => pos += 2,
            _ => {
                let (Some(&hi), Some(&lo)) = (head.get(pos + 2), head.get(pos + 3)) else {
                    return false;
                };
                pos += 2 + usize::from(u16::from_be_bytes([hi, lo]));
            }
        }
    }
}

/// The EXIF Orientation of the file at `path`, 1-8 (1 for upright, untagged or out-of-range), or
/// `None` when it could not be established: the file is unreadable, or (JPEG) its header segments
/// were not fully read, or (RAW) its IFD0 could not be parsed. `None` is stored as NULL so the
/// backfill retries; a wrong 1 would be permanent.
pub fn file_orientation(path: &Path) -> Option<u8> {
    let raw = if is_jpeg(path) {
        let mut head = Vec::new();
        std::fs::File::open(path)
            .ok()?
            .take(JPEG_HEAD_BYTES)
            .read_to_end(&mut head)
            .ok()?;
        match jpeg_orientation(&head) {
            Some(o) => Some(o),
            // No readable tag: only "untagged" if the walk proves nothing was cut off.
            None if headers_fully_read(&head) => None,
            None => return None,
        }
    } else {
        let mut walker = Walker::new(FileSource::open(path).ok()?).ok()?;
        walker.ifd0_orientation().ok()?
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

    /// A tiny real JPEG with an APP1/EXIF segment holding only `Orientation = value`.
    fn tagged_jpeg(value: u16) -> Vec<u8> {
        use image::ImageEncoder;
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
            .write_image(&[90u8; 8 * 8 * 3], 8, 8, image::ExtendedColorType::Rgb8)
            .unwrap();
        let mut tiff = b"II*\0\x08\0\0\0".to_vec();
        tiff.extend_from_slice(&1u16.to_le_bytes());
        tiff.extend_from_slice(&0x0112u16.to_le_bytes());
        tiff.extend_from_slice(&3u16.to_le_bytes());
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&value.to_le_bytes());
        tiff.extend_from_slice(&[0, 0]);
        tiff.extend_from_slice(&0u32.to_le_bytes());
        let mut out = jpeg[..2].to_vec();
        out.extend_from_slice(&[0xFF, 0xE1]);
        out.extend_from_slice(&((tiff.len() + 8) as u16).to_be_bytes());
        out.extend_from_slice(b"Exif\0\0");
        out.extend_from_slice(&tiff);
        out.extend_from_slice(&jpeg[2..]);
        out
    }

    /// #652: the catalog column (kamadak-exif, 1-8) and the decoder's frame orientation (the
    /// `image` crate, 0 or 2-8) read the same tag; they may differ only by 1 <-> 0.
    #[test]
    fn catalog_and_decoder_agree_on_every_orientation_tag() {
        use nicti_cornea::RawDecoder;
        let dir = tempfile::tempdir().unwrap();
        for tag in 1u16..=9 {
            let p = dir.path().join(format!("o{tag}.jpg"));
            std::fs::write(&p, tagged_jpeg(tag)).unwrap();
            let catalog = file_orientation(&p).unwrap();
            let frame = nicti_cornea::JpegDecoder
                .decode_linear(&p)
                .unwrap()
                .orientation;
            assert_eq!(
                catalog.saturating_sub(1).min(7) + 1,
                catalog,
                "catalog is 1-8"
            );
            let want = if catalog == 1 { 0 } else { catalog };
            assert_eq!(frame, want, "EXIF tag {tag}");
        }
    }

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
    fn a_truncated_jpeg_is_unknown_not_upright() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.jpg");
        let img = image::RgbImage::from_pixel(4, 4, image::Rgb([1, 2, 3]));
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut bytes)
            .encode(img.as_raw(), 4, 4, image::ExtendedColorType::Rgb8)
            .unwrap();
        // Cut inside the header segments: nothing proves the file is untagged.
        std::fs::write(&p, &bytes[..12]).unwrap();
        assert_eq!(file_orientation(&p), None);
        // Not a JPEG at all.
        std::fs::write(&p, b"garbage").unwrap();
        assert_eq!(file_orientation(&p), None);
    }

    #[test]
    fn the_segment_walk_distinguishes_complete_headers_from_a_cut() {
        let full = [
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00, 0xFF, 0xDA, 0x00,
        ];
        assert!(headers_fully_read(&full));
        assert!(!headers_fully_read(&full[..7]));
        assert!(!headers_fully_read(&[0x00, 0x01]));
    }

    #[test]
    fn ingest_records_a_jpeg_assets_orientation() {
        use crate::CatalogStore;
        let dir = tempfile::tempdir().unwrap();
        let img = image::RgbImage::from_pixel(8, 8, image::Rgb([1, 2, 3]));
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut bytes)
            .encode(img.as_raw(), 8, 8, image::ExtendedColorType::Rgb8)
            .unwrap();
        std::fs::write(
            dir.path().join("p.jpg"),
            crate::scruff::tag_orientation(bytes, 8),
        )
        .unwrap();
        let store = crate::SqliteCatalog::open_in_memory().unwrap();
        let vol = store.upsert_volume("v", None, None, 0).unwrap();
        let root = store
            .ensure_root(vol, &dir.path().to_string_lossy())
            .unwrap();
        crate::scruff::ingest_root(&store, root, dir.path()).unwrap();
        let a = store.find_asset_by_path(root, "p.jpg").unwrap().unwrap();
        assert_eq!(a.orientation, Some(8));
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
