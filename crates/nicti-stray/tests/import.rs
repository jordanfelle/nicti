//! End-to-end: a synthetic `.lrcat` (real v13 table names, invented values) imported into a real
//! SQLite catalog by driving `LrcImportJob::step()` to completion, the way Pounce would.

#[path = "../src/test_fixture.rs"]
mod test_fixture;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use nicti_lair::{AssetMeta, CatalogStore, SqliteCatalog};
use nicti_pounce::{ChunkedJob, Step};
use nicti_stray::{ImportConfig, LrcImportJob, LrcImportReport};
use rusqlite::Connection;
use test_fixture::*;

/// A fixture library: two folders under one root with `a`, `b` (+ a virtual copy of `b`), `c`, a
/// photo LRC knows but the disk doesn't, keywords, a collection set holding a collection, and
/// develop settings.
struct World {
    _dir: tempfile::TempDir,
    root: PathBuf,
    lrcat: PathBuf,
}

const DEV_A: &str =
    "s = { Exposure2012 = 0.5, Contrast2012 = 20, ToneCurvePV2012 = { 0, 0, 64, 70, 255, 255, } }";

fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(root.join("2026/Event")).unwrap();
    for (rel, body) in [
        ("2026/Event/a.NEF", "aaaa"),
        ("2026/Event/b.NEF", "bbbbbb"),
        ("2026/c.NEF", "cccccccc"),
    ] {
        std::fs::write(root.join(rel), body).unwrap();
    }
    let lrcat = dir.path().join("test.lrcat");
    let conn = Connection::open(&lrcat).unwrap();
    create_schema(&conn);
    add_root(&conn, 1, root.to_str().unwrap());
    add_folder(&conn, 10, 1, "2026/Event/");
    add_folder(&conn, 11, 1, "2026/");
    add_file(&conn, 100, 10, "a", "NEF");
    add_file(&conn, 101, 10, "b", "NEF");
    add_file(&conn, 102, 11, "c", "NEF");
    add_file(&conn, 103, 11, "gone", "NEF");
    add_image(
        &conn,
        &Image {
            rating: Some(4.0),
            pick: 1.0,
            label: "Red",
            ..Image::plain(1, 100)
        },
    );
    add_image(
        &conn,
        &Image {
            rating: Some(2.0),
            ..Image::plain(2, 101)
        },
    );
    add_image(
        &conn,
        &Image {
            master: Some(2),
            copy_name: Some("Copy 1"),
            ..Image::plain(3, 101)
        },
    );
    add_image(
        &conn,
        &Image {
            rating: Some(5.0),
            pick: -1.0,
            ..Image::plain(4, 102)
        },
    );
    add_image(&conn, &Image::plain(5, 103));
    add_develop(&conn, 1, DEV_A);
    add_develop(&conn, 3, "s = { Exposure2012 = -1 }");
    add_develop(&conn, 4, "this is not lua");
    add_iptc(&conn, 1, "a caption", "(c) test");
    add_keyword(&conn, 1, None, "/1");
    add_keyword(&conn, 2, Some("Events"), "/1/2");
    add_keyword(&conn, 3, Some("Named"), "/1/2/3");
    add_keyword(&conn, 4, Some("Solo"), "/1/4");
    tag(&conn, 1, 3);
    tag(&conn, 2, 3);
    tag(&conn, 5, 3); // unmatched photo: silently has no asset to tag
    tag(&conn, 4, 4);
    add_collection(&conn, 20, "Set", None, "com.adobe.ag.library.group");
    add_collection(
        &conn,
        21,
        "Picks",
        Some(20),
        "com.adobe.ag.library.collection",
    );
    add_collection(&conn, 22, "scratch", None, "com.adobe.ag.print.unsaved");
    add_collection(
        &conn,
        23,
        "Smart",
        None,
        "com.adobe.ag.library.smart_collection",
    );
    add_to_collection(&conn, 21, 4, 1.0);
    add_to_collection(&conn, 21, 1, 2.0);
    World {
        _dir: dir,
        root,
        lrcat,
    }
}

