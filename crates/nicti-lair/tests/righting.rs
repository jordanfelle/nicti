//! #561: backfilling the RAW's EXIF Orientation onto T0 thumbnails (and their sidecars) that were
//! ingested before #476. A real (temp-file) catalog, a synthetic little-endian NEF-shaped TIFF with
//! an IFD0 Orientation and a real embedded JPEG, as `tests/ingest.rs` builds them.

use std::path::PathBuf;

use nicti_lair::righting::{backfill_root, righting_asset, RightingReport};
use nicti_lair::thumb_sidecar::{self, SidecarCodec};
use nicti_lair::{tier, CatalogStore, NewAsset, Preview, PreviewTier, SqliteCatalog};

const TAG_ORIENTATION: u16 = 0x0112;
const TAG_IMAGE_WIDTH: u16 = 0x0100;
const TAG_IMAGE_LENGTH: u16 = 0x0101;
const TAG_JPEG_IF_OFFSET: u16 = 0x0201;
const TAG_JPEG_IF_LENGTH: u16 = 0x0202;
const TAG_EXIF_IFD: u16 = 0x8769;
const TAG_MAKER_NOTE: u16 = 0x927C;
const TAG_NIKON_PREVIEW_IFD: u16 = 0x0011;
const TY_SHORT: u16 = 3;
const TY_LONG: u16 = 4;

fn real_jpeg(seed: u8) -> Vec<u8> {
    let mut out = Vec::new();
    let px: Vec<u8> = (0..16 * 8 * 3)
        .map(|i| (i as u8).wrapping_add(seed))
        .collect();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 80)
        .encode(&px, 16, 8, image::ExtendedColorType::Rgb8)
        .unwrap();
    out
}

fn orientation_of(jpeg: &[u8]) -> Option<u32> {
    exif::Reader::new()
        .read_from_container(&mut std::io::Cursor::new(jpeg))
        .ok()
        .and_then(|e| {
            e.get_field(exif::Tag::Orientation, exif::In::PRIMARY)
                .and_then(|f| f.value.get_uint(0))
        })
}

struct B {
    buf: Vec<u8>,
}

impl B {
    fn off(&self) -> u32 {
        self.buf.len() as u32
    }
    fn ifd(&mut self, entries: &[(u16, u16, u32, u32)], next: u32) -> (u32, Vec<u32>) {
        let at = self.off();
        self.buf
            .extend_from_slice(&(entries.len() as u16).to_le_bytes());
        let mut pos = Vec::new();
        for &(tag, ty, count, value) in entries {
            self.buf.extend_from_slice(&tag.to_le_bytes());
            self.buf.extend_from_slice(&ty.to_le_bytes());
            self.buf.extend_from_slice(&count.to_le_bytes());
            pos.push(self.off());
            self.buf.extend_from_slice(&value.to_le_bytes());
        }
        self.buf.extend_from_slice(&next.to_le_bytes());
        (at, pos)
    }
    fn patch(&mut self, at: u32, v: u32) {
        self.buf[at as usize..at as usize + 4].copy_from_slice(&v.to_le_bytes());
    }
}

/// A NEF-shaped TIFF: IFD0 (Orientation if given) -> ExifIFD -> Nikon MakerNote -> PreviewIFD(jpeg).
fn nef(orientation: Option<u16>, jpeg: &[u8]) -> Vec<u8> {
    let mut b = B {
        buf: vec![b'I', b'I', 42, 0, 0, 0, 0, 0],
    };
    let mn_off = b.off();
    b.buf.extend_from_slice(b"Nikon\0");
    b.buf.extend_from_slice(&[0x02, 0x10, 0x00, 0x00]);
    let maker_base = b.off();
    b.buf.extend_from_slice(&[b'I', b'I', 42, 0]);
    b.buf.extend_from_slice(&8u32.to_le_bytes());
    let (_, mn_pos) = b.ifd(&[(TAG_NIKON_PREVIEW_IFD, TY_LONG, 1, 0)], 0);
    let preview_ifd = b.off();
    let (_, pv_pos) = b.ifd(
        &[
            (TAG_IMAGE_WIDTH, TY_SHORT, 1, 16),
            (TAG_IMAGE_LENGTH, TY_SHORT, 1, 8),
            (TAG_JPEG_IF_OFFSET, TY_LONG, 1, 0),
            (TAG_JPEG_IF_LENGTH, TY_LONG, 1, jpeg.len() as u32),
        ],
        0,
    );
    b.patch(mn_pos[0], preview_ifd - maker_base);
    let jpeg_off = b.off();
    b.buf.extend_from_slice(jpeg);
    b.patch(pv_pos[2], jpeg_off - maker_base);
    let (exif_ifd, _) = b.ifd(&[(TAG_MAKER_NOTE, 7, 1, mn_off)], 0);
    let mut e = Vec::new();
    if let Some(o) = orientation {
        e.push((TAG_ORIENTATION, TY_SHORT, 1, u32::from(o)));
    }
    e.push((TAG_EXIF_IFD, TY_LONG, 1, exif_ifd));
    let (ifd0, _) = b.ifd(&e, 0);
    b.buf.extend(std::iter::repeat_n(0x5A, 70 * 1024));
    b.patch(4, ifd0);
    b.buf
}

