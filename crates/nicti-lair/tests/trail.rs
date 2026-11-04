//! #536: applying a GPX log to catalogued assets, end to end through the catalog and the XMP
//! sidecar / embedded-JPEG write-back.

use std::path::{Path, PathBuf};

use nicti_lair::scent_sync::{clear_position, import_sidecar, ClearOutcome, SyncOutcome};
use nicti_lair::trail::geotag_assets;
use nicti_lair::{CatalogStore, NewAsset, SqliteCatalog};
use nicti_scent::gpx::{parse_gpx, read_gps, GeotagOptions, Gps};

const LOG: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<gpx version="1.1" creator="test" xmlns="http://www.topografix.com/GPX/1/1"><trk><trkseg>
<trkpt lat="46.0000" lon="7.0000"><ele>1600</ele><time>2026-05-01T10:00:00Z</time></trkpt>
<trkpt lat="46.0010" lon="7.0020"><ele>1610</ele><time>2026-05-01T10:01:00Z</time></trkpt>
</trkseg></trk></gpx>"#;

fn asset(rel: &str, captured: Option<&str>) -> NewAsset {
    NewAsset {
        rel_path: rel.into(),
        rel_path_fold: rel.to_lowercase(),
        size_bytes: 1,
        mtime_unix: 0,
        fingerprint: None,
        natural_key: None,
        make: None,
        model: None,
        captured_at: captured.map(str::to_owned),
        width: None,
        height: None,
        imported_at: 0,
    }
}

struct Fx {
    store: SqliteCatalog,
    dir: tempfile::TempDir,
    root: i64,
}

fn fx() -> Fx {
    let store = SqliteCatalog::open_in_memory().unwrap();
    let vol = store.upsert_volume("v", None, None, 0).unwrap();
    let root = store.ensure_root(vol, "").unwrap();
    Fx {
        store,
        dir: tempfile::tempdir().unwrap(),
        root,
    }
}

impl Fx {
    fn add(&self, rel: &str, captured: Option<&str>) -> (i64, PathBuf) {
        let path = self.dir.path().join(rel);
        if rel.ends_with(".jpg") {
            let mut buf = Vec::new();
            image::codecs::jpeg::JpegEncoder::new(&mut buf)
                .encode(&[90u8; 8 * 8 * 3], 8, 8, image::ExtendedColorType::Rgb8)
                .unwrap();
            std::fs::write(&path, buf).unwrap();
        } else {
            std::fs::write(&path, b"raw").unwrap();
        }
        let id = self
            .store
            .insert_asset(self.root, &asset(rel, captured), None)
            .unwrap();
        (id, path)
    }
}

fn sidecar(raw: &Path) -> String {
    std::fs::read_to_string(raw.with_extension("xmp")).unwrap()
}

#[test]
fn a_gpx_log_tags_the_catalog_and_the_sidecar_and_reports_every_outcome() {
    let f = fx();
    let log = parse_gpx(LOG).unwrap();
    let opts = GeotagOptions::default();
    let hit = f.add("a.NEF", Some("2026:05:01 10:00:30"));
    let far = f.add("b.NEF", Some("2026:05:01 14:00:00"));
    let none = f.add("c.NEF", None);

    let r = geotag_assets(
        &f.store,
        &[hit.clone(), far.clone(), none.clone()],
        &log,
        &opts,
        false,
        true,
        1,
    );
    assert_eq!((r.tagged, r.no_position, r.no_capture_time), (1, 1, 1));
    assert!(r.failed.is_empty() && r.kept_existing == 0 && r.held_for_review == 0);

    let g = f.store.get_gps(&[hit.0, far.0]).unwrap();
    assert!((g[&hit.0].latitude - 46.0005).abs() < 1e-6);
    assert!(!g.contains_key(&far.0), "no position means nothing stored");
    let back = read_gps(&sidecar(&hit.1)).expect("position in the sidecar");
    assert!((back.longitude - 7.001).abs() < 1e-5, "{back:?}");
    assert!(!far.1.with_extension("xmp").exists());
}