fn run(store: &Arc<SqliteCatalog>, lrcat: &Path) -> LrcImportReport {
    let dynamic: Arc<dyn CatalogStore + Send + Sync> = store.clone();
    let (mut job, slot) = LrcImportJob::new(
        dynamic,
        ImportConfig {
            catalog_path: lrcat.to_path_buf(),
            remaps: vec![],
            only_roots: vec![],
        },
    );
    loop {
        match job.step().unwrap() {
            Step::Done => break,
            Step::Yield => {}
        }
    }
    drop(job);
    let report = slot
        .lock()
        .unwrap()
        .take()
        .expect("the slot always resolves");
    report
}

fn asset_id(store: &SqliteCatalog, root: &Path, rel: &str) -> i64 {
    let root_id = store
        .list_roots()
        .unwrap()
        .into_iter()
        .find(|r| Path::new(&r.path) == root)
        .expect("root registered")
        .id;
    store
        .find_asset_by_path(root_id, rel)
        .unwrap()
        .expect("asset")
        .id
}

#[test]
fn imports_metadata_keywords_collections_variants_and_provenance() {
    let w = world();
    let store = Arc::new(SqliteCatalog::open_in_memory().unwrap());
    let report = run(&store, &w.lrcat);

    assert_eq!(report.error, None);
    assert!(!report.cancelled);
    assert_eq!((report.matched(), report.missing()), (4, 1));
    assert_eq!(report.roots[0].missing_examples, vec!["2026/gone.NEF"]);
    assert_eq!(report.virtual_copies, 1);
    assert_eq!(report.develop_parse_failures, 1);
    assert_eq!(report.untranslated.get("ToneCurvePV2012"), Some(&1));

    let a = asset_id(&store, &w.root, "2026/Event/a.NEF");
    let b = asset_id(&store, &w.root, "2026/Event/b.NEF");
    let c = asset_id(&store, &w.root, "2026/c.NEF");
    let meta = store.get_meta(&[a, b, c]).unwrap();
    assert_eq!(
        meta[&a],
        AssetMeta {
            rating: Some(4),
            flag: Some(1),
            label: Some("Red".into())
        }
    );
    assert_eq!(
        meta[&b],
        AssetMeta {
            rating: Some(2),
            flag: None,
            label: None
        }
    );
    // Reject wins over the 5 stars, which survive in provenance.
    assert_eq!(meta[&c].rating, Some(-1));
    assert_eq!(
        store.lrc_provenance("G4").unwrap().unwrap().lrc_rating,
        Some(5.0)
    );

    // Develop: translated edit for a; provenance keeps the verbatim text and IPTC.
    let doc = store.get_master_edit(a).unwrap().unwrap();
    assert_eq!(doc.stages["nicti.exposure"].params["stops"], 0.5);
    let prov = store.lrc_provenance("G1").unwrap().unwrap();
    assert_eq!(prov.develop_text.as_deref(), Some(DEV_A));
    assert_eq!(prov.untranslated, vec!["ToneCurvePV2012".to_string()]);
    assert_eq!(prov.iptc_caption.as_deref(), Some("a caption"));

    // Virtual copy: an extra variant with its own edit; the master's markers untouched by it.
    assert_eq!(store.variant_names(b).unwrap(), vec!["master", "Copy 1"]);
    let copy = store.get_variant_edit(b, "Copy 1").unwrap().unwrap();
    assert_eq!(copy.stages["nicti.exposure"].params["stops"], -1.0);

    // Keywords: Events/Named tags a and b (unmatched photo ignored); root skipped; Solo on c.
    let named = store
        .keyword_by_path(&["Events", "Named"])
        .unwrap()
        .unwrap();
    assert_eq!(
        store
            .keywords_for(a)
            .unwrap()
            .iter()
            .map(|k| k.id)
            .collect::<Vec<_>>(),
        vec![named.id]
    );
    assert_eq!(store.keywords_for(b).unwrap().len(), 1);
    assert_eq!(store.keywords_for(c).unwrap()[0].name, "Solo");
    assert_eq!(report.keywords_created, 3);
    assert_eq!(report.assets_tagged, 3);

    // Collections: the set is a parent, scratch/smart are not imported, membership keeps LRC order.
    assert_eq!(report.smart_collections_skipped, 1);
    let cols = store.list_collections().unwrap();
    assert_eq!(cols.len(), 2);
    let set = cols.iter().find(|c| c.name == "Set").unwrap();
    let picks = cols.iter().find(|c| c.name == "Picks").unwrap();
    assert_eq!(picks.parent_id, Some(set.id));
    assert_eq!(store.collection_assets(picks.id).unwrap(), vec![c, a]);
}

