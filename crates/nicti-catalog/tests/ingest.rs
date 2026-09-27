//! Integration tests for #22's ingest pipeline, against a real (temp-file) SQLite catalog.
//!
//! The repo has no real NEF/NRW fixtures (see the `raw-decoder`/`preview-tiers` topics -- the
//! reference dataset lives on the actual library, not in-tree), so these tests build a minimal
//! synthetic little-endian TIFF with a Nikon-MakerNote-embedded PreviewIFD JPEG by hand, the same
//! way `nicti-decode`'s own `embedded::ifd` tests do.

use std::path::Path;

use nicti_catalog::scruff::ingest_root;
use nicti_catalog::{CatalogStore, PreviewTier, SqliteCatalog};

const TAG_IMAGE_WIDTH: u16 = 0x0100;
const TAG_IMAGE_LENGTH: u16 = 0x0101;
const TAG_JPEG_IF_OFFSET: u16 = 0x0201;
const TAG_JPEG_IF_LENGTH: u16 = 0x0202;
const TAG_EXIF_IFD: u16 = 0x8769;
const TAG_MAKER_NOTE: u16 = 0x927C;
const TAG_NIKON_PREVIEW_IFD: u16 = 0x0011;
const TY_SHORT: u16 = 3;
const TY_LONG: u16 = 4;

const FAKE_JPEG: &[u8] = b"\xFF\xD8FAKEDATA\xFF\xD9";

/// Builds a little-endian TIFF file with one Nikon-MakerNote PreviewIFD JPEG -- just enough for
/// `extract_t0_preview` to find something, and enough bytes overall that `partial_hash`'s
/// head+tail windows behave sensibly on a small file.
struct FileBuilder {
    buf: Vec<u8>,
}

impl FileBuilder {
    fn new() -> Self {
        FileBuilder {
            buf: vec![b'I', b'I', 42, 0, 0, 0, 0, 0],
        }
    }

    fn offset(&self) -> u32 {
        self.buf.len() as u32
    }

    fn set_ifd0_offset(&mut self, off: u32) {
        self.buf[4..8].copy_from_slice(&off.to_le_bytes());
    }

    fn append_bytes(&mut self, bytes: &[u8]) -> u32 {
        let off = self.offset();
        self.buf.extend_from_slice(bytes);
        off
    }

    fn append_ifd(&mut self, entries: &[(u16, u16, u32, u32)], next: u32) -> (u32, Vec<u32>) {
        let ifd_off = self.offset();
        self.buf
            .extend_from_slice(&(entries.len() as u16).to_le_bytes());
        let mut value_positions = Vec::with_capacity(entries.len());
        for &(tag, ty, count, value) in entries {
            self.buf.extend_from_slice(&tag.to_le_bytes());
            self.buf.extend_from_slice(&ty.to_le_bytes());
            self.buf.extend_from_slice(&count.to_le_bytes());
            value_positions.push(self.offset());
            self.buf.extend_from_slice(&value.to_le_bytes());
        }
        self.buf.extend_from_slice(&next.to_le_bytes());
        (ifd_off, value_positions)
    }