#[test]
fn an_existing_position_is_kept_unless_overwrite_is_asked_for() {
    let f = fx();
    let log = parse_gpx(LOG).unwrap();
    let opts = GeotagOptions::default();
    let a = f.add("a.NEF", Some("2026:05:01 10:00:30"));
    let old = Gps {
        latitude: 10.0,
        longitude: 10.0,
        altitude: None,
    };
    f.store.set_gps(a.0, Some(old)).unwrap();

    let r = geotag_assets(
        &f.store,
        std::slice::from_ref(&a),
        &log,
        &opts,
        false,
        true,
        1,
    );
    assert_eq!((r.kept_existing, r.tagged), (1, 0));
    assert_eq!(f.store.get_gps(&[a.0]).unwrap()[&a.0], old);

    let r = geotag_assets(
        &f.store,
        std::slice::from_ref(&a),
        &log,
        &opts,
        true,
        true,
        2,
    );
    assert_eq!((r.kept_existing, r.tagged), (0, 1));
    assert!((f.store.get_gps(&[a.0]).unwrap()[&a.0].latitude - 46.0005).abs() < 1e-6);
}

#[test]
fn the_camera_clock_policy_moves_the_match() {
    let f = fx();
    let log = parse_gpx(LOG).unwrap();
    // Camera clock is UTC+2: 12:00:30 local is 10:00:30Z, so it matches the log.
    let a = f.add("a.NEF", Some("2026:05:01 12:00:30"));
    let wrong = geotag_assets(
        &f.store,
        std::slice::from_ref(&a),
        &log,
        &GeotagOptions::default(),
        false,
        true,
        1,
    );
    assert_eq!(wrong.no_position, 1);
    let opts = GeotagOptions {
        camera_utc_offset_minutes: 120,
        ..Default::default()
    };
    let r = geotag_assets(&f.store, &[a], &log, &opts, false, true, 2);
    assert_eq!(r.tagged, 1);
}

#[test]
fn a_jpeg_gets_its_position_embedded_in_the_file() {
    let f = fx();
    let log = parse_gpx(LOG).unwrap();
    let j = f.add("p.jpg", Some("2026:05:01 10:00:30"));
    let r = geotag_assets(
        &f.store,
        std::slice::from_ref(&j),
        &log,
        &GeotagOptions::default(),
        false,
        true,
        1,
    );
    assert_eq!(r.tagged, 1, "{r:?}");
    let bytes = std::fs::read(&j.1).unwrap();
    let xmp = nicti_scent::embedded::read_xmp(&bytes).unwrap().unwrap();
    assert!(read_gps(&xmp).is_some());
    assert!(!j.1.with_extension("xmp").exists());
}

#[test]
fn with_auto_write_off_the_position_waits_and_a_later_import_pushes_it() {
    let f = fx();
    let log = parse_gpx(LOG).unwrap();
    let a = f.add("a.NEF", Some("2026:05:01 10:00:30"));
    // The sidecar exists with a rating, in sync, before the geotag.
    f.store.set_rating(&[a.0], Some(3)).unwrap();
    nicti_lair::scent_sync::write_sidecar(&f.store, a.0, &a.1, 1).unwrap();

    let r = geotag_assets(
        &f.store,
        std::slice::from_ref(&a),
        &log,
        &GeotagOptions::default(),
        false,
        false,
        10,
    );
    assert_eq!(r.tagged, 1);
    assert!(read_gps(&sidecar(&a.1)).is_none(), "nothing written yet");
    assert_eq!(f.store.sidecar_dirty_assets().unwrap(), vec![a.0]);

    let out = import_sidecar(&f.store, a.0, &a.1, 100_000).unwrap();
    assert_eq!(out, SyncOutcome::WroteSidecar);
    assert!(read_gps(&sidecar(&a.1)).is_some());
    assert!(f.store.sidecar_dirty_assets().unwrap().is_empty());
}