struct Fx {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    store: SqliteCatalog,
    root_id: i64,
}

impl Fx {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("shoot");
        std::fs::create_dir_all(&dir).unwrap();
        let store = SqliteCatalog::open_in_memory().unwrap();
        let vol = store.upsert_volume("vol", None, None, 0).unwrap();
        let root_id = store.ensure_root(vol, &dir.to_string_lossy()).unwrap();
        Fx {
            _tmp: tmp,
            dir,
            store,
            root_id,
        }
    }

    /// A RAW on disk with `orientation`, catalogued with an (untagged, pre-#476) T0 `blob`.
    fn asset(&self, name: &str, orientation: Option<u16>, blob: Option<&Preview>) -> i64 {
        std::fs::write(self.dir.join(name), nef(orientation, &real_jpeg(1))).unwrap();
        let meta = std::fs::metadata(self.dir.join(name)).unwrap();
        let new = NewAsset {
            rel_path: name.to_string(),
            rel_path_fold: name.to_lowercase(),
            size_bytes: meta.len(),
            mtime_unix: meta
                .modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64,
            fingerprint: None,
            natural_key: None,
            make: Some("NIKON".into()),
            model: Some("Z8".into()),
            captured_at: None,
            width: None,
            height: None,
            imported_at: 0,
        };
        self.store.insert_asset(self.root_id, &new, blob).unwrap()
    }

    fn run(&self, id: i64) -> RightingReport {
        let asset = self
            .store
            .list_assets_by_root(self.root_id)
            .unwrap()
            .into_iter()
            .find(|a| a.id == id)
            .unwrap();
        let mut r = RightingReport::default();
        righting_asset(&self.store, &self.dir, &asset, &mut r).unwrap();
        r
    }

    fn blob(&self, id: i64) -> Option<Preview> {
        self.store.get_preview(id, PreviewTier::T0).unwrap()
    }

    fn raw(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

fn pv(bytes: Vec<u8>) -> Preview {
    Preview {
        width: Some(16),
        height: Some(8),
        bytes,
    }
}

#[test]
fn an_untagged_catalog_t0_of_a_rotated_raw_is_tagged_in_place_and_a_rerun_does_nothing() {
    let fx = Fx::new();
    let jpeg = real_jpeg(1);
    let id = fx.asset("a.NEF", Some(6), Some(&pv(jpeg.clone())));
    assert_eq!(orientation_of(&fx.blob(id).unwrap().bytes), None);

    let r = fx.run(id);
    assert_eq!(
        (r.fixed_blobs, r.fixed_sidecars, r.already_right),
        (1, 0, 0),
        "{r:?}"
    );
    let after = fx.blob(id).unwrap();
    assert_eq!(orientation_of(&after.bytes), Some(6));
    assert_eq!(
        (after.width, after.height),
        (Some(16), Some(8)),
        "dimensions kept"
    );
    // The same pixels: the tagged bytes still decode to the original image.
    let a = image::load_from_memory(&jpeg).unwrap().to_rgb8();
    let b = image::load_from_memory(&after.bytes).unwrap().to_rgb8();
    assert_eq!(a.as_raw(), b.as_raw());

    // Idempotent: a second run changes nothing and says so.
    let again = fx.run(id);
    assert_eq!(
        (again.fixed_blobs, again.already_right),
        (0, 1),
        "{again:?}"
    );
    assert_eq!(fx.blob(id).unwrap().bytes, after.bytes);
}

#[test]
fn only_rotations_are_touched() {
    let fx = Fx::new();
    let jpeg = real_jpeg(2);
    let none = fx.asset("none.NEF", None, Some(&pv(jpeg.clone())));
    let upright = fx.asset("upright.NEF", Some(1), Some(&pv(jpeg.clone())));
    for id in [none, upright] {
        let r = fx.run(id);
        assert_eq!((r.no_rotation, r.fixed_blobs), (1, 0), "{r:?}");
        assert_eq!(fx.blob(id).unwrap().bytes, jpeg);
    }
    // Every EXIF rotation 2-8 is tagged with its own value.
    for o in 2..=8u16 {
        let id = fx.asset(&format!("r{o}.NEF"), Some(o), Some(&pv(jpeg.clone())));
        assert_eq!(fx.run(id).fixed_blobs, 1, "orientation {o}");
        assert_eq!(
            orientation_of(&fx.blob(id).unwrap().bytes),
            Some(u32::from(o))
        );
    }
}

#[test]
fn jpeg_assets_missing_files_and_assets_with_no_thumbnail_are_counted_not_touched() {
    let fx = Fx::new();
    let jpeg_asset = fx.asset("photo.JPG", Some(6), Some(&pv(real_jpeg(3))));
    let r = fx.run(jpeg_asset);
    assert_eq!(
        (r.no_rotation, r.fixed_blobs),
        (1, 0),
        "a JPEG carries its own EXIF: {r:?}"
    );

    let gone = fx.asset("gone.NEF", Some(6), Some(&pv(real_jpeg(3))));
    std::fs::remove_file(fx.raw("gone.NEF")).unwrap();
    assert_eq!(fx.run(gone).missing_file, 1);

    let bare = fx.asset("bare.NEF", Some(6), None);
    assert_eq!(fx.run(bare).no_thumbnail, 1);
}

#[test]
fn a_jpeg_sidecar_is_retagged_with_its_blob_so_the_pair_stays_identical() {
    let fx = Fx::new();
    let jpeg = real_jpeg(4);
    let id = fx.asset("a.NEF", Some(8), Some(&pv(jpeg.clone())));
    thumb_sidecar::export(&fx.raw("a.NEF"), &pv(jpeg), SidecarCodec::Jpeg).unwrap();

    let r = fx.run(id);
    assert_eq!((r.fixed_blobs, r.fixed_sidecars), (1, 1), "{r:?}");
    let (codec, side) =
        thumb_sidecar::read_with_codec(&fx.raw("a.NEF"), SidecarCodec::Jpeg).unwrap();
    assert_eq!(codec, SidecarCodec::Jpeg);
    assert_eq!(orientation_of(&side.bytes), Some(8));
    assert_eq!(
        side.bytes,
        fx.blob(id).unwrap().bytes,
        "blob and sidecar agree byte for byte"
    );
    assert_eq!(fx.run(id).already_right, 1);
}

#[test]
fn an_avif_sidecar_cannot_carry_the_rotation_and_is_replaced_by_a_tagged_jpeg() {
    let fx = Fx::new();
    let jpeg = real_jpeg(5);
    let id = fx.asset("a.NEF", Some(6), Some(&pv(jpeg.clone())));
    let written = thumb_sidecar::export(&fx.raw("a.NEF"), &pv(jpeg), SidecarCodec::Avif).unwrap();
    assert_eq!(
        written,
        SidecarCodec::Avif,
        "the legacy sidecar really is AVIF"
    );
    assert!(thumb_sidecar::sidecar_path(&fx.raw("a.NEF"), SidecarCodec::Avif).exists());

    let r = fx.run(id);
    assert_eq!(r.fixed_sidecars, 1, "{r:?}");
    assert!(!thumb_sidecar::sidecar_path(&fx.raw("a.NEF"), SidecarCodec::Avif).exists());
    let (codec, side) =
        thumb_sidecar::read_with_codec(&fx.raw("a.NEF"), SidecarCodec::Jpeg).unwrap();
    assert_eq!(codec, SidecarCodec::Jpeg);
    assert_eq!(orientation_of(&side.bytes), Some(6));
}

#[test]
fn an_archived_folder_with_only_an_avif_sidecar_is_rebuilt_from_the_raws_own_preview() {
    let fx = Fx::new();
    let id = fx.asset("a.NEF", Some(3), None);
    // Archived: the catalog blob was dropped, only the (legacy, sideways) AVIF remains.
    thumb_sidecar::export(&fx.raw("a.NEF"), &pv(real_jpeg(1)), SidecarCodec::Avif).unwrap();
    assert!(fx.blob(id).is_none());

    let r = fx.run(id);
    assert_eq!((r.fixed_sidecars, r.failed.len()), (1, 0), "{r:?}");
    let (codec, side) =
        thumb_sidecar::read_with_codec(&fx.raw("a.NEF"), SidecarCodec::Jpeg).unwrap();
    assert_eq!(codec, SidecarCodec::Jpeg);
    assert_eq!(orientation_of(&side.bytes), Some(3));
    assert!(fx.blob(id).is_none(), "an archived root stays blob-free");
}

#[test]
fn a_thumbnail_that_is_not_a_taggable_jpeg_is_reported_not_ignored() {
    let fx = Fx::new();
    let id = fx.asset("a.NEF", Some(6), Some(&pv(b"not a jpeg at all".to_vec())));
    let r = fx.run(id);
    assert_eq!((r.fixed_blobs, r.failed.len()), (0, 1), "{r:?}");
    assert_eq!(fx.blob(id).unwrap().bytes, b"not a jpeg at all");
}

#[test]
fn backfill_root_walks_every_asset_and_reports_the_totals() {
    let fx = Fx::new();
    let jpeg = real_jpeg(6);
    fx.asset("a.NEF", Some(6), Some(&pv(jpeg.clone())));
    fx.asset("b.NEF", Some(1), Some(&pv(jpeg.clone())));
    fx.asset("c.NEF", Some(8), Some(&pv(jpeg)));
    let r = backfill_root(&fx.store, fx.root_id).unwrap();
    assert_eq!(
        (r.examined, r.fixed_blobs, r.no_rotation),
        (3, 2, 1),
        "{r:?}"
    );
    assert_eq!(
        backfill_root(&fx.store, 9999).unwrap(),
        RightingReport::default()
    );
}

#[test]
fn settle_refreshes_a_stale_sidecar_from_a_retagged_blob_instead_of_keeping_the_pair_forever() {
    // The crash window / partial failure: the catalog blob was re-tagged, the sidecar still holds
    // the old untagged bytes. Before #561 settle called this pair `kept` for ever (byte-different)
    // while the archived read path kept serving the sideways sidecar.
    let fx = Fx::new();
    let jpeg = real_jpeg(7);
    let id = fx.asset("a.NEF", Some(6), Some(&pv(jpeg.clone())));
    // Tag the blob (no sidecar yet), then leave a stale untagged sidecar behind it.
    assert_eq!(fx.run(id).fixed_blobs, 1);
    let tagged = fx.blob(id).unwrap().bytes;
    thumb_sidecar::export(&fx.raw("a.NEF"), &pv(jpeg), SidecarCodec::Jpeg).unwrap();
    fx.store.set_root_archived(fx.root_id, true).unwrap();

    let rep = tier::settle_root(&fx.store, fx.root_id).unwrap();
    assert_eq!(
        (rep.sidecars_refreshed, rep.blobs_dropped, rep.kept),
        (1, 1, 0),
        "{rep:?}"
    );
    assert!(
        fx.blob(id).is_none(),
        "the blob is dropped once the sidecar reproduces it"
    );
    let (_, side) = thumb_sidecar::read_with_codec(&fx.raw("a.NEF"), SidecarCodec::Jpeg).unwrap();
    assert_eq!(side.bytes, tagged);
    assert_eq!(orientation_of(&side.bytes), Some(6));

    // An unrelated byte difference is still kept, never refreshed.
    let id2 = fx.asset("b.NEF", Some(6), Some(&pv(tagged.clone())));
    thumb_sidecar::export(&fx.raw("b.NEF"), &pv(real_jpeg(9)), SidecarCodec::Jpeg).unwrap();
    let rep = tier::settle_root(&fx.store, fx.root_id).unwrap();
    assert_eq!((rep.sidecars_refreshed, rep.kept), (0, 1), "{rep:?}");
    assert!(fx.blob(id2).is_some());
}

/// A tagged JPEG for orientation `o`, made through the public API (righting a throwaway RAW).
fn tagged_jpeg(fx: &Fx, name: &str, o: u16) -> Vec<u8> {
    let id = fx.asset(name, Some(o), Some(&pv(real_jpeg(1))));
    assert_eq!(fx.run(id).fixed_blobs, 1);
    fx.blob(id).unwrap().bytes
}

/// Makes `export`'s temp file unwritable (a directory sits where it goes), portably and without
/// depending on file permissions.
fn block_sidecar_writes(raw: &std::path::Path) {
    let sc = thumb_sidecar::sidecar_path(raw, SidecarCodec::Jpeg);
    let tmp = sc.with_file_name(format!(
        ".{}.thumb-tmp",
        sc.file_name().unwrap().to_string_lossy()
    ));
    std::fs::create_dir_all(tmp).unwrap();
}

#[test]
fn a_garbage_blob_never_replaces_an_avif_sidecar_with_garbage() {
    let fx = Fx::new();
    let id = fx.asset("a.NEF", Some(6), Some(&pv(b"not a jpeg".to_vec())));
    thumb_sidecar::export(&fx.raw("a.NEF"), &pv(real_jpeg(1)), SidecarCodec::Avif).unwrap();

    let r = fx.run(id);
    assert_eq!(r.failed.len(), 1, "the bad blob is reported: {r:?}");
    assert_eq!(r.already_right, 0, "a failure is not 'already right'");
    // The sidecar was rebuilt from the RAW's own preview, never from the garbage blob.
    let (codec, side) = thumb_sidecar::read_with_codec(&fx.raw("a.NEF"), SidecarCodec::Jpeg)
        .expect("a readable sidecar exists");
    assert_eq!(codec, SidecarCodec::Jpeg);
    assert_eq!(orientation_of(&side.bytes), Some(6));
    assert_eq!(fx.blob(id).unwrap().bytes, b"not a jpeg");
}

#[test]
fn a_raw_that_changed_since_ingest_is_left_alone_until_it_is_re_ingested() {
    let fx = Fx::new();
    let jpeg = real_jpeg(1);
    let size = fx.asset("size.NEF", Some(6), Some(&pv(jpeg.clone())));
    let mtime = fx.asset("mtime.NEF", Some(6), Some(&pv(jpeg.clone())));
    // The file grew (an in-place edit) / was touched after the catalog row was written.
    let mut grown = std::fs::read(fx.raw("size.NEF")).unwrap();
    grown.extend_from_slice(&[0u8; 16]);
    std::fs::write(fx.raw("size.NEF"), grown).unwrap();
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(fx.raw("mtime.NEF"))
        .unwrap();
    f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(3600))
        .unwrap();
    drop(f);

    for id in [size, mtime] {
        let r = fx.run(id);
        assert_eq!((r.stale_file, r.fixed_blobs), (1, 0), "{r:?}");
        assert_eq!(
            fx.blob(id).unwrap().bytes,
            jpeg,
            "the old thumbnail is not re-tagged"
        );
        assert!(!r.can_mark_done(), "a stale file is left undone");
    }
}

