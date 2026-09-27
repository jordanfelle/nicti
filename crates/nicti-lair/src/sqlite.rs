//! `rusqlite`-backed `CatalogStore` (ADR-0067: SQLite, WAL). One `Connection` per store, guarded
//! by a `Mutex` — `rusqlite::Connection` needs `&mut self` for a transaction, and `Module` (via
//! `CatalogStore`) requires `Send + Sync` since it's shared as `Arc<dyn CatalogStore>` through the
//! Claw registry.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};

use crate::{Asset, CatalogError, CatalogStore, NewAsset, Preview, PreviewTier};
use nicti_claw::Module;

/// The empty `EditDocument` shape (`{ stages: BTreeMap<String, StageEntry> }`, ADR-0021),
/// serialized from the real `nicti_pawprint::EditDocument` (#45 promoted it from `spikes/pawprint`)
/// rather than a hand-written literal, so this can never drift from that type's actual shape.
fn empty_edit_document() -> String {
    serde_json::to_string(&nicti_pawprint::EditDocument::default())
        .expect("EditDocument always serializes")
}

pub struct SqliteCatalog {
    conn: Mutex<Connection>,
}

impl SqliteCatalog {
    pub fn open(path: &Path) -> Result<Self, CatalogError> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self, CatalogError> {
        let conn = Connection::open_in_memory()?;
        Self::init(conn)
    }

    fn init(mut conn: Connection) -> Result<Self, CatalogError> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        crate::schema::migrate(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Total asset count across every root/volume -- a placeholder library-view figure
    /// (`nicti-pelt`'s Library panel, #241) until #30's real paged/faceted grid query exists.
    /// Inherent, not on `CatalogStore`: a plain count has no per-backend variation worth an
    /// extension-point method yet, unlike the trait's other queries.
    pub fn asset_count(&self) -> Result<u64, CatalogError> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM asset", [], |row| row.get(0))?;
        Ok(count as u64)
    }

    fn row_to_asset(row: &rusqlite::Row) -> rusqlite::Result<Asset> {
        Ok(Asset {
            id: row.get(0)?,
            root_id: row.get(1)?,
            rel_path: row.get(2)?,
            rel_path_fold: row.get(3)?,
            size_bytes: row.get::<_, i64>(4)? as u64,
            mtime_unix: row.get(5)?,
            fingerprint: row.get(6)?,
            natural_key: row.get(7)?,
            make: row.get(8)?,
            model: row.get(9)?,
            captured_at: row.get(10)?,
            rating: row.get(11)?,
            width: row.get::<_, Option<i64>>(12)?.map(|w| w as u32),
            height: row.get::<_, Option<i64>>(13)?.map(|h| h as u32),
            imported_at: row.get(14)?,
        })
    }
}

const ASSET_COLUMNS: &str = "id, root_id, rel_path, rel_path_fold, size_bytes, mtime_unix, \
    fingerprint, natural_key, make, model, captured_at, rating, width, height, imported_at";

impl Module for SqliteCatalog {
    fn id(&self) -> &str {
        "nicti.catalog.sqlite"
    }

    fn schema_version(&self) -> u32 {
        1
    }

    fn migrate_params(
        &self,
        _from_version: u32,
        params: serde_json::Value,
    ) -> Option<serde_json::Value> {
        Some(params)
    }
}

impl CatalogStore for SqliteCatalog {
    fn upsert_volume(
        &self,
        identity_key: &str,
        label: Option<&str>,
        marker_uuid: Option<&str>,
        now_unix: i64,
    ) -> Result<i64, CatalogError> {
        let conn = self.conn.lock().unwrap();
        let existing: Option<(i64, Option<String>)> = conn
            .query_row(
                "SELECT id, marker_uuid FROM volume WHERE identity_key = ?1",
                [identity_key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;

        if let Some((id, existing_marker)) = existing {
            if let (Some(existing_marker), Some(incoming_marker)) = (&existing_marker, marker_uuid)
            {
                if existing_marker != incoming_marker {
                    return Err(CatalogError::VolumeIdentityConflict {
                        identity_key: identity_key.to_string(),
                        existing: Some(existing_marker.clone()),
                        incoming: Some(incoming_marker.to_string()),
                    });
                }
            }
            let marker_to_store = marker_uuid.map(str::to_string).or(existing_marker);
            conn.execute(
                "UPDATE volume SET label = ?1, marker_uuid = ?2, online = 1, last_seen_at = ?3 \
                 WHERE id = ?4",
                params![label, marker_to_store, now_unix, id],
            )?;
            Ok(id)
        } else {
            conn.execute(
                "INSERT INTO volume (identity_key, label, marker_uuid, online, last_seen_at) \
                 VALUES (?1, ?2, ?3, 1, ?4)",
                params![identity_key, label, marker_uuid, now_unix],
            )?;
            Ok(conn.last_insert_rowid())
        }
    }

    fn ensure_root(&self, volume_id: i64, rel_path: &str) -> Result<i64, CatalogError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO root (volume_id, rel_path) VALUES (?1, ?2) \
             ON CONFLICT(volume_id, rel_path) DO NOTHING",
            params![volume_id, rel_path],
        )?;
        Ok(conn.query_row(
            "SELECT id FROM root WHERE volume_id = ?1 AND rel_path = ?2",
            params![volume_id, rel_path],
            |row| row.get(0),
        )?)
    }