#[test]
fn a_position_in_an_existing_sidecar_is_adopted_by_the_catalog_but_never_cleared() {
    let f = fx();
    let a = f.add("a.NEF", None);
    let packet = "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"><rdf:Description rdf:about=\"\" xmlns:exif=\"http://ns.adobe.com/exif/1.0/\" exif:GPSLatitude=\"46,0.300000N\" exif:GPSLongitude=\"7,0.000000E\"/></rdf:RDF></x:xmpmeta>";
    std::fs::write(a.1.with_extension("xmp"), packet).unwrap();
    import_sidecar(&f.store, a.0, &a.1, 100_000).unwrap();
    let g = f.store.get_gps(&[a.0]).unwrap()[&a.0];
    assert!((g.latitude - 46.005).abs() < 1e-6);

    // A marker write afterwards keeps the position in the file.
    f.store.set_rating(&[a.0], Some(5)).unwrap();
    nicti_lair::scent_sync::write_sidecar(&f.store, a.0, &a.1, 200_000).unwrap();
    assert!(read_gps(&sidecar(&a.1)).is_some());
}

#[test]
fn an_invalid_position_is_refused_by_the_store() {
    let f = fx();
    let a = f.add("a.NEF", None);
    let bad = Gps {
        latitude: 91.0,
        longitude: 0.0,
        altitude: None,
    };
    assert!(f.store.set_gps(a.0, Some(bad)).is_err());
    assert!(f.store.get_gps(&[a.0]).unwrap().is_empty());
}

const GPS_PACKET: &str = "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"><rdf:Description rdf:about=\"\" xmlns:exif=\"http://ns.adobe.com/exif/1.0/\" exif:GPSLatitude=\"LAT\" exif:GPSLongitude=\"7,0.000000E\"/></rdf:RDF></x:xmpmeta>";

fn packet_at(lat: &str) -> String {
    GPS_PACKET.replace("LAT", lat)
}

#[test]
fn an_external_position_change_wins_when_the_catalog_has_no_pending_edit() {
    let f = fx();
    let a = f.add("a.NEF", None);
    std::fs::write(a.1.with_extension("xmp"), packet_at("46,0.300000N")).unwrap();
    import_sidecar(&f.store, a.0, &a.1, 100_000).unwrap();
    // Another tool moves the position in the file; the catalog has nothing pending.
    std::fs::write(a.1.with_extension("xmp"), packet_at("10,0.000000N")).unwrap();
    import_sidecar(&f.store, a.0, &a.1, 200_000).unwrap();
    let g = f.store.get_gps(&[a.0]).unwrap()[&a.0];
    assert!((g.latitude - 10.0).abs() < 1e-6, "{g:?}");
    // A later rating write must not put the old position back.
    f.store.set_rating(&[a.0], Some(2)).unwrap();
    nicti_lair::scent_sync::write_sidecar(&f.store, a.0, &a.1, 300_000).unwrap();
    assert!((read_gps(&sidecar(&a.1)).unwrap().latitude - 10.0).abs() < 1e-5);
}

#[test]
fn a_held_geotag_is_not_forced_over_a_position_the_file_changed_under_us() {
    let f = fx();
    let log = parse_gpx(LOG).unwrap();
    let a = f.add("a.NEF", Some("2026:05:01 10:00:30"));
    f.store.set_rating(&[a.0], Some(3)).unwrap();
    nicti_lair::scent_sync::write_sidecar(&f.store, a.0, &a.1, 1).unwrap();
    // Another tool rewrites the sidecar with its own position, which nicti has not ingested.
    let mut theirs = sidecar(&a.1);
    theirs = theirs.replacen(
        "<rdf:Description ",
        "<rdf:Description exif:GPSLatitude=\"10,0.000000N\" exif:GPSLongitude=\"20,0.000000E\" xmlns:exif=\"http://ns.adobe.com/exif/1.0/\" ",
        1,
    );
    std::fs::write(a.1.with_extension("xmp"), theirs).unwrap();

    let r = geotag_assets(
        &f.store,
        std::slice::from_ref(&a),
        &log,
        &GeotagOptions::default(),
        false,
        true,
        100_000,
    );
    assert_eq!((r.tagged, r.held_for_review), (1, 1), "{r:?}");
    // Rescanning must keep it held and leave the other tool's position in the file.
    let out = import_sidecar(&f.store, a.0, &a.1, 200_000).unwrap();
    assert_eq!(out, SyncOutcome::NeedsReview);
    let file = read_gps(&sidecar(&a.1)).unwrap();
    assert!((file.latitude - 10.0).abs() < 1e-5, "{file:?}");
    assert_eq!(f.store.sidecar_review_assets().unwrap(), vec![a.0]);
}