#[test]
fn a_failed_sidecar_write_is_a_failure_not_already_right() {
    let fx = Fx::new();
    let id = fx.asset("a.NEF", Some(6), None);
    thumb_sidecar::export(&fx.raw("a.NEF"), &pv(real_jpeg(1)), SidecarCodec::Jpeg).unwrap();
    block_sidecar_writes(&fx.raw("a.NEF"));

    let r = fx.run(id);
    assert_eq!(
        (r.fixed_sidecars, r.already_right, r.failed.len()),
        (0, 0, 1),
        "{r:?}"
    );
    assert!(r.failed[0].1.contains("writing sidecar"));
    assert!(!r.can_mark_done());
}

#[test]
fn an_offline_root_is_not_a_pile_of_deleted_files() {
    let fx = Fx::new();
    fx.asset("a.NEF", Some(6), Some(&pv(real_jpeg(1))));
    fx.asset("b.NEF", Some(6), Some(&pv(real_jpeg(1))));
    std::fs::remove_dir_all(&fx.dir).unwrap(); // the drive is unmounted

    let r = backfill_root(&fx.store, fx.root_id).unwrap();
    assert!(r.root_offline);
    assert_eq!(
        (r.examined, r.missing_file),
        (0, 0),
        "nothing was judged: {r:?}"
    );
    assert!(
        !r.can_mark_done(),
        "an offline root must not be marked done"
    );
}

