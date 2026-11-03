//! #547: lens model and ISO extracted at ingest and by the explicit backfill.

use std::path::Path;

use nicti_lair::lens_iso::backfill_root;
use nicti_lair::scruff::ingest_root;
use nicti_lair::{CatalogStore, NewAsset, SqliteCatalog};

/// A little-endian TIFF with IFD0 -> Exif IFD carrying ISO (0x8827), optionally ISOSpeed (0x8833)
/// and LensModel (0xA434).
fn exif_tiff(lens: Option<&str>, iso_short: u16, iso_long: Option<u32>) -> Vec<u8> {
    let mut entries: Vec<(u16, u16, u32, [u8; 4])> = Vec::new();
    entries.push((0x8827, 3, 1, {
        let mut v = [0u8; 4];
        v[..2].copy_from_slice(&iso_short.to_le_bytes());
        v
    }));
    if let Some(l) = iso_long {
        entries.push((0x8833, 4, 1, l.to_le_bytes()));
    }
    let n = entries.len() + usize::from(lens.is_some());
    let exif_ifd_len = 2 + n * 12 + 4;
    let data_at = 26 + exif_ifd_len;
    let mut tiff = Vec::new();
    tiff.extend_from_slice(b"II*\0");
    tiff.extend_from_slice(&8u32.to_le_bytes());
    // IFD0: one entry, the Exif IFD pointer (at 26).
    tiff.extend_from_slice(&1u16.to_le_bytes());
    for v in [0x8769u16, 4] {
        tiff.extend_from_slice(&v.to_le_bytes());
    }
    tiff.extend_from_slice(&1u32.to_le_bytes());
    tiff.extend_from_slice(&26u32.to_le_bytes());
    tiff.extend_from_slice(&0u32.to_le_bytes());
    assert_eq!(tiff.len(), 26);
    tiff.extend_from_slice(&(n as u16).to_le_bytes());
    let mut lens_bytes = Vec::new();
    if let Some(l) = lens {
        lens_bytes.extend_from_slice(l.as_bytes());
        lens_bytes.push(0);
    }
    let mut all: Vec<(u16, u16, u32, [u8; 4])> = entries;
    if lens.is_some() {
        all.push((
            0xA434,
            2,
            lens_bytes.len() as u32,
            (data_at as u32).to_le_bytes(),
        ));
    }
    for (tag, ty, count, val) in all {
        tiff.extend_from_slice(&tag.to_le_bytes());
        tiff.extend_from_slice(&ty.to_le_bytes());
        tiff.extend_from_slice(&count.to_le_bytes());
        tiff.extend_from_slice(&val);
    }
    tiff.extend_from_slice(&0u32.to_le_bytes());
    tiff.extend_from_slice(&lens_bytes);
    tiff
}

fn jpeg_with_exif(tiff: &[u8]) -> Vec<u8> {
    let mut base = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut base)
        .encode(&[90u8; 8 * 8 * 3], 8, 8, image::ExtendedColorType::Rgb8)
        .unwrap();
    let mut seg = vec![0xFF, 0xE1];
    seg.extend_from_slice(&((tiff.len() + 8) as u16).to_be_bytes());
    seg.extend_from_slice(b"Exif\0\0");
    seg.extend_from_slice(tiff);
    let mut out = base[..2].to_vec();
    out.extend_from_slice(&seg);
    out.extend_from_slice(&base[2..]);
    out
}

fn plain_jpeg() -> Vec<u8> {
    let mut base = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut base)
        .encode(&[90u8; 8 * 8 * 3], 8, 8, image::ExtendedColorType::Rgb8)
        .unwrap();
    base
}

fn catalog(dir: &Path) -> (SqliteCatalog, i64) {
    let store = SqliteCatalog::open_in_memory().unwrap();
    let vol = store.upsert_volume("v", None, None, 0).unwrap();
    let root = store.ensure_root(vol, &dir.to_string_lossy()).unwrap();
    (store, root)
}