#[test]
fn an_unreachable_file_is_a_failure_and_stores_no_position() {
    let f = fx();
    let log = parse_gpx(LOG).unwrap();
    let a = f.add("a.NEF", Some("2026:05:01 10:00:30"));
    std::fs::remove_file(&a.1).unwrap();
    let r = geotag_assets(
        &f.store,
        std::slice::from_ref(&a),
        &log,
        &GeotagOptions::default(),
        false,
        true,
        1,
    );
    assert_eq!((r.tagged, r.failed.len()), (0, 1), "{r:?}");
    assert!(f.store.get_gps(&[a.0]).unwrap().is_empty());
    assert!(!a.1.with_extension("xmp").exists(), "no orphan sidecar");
}

#[test]
fn clearing_a_position_removes_it_from_the_catalog_and_the_file_and_it_stays_gone() {
    let f = fx();
    let log = parse_gpx(LOG).unwrap();
    for name in ["a.NEF", "a.jpg"] {
        let a = f.add(name, Some("2026:05:01 10:00:30"));
        geotag_assets(
            &f.store,
            std::slice::from_ref(&a),
            &log,
            &GeotagOptions::default(),
            false,
            true,
            1,
        );
        assert!(
            f.store.get_gps(&[a.0]).unwrap().contains_key(&a.0),
            "{name}"
        );

        let out = clear_position(&f.store, a.0, &a.1, true, 2).unwrap();
        assert_eq!(out, ClearOutcome::Cleared, "{name}");
        assert!(
            f.store.get_gps(&[a.0]).unwrap().is_empty(),
            "{name}: catalog"
        );
        let text = if name.ends_with(".jpg") {
            nicti_scent::embedded::read_xmp(&std::fs::read(&a.1).unwrap())
                .unwrap()
                .unwrap_or_default()
        } else {
            sidecar(&a.1)
        };
        assert!(
            read_gps(&text).is_none(),
            "{name}: file still has a position"
        );

        // A rescan must not adopt a position back from the file.
        import_sidecar(&f.store, a.0, &a.1, 10_000_000).unwrap();
        assert!(
            f.store.get_gps(&[a.0]).unwrap().is_empty(),
            "{name}: re-adopted"
        );
        // And clearing again is a no-op.
        assert_eq!(
            clear_position(&f.store, a.0, &a.1, true, 3).unwrap(),
            ClearOutcome::NothingToClear
        );
    }
}

#[test]
fn clearing_a_position_the_file_carries_needs_auto_write_and_changes_nothing_without_it() {
    let f = fx();
    let log = parse_gpx(LOG).unwrap();
    let a = f.add("a.NEF", Some("2026:05:01 10:00:30"));
    geotag_assets(
        &f.store,
        std::slice::from_ref(&a),
        &log,
        &GeotagOptions::default(),
        false,
        true,
        1,
    );

    let out = clear_position(&f.store, a.0, &a.1, false, 2).unwrap();
    assert_eq!(out, ClearOutcome::NeedsAutoWrite);
    assert!(
        f.store.get_gps(&[a.0]).unwrap().contains_key(&a.0),
        "catalog untouched"
    );
    assert!(read_gps(&sidecar(&a.1)).is_some(), "file untouched");
}