#[test]
fn a_clean_run_may_be_marked_done_even_with_deleted_files_and_unrotated_raws() {
    let fx = Fx::new();
    let jpeg = real_jpeg(1);
    fx.asset("rot.NEF", Some(6), Some(&pv(jpeg.clone())));
    fx.asset("flat.NEF", None, Some(&pv(jpeg.clone())));
    let gone = fx.asset("gone.NEF", Some(6), Some(&pv(jpeg)));
    std::fs::remove_file(fx.raw("gone.NEF")).unwrap();
    let _ = gone;
    let r = backfill_root(&fx.store, fx.root_id).unwrap();
    assert_eq!(
        (r.fixed_blobs, r.no_rotation, r.missing_file),
        (1, 1, 1),
        "{r:?}"
    );
    assert!(r.can_mark_done());
}

#[test]
fn a_t0_with_its_own_orientation_is_left_alone_as_ingest_does() {
    let fx = Fx::new();
    let other = tagged_jpeg(&fx, "other.NEF", 8);
    let id = fx.asset("a.NEF", Some(6), Some(&pv(other.clone())));
    let r = fx.run(id);
    assert_eq!((r.fixed_blobs, r.already_right), (0, 1), "{r:?}");
    assert_eq!(fx.blob(id).unwrap().bytes, other);
}