    fn find_asset_by_path(
        &self,
        root_id: i64,
        rel_path: &str,
    ) -> Result<Option<Asset>, CatalogError> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                &format!("SELECT {ASSET_COLUMNS} FROM asset WHERE root_id = ?1 AND rel_path = ?2"),
                params![root_id, rel_path],
                Self::row_to_asset,
            )
            .optional()?)
    }

    fn find_by_fingerprint(&self, fingerprint: &str) -> Result<Vec<Asset>, CatalogError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {ASSET_COLUMNS} FROM asset WHERE fingerprint = ?1 ORDER BY id ASC"
        ))?;
        let rows = stmt
            .query_map([fingerprint], Self::row_to_asset)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn insert_asset(
        &self,
        root_id: i64,
        asset: &NewAsset,
        t0_preview: Option<&Preview>,
    ) -> Result<i64, CatalogError> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO asset (root_id, rel_path, rel_path_fold, size_bytes, mtime_unix, \
                fingerprint, natural_key, make, model, captured_at, rating, width, height, \
                imported_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,0,?11,?12,?13) \
             ON CONFLICT(root_id, rel_path) DO UPDATE SET \
                rel_path_fold = excluded.rel_path_fold, \
                size_bytes = excluded.size_bytes, \
                mtime_unix = excluded.mtime_unix, \
                fingerprint = excluded.fingerprint, \
                natural_key = excluded.natural_key, \
                make = excluded.make, \
                model = excluded.model, \
                captured_at = excluded.captured_at, \
                width = excluded.width, \
                height = excluded.height",
            params![
                root_id,
                asset.rel_path,
                asset.rel_path_fold,
                asset.size_bytes as i64,
                asset.mtime_unix,
                asset.fingerprint,
                asset.natural_key,
                asset.make,
                asset.model,
                asset.captured_at,
                asset.width.map(|w| w as i64),
                asset.height.map(|h| h as i64),
                asset.imported_at,
            ],
        )?;
        // `last_insert_rowid()` isn't reliable across an ON CONFLICT DO UPDATE branch (only a real
        // INSERT sets it) -- look the row up by its actual unique key instead, same pattern
        // `spikes/homing`'s upsert helpers use.
        let asset_id: i64 = tx.query_row(
            "SELECT id FROM asset WHERE root_id = ?1 AND rel_path = ?2",
            params![root_id, asset.rel_path],
            |row| row.get(0),
        )?;
        // Only ever created once per asset -- `ON CONFLICT DO NOTHING` makes a rescan of an
        // already-imported asset a no-op here, leaving any real edits the master variant has
        // since accumulated untouched.
        tx.execute(
            "INSERT INTO edit_variant (asset_id, name, is_master, document) \
             VALUES (?1, 'master', 1, ?2) \
             ON CONFLICT(asset_id, name) DO NOTHING",
            params![asset_id, empty_edit_document()],
        )?;
        match t0_preview {
            Some(preview) => tx.execute(
                "INSERT INTO preview (asset_id, tier, width, height, bytes) \
                 VALUES (?1,?2,?3,?4,?5) \
                 ON CONFLICT(asset_id, tier) DO UPDATE SET \
                    width = excluded.width, height = excluded.height, bytes = excluded.bytes",
                params![
                    asset_id,
                    PreviewTier::T0.as_str(),
                    preview.width.map(|w| w as i64),
                    preview.height.map(|h| h as i64),
                    preview.bytes,
                ],
            )?,
            None => tx.execute(
                "DELETE FROM preview WHERE asset_id = ?1 AND tier = ?2",
                params![asset_id, PreviewTier::T0.as_str()],
            )?,
        };
        tx.commit()?;
        Ok(asset_id)
    }

    fn relink_asset(
        &self,
        asset_id: i64,
        new_root_id: i64,
        new_rel_path: &str,
        new_rel_path_fold: &str,
        size_bytes: u64,
        mtime_unix: i64,
    ) -> Result<(), CatalogError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE asset SET root_id = ?1, rel_path = ?2, rel_path_fold = ?3, \
                size_bytes = ?4, mtime_unix = ?5 WHERE id = ?6",
            params![
                new_root_id,
                new_rel_path,
                new_rel_path_fold,
                size_bytes as i64,
                mtime_unix,
                asset_id
            ],
        )?;
        Ok(())
    }

    fn put_preview(
        &self,
        asset_id: i64,
        tier: PreviewTier,
        preview: &Preview,
    ) -> Result<(), CatalogError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO preview (asset_id, tier, width, height, bytes) VALUES (?1,?2,?3,?4,?5) \
             ON CONFLICT(asset_id, tier) DO UPDATE SET \
                width = excluded.width, height = excluded.height, bytes = excluded.bytes",
            params![
                asset_id,
                tier.as_str(),
                preview.width.map(|w| w as i64),
                preview.height.map(|h| h as i64),
                preview.bytes,
            ],
        )?;
        Ok(())
    }

    fn get_preview(
        &self,
        asset_id: i64,
        tier: PreviewTier,
    ) -> Result<Option<Preview>, CatalogError> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT width, height, bytes FROM preview WHERE asset_id = ?1 AND tier = ?2",
                params![asset_id, tier.as_str()],
                |row| {
                    Ok(Preview {
                        width: row.get::<_, Option<i64>>(0)?.map(|w| w as u32),
                        height: row.get::<_, Option<i64>>(1)?.map(|h| h as u32),
                        bytes: row.get(2)?,
                    })
                },
            )
            .optional()?)
    }

    fn clear_preview(&self, asset_id: i64, tier: PreviewTier) -> Result<(), CatalogError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM preview WHERE asset_id = ?1 AND tier = ?2",
            params![asset_id, tier.as_str()],
        )?;
        Ok(())
    }

    fn facet_count(&self, model: Option<&str>, rating: i64) -> Result<u64, CatalogError> {
        let conn = self.conn.lock().unwrap();
        let cnt: Option<i64> = conn
            .query_row(
                "SELECT cnt FROM facet_counts WHERE model = ?1 AND rating = ?2",
                params![model.unwrap_or(""), rating],
                |row| row.get(0),
            )
            .optional()?;
        Ok(cnt.unwrap_or(0) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::empty_edit_document;
    use super::SqliteCatalog;

    #[test]
    fn empty_edit_document_matches_the_on_disk_shape_adr_0021_documents() {
        assert_eq!(empty_edit_document(), r#"{"stages":{}}"#);
    }

    #[test]
    fn asset_count_is_zero_on_a_fresh_catalog() {
        let catalog = SqliteCatalog::open_in_memory().unwrap();
        assert_eq!(catalog.asset_count().unwrap(), 0);
    }

    #[test]
    fn asset_count_reflects_inserted_assets() {
        use crate::{CatalogStore, NewAsset};

        let catalog = SqliteCatalog::open_in_memory().unwrap();
        let volume_id = catalog.upsert_volume("test-volume", None, None, 0).unwrap();
        let root_id = catalog.ensure_root(volume_id, "root").unwrap();
        for i in 0..3 {
            catalog
                .insert_asset(
                    root_id,
                    &NewAsset {
                        rel_path: format!("photo{i}.nef"),
                        rel_path_fold: format!("photo{i}.nef"),
                        size_bytes: 100,
                        mtime_unix: 0,
                        fingerprint: Some(format!("fp{i}")),
                        natural_key: None,
                        make: None,
                        model: None,
                        captured_at: None,
                        width: None,
                        height: None,
                        imported_at: 0,
                    },
                    None,
                )
                .unwrap();
        }
        assert_eq!(catalog.asset_count().unwrap(), 3);
    }
}
