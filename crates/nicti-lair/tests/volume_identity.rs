//! #510: roots are filed under their real volume (ADR-0071's identity), an existing root on the
//! placeholder volume is adopted rather than duplicated, and the marker guard still refuses a
//! conflicting volume.

use std::path::Path;

use nicti_lair::scruff::{
    migrate_placeholder_roots_with, register_root_with, PLACEHOLDER_VOLUME_IDENTITY_KEY,
};
use nicti_lair::volume::VolumeIdentity;
use nicti_lair::{CatalogError, CatalogStore, NewAsset, SqliteCatalog};

fn ident(key: &str, label: &str, marker: Option<&str>) -> VolumeIdentity {
    VolumeIdentity {
        identity_key: key.into(),
        label: Some(label.into()),
        marker_uuid: marker.map(str::to_owned),
    }
}

/// A fake `identify`: folders under /photos are volume A, under /archive volume B, else unknown.
fn probe(path: &Path) -> Option<VolumeIdentity> {
    let p = path.to_string_lossy();
    if p.starts_with("/photos") {
        Some(ident("ntfs64:a+gpt:a", "Photos", Some("marker-a")))
    } else if p.starts_with("/archive") {
        Some(ident("ntfs64:b+gpt:b", "Archive", None))
    } else {
        None
    }
}

fn volume_key_of(store: &SqliteCatalog, root_id: i64) -> String {
    let vol = store
        .list_roots()
        .unwrap()
        .into_iter()
        .find(|r| r.id == root_id)
        .unwrap()
        .volume_id;
    // The only way to read a volume's key through the trait is to upsert it back.
    for key in [
        "ntfs64:a+gpt:a",
        "ntfs64:b+gpt:b",
        PLACEHOLDER_VOLUME_IDENTITY_KEY,
    ] {
        if store.upsert_volume(key, None, None, 0).ok() == Some(vol) {
            return key.to_owned();
        }
    }
    panic!("root {root_id} is on an unknown volume {vol}");
}

fn asset(rel: &str) -> NewAsset {
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
fn a_root_registers_under_its_real_volume_and_an_unidentified_one_under_the_placeholder() {
    let store = SqliteCatalog::open_in_memory().unwrap();
    let a = register_root_with(&store, Path::new("/photos/2026"), &probe).unwrap();
    let b = register_root_with(&store, Path::new("/archive/old"), &probe).unwrap();
    let c = register_root_with(&store, Path::new("/elsewhere/x"), &probe).unwrap();
    assert_eq!(volume_key_of(&store, a), "ntfs64:a+gpt:a");
    assert_eq!(volume_key_of(&store, b), "ntfs64:b+gpt:b");
    assert_eq!(volume_key_of(&store, c), PLACEHOLDER_VOLUME_IDENTITY_KEY);
    // Registering the same folder again finds the same root.
    assert_eq!(
        register_root_with(&store, Path::new("/photos/2026/"), &probe).unwrap(),
        a
    );
}

#[test]
fn a_root_already_on_the_placeholder_volume_is_adopted_not_duplicated() {
    let store = SqliteCatalog::open_in_memory().unwrap();
    // Registered before #510: nothing could be identified.
    let old = register_root_with(&store, Path::new("/photos/2026"), &|_| None).unwrap();
    let asset_id = store.insert_asset(old, &asset("a.NEF"), None).unwrap();
    assert_eq!(volume_key_of(&store, old), PLACEHOLDER_VOLUME_IDENTITY_KEY);

    let again = register_root_with(&store, Path::new("/photos/2026"), &probe).unwrap();
    assert_eq!(again, old, "same root id, so every asset under it is kept");
    assert_eq!(volume_key_of(&store, old), "ntfs64:a+gpt:a");
    assert_eq!(store.list_roots().unwrap().len(), 1);
    assert!(store.get_asset(asset_id).unwrap().is_some());
}

#[test]
fn migration_moves_the_identifiable_roots_and_leaves_the_rest() {
    let store = SqliteCatalog::open_in_memory().unwrap();
    let a = register_root_with(&store, Path::new("/photos/2026"), &|_| None).unwrap();
    let b = register_root_with(&store, Path::new("/archive/old"), &|_| None).unwrap();
    let c = register_root_with(&store, Path::new("/elsewhere/x"), &|_| None).unwrap();
    let moved = migrate_placeholder_roots_with(&store, &probe).unwrap();
    assert_eq!(moved, 2);
    assert_eq!(volume_key_of(&store, a), "ntfs64:a+gpt:a");
    assert_eq!(volume_key_of(&store, b), "ntfs64:b+gpt:b");
    assert_eq!(volume_key_of(&store, c), PLACEHOLDER_VOLUME_IDENTITY_KEY);
    // Running it again has nothing left to do.
    assert_eq!(migrate_placeholder_roots_with(&store, &probe).unwrap(), 0);
    assert_eq!(store.list_roots().unwrap().len(), 3);
}

#[test]
fn a_clone_with_a_different_marker_is_refused_not_merged() {
    let store = SqliteCatalog::open_in_memory().unwrap();
    register_root_with(&store, Path::new("/photos/2026"), &probe).unwrap();
    // Same identity key, different `.nicti-volume` marker: a cloned volume.
    let clone = |_: &Path| Some(ident("ntfs64:a+gpt:a", "Photos", Some("marker-OTHER")));
    let err = register_root_with(&store, Path::new("/photos/2027"), &clone).unwrap_err();
    assert!(
        matches!(err, CatalogError::VolumeIdentityConflict { .. }),
        "{err:?}"
    );
    assert_eq!(
        store.list_roots().unwrap().len(),
        1,
        "nothing was registered"
    );
}

#[test]
fn a_collision_on_the_real_volume_is_left_alone_not_merged() {
    let store = SqliteCatalog::open_in_memory().unwrap();
    // The folder is already a root on the real volume AND on the placeholder (two rows).
    let real = register_root_with(&store, Path::new("/photos/2026"), &probe).unwrap();
    let ph = register_root_with(&store, Path::new("/photos/2026"), &|_| None).unwrap();
    assert_ne!(real, ph);
    // Migration can't move the placeholder one onto the real volume's identical path: skipped.
    assert_eq!(migrate_placeholder_roots_with(&store, &probe).unwrap(), 0);
    assert_eq!(volume_key_of(&store, ph), PLACEHOLDER_VOLUME_IDENTITY_KEY);
    // Registering the folder resolves to the real volume's root rather than failing.
    assert_eq!(
        register_root_with(&store, Path::new("/photos/2026"), &probe).unwrap(),
        real
    );
}
