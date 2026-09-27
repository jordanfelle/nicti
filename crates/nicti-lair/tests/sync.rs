//! Integration tests for #24's `patrol::sync_root`, against a real (temp-file/in-memory) SQLite
//! catalog. Reuses `tests/ingest.rs`'s approach of hand-building a minimal synthetic TIFF rather
//! than depending on a real NEF fixture (none exists in-tree, see the `raw-decoder`/
//! `preview-tiers` topics) -- kept as its own small copy here rather than factored into a shared
//! test-helper module, so this file stays independent of `tests/ingest.rs`'s own churn.

use std::path::Path;

use nicti_lair::patrol::{sync_root, SyncOptions};
use nicti_lair::scruff::ingest_root;
use nicti_lair::{CatalogStore, SqliteCatalog};

const FAKE_JPEG: &[u8] = b"\xFF\xD8FAKEDATA\xFF\xD9";

/// Builds a minimal synthetic RAW-shaped file: not a real TIFF, just distinguishable content of a
/// realistic size. `sync_root`'s own logic never parses file content (that's Scruff's job, already
/// covered by `tests/ingest.rs`) -- it only cares whether a cataloged path resolves on disk, so
/// these tests don't need a real embedded-preview-bearing TIFF the way `tests/ingest.rs` does.
fn write_fake_raw(dir: &Path, name: &str, fill_byte: u8) -> std::path::PathBuf {
    let path = dir.join(name);
    let mut bytes = FAKE_JPEG.to_vec();
    bytes.extend(std::iter::repeat_n(fill_byte, 4096));
    std::fs::write(&path, bytes).unwrap();
    path
}

fn setup() -> (SqliteCatalog, i64) {
    let store = SqliteCatalog::open_in_memory().unwrap();
    let volume_id = store
        .upsert_volume("test-volume", None, None, 1000)
        .unwrap();
    let root_id = store.ensure_root(volume_id, "").unwrap();
    (store, root_id)
}

#[test]
fn a_deleted_file_is_marked_missing_and_its_row_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let (store, root_id) = setup();
    let path = write_fake_raw(dir.path(), "a.NEF", 1);

    sync_root(&store, root_id, dir.path(), &SyncOptions::default()).unwrap();
    std::fs::remove_file(&path).unwrap();

    let report = sync_root(&store, root_id, dir.path(), &SyncOptions::default()).unwrap();
    assert_eq!(report.newly_missing, 1);
    assert_eq!(report.removed, 0);

    let asset = store
        .find_asset_by_path(root_id, "a.NEF")
        .unwrap()
        .expect("row is kept, not deleted");
    assert!(asset.missing_since.is_some());

    // Re-running without the file coming back must be idempotent: still missing, not re-counted.
    let second = sync_root(&store, root_id, dir.path(), &SyncOptions::default()).unwrap();
    assert_eq!(second.newly_missing, 0);
    assert_eq!(second.found_again, 0);
}

#[test]
fn remove_missing_deletes_the_row_and_its_preview_and_updates_facet_counts() {
    let dir = tempfile::tempdir().unwrap();
    let (store, root_id) = setup();
    let path = write_fake_raw(dir.path(), "a.NEF", 1);

    sync_root(&store, root_id, dir.path(), &SyncOptions::default()).unwrap();
    assert_eq!(store.facet_count(None, 0).unwrap(), 1);
    std::fs::remove_file(&path).unwrap();

    let opts = SyncOptions {
        remove_missing: true,
    };
    let report = sync_root(&store, root_id, dir.path(), &opts).unwrap();
    assert_eq!(report.removed, 1);
    assert_eq!(report.newly_missing, 0);

    assert!(store
        .find_asset_by_path(root_id, "a.NEF")
        .unwrap()
        .is_none());
    assert_eq!(
        store.facet_count(None, 0).unwrap(),
        0,
        "facet count must drop along with the removed asset"
    );
}

