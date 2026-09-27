//! `rusqlite`-backed `CatalogStore` (ADR-0008: SQLite, WAL). One `Connection` per store, guarded
//! by a `Mutex` — `rusqlite::Connection` needs `&mut self` for a transaction, and `Module` (via
//! `CatalogStore`) requires `Send + Sync` since it's shared as `Arc<dyn CatalogStore>` through the
//! Claw registry.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};

use crate::{Asset, CatalogError, CatalogStore, NewAsset, Preview, PreviewTier};
use nicti_claw::Module;

/// The empty `EditDocument` shape (`{ stages: BTreeMap<String, StageEntry> }`, ADR-0002),
/// serialized by hand rather than depending on `spikes/pawprint` (a spike crate — not something
/// production code builds on top of, per this repo's package-map convention). Whichever ticket
/// promotes `pawprint`'s `EditDocument` to a production crate can replace this literal with a real
/// `serde_json::to_string(&EditDocument::default())` call; the on-disk shape is identical either
/// way.
const EMPTY_EDIT_DOCUMENT: &str = r#"{"stages":{}}"#;

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

    fn init(conn: Connection) -> Result<Self, CatalogError> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        crate::schema::migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
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

    fn find_by_fingerprint(&self, fingerprint: &str) -> Result<Option<Asset>, CatalogError> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                &format!("SELECT {ASSET_COLUMNS} FROM asset WHERE fingerprint = ?1 LIMIT 1"),
                [fingerprint],
                Self::row_to_asset,
            )
            .optional()?)
    }

    fn insert_asset(&self, root_id: i64, asset: &NewAsset) -> Result<i64, CatalogError> {
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
            params![asset_id, EMPTY_EDIT_DOCUMENT],
        )?;
        tx.commit()?;
        Ok(asset_id)
    }

    fn relink_asset(
        &self,
        asset_id: i64,
        new_root_id: i64,
        new_rel_path: &str,
        new_rel_path_fold: &str,
    ) -> Result<(), CatalogError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE asset SET root_id = ?1, rel_path = ?2, rel_path_fold = ?3 WHERE id = ?4",
            params![new_root_id, new_rel_path, new_rel_path_fold, asset_id],
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
