//! Integration tests for #60's XMP sidecar sync (`scent_sync`), against a real temp-file SQLite
//! catalog and real `.xmp` files on disk. No RAW decoding is involved -- the `.NEF` beside each
//! sidecar is a placeholder, since sync only ever derives the sidecar path from it.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use nicti_lair::scent_sync::{
    catalog_markers, import_sidecar, mark_catalog_dirty, resolve_review, write_sidecar, SyncOutcome,
};
use nicti_lair::{AssetMeta, CatalogStore, NewAsset, SqliteCatalog};
use nicti_scent::lrc_fields;

struct Fixture {
    _dir: tempfile::TempDir,
    catalog: SqliteCatalog,
    asset_id: i64,
    raw: PathBuf,
}

fn new_asset(rel: &str) -> NewAsset {
    NewAsset {
        rel_path: rel.to_string(),
        rel_path_fold: rel.to_lowercase(),
        size_bytes: 4,
        mtime_unix: 1,
        fingerprint: None,
        natural_key: None,
        make: None,
        model: None,
        captured_at: None,
        width: None,
        height: None,
        imported_at: 1,
    }
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let catalog = SqliteCatalog::open(&dir.path().join("catalog.db")).unwrap();
    let vol = catalog.upsert_volume("vol", None, None, 1).unwrap();
    let root = catalog
        .ensure_root(vol, dir.path().to_str().unwrap())
        .unwrap();
    let raw = dir.path().join("IMG_0001.NEF");
    fs::write(&raw, b"raw!").unwrap();
    let asset_id = catalog
        .insert_asset(root, &new_asset("IMG_0001.NEF"), None)
        .unwrap();
    Fixture {
        _dir: dir,
        catalog,
        asset_id,
        raw,
    }
}

fn sidecar(raw: &Path) -> PathBuf {
    raw.with_extension("xmp")
}

fn xmp(description_attrs: &str, children: &str) -> String {
    format!(
        "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\
<rdf:Description rdf:about=\"\" xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\" \
xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:lr=\"http://ns.adobe.com/lightroom/1.0/\" \
xmlns:crs=\"http://ns.adobe.com/camera-raw-settings/1.0/\" {description_attrs}>{children}\
</rdf:Description></rdf:RDF></x:xmpmeta>\n<?xpacket end=\"w\"?>"
    )
}