#[test]
fn a_file_restored_after_being_missing_is_found_again() {
    let dir = tempfile::tempdir().unwrap();
    let (store, root_id) = setup();
    let path = write_fake_raw(dir.path(), "a.NEF", 1);

    sync_root(&store, root_id, dir.path(), &SyncOptions::default()).unwrap();
    std::fs::remove_file(&path).unwrap();
    sync_root(&store, root_id, dir.path(), &SyncOptions::default()).unwrap();

    let asset = store.find_asset_by_path(root_id, "a.NEF").unwrap().unwrap();
    assert!(asset.missing_since.is_some());

    write_fake_raw(dir.path(), "a.NEF", 1);
    let report = sync_root(&store, root_id, dir.path(), &SyncOptions::default()).unwrap();
    assert_eq!(report.found_again, 1);

    let asset = store.find_asset_by_path(root_id, "a.NEF").unwrap().unwrap();
    assert!(asset.missing_since.is_none());
}

#[test]
fn a_renamed_file_is_relinked_not_marked_missing() {
    let dir = tempfile::tempdir().unwrap();
    let (store, root_id) = setup();
    let path = write_fake_raw(dir.path(), "a.NEF", 1);
    ingest_root(&store, root_id, dir.path()).unwrap();

    std::fs::rename(&path, dir.path().join("renamed.NEF")).unwrap();

    let report = sync_root(&store, root_id, dir.path(), &SyncOptions::default()).unwrap();
    assert_eq!(report.ingest.moved, 1);
    assert_eq!(
        report.newly_missing, 0,
        "a relink must not also flag missing"
    );

    let asset = store
        .find_asset_by_path(root_id, "renamed.NEF")
        .unwrap()
        .expect("relinked to its new path");
    assert!(asset.missing_since.is_none());
}

#[test]
fn an_unreachable_root_touches_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (store, root_id) = setup();
    write_fake_raw(dir.path(), "a.NEF", 1);
    sync_root(&store, root_id, dir.path(), &SyncOptions::default()).unwrap();

    let gone = dir.path().join("does-not-exist-anymore");

    let report = sync_root(&store, root_id, &gone, &SyncOptions::default()).unwrap();
    assert!(report.root_unreachable);
    assert_eq!(report.newly_missing, 0);
    assert_eq!(report.removed, 0);

    let asset = store.find_asset_by_path(root_id, "a.NEF").unwrap().unwrap();
    assert!(
        asset.missing_since.is_none(),
        "an unreachable root must never flag its assets missing"
    );
}

#[cfg(unix)]
#[test]
fn an_unreadable_subdirectory_leaves_its_assets_unmarked() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let (store, root_id) = setup();
    write_fake_raw(dir.path(), "visible.NEF", 1);
    let blocked = dir.path().join("blocked");
    std::fs::create_dir(&blocked).unwrap();
    write_fake_raw(&blocked, "hidden.NEF", 2);

    sync_root(&store, root_id, dir.path(), &SyncOptions::default()).unwrap();
    let hidden = store
        .find_asset_by_path(root_id, "blocked/hidden.NEF")
        .unwrap()
        .expect("cataloged while readable");

    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let report = sync_root(&store, root_id, dir.path(), &SyncOptions::default()).unwrap();
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert!(!report.ingest.failed.is_empty());
    let hidden_after = store
        .find_asset_by_path(root_id, "blocked/hidden.NEF")
        .unwrap()
        .unwrap();
    assert_eq!(
        hidden.missing_since, hidden_after.missing_since,
        "an asset under an unreadable directory must be left alone, not flagged missing"
    );
}