#[test]
fn a_position_only_the_catalog_has_can_be_cleared_without_auto_write() {
    let f = fx();
    let log = parse_gpx(LOG).unwrap();
    let a = f.add("a.NEF", Some("2026:05:01 10:00:30"));
    // Auto-write off when tagging: stored and marked dirty, no sidecar yet.
    geotag_assets(
        &f.store,
        std::slice::from_ref(&a),
        &log,
        &GeotagOptions::default(),
        false,
        false,
        1,
    );
    assert!(!a.1.with_extension("xmp").exists());
    let out = clear_position(&f.store, a.0, &a.1, false, 2).unwrap();
    assert_eq!(out, ClearOutcome::Cleared);
    assert!(f.store.get_gps(&[a.0]).unwrap().is_empty());
}

#[test]
fn a_sidecar_changed_under_us_holds_the_clear_for_review_and_loses_nothing() {
    let f = fx();
    let log = parse_gpx(LOG).unwrap();
    let a = f.add("a.NEF", Some("2026:05:01 10:00:30"));
    geotag_assets(
        &f.store,
        std::slice::from_ref(&a),
        &log,
        &GeotagOptions::default(),
        false,
        true,
        1,
    );
    // Another program edits the sidecar's rating after nicti last wrote it.
    let edited = sidecar(&a.1).replacen("<rdf:Description", "<rdf:Description xmp:Rating=\"4\"", 1);
    std::fs::write(a.1.with_extension("xmp"), edited).unwrap();

    let out = clear_position(&f.store, a.0, &a.1, true, 2).unwrap();
    assert_eq!(out, ClearOutcome::HeldForReview);
    assert!(
        f.store.get_gps(&[a.0]).unwrap().contains_key(&a.0),
        "catalog position kept"
    );
}

#[test]
fn clearing_an_unreachable_file_changes_nothing_so_its_position_cannot_come_back() {
    let f = fx();
    let log = parse_gpx(LOG).unwrap();
    let a = f.add("a.NEF", Some("2026:05:01 10:00:30"));
    geotag_assets(
        &f.store,
        std::slice::from_ref(&a),
        &log,
        &GeotagOptions::default(),
        false,
        true,
        1,
    );
    // The volume goes offline: the RAW and its sidecar are not reachable.
    let moved = f.dir.path().join("offline");
    std::fs::create_dir(&moved).unwrap();
    std::fs::rename(&a.1, moved.join("a.NEF")).unwrap();
    std::fs::rename(a.1.with_extension("xmp"), moved.join("a.xmp")).unwrap();

    let out = clear_position(&f.store, a.0, &a.1, true, 2).unwrap();
    assert_eq!(out, ClearOutcome::Unreachable);
    assert!(
        f.store.get_gps(&[a.0]).unwrap().contains_key(&a.0),
        "the catalog keeps the position the offline file still carries"
    );
}

#[test]
fn clearing_a_jpeg_leaves_its_raw_pairs_sidecar_position_alone() {
    let f = fx();
    let log = parse_gpx(LOG).unwrap();
    let raw = f.add("IMG_1.NEF", Some("2026:05:01 10:00:30"));
    let jpg = f.add("IMG_1.jpg", Some("2026:05:01 10:00:30"));
    geotag_assets(
        &f.store,
        &[raw.clone(), jpg.clone()],
        &log,
        &GeotagOptions::default(),
        false,
        true,
        1,
    );
    assert!(read_gps(&sidecar(&raw.1)).is_some());

    assert_eq!(
        clear_position(&f.store, jpg.0, &jpg.1, true, 2).unwrap(),
        ClearOutcome::Cleared
    );
    assert!(f.store.get_gps(&[jpg.0]).unwrap().is_empty());
    assert!(
        read_gps(&sidecar(&raw.1)).is_some(),
        "the RAW's own sidecar (IMG_1.xmp) keeps its position"
    );
    assert!(f.store.get_gps(&[raw.0]).unwrap().contains_key(&raw.0));
}