fn bag(tag: &str, items: &[&str]) -> String {
    let lis: String = items
        .iter()
        .map(|i| format!("<rdf:li>{i}</rdf:li>"))
        .collect();
    format!("<{tag}><rdf:Bag>{lis}</rdf:Bag></{tag}>")
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn kw_paths(f: &Fixture) -> Vec<Vec<String>> {
    catalog_markers(&f.catalog, f.asset_id)
        .unwrap()
        .keywords
        .into_iter()
        .collect()
}

fn set(f: &Fixture, rating: Option<i64>, label: Option<&str>) {
    f.catalog
        .set_meta(&[(
            f.asset_id,
            AssetMeta {
                rating,
                flag: None,
                label: label.map(String::from),
            },
        )])
        .unwrap();
}

#[test]
fn import_reads_rating_label_and_keyword_hierarchy() {
    let f = fixture();
    let children = format!(
        "{}{}",
        bag("dc:subject", &["Anthrocon 2025", "fox"]),
        bag("lr:hierarchicalSubject", &["Events|Anthrocon 2025"])
    );
    fs::write(
        sidecar(&f.raw),
        xmp("xmp:Rating=\"4\" xmp:Label=\"Green\"", &children),
    )
    .unwrap();

    let out = import_sidecar(&f.catalog, f.asset_id, &f.raw, now_ms()).unwrap();
    assert_eq!(out, SyncOutcome::AppliedSidecar);

    let m = catalog_markers(&f.catalog, f.asset_id).unwrap();
    assert_eq!(m.rating, Some(4));
    assert_eq!(m.label.as_deref(), Some("Green"));
    // "Anthrocon 2025" appears flat *and* as a hierarchy leaf: one keyword, not two.
    assert_eq!(
        kw_paths(&f),
        vec![
            vec!["Events".to_string(), "Anthrocon 2025".to_string()],
            vec!["fox".to_string()],
        ]
    );
    // The parent "Events" exists as a real keyword, but only the leaf is tagged.
    let names: Vec<String> = f
        .catalog
        .list_keywords()
        .unwrap()
        .into_iter()
        .map(|k| k.name)
        .collect();
    assert!(names.contains(&"Events".to_string()), "{names:?}");
    assert_eq!(f.catalog.keywords_for(f.asset_id).unwrap().len(), 2);
}

#[test]
fn missing_rating_stays_unrated_and_reject_maps_to_minus_one() {
    let f = fixture();
    fs::write(sidecar(&f.raw), xmp("xmp:Label=\"Red\"", "")).unwrap();
    import_sidecar(&f.catalog, f.asset_id, &f.raw, now_ms()).unwrap();
    assert_eq!(
        catalog_markers(&f.catalog, f.asset_id).unwrap().rating,
        None
    );

    fs::write(sidecar(&f.raw), xmp("xmp:Rating=\"-1\"", "")).unwrap();
    // Catalog now holds a label the file dropped: both sides changed since the last look, but
    // the catalog was never dirtied, so the file wins.
    import_sidecar(&f.catalog, f.asset_id, &f.raw, now_ms()).unwrap();
    let m = catalog_markers(&f.catalog, f.asset_id).unwrap();
    assert_eq!(m.rating, Some(-1));
    assert_eq!(m.label, None);
}

#[test]
fn unchanged_sidecar_is_a_noop_on_reimport() {
    let f = fixture();
    fs::write(sidecar(&f.raw), xmp("xmp:Rating=\"2\"", "")).unwrap();
    assert_eq!(
        import_sidecar(&f.catalog, f.asset_id, &f.raw, now_ms()).unwrap(),
        SyncOutcome::AppliedSidecar
    );
    assert_eq!(
        import_sidecar(&f.catalog, f.asset_id, &f.raw, now_ms()).unwrap(),
        SyncOutcome::InSync
    );
}

#[test]
fn import_without_sidecar_reports_none() {
    let f = fixture();
    assert_eq!(
        import_sidecar(&f.catalog, f.asset_id, &f.raw, now_ms()).unwrap(),
        SyncOutcome::NoSidecar
    );
}

#[test]
fn unparseable_sidecar_is_an_error_and_is_never_overwritten() {
    let f = fixture();
    let truncated =
        "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF><rdf:Description xmp:Rating=\"3\"";
    fs::write(sidecar(&f.raw), truncated).unwrap();
    set(&f, Some(5), None);
    assert!(write_sidecar(&f.catalog, f.asset_id, &f.raw, now_ms()).is_err());
    assert_eq!(fs::read_to_string(sidecar(&f.raw)).unwrap(), truncated);
}

#[test]
fn write_creates_a_fresh_sidecar_that_round_trips() {
    let f = fixture();
    set(&f, Some(3), Some("Blue"));
    let events = f.catalog.create_keyword(None, "Events").unwrap();
    let con = f.catalog.create_keyword(Some(events), "Con").unwrap();
    f.catalog.tag(&[f.asset_id], con).unwrap();
    let mut meta = f
        .catalog
        .get_meta(&[f.asset_id])
        .unwrap()
        .remove(&f.asset_id)
        .unwrap();
    meta.flag = Some(1);
    f.catalog.set_meta(&[(f.asset_id, meta)]).unwrap();

    assert_eq!(
        write_sidecar(&f.catalog, f.asset_id, &f.raw, now_ms()).unwrap(),
        SyncOutcome::WroteSidecar
    );
    let text = fs::read_to_string(sidecar(&f.raw)).unwrap();
    let parsed = lrc_fields::read(&text).unwrap();
    assert_eq!(parsed.rating, Some(3));
    assert_eq!(parsed.label.as_deref(), Some("Blue"));
    assert_eq!(parsed.keywords, vec!["Con".to_string()]);
    assert_eq!(
        parsed.hierarchical_keywords,
        vec![vec!["Events".to_string(), "Con".to_string()]]
    );
    assert!(parsed.pick);

    // Writing the same state again touches nothing.
    let before = fs::read(sidecar(&f.raw)).unwrap();
    assert_eq!(
        write_sidecar(&f.catalog, f.asset_id, &f.raw, now_ms()).unwrap(),
        SyncOutcome::InSync
    );
    assert_eq!(fs::read(sidecar(&f.raw)).unwrap(), before);
}

#[test]
fn write_preserves_foreign_properties_in_an_existing_sidecar() {
    let f = fixture();
    fs::write(
        sidecar(&f.raw),
        xmp("xmp:Rating=\"1\" crs:WhiteBalance=\"Custom\"", ""),
    )
    .unwrap();
    import_sidecar(&f.catalog, f.asset_id, &f.raw, now_ms()).unwrap();

    set(&f, Some(5), None);
    assert_eq!(
        write_sidecar(&f.catalog, f.asset_id, &f.raw, now_ms()).unwrap(),
        SyncOutcome::WroteSidecar
    );
    let text = fs::read_to_string(sidecar(&f.raw)).unwrap();
    assert!(text.contains("crs:WhiteBalance=\"Custom\""), "{text}");
    assert_eq!(lrc_fields::read(&text).unwrap().rating, Some(5));
}

#[test]
fn an_unseen_external_edit_is_never_clobbered_by_a_write() {
    let f = fixture();
    fs::write(sidecar(&f.raw), xmp("xmp:Rating=\"1\"", "")).unwrap();
    import_sidecar(&f.catalog, f.asset_id, &f.raw, now_ms()).unwrap();

    // LRC rewrites the sidecar; nicti hasn't synced yet. Then the user rates in nicti.
    let lrc_version = xmp("xmp:Rating=\"2\" xmp:Label=\"Red\"", "");
    fs::write(sidecar(&f.raw), &lrc_version).unwrap();
    set(&f, Some(5), None);

    assert_eq!(
        write_sidecar(&f.catalog, f.asset_id, &f.raw, now_ms()).unwrap(),
        SyncOutcome::NeedsReview
    );
    assert_eq!(fs::read_to_string(sidecar(&f.raw)).unwrap(), lrc_version);
    assert_eq!(f.catalog.sidecar_review_assets().unwrap(), vec![f.asset_id]);

    // Still held on the next write attempt, until someone resolves it.
    assert_eq!(
        write_sidecar(&f.catalog, f.asset_id, &f.raw, now_ms()).unwrap(),
        SyncOutcome::NeedsReview
    );
}

#[test]
fn resolving_a_review_either_way_clears_the_flag() {
    for use_catalog in [true, false] {
        let f = fixture();
        fs::write(sidecar(&f.raw), xmp("xmp:Rating=\"1\"", "")).unwrap();
        import_sidecar(&f.catalog, f.asset_id, &f.raw, now_ms()).unwrap();
        fs::write(sidecar(&f.raw), xmp("xmp:Rating=\"2\"", "")).unwrap();
        set(&f, Some(5), None);
        write_sidecar(&f.catalog, f.asset_id, &f.raw, now_ms()).unwrap();

        resolve_review(&f.catalog, f.asset_id, &f.raw, use_catalog, now_ms()).unwrap();
        assert!(f.catalog.sidecar_review_assets().unwrap().is_empty());
        let on_disk = lrc_fields::read(&fs::read_to_string(sidecar(&f.raw)).unwrap())
            .unwrap()
            .rating;
        let in_catalog = catalog_markers(&f.catalog, f.asset_id).unwrap().rating;
        assert_eq!(on_disk, in_catalog);
        assert_eq!(in_catalog, Some(if use_catalog { 5 } else { 2 }));
    }
}

#[test]
fn a_newer_sidecar_beats_an_older_dirty_catalog() {
    let f = fixture();
    set(&f, Some(4), None);
    mark_catalog_dirty(&f.catalog, f.asset_id, &f.raw, 1_000).unwrap();
    fs::write(sidecar(&f.raw), xmp("xmp:Rating=\"2\"", "")).unwrap();

    assert_eq!(
        import_sidecar(&f.catalog, f.asset_id, &f.raw, now_ms()).unwrap(),
        SyncOutcome::AppliedSidecar
    );
    assert_eq!(
        catalog_markers(&f.catalog, f.asset_id).unwrap().rating,
        Some(2)
    );
}

#[test]
fn a_newer_dirty_catalog_beats_an_older_sidecar() {
    let f = fixture();
    fs::write(sidecar(&f.raw), xmp("xmp:Rating=\"2\"", "")).unwrap();
    set(&f, Some(4), None);
    mark_catalog_dirty(&f.catalog, f.asset_id, &f.raw, now_ms() + 60_000).unwrap();

    assert_eq!(
        import_sidecar(&f.catalog, f.asset_id, &f.raw, now_ms()).unwrap(),
        SyncOutcome::WroteSidecar
    );
    let text = fs::read_to_string(sidecar(&f.raw)).unwrap();
    assert_eq!(lrc_fields::read(&text).unwrap().rating, Some(4));
}

#[test]
fn changes_inside_the_ambiguity_window_are_flagged_not_guessed() {
    let f = fixture();
    fs::write(sidecar(&f.raw), xmp("xmp:Rating=\"2\"", "")).unwrap();
    let on_disk_before = fs::read(sidecar(&f.raw)).unwrap();
    set(&f, Some(4), None);
    // Dirty "now", within the window of the sidecar that was just written.
    mark_catalog_dirty(&f.catalog, f.asset_id, &f.raw, now_ms()).unwrap();

    assert_eq!(
        import_sidecar(&f.catalog, f.asset_id, &f.raw, now_ms()).unwrap(),
        SyncOutcome::NeedsReview
    );
    assert_eq!(fs::read(sidecar(&f.raw)).unwrap(), on_disk_before);
    assert_eq!(
        catalog_markers(&f.catalog, f.asset_id).unwrap().rating,
        Some(4)
    );
}

#[test]
fn a_catalog_with_unsynced_edits_and_no_sync_record_is_flagged_not_overwritten() {
    let f = fixture();
    set(&f, Some(4), Some("Red"));
    fs::write(sidecar(&f.raw), xmp("xmp:Rating=\"1\"", "")).unwrap();

    assert_eq!(
        import_sidecar(&f.catalog, f.asset_id, &f.raw, now_ms()).unwrap(),
        SyncOutcome::NeedsReview
    );
    assert_eq!(
        catalog_markers(&f.catalog, f.asset_id).unwrap().rating,
        Some(4)
    );
}

#[test]
fn ingesting_a_nef_with_an_xmp_pair_imports_its_markers() {
    use nicti_lair::scruff::ingest_root;

    let dir = tempfile::tempdir().unwrap();
    let catalog = SqliteCatalog::open(&dir.path().join("catalog.db")).unwrap();
    let vol = catalog.upsert_volume("vol", None, None, 1).unwrap();
    let shots = dir.path().join("shots");
    fs::create_dir(&shots).unwrap();
    let root = catalog.ensure_root(vol, shots.to_str().unwrap()).unwrap();

    let raw = shots.join("DSC_0001.NEF");
    fs::write(&raw, b"not a real nef, just bytes").unwrap();
    fs::write(
        sidecar(&raw),
        xmp(
            "xmp:Rating=\"5\" xmp:Label=\"Purple\"",
            &bag("dc:subject", &["fox"]),
        ),
    )
    .unwrap();
    // A second NEF with no sidecar must be unaffected.
    fs::write(
        shots.join("DSC_0002.NEF"),
        b"another, different set of bytes",
    )
    .unwrap();

    let report = ingest_root(&catalog, root, &shots).unwrap();
    assert_eq!(report.added, 2);
    assert!(
        report.sidecar_errors.is_empty(),
        "{:?}",
        report.sidecar_errors
    );

    let a = catalog
        .find_asset_by_path(root, "DSC_0001.NEF")
        .unwrap()
        .unwrap();
    let m = catalog_markers(&catalog, a.id).unwrap();
    assert_eq!(m.rating, Some(5));
    assert_eq!(m.label.as_deref(), Some("Purple"));
    assert_eq!(m.keywords.len(), 1);

    let b = catalog
        .find_asset_by_path(root, "DSC_0002.NEF")
        .unwrap()
        .unwrap();
    assert_eq!(catalog_markers(&catalog, b.id).unwrap().rating, None);

    // LRC edits the sidecar; a rescan (RAW unchanged) picks the change up.
    fs::write(sidecar(&raw), xmp("xmp:Rating=\"2\"", "")).unwrap();
    let again = ingest_root(&catalog, root, &shots).unwrap();
    assert_eq!(again.skipped_unchanged, 2);
    assert_eq!(catalog_markers(&catalog, a.id).unwrap().rating, Some(2));
}

#[test]
fn a_corrupt_sidecar_is_reported_but_does_not_fail_the_ingest() {
    use nicti_lair::scruff::ingest_root;

    let dir = tempfile::tempdir().unwrap();
    let catalog = SqliteCatalog::open(&dir.path().join("catalog.db")).unwrap();
    let vol = catalog.upsert_volume("vol", None, None, 1).unwrap();
    let root = catalog
        .ensure_root(vol, dir.path().to_str().unwrap())
        .unwrap();
    let raw = dir.path().join("DSC_0001.NEF");
    fs::write(&raw, b"bytes").unwrap();
    fs::write(sidecar(&raw), [0xff, 0xfe, 0x00]).unwrap();

    let report = ingest_root(&catalog, root, dir.path()).unwrap();
    assert_eq!(report.added, 1);
    assert!(report.failed.is_empty());
    assert_eq!(report.sidecar_errors.len(), 1);
}

// ---- regression tests for the pre-PR adversarial review's confirmed findings ----

fn tag_path(f: &Fixture, path: &[&str]) {
    let mut parent = None;
    for seg in path {
        parent = Some(
            match f
                .catalog
                .keyword_by_path(&path[..=path.iter().position(|s| s == seg).unwrap()])
                .unwrap()
            {
                Some(k) => k.id,
                None => f.catalog.create_keyword(parent, seg).unwrap(),
            },
        );
    }
    f.catalog.tag(&[f.asset_id], parent.unwrap()).unwrap();
}

#[test]
fn special_characters_round_trip_through_a_write_and_a_reimport_without_renaming() {
    let f = fixture();
    set(&f, Some(2), Some("Tom & Jerry"));
    tag_path(&f, &["Tom & Jerry"]);
    tag_path(&f, &["a<b"]);
    tag_path(&f, &["it's \"q\""]);
    write_sidecar(&f.catalog, f.asset_id, &f.raw, now_ms()).unwrap();
    let before = catalog_markers(&f.catalog, f.asset_id).unwrap();

    // LRC touches an unrelated attribute, so the hash changes and the file is re-read.
    let text = fs::read_to_string(sidecar(&f.raw)).unwrap();
    fs::write(
        sidecar(&f.raw),
        text.replacen(
            "<rdf:Description",
            "<rdf:Description xmlns:foo=\"urn:foo\" foo:x=\"1\"",
            1,
        ),
    )
    .unwrap();
    let out = import_sidecar(&f.catalog, f.asset_id, &f.raw, now_ms()).unwrap();
    assert_eq!(
        out,
        SyncOutcome::InSync,
        "an unrelated change must not look like a marker change"
    );
    assert_eq!(catalog_markers(&f.catalog, f.asset_id).unwrap(), before);
    assert_eq!(before.label.as_deref(), Some("Tom & Jerry"));
}

#[test]
fn a_held_conflict_is_not_resolved_by_the_next_rescan() {
    let f = fixture();
    fs::write(sidecar(&f.raw), xmp("xmp:Rating=\"3\"", "")).unwrap();
    import_sidecar(&f.catalog, f.asset_id, &f.raw, now_ms()).unwrap();

    // LRC adds a keyword (un-ingested); nicti then changes the rating and is held.
    let lrc = xmp("xmp:Rating=\"3\"", &bag("dc:subject", &["lrc-keyword"]));
    fs::write(sidecar(&f.raw), &lrc).unwrap();
    set(&f, Some(5), None);
    assert_eq!(
        write_sidecar(&f.catalog, f.asset_id, &f.raw, now_ms() + 30_000).unwrap(),
        SyncOutcome::NeedsReview
    );
    // A rescan well after the file's mtime must not flip the hold into an overwrite.
    assert_eq!(
        import_sidecar(&f.catalog, f.asset_id, &f.raw, now_ms() + 60_000).unwrap(),
        SyncOutcome::NeedsReview
    );
    assert_eq!(fs::read_to_string(sidecar(&f.raw)).unwrap(), lrc);
}

#[test]
fn a_bad_rating_is_an_error_that_leaves_the_catalog_alone() {
    for bad in ["6", "-2", "200", "abc", "3.5"] {
        let f = fixture();
        set(&f, Some(3), None);
        mark_catalog_dirty(&f.catalog, f.asset_id, &f.raw, 1).unwrap();
        fs::write(sidecar(&f.raw), xmp(&format!("xmp:Rating=\"{bad}\""), "")).unwrap();
        assert!(
            import_sidecar(&f.catalog, f.asset_id, &f.raw, now_ms()).is_err(),
            "{bad}"
        );
        assert_eq!(
            catalog_markers(&f.catalog, f.asset_id).unwrap().rating,
            Some(3),
            "{bad}"
        );
    }
}

#[test]
fn a_top_level_keyword_and_a_nested_one_with_the_same_leaf_stay_distinct() {
    let f = fixture();
    tag_path(&f, &["Anthrocon"]);
    tag_path(&f, &["Events", "Anthrocon"]);
    let before = catalog_markers(&f.catalog, f.asset_id).unwrap();
    assert_eq!(before.keywords.len(), 2);

    write_sidecar(&f.catalog, f.asset_id, &f.raw, now_ms()).unwrap();
    // Reading back the file nicti wrote yields the same marker set, so nothing flaps.
    let text = fs::read_to_string(sidecar(&f.raw)).unwrap();
    let back = nicti_lair::scent_sync::Markers::from_lrc(&lrc_fields::read(&text).unwrap());
    assert_eq!(back.keywords, before.keywords);
    assert_eq!(
        write_sidecar(&f.catalog, f.asset_id, &f.raw, now_ms()).unwrap(),
        SyncOutcome::InSync
    );
}

#[test]
fn a_pipe_in_a_keyword_name_is_stable_not_a_perpetual_diff() {
    let f = fixture();
    tag_path(&f, &["x", "a|b"]);
    write_sidecar(&f.catalog, f.asset_id, &f.raw, now_ms()).unwrap();
    assert_eq!(
        write_sidecar(&f.catalog, f.asset_id, &f.raw, now_ms()).unwrap(),
        SyncOutcome::InSync
    );
}

#[test]
fn a_sidecar_with_no_markers_is_filled_from_a_rated_catalog_not_flagged() {
    let f = fixture();
    set(&f, Some(4), None);
    fs::write(sidecar(&f.raw), xmp("crs:WhiteBalance=\"Custom\"", "")).unwrap();
    assert_eq!(
        import_sidecar(&f.catalog, f.asset_id, &f.raw, now_ms()).unwrap(),
        SyncOutcome::WroteSidecar
    );
    let text = fs::read_to_string(sidecar(&f.raw)).unwrap();
    assert!(text.contains("crs:WhiteBalance=\"Custom\""), "{text}");
    assert_eq!(lrc_fields::read(&text).unwrap().rating, Some(4));
    assert!(f.catalog.sidecar_review_assets().unwrap().is_empty());
}