#[test]
fn a_rerun_changes_nothing_and_never_overwrites_a_later_nicti_edit() {
    let w = world();
    let store = Arc::new(SqliteCatalog::open_in_memory().unwrap());
    run(&store, &w.lrcat);
    let a = asset_id(&store, &w.root, "2026/Event/a.NEF");

    let again = run(&store, &w.lrcat);
    assert_eq!(again.error, None);
    assert_eq!(
        (
            again.docs_written,
            again.kept_local_docs,
            again.meta_applied,
            again.kept_local_meta
        ),
        (0, 0, 0, 0)
    );
    assert_eq!((again.keywords_created, again.collections_created), (0, 0));
    assert_eq!(store.list_collections().unwrap().len(), 2);
    assert_eq!(
        store
            .variant_names(asset_id(&store, &w.root, "2026/Event/b.NEF"))
            .unwrap()
            .len(),
        2
    );

    // The user rates and edits in nicti; LRC data is unchanged, so the re-run must leave both be.
    store.set_rating(&[a], Some(1)).unwrap();
    let mut doc = store.get_master_edit(a).unwrap().unwrap();
    doc.stages.get_mut("nicti.exposure").unwrap().params = serde_json::json!({ "stops": 2.0 });
    store.put_master_edit(a, &doc).unwrap();
    let third = run(&store, &w.lrcat);
    assert_eq!((third.kept_local_docs, third.kept_local_meta), (1, 1));
    assert_eq!(store.get_meta(&[a]).unwrap()[&a].rating, Some(1));
    assert_eq!(
        store.get_master_edit(a).unwrap().unwrap().stages["nicti.exposure"].params["stops"],
        2.0
    );
}

#[test]
fn imported_markers_are_marked_catalog_dirty_for_the_xmp_sync() {
    let w = world();
    let store = Arc::new(SqliteCatalog::open_in_memory().unwrap());
    let report = run(&store, &w.lrcat);
    assert!(report.sidecars_marked >= 3);
    let a = asset_id(&store, &w.root, "2026/Event/a.NEF");
    let state = store.sidecar_state(a).unwrap().expect("dirty row written");
    assert!(state.catalog_dirty_since_ms.is_some());
}

#[test]
fn a_live_catalog_is_refused_and_the_slot_still_resolves() {
    let w = world();
    std::fs::write(
        w.lrcat.with_file_name("test.lrcat.lock"),
        b"Lightroom.exe 1234",
    )
    .unwrap();
    let store = Arc::new(SqliteCatalog::open_in_memory().unwrap());
    let report = run(&store, &w.lrcat);
    assert!(
        report.error.as_deref().is_some_and(|e| e.contains("lock")),
        "{:?}",
        report.error
    );
    assert_eq!(store.asset_count().unwrap(), 0);
}

#[test]
fn a_missing_root_folder_counts_its_photos_missing_without_failing() {
    let w = world();
    std::fs::remove_dir_all(&w.root).unwrap();
    let store = Arc::new(SqliteCatalog::open_in_memory().unwrap());
    let report = run(&store, &w.lrcat);
    assert_eq!(report.error, None);
    assert!(!report.roots[0].exists);
    assert_eq!((report.matched(), report.missing()), (0, 5));
    assert_eq!(store.asset_count().unwrap(), 0);
}

#[test]
fn dropping_the_job_early_resolves_the_slot_as_cancelled() {
    let w = world();
    let store = Arc::new(SqliteCatalog::open_in_memory().unwrap());
    let dynamic: Arc<dyn CatalogStore + Send + Sync> = store.clone();
    let (mut job, slot) = LrcImportJob::new(
        dynamic,
        ImportConfig {
            catalog_path: w.lrcat.clone(),
            remaps: vec![],
            only_roots: vec![],
        },
    );
    assert!(matches!(job.step().unwrap(), Step::Yield));
    drop(job);
    let report = slot.lock().unwrap().take().expect("Drop resolves the slot");
    assert!(report.cancelled);
}