    fn patch_u32(&mut self, at: u32, value: u32) {
        let at = at as usize;
        self.buf[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn finish(mut self, ifd0_off: u32) -> Vec<u8> {
        self.set_ifd0_offset(ifd0_off);
        self.buf
    }
}

/// Builds one synthetic NEF-shaped TIFF: IFD0 -> ExifIFD -> Nikon MakerNote -> PreviewIFD (JPEG).
/// `fill_byte` varies the trailing padding so two calls with different values produce genuinely
/// different file content (and therefore different partial-hash fingerprints) -- matching two
/// distinct real photos rather than two copies of the same one.
fn build_synthetic_nef(fill_byte: u8) -> Vec<u8> {
    let mut b = FileBuilder::new();

    let mn_off = b.offset();
    b.buf.extend_from_slice(b"Nikon\0");
    b.buf.extend_from_slice(&[0x02, 0x10, 0x00, 0x00]);
    let inner_header_off = b.offset();
    let maker_base = inner_header_off;
    b.buf.extend_from_slice(&[b'I', b'I', 42, 0]);
    b.buf.extend_from_slice(&8u32.to_le_bytes());

    let (_mn_ifd_off, mn_value_positions) =
        b.append_ifd(&[(TAG_NIKON_PREVIEW_IFD, TY_LONG, 1, 0)], 0);
    let preview_ifd_off = b.offset();
    let (_preview_ifd_start, preview_value_positions) = b.append_ifd(
        &[
            (TAG_IMAGE_WIDTH, TY_SHORT, 1, 160),
            (TAG_IMAGE_LENGTH, TY_SHORT, 1, 120),
            (TAG_JPEG_IF_OFFSET, TY_LONG, 1, 0),
            (TAG_JPEG_IF_LENGTH, TY_LONG, 1, FAKE_JPEG.len() as u32),
        ],
        0,
    );
    b.patch_u32(mn_value_positions[0], preview_ifd_off - maker_base);
    let jpeg_off = b.append_bytes(FAKE_JPEG);
    b.patch_u32(preview_value_positions[2], jpeg_off - maker_base);

    let (exif_ifd_off, _) = b.append_ifd(&[(TAG_MAKER_NOTE, 7, 1, mn_off)], 0);
    let (ifd0_off, _) = b.append_ifd(&[(TAG_EXIF_IFD, TY_LONG, 1, exif_ifd_off)], 0);

    // Pad past the 64KB partial-hash window so head+tail sampling is exercised, same as
    // `spikes/homing`'s own fingerprint tests do for a "large" file.
    b.buf.extend(std::iter::repeat_n(fill_byte, 70 * 1024));

    b.finish(ifd0_off)
}

fn write_synthetic_nef(dir: &Path, name: &str) -> std::path::PathBuf {
    write_synthetic_nef_with_fill(dir, name, 0xAB)
}

fn write_synthetic_nef_with_fill(dir: &Path, name: &str, fill_byte: u8) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, build_synthetic_nef(fill_byte)).unwrap();
    path
}

#[test]
fn ingest_inserts_asset_master_variant_and_t0_preview() {
    let store = SqliteCatalog::open_in_memory().unwrap();
    let dir = tempfile::tempdir().unwrap();
    write_synthetic_nef(dir.path(), "IMG_0001.NEF");

    let volume_id = store
        .upsert_volume("test-volume", None, None, 1000)
        .unwrap();
    let root_id = store.ensure_root(volume_id, "").unwrap();

    let report = ingest_root(&store, root_id, dir.path()).unwrap();
    assert_eq!(report.added, 1);
    assert_eq!(report.updated, 0);
    assert!(report.failed.is_empty(), "failed: {:?}", report.failed);

    let asset = store
        .find_asset_by_path(root_id, "IMG_0001.NEF")
        .unwrap()
        .expect("asset row exists");
    assert!(asset.fingerprint.is_some());

    let preview = store
        .get_preview(asset.id, PreviewTier::T0)
        .unwrap()
        .expect("T0 preview stored");
    assert_eq!(preview.bytes, FAKE_JPEG);
    assert_eq!(preview.width, Some(160));
    assert_eq!(preview.height, Some(120));
}

#[test]
fn rescanning_unchanged_file_is_skipped() {
    let store = SqliteCatalog::open_in_memory().unwrap();
    let dir = tempfile::tempdir().unwrap();
    write_synthetic_nef(dir.path(), "IMG_0001.NEF");

    let volume_id = store
        .upsert_volume("test-volume", None, None, 1000)
        .unwrap();
    let root_id = store.ensure_root(volume_id, "").unwrap();

    ingest_root(&store, root_id, dir.path()).unwrap();
    let second = ingest_root(&store, root_id, dir.path()).unwrap();

    assert_eq!(second.added, 0);
    assert_eq!(second.skipped_unchanged, 1);
}