#[test]
fn settle_keeps_the_pair_when_the_sidecar_is_not_the_blobs_untagged_original() {
    let fx = Fx::new();
    let tagged6 = tagged_jpeg(&fx, "six.NEF", 6);
    let tagged8 = tagged_jpeg(&fx, "eight.NEF", 8);
    let id = fx.asset("a.NEF", Some(6), Some(&pv(tagged6)));
    // The sidecar has its own, different orientation: not the untagged original of this blob.
    thumb_sidecar::export(&fx.raw("a.NEF"), &pv(tagged8.clone()), SidecarCodec::Jpeg).unwrap();
    fx.store.set_root_archived(fx.root_id, true).unwrap();

    let rep = tier::settle_root(&fx.store, fx.root_id).unwrap();
    let _ = id;
    assert_eq!(rep.sidecars_refreshed, 0, "{rep:?}");
    let (_, side) = thumb_sidecar::read_with_codec(&fx.raw("a.NEF"), SidecarCodec::Jpeg).unwrap();
    assert_eq!(
        side.bytes, tagged8,
        "an unrelated sidecar is never overwritten"
    );
}

#[test]
fn settle_does_not_abort_the_root_when_one_sidecar_cannot_be_refreshed() {
    let fx = Fx::new();
    let jpeg = real_jpeg(1);
    let id = fx.asset("a.NEF", Some(6), Some(&pv(jpeg.clone())));
    assert_eq!(fx.run(id).fixed_blobs, 1); // blob tagged, no sidecar yet
    thumb_sidecar::export(&fx.raw("a.NEF"), &pv(jpeg), SidecarCodec::Jpeg).unwrap(); // stale
    block_sidecar_writes(&fx.raw("a.NEF"));
    fx.store.set_root_archived(fx.root_id, true).unwrap();

    let rep = tier::settle_root(&fx.store, fx.root_id).expect("settle carries on");
    assert_eq!((rep.sidecars_refreshed, rep.kept), (0, 1), "{rep:?}");
    assert!(
        fx.blob(id).is_some(),
        "the blob stays until its sidecar can be refreshed"
    );
}