#[test]
fn a_directory_whose_every_asset_is_missing_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let (store, root_id) = setup();
    let sub = dir.path().join("2026").join("09");
    std::fs::create_dir_all(&sub).unwrap();
    write_fake_raw(&sub, "a.NEF", 1);
    write_fake_raw(dir.path(), "top.NEF", 2);

    sync_root(&store, root_id, dir.path(), &SyncOptions::default()).unwrap();
    std::fs::remove_dir_all(dir.path().join("2026")).unwrap();

    let report = sync_root(&store, root_id, dir.path(), &SyncOptions::default()).unwrap();
    assert_eq!(report.missing_folders, vec!["2026/09".to_string()]);
}

/// Regression test for a finding from adversarial review: a directory that directly contains a
/// missing file *and* has a subdirectory with a still-present file must not be reported as "every
/// asset missing" -- the subdirectory's presence has to roll up to every ancestor above it, not
/// just its own immediate parent.
/// Regression test for a finding from CodeRabbit's review: `IngestReport::failed` carries the
/// literal (possibly NFD-decomposed) filesystem path a directory walk returned, while a cataloged
/// asset's own `rel_path` is always NFC-composed (`scruff::normalize_rel_path`). Comparing the two
/// without normalizing both to the same form first could fail to recognize an asset as living under
/// an unreadable directory whose on-disk name uses decomposed Unicode.
#[cfg(unix)]
#[test]
fn an_unreadable_subdirectory_with_an_nfd_name_still_protects_its_asset() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let (store, root_id) = setup();
    // "café" spelled with a combining acute accent (NFD) -- the literal bytes this directory is
    // created with, and therefore what `WalkDir` returns in `IngestReport::failed` once blocked.
    let nfd_name = "cafe\u{0301}";
    let blocked = dir.path().join(nfd_name);
    std::fs::create_dir(&blocked).unwrap();
    write_fake_raw(&blocked, "hidden.NEF", 1);

    sync_root(&store, root_id, dir.path(), &SyncOptions::default()).unwrap();
    // `normalize_rel_path` NFC-composes the stored path -- "café" with a single precomposed
    // character, not the two-codepoint NFD form the directory itself was created with.
    let nfc_rel_path = "caf\u{e9}/hidden.NEF";
    let hidden = store
        .find_asset_by_path(root_id, nfc_rel_path)
        .unwrap()
        .expect("cataloged under its NFC-composed rel_path while readable");

    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let report = sync_root(
        &store,
        root_id,
        dir.path(),
        &SyncOptions {
            remove_missing: true,
        },
    )
    .unwrap();
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert!(!report.ingest.failed.is_empty());
    let hidden_after = store
        .find_asset_by_path(root_id, nfc_rel_path)
        .unwrap()
        .expect("asset row must survive -- it lives under an unreadable directory, not a genuinely missing one");
    assert_eq!(
        hidden.missing_since, hidden_after.missing_since,
        "an asset under an NFD-named unreadable directory must be left alone, not flagged or removed"
    );
}

#[test]
fn a_directory_with_a_missing_direct_file_but_a_present_nested_file_is_not_reported() {
    let dir = tempfile::tempdir().unwrap();
    let (store, root_id) = setup();
    let sub = dir.path().join("2026").join("09");
    std::fs::create_dir_all(&sub).unwrap();
    write_fake_raw(dir.path().join("2026").as_path(), "a.NEF", 1);
    write_fake_raw(&sub, "b.NEF", 2);

    sync_root(&store, root_id, dir.path(), &SyncOptions::default()).unwrap();
    std::fs::remove_file(dir.path().join("2026").join("a.NEF")).unwrap();

    let report = sync_root(&store, root_id, dir.path(), &SyncOptions::default()).unwrap();
    assert!(
        report.missing_folders.is_empty(),
        "\"2026\" still has live content nested under 2026/09 and must not be listed: {:?}",
        report.missing_folders
    );

    let a = store
        .find_asset_by_path(root_id, "2026/a.NEF")
        .unwrap()
        .unwrap();
    assert!(
        a.missing_since.is_some(),
        "the directly-missing file is still flagged"
    );
}
