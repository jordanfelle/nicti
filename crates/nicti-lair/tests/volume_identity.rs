//! #510: roots are filed under their real volume (ADR-0071's identity), an existing root on the
//! placeholder volume is adopted rather than duplicated, and the marker guard still refuses a
//! conflicting volume.

use std::path::Path;

use nicti_lair::hunt::Filter;
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
    // Roots are stored under `canonical_root_string`, which uses `\\` on Windows.
    let p = path.to_string_lossy().replace('\\', "/");
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
    // A catalog that already has the folder as a root on the real volume AND on the placeholder
    // (two rows; registration itself can no longer create this, so build it through the store).
    let real_vol = store
        .upsert_volume("ntfs64:a+gpt:a", Some("Photos"), Some("marker-a"), 0)
        .unwrap();
    let ph_vol = store
        .upsert_volume(PLACEHOLDER_VOLUME_IDENTITY_KEY, None, None, 0)
        .unwrap();
    // The spelling registration files a root under (`\\` separators on Windows).
    let path = nicti_lair::scruff::canonical_root_string(Path::new("/photos/2026"));
    let real = store.ensure_root(real_vol, &path).unwrap();
    let ph = store.ensure_root(ph_vol, &path).unwrap();
    assert_ne!(real, ph);
    // Migration can't move the placeholder one onto the real volume's identical path: skipped.
    assert_eq!(migrate_placeholder_roots_with(&store, &probe).unwrap(), 0);
    assert_eq!(volume_key_of(&store, ph), PLACEHOLDER_VOLUME_IDENTITY_KEY);
    // Registering the folder resolves to the real volume's root rather than failing.
    assert_eq!(
        register_root_with(&store, Path::new("/photos/2026"), &probe).unwrap(),
        real
    );
    assert_eq!(
        store.list_roots().unwrap().len(),
        2,
        "nothing merged or added"
    );
}

fn model_asset(rel: &str, model: &str) -> NewAsset {
    NewAsset {
        model: Some(model.into()),
        ..asset(rel)
    }
}

/// The unfiltered facets must equal what the asset rows say (the cache is trigger-maintained, keyed
/// by volume, and reassigning a root fires none of those triggers).
fn assert_facets_match_the_rows(store: &SqliteCatalog, expected_total: u64) {
    let f = store.facets(&Filter::default()).unwrap();
    assert_eq!(f.total, expected_total, "{f:?}");
    let by_model: u64 = f.by_model.iter().map(|(_, n)| n).sum();
    let by_rating: u64 = f.by_rating.iter().map(|(_, n)| n).sum();
    assert_eq!(
        (by_model, by_rating),
        (expected_total, expected_total),
        "{f:?}"
    );
}

#[test]
fn adopting_a_root_moves_its_assets_facet_counts_with_it() {
    let store = SqliteCatalog::open_in_memory().unwrap();
    let old = register_root_with(&store, Path::new("/photos/2026"), &|_| None).unwrap();
    let ids: Vec<i64> = (0..3)
        .map(|i| {
            store
                .insert_asset(old, &model_asset(&format!("{i}.NEF"), "Z8"), None)
                .unwrap()
        })
        .collect();
    store.set_rating(&ids[..2], Some(4)).unwrap();
    assert_facets_match_the_rows(&store, 3);

    register_root_with(&store, Path::new("/photos/2026"), &probe).unwrap();
    assert_facets_match_the_rows(&store, 3);
    // Changes after adoption are booked against the new volume and stay exact.
    store.set_rating(&ids[2..], Some(1)).unwrap();
    assert_facets_match_the_rows(&store, 3);
    store.remove_assets(&ids[..2]).unwrap();
    assert_facets_match_the_rows(&store, 1);
    store.remove_assets(&ids[2..]).unwrap();
    let f = store.facets(&Filter::default()).unwrap();
    assert_eq!(f.total, 0);
    assert!(f.by_model.is_empty(), "no ghost counts left behind: {f:?}");
}

#[test]
fn identification_flipping_never_registers_a_second_root_for_the_same_folder() {
    let store = SqliteCatalog::open_in_memory().unwrap();
    // Identified first (a junction target is present), then not (it is unplugged), then again.
    let first = register_root_with(&store, Path::new("/photos/2026"), &probe).unwrap();
    let second = register_root_with(&store, Path::new("/photos/2026"), &|_| None).unwrap();
    let third = register_root_with(&store, Path::new("/photos/2026"), &probe).unwrap();
    assert_eq!((first, second), (first, first));
    assert_eq!(third, first);
    assert_eq!(store.list_roots().unwrap().len(), 1);
    // A folder that was only ever unidentified is adopted by whichever volume then claims it.
    let a = register_root_with(&store, Path::new("/elsewhere/x"), &|_| None).unwrap();
    let other = |_: &Path| Some(ident("ntfs64:z+gpt:z", "Z", None));
    assert_eq!(
        register_root_with(&store, Path::new("/elsewhere/x"), &other).unwrap(),
        a
    );
    assert_eq!(volume_key_of_any(&store, a), "ntfs64:z+gpt:z");
    // But a *different* real volume appearing at a path already filed under another real volume
    // (a reused drive letter, a swapped removable disk) gets its own root: its files must not
    // inherit the first disk's root, ratings and edits.
    let swapped = register_root_with(&store, Path::new("/photos/2026"), &other).unwrap();
    assert_ne!(swapped, first, "a different disk is a different root");
    assert_eq!(store.list_roots().unwrap().len(), 3);
    // ...while the unidentified lookup (identification flipping off) still reuses a root.
    assert_eq!(
        register_root_with(&store, Path::new("/photos/2026"), &|_| None).unwrap(),
        first.min(swapped)
    );
    assert_eq!(store.list_roots().unwrap().len(), 3);
}

/// The key of the real volume a root is filed under, through the same upsert-back trick as
/// `volume_key_of`, for volumes beyond the three it knows.
fn volume_key_of_any(store: &SqliteCatalog, root_id: i64) -> String {
    let vol = store
        .list_roots()
        .unwrap()
        .into_iter()
        .find(|r| r.id == root_id)
        .unwrap()
        .volume_id;
    for key in [
        "ntfs64:a+gpt:a",
        "ntfs64:b+gpt:b",
        "ntfs64:z+gpt:z",
        PLACEHOLDER_VOLUME_IDENTITY_KEY,
    ] {
        if store.upsert_volume(key, None, None, 0).ok() == Some(vol) {
            return key.to_owned();
        }
    }
    panic!("unknown volume {vol}");
}

#[test]
fn a_root_with_a_folder_move_in_flight_is_not_re_filed_and_not_duplicated() {
    let store = SqliteCatalog::open_in_memory().unwrap();
    let old = register_root_with(&store, Path::new("/photos/2026"), &|_| None).unwrap();
    store.insert_asset(old, &asset("a.NEF"), None).unwrap();
    store.begin_root_move(old, "/photos/2027", 1).unwrap();
    // Registering (or migrating) while the move is open must leave the root where it is.
    assert_eq!(
        register_root_with(&store, Path::new("/photos/2026"), &probe).unwrap(),
        old
    );
    assert_eq!(volume_key_of(&store, old), PLACEHOLDER_VOLUME_IDENTITY_KEY);
    assert_eq!(migrate_placeholder_roots_with(&store, &probe).unwrap(), 0);
    assert_eq!(store.list_roots().unwrap().len(), 1);
}