#[test]
fn only_the_selected_roots_are_ingested_and_imported() {
    let w = world();
    // A second LRC root with its own photo on disk, not selected.
    let other = w.root.parent().unwrap().join("other");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("z.NEF"), "zzzzzzzz").unwrap();
    let conn = Connection::open(&w.lrcat).unwrap();
    add_root(&conn, 2, other.to_str().unwrap());
    add_folder(&conn, 20, 2, "");
    add_file(&conn, 200, 20, "z", "NEF");
    add_image(
        &conn,
        &Image {
            rating: Some(3.0),
            ..Image::plain(50, 200)
        },
    );
    drop(conn);

    let store = Arc::new(SqliteCatalog::open_in_memory().unwrap());
    let dynamic: Arc<dyn CatalogStore + Send + Sync> = store.clone();
    let (mut job, slot) = LrcImportJob::new(
        dynamic,
        ImportConfig {
            catalog_path: w.lrcat.clone(),
            remaps: vec![],
            only_roots: vec![2],
        },
    );
    while matches!(job.step().unwrap(), Step::Yield) {}
    drop(job);
    let report = slot.lock().unwrap().take().unwrap();
    assert_eq!(report.error, None);
    assert_eq!(report.roots.len(), 1);
    assert_eq!(report.matched(), 1);
    assert_eq!(store.asset_count().unwrap(), 1);
    assert!(
        store.lrc_provenance("G1").unwrap().is_none(),
        "root 1's photos untouched"
    );
    assert!(store.lrc_provenance("G50").unwrap().is_some());
}

#[test]
fn a_root_with_a_trailing_separator_reuses_the_root_the_import_button_registered() {
    let w = world();
    // The Import button registers the folder as typed: no trailing slash.
    let store = Arc::new(SqliteCatalog::open_in_memory().unwrap());
    let volume = store
        .upsert_volume(
            nicti_lair::scruff::PLACEHOLDER_VOLUME_IDENTITY_KEY,
            None,
            None,
            0,
        )
        .unwrap();
    store.ensure_root(volume, w.root.to_str().unwrap()).unwrap();
    // LRC wrote the same folder with a trailing separator.
    let conn = Connection::open(&w.lrcat).unwrap();
    conn.execute(
        "UPDATE AgLibraryRootFolder SET absolutePath = absolutePath || '/'",
        [],
    )
    .unwrap();
    drop(conn);

    let report = run(&store, &w.lrcat);
    assert_eq!(report.error, None);
    assert_eq!(store.list_roots().unwrap().len(), 1, "one root, not two");
    assert_eq!(store.asset_count().unwrap(), 3, "no file is ingested twice");
}

#[test]
fn an_unrated_lrc_photo_never_erases_a_rating_already_in_nicti() {
    let w = world();
    let store = Arc::new(SqliteCatalog::open_in_memory().unwrap());
    // The user already has the folder in nicti and has rated `b` five stars there.
    let volume = store
        .upsert_volume(
            nicti_lair::scruff::PLACEHOLDER_VOLUME_IDENTITY_KEY,
            None,
            None,
            0,
        )
        .unwrap();
    let root = store.ensure_root(volume, w.root.to_str().unwrap()).unwrap();
    nicti_lair::scruff::ingest_root(store.as_ref(), root, &w.root).unwrap();
    let b = asset_id(&store, &w.root, "2026/Event/b.NEF");
    store.set_rating(&[b], Some(5)).unwrap();

    // LRC has `b` unrated; a first import must keep the user's rating.
    let conn = Connection::open(&w.lrcat).unwrap();
    conn.execute(
        "UPDATE Adobe_images SET rating = NULL WHERE id_local = 2",
        [],
    )
    .unwrap();
    drop(conn);
    let report = run(&store, &w.lrcat);
    assert_eq!(report.error, None);
    assert_eq!(store.get_meta(&[b]).unwrap()[&b].rating, Some(5));
}