#[test]
fn ingest_records_lens_and_iso_from_the_exif() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.jpg"),
        jpeg_with_exif(&exif_tiff(Some("NIKKOR Z 24-70mm f/4 S"), 800, None)),
    )
    .unwrap();
    std::fs::write(dir.path().join("b.jpg"), plain_jpeg()).unwrap();
    let (store, root) = catalog(dir.path());
    ingest_root(&store, root, dir.path()).unwrap();

    let a = store.find_asset_by_path(root, "a.jpg").unwrap().unwrap();
    assert_eq!(a.lens.as_deref(), Some("NIKKOR Z 24-70mm f/4 S"));
    assert_eq!(a.iso, Some(800));
    // A readable file without them is "read, none": the backfill must not revisit it.
    let b = store.find_asset_by_path(root, "b.jpg").unwrap();
    assert!(b.is_some());
    assert!(store
        .assets_missing_exif_extra(root, 0, 10)
        .unwrap()
        .iter()
        .all(|x| x.rel_path != "a.jpg"));
}

#[test]
fn a_65535_iso_reads_the_long_isospeed_tag() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("hi.jpg"),
        jpeg_with_exif(&exif_tiff(None, 65535, Some(102_400))),
    )
    .unwrap();
    let (store, root) = catalog(dir.path());
    ingest_root(&store, root, dir.path()).unwrap();
    let a = store.find_asset_by_path(root, "hi.jpg").unwrap().unwrap();
    assert_eq!((a.iso, a.lens), (Some(102_400), None));
}

fn bare_asset(rel: &str) -> NewAsset {
    NewAsset {
        rel_path: rel.into(),
        rel_path_fold: rel.to_lowercase(),
        size_bytes: 1,
        mtime_unix: 0,
        fingerprint: None,
        natural_key: None,
        make: None,
        model: None,
        captured_at: None,
        width: None,
        height: None,
        imported_at: 0,
    }
}

#[test]
fn the_backfill_fills_existing_rows_and_leaves_unreadable_files_for_a_retry() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("old.jpg"),
        jpeg_with_exif(&exif_tiff(Some("Lens A"), 200, None)),
    )
    .unwrap();
    std::fs::write(dir.path().join("junk.jpg"), b"not a jpeg").unwrap();
    let (store, root) = catalog(dir.path());
    // Rows as they exist after the v13 migration: catalogued earlier, never read.
    let old = store
        .insert_asset(root, &bare_asset("old.jpg"), None)
        .unwrap();
    let junk = store
        .insert_asset(root, &bare_asset("junk.jpg"), None)
        .unwrap();
    let gone = store
        .insert_asset(root, &bare_asset("gone.jpg"), None)
        .unwrap();

    let r = backfill_root(&store, root, 2).unwrap();
    assert_eq!((r.examined, r.filled, r.unreadable), (3, 1, 2), "{r:?}");
    let a = store.get_asset(old).unwrap().unwrap();
    assert_eq!((a.lens.as_deref(), a.iso), (Some("Lens A"), Some(200)));
    for id in [junk, gone] {
        let x = store.get_asset(id).unwrap().unwrap();
        assert_eq!((x.lens, x.iso), (None, None));
    }
    // The unreadable ones are retried, the filled one is not.
    let again = backfill_root(&store, root, 10).unwrap();
    assert_eq!((again.examined, again.filled), (2, 0));
}

#[test]
fn an_offline_root_is_left_alone_and_a_changed_file_is_read_again() {
    let (store, root) = catalog(Path::new("/nonexistent/offline-root"));
    store
        .insert_asset(root, &bare_asset("a.jpg"), None)
        .unwrap();
    let r = backfill_root(&store, root, 10).unwrap();
    assert!(r.root_offline);
    assert_eq!(r.examined, 0);

    // A file replaced with a different lens is re-read on the next ingest (size/mtime changed).
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("c.jpg");
    std::fs::write(&p, jpeg_with_exif(&exif_tiff(Some("Old"), 100, None))).unwrap();
    let (store, root) = catalog(dir.path());
    ingest_root(&store, root, dir.path()).unwrap();
    std::fs::write(&p, jpeg_with_exif(&exif_tiff(Some("New lens"), 400, None))).unwrap();
    ingest_root(&store, root, dir.path()).unwrap();
    let a = store.find_asset_by_path(root, "c.jpg").unwrap().unwrap();
    assert_eq!((a.lens.as_deref(), a.iso), (Some("New lens"), Some(400)));
}