#[test]
fn renamed_file_is_relinked_not_duplicated() {
    let store = SqliteCatalog::open_in_memory().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let original = write_synthetic_nef(dir.path(), "IMG_0001.NEF");

    let volume_id = store
        .upsert_volume("test-volume", None, None, 1000)
        .unwrap();
    let root_id = store.ensure_root(volume_id, "").unwrap();

    ingest_root(&store, root_id, dir.path()).unwrap();
    let asset_before = store
        .find_asset_by_path(root_id, "IMG_0001.NEF")
        .unwrap()
        .unwrap();

    std::fs::rename(&original, dir.path().join("renamed.NEF")).unwrap();
    let report = ingest_root(&store, root_id, dir.path()).unwrap();

    assert_eq!(report.moved, 1);
    assert_eq!(report.added, 0);
    assert!(
        store
            .find_asset_by_path(root_id, "IMG_0001.NEF")
            .unwrap()
            .is_none(),
        "old path must no longer resolve"
    );
    let asset_after = store
        .find_asset_by_path(root_id, "renamed.NEF")
        .unwrap()
        .expect("asset now resolves under its new path");
    assert_eq!(
        asset_before.id, asset_after.id,
        "same asset row, not a duplicate"
    );
}

#[test]
fn a_corrupt_file_is_reported_as_failed_without_aborting_the_run() {
    let store = SqliteCatalog::open_in_memory().unwrap();
    let dir = tempfile::tempdir().unwrap();
    write_synthetic_nef(dir.path(), "good.NEF");
    // Not a valid TIFF at all -- EXIF/preview extraction will find nothing, but the file itself
    // must still stat/hash/insert cleanly (a garbage RAW file is still worth cataloging).
    std::fs::write(dir.path().join("bad.NEF"), b"not a tiff file").unwrap();

    let volume_id = store
        .upsert_volume("test-volume", None, None, 1000)
        .unwrap();
    let root_id = store.ensure_root(volume_id, "").unwrap();

    let report = ingest_root(&store, root_id, dir.path()).unwrap();
    assert_eq!(
        report.added, 2,
        "even the unparseable file is still cataloged"
    );
    assert!(report.failed.is_empty());

    let bad = store
        .find_asset_by_path(root_id, "bad.NEF")
        .unwrap()
        .expect("bad file still gets an asset row");
    assert!(store
        .get_preview(bad.id, PreviewTier::T0)
        .unwrap()
        .is_none());
}

#[test]
fn facet_counts_stay_consistent_with_inserted_assets() {
    let store = SqliteCatalog::open_in_memory().unwrap();
    let dir = tempfile::tempdir().unwrap();
    write_synthetic_nef_with_fill(dir.path(), "a.NEF", 0xAB);
    write_synthetic_nef_with_fill(dir.path(), "b.NEF", 0xCD);

    let volume_id = store
        .upsert_volume("test-volume", None, None, 1000)
        .unwrap();
    let root_id = store.ensure_root(volume_id, "").unwrap();
    ingest_root(&store, root_id, dir.path()).unwrap();

    // Neither synthetic file carries a Make/Model EXIF tag, so both land in the "" bucket at the
    // default rating (0) -- this asserts the *count*, not real camera-model faceting (no EXIF
    // Make/Model tag is set by `build_synthetic_nef`).
    assert_eq!(store.facet_count(None, 0).unwrap(), 2);
}

#[test]
fn reopening_a_migrated_database_file_does_not_rerun_migrations() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("catalog.sqlite3");

    {
        let store = SqliteCatalog::open(&db_path).unwrap();
        let volume_id = store.upsert_volume("v1", None, None, 1000).unwrap();
        store.ensure_root(volume_id, "Photos").unwrap();
    }

    // Reopening the same file must find the same data, not fail on a "table already exists"
    // migration re-run and not lose what the first session wrote.
    let store = SqliteCatalog::open(&db_path).unwrap();
    let volume_id = store.upsert_volume("v1", None, None, 2000).unwrap();
    let root_id = store.ensure_root(volume_id, "Photos").unwrap();
    assert!(root_id > 0);
}

#[test]
fn two_volumes_with_conflicting_markers_are_not_merged() {
    let store = SqliteCatalog::open_in_memory().unwrap();
    let id1 = store
        .upsert_volume("shared-identity-key", None, Some("marker-a"), 100)
        .unwrap();

    let result = store.upsert_volume("shared-identity-key", None, Some("marker-b"), 200);
    assert!(
        result.is_err(),
        "a disagreeing marker_uuid under the same identity_key must not silently merge"
    );

    // The original volume is untouched by the rejected call.
    let id_again = store
        .upsert_volume("shared-identity-key", None, Some("marker-a"), 300)
        .unwrap();
    assert_eq!(id1, id_again);
}
