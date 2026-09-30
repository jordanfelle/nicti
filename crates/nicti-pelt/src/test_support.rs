//! Shared fixtures for unit tests that need a catalog with real asset rows.

use nicti_lair::{CatalogStore, NewAsset, SqliteCatalog};

/// Inserts `n` assets under one fresh root and returns their ids.
pub fn seed_assets(store: &SqliteCatalog, n: usize) -> Vec<i64> {
    let volume = store.upsert_volume("v", None, None, 0).unwrap();
    let root = store.ensure_root(volume, "").unwrap();
    (0..n)
        .map(|i| {
            let path = format!("{i}.NEF");
            let asset = NewAsset {
                rel_path_fold: path.to_lowercase(),
                rel_path: path,
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
            };
            store.insert_asset(root, &asset, None).unwrap()
        })
        .collect()
}
