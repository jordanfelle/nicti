//! `rusqlite`-backed `CatalogStore` (ADR-0067: SQLite, WAL). One `Connection` per store, guarded
//! by a `Mutex` — `rusqlite::Connection` needs `&mut self` for a transaction, and `Module` (via
//! `CatalogStore`) requires `Send + Sync` since it's shared as `Arc<dyn CatalogStore>` through the
//! Claw registry.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension, ToSql};

use crate::{Asset, CatalogError, CatalogStore, NewAsset, Preview, PreviewTier};
use nicti_claw::Module;

/// `facet_counts.rating`'s stand-in for NULL/unrated (schema.rs's `MIGRATION_V3` — a PRIMARY KEY
/// column can't itself be NULL), chosen well outside the real `-1..=5` range so it can never
/// collide with a genuine rating.
const FACET_UNRATED_SENTINEL: i64 = -128;

/// SQLite's default `SQLITE_MAX_VARIABLE_NUMBER` is 32766 -- comfortably above any realistic
/// culling multi-selection, but chunked well under it anyway so a "select all" bulk action
/// against an unusually large batch can't hit the limit (found by CodeRabbit's review).
const MAX_IDS_PER_STATEMENT: usize = 500;

/// Shared by `set_rating`/`set_flag`/`set_label`: builds and runs `UPDATE asset SET <column> = ?
/// WHERE id IN (...)`, chunked to stay under SQLite's parameter limit, all chunks committed
/// together in one transaction so the whole call is atomic (a partial write across chunks would
/// otherwise be possible with autocommit-per-statement). `column` is always one of this module's
/// own hardcoded literals, never external input, so building its SQL with `format!` carries no
/// injection risk -- every actual *value* (`value`, and every element of `asset_ids`) is bound as
/// a `?` parameter, never interpolated.
fn set_column_for_assets(
    conn: &mut Connection,
    column: &str,
    value: &dyn ToSql,
    asset_ids: &[i64],
) -> Result<(), CatalogError> {
    if asset_ids.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction()?;
    for chunk in asset_ids.chunks(MAX_IDS_PER_STATEMENT) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!("UPDATE asset SET {column} = ?1 WHERE id IN ({placeholders})");
        let mut bound: Vec<&dyn ToSql> = Vec::with_capacity(chunk.len() + 1);
        bound.push(value);
        for id in chunk {
            bound.push(id);
        }
        tx.execute(&sql, bound.as_slice())?;
    }
    tx.commit()?;
    Ok(())
}

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
            flag: row.get(12)?,
            label: row.get(13)?,
            width: row.get::<_, Option<i64>>(14)?.map(|w| w as u32),
            height: row.get::<_, Option<i64>>(15)?.map(|h| h as u32),
            imported_at: row.get(16)?,
            missing_since: row.get(17)?,
        })
    }
}

const ASSET_COLUMNS: &str = "id, root_id, rel_path, rel_path_fold, size_bytes, mtime_unix, \
    fingerprint, natural_key, make, model, captured_at, rating, flag, label, width, height, \
    imported_at, missing_since";

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
        // `rating`/`flag`/`label` are never written here -- omitted from the column list means
        // they default to NULL on a fresh insert, and the ON CONFLICT branch below likewise never
        // touches them, so a rescan of an already-imported asset can't clobber ratings/flags/
        // labels the user has since set.
        tx.execute(
            "INSERT INTO asset (root_id, rel_path, rel_path_fold, size_bytes, mtime_unix, \
                fingerprint, natural_key, make, model, captured_at, width, height, \
                imported_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13) \
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
                size_bytes = ?4, mtime_unix = ?5, missing_since = NULL WHERE id = ?6",
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

    fn facet_count(&self, model: Option<&str>, rating: Option<i64>) -> Result<u64, CatalogError> {
        let conn = self.conn.lock().unwrap();
        let cnt: i64 = conn.query_row(
            "SELECT COALESCE(SUM(fc.cnt), 0) FROM facet_counts fc \
             JOIN volume v ON v.id = fc.volume_id \
             WHERE v.online = 1 AND fc.model = ?1 AND fc.rating = ?2",
            params![
                model.unwrap_or(""),
                rating.unwrap_or(FACET_UNRATED_SENTINEL)
            ],
            |row| row.get(0),
        )?;
        Ok(cnt as u64)
    }

    fn set_rating(&self, asset_ids: &[i64], rating: Option<i64>) -> Result<(), CatalogError> {
        let mut conn = self.conn.lock().unwrap();
        set_column_for_assets(&mut conn, "rating", &rating, asset_ids)
    }

    fn set_flag(&self, asset_ids: &[i64], flag: Option<i64>) -> Result<(), CatalogError> {
        let mut conn = self.conn.lock().unwrap();
        set_column_for_assets(&mut conn, "flag", &flag, asset_ids)
    }

    fn set_label(&self, asset_ids: &[i64], label: Option<&str>) -> Result<(), CatalogError> {
        let mut conn = self.conn.lock().unwrap();
        set_column_for_assets(&mut conn, "label", &label, asset_ids)
    }

    fn list_assets_by_root(&self, root_id: i64) -> Result<Vec<Asset>, CatalogError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {ASSET_COLUMNS} FROM asset WHERE root_id = ?1 ORDER BY id ASC"
        ))?;
        let rows = stmt
            .query_map([root_id], Self::row_to_asset)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn set_asset_missing(
        &self,
        asset_id: i64,
        missing_since: Option<i64>,
    ) -> Result<(), CatalogError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE asset SET missing_since = ?1 WHERE id = ?2",
            params![missing_since, asset_id],
        )?;
        Ok(())
    }

    fn remove_asset(&self, asset_id: i64) -> Result<(), CatalogError> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        // No `ON DELETE CASCADE` on any of these foreign keys (schema.rs's `asset`/`edit_variant`
        // references are plain `REFERENCES`, and `PRAGMA foreign_keys = ON` only *enforces*
        // referential integrity -- it never cascades a delete on its own) -- every child row is
        // deleted explicitly, in dependency order, or the asset delete itself would fail its own
        // foreign-key check with orphaned children left behind.
        tx.execute(
            "DELETE FROM edit_history WHERE variant_id IN \
                (SELECT id FROM edit_variant WHERE asset_id = ?1)",
            [asset_id],
        )?;
        tx.execute("DELETE FROM edit_variant WHERE asset_id = ?1", [asset_id])?;
        tx.execute("DELETE FROM preview WHERE asset_id = ?1", [asset_id])?;
        tx.execute("DELETE FROM asset WHERE id = ?1", [asset_id])?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn new_asset(rel_path: &str, model: Option<&str>) -> NewAsset {
        NewAsset {
            rel_path: rel_path.to_string(),
            rel_path_fold: rel_path.to_lowercase(),
            size_bytes: 100,
            mtime_unix: 0,
            fingerprint: None,
            natural_key: None,
            make: None,
            model: model.map(str::to_string),
            captured_at: None,
            width: None,
            height: None,
            imported_at: 0,
        }
    }

    #[test]
    fn set_rating_flag_label_round_trip_across_multiple_assets() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let volume_id = store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = store.ensure_root(volume_id, "").unwrap();
        let a = store
            .insert_asset(root_id, &new_asset("a.NEF", Some("Z8")), None)
            .unwrap();
        let b = store
            .insert_asset(root_id, &new_asset("b.NEF", Some("Z8")), None)
            .unwrap();

        store.set_rating(&[a, b], Some(4)).unwrap();
        store.set_flag(&[a], Some(1)).unwrap();
        store.set_label(&[b], Some("Selects")).unwrap();

        let asset_a = store.find_asset_by_path(root_id, "a.NEF").unwrap().unwrap();
        let asset_b = store.find_asset_by_path(root_id, "b.NEF").unwrap().unwrap();
        assert_eq!(asset_a.rating, Some(4));
        assert_eq!(asset_a.flag, Some(1));
        assert_eq!(asset_a.label, None);
        assert_eq!(asset_b.rating, Some(4));
        assert_eq!(asset_b.flag, None);
        assert_eq!(asset_b.label, Some("Selects".to_string()));
    }

    #[test]
    fn set_rating_on_an_empty_slice_is_a_no_op() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        // Must not panic/error building a `WHERE id IN ()` with zero placeholders.
        store.set_rating(&[], Some(3)).unwrap();
    }

    #[test]
    fn set_rating_across_more_ids_than_one_chunk_holds_updates_all_of_them() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let volume_id = store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = store.ensure_root(volume_id, "").unwrap();

        let count = MAX_IDS_PER_STATEMENT + 5; // forces at least two chunks
        let ids: Vec<i64> = (0..count)
            .map(|i| {
                store
                    .insert_asset(root_id, &new_asset(&format!("{i}.NEF"), Some("Z8")), None)
                    .unwrap()
            })
            .collect();

        store.set_rating(&ids, Some(3)).unwrap();

        assert_eq!(
            store.facet_count(Some("Z8"), Some(3)).unwrap(),
            count as u64,
            "every id across every chunk must be updated, not just the first chunk"
        );
    }

    #[test]
    fn facet_count_distinguishes_unrated_from_reject_and_star_ratings() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let volume_id = store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = store.ensure_root(volume_id, "").unwrap();
        let a = store
            .insert_asset(root_id, &new_asset("a.NEF", Some("Z8")), None)
            .unwrap();
        let b = store
            .insert_asset(root_id, &new_asset("b.NEF", Some("Z8")), None)
            .unwrap();
        store
            .insert_asset(root_id, &new_asset("c.NEF", Some("Z8")), None)
            .unwrap();

        store.set_rating(&[a], Some(-1)).unwrap();
        store.set_rating(&[b], Some(5)).unwrap();
        // c is left unrated (NULL).

        assert_eq!(store.facet_count(Some("Z8"), Some(-1)).unwrap(), 1);
        assert_eq!(store.facet_count(Some("Z8"), Some(5)).unwrap(), 1);
        assert_eq!(store.facet_count(Some("Z8"), None).unwrap(), 1);
    }

    #[test]
    fn facet_count_excludes_assets_on_an_offline_volume() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let online_volume = store.upsert_volume("online", None, None, 0).unwrap();
        let offline_volume = store.upsert_volume("offline", None, None, 0).unwrap();
        let online_root = store.ensure_root(online_volume, "").unwrap();
        let offline_root = store.ensure_root(offline_volume, "").unwrap();

        store
            .insert_asset(online_root, &new_asset("a.NEF", Some("Z8")), None)
            .unwrap();
        store
            .insert_asset(offline_root, &new_asset("b.NEF", Some("Z8")), None)
            .unwrap();

        assert_eq!(
            store.facet_count(Some("Z8"), None).unwrap(),
            2,
            "both volumes are online so far"
        );

        // No `CatalogStore` method sets a volume offline yet (that's #24's live-sync scope) --
        // flip it directly for this test.
        store
            .conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE volume SET online = 0 WHERE id = ?1",
                params![offline_volume],
            )
            .unwrap();

        assert_eq!(
            store.facet_count(Some("Z8"), None).unwrap(),
            1,
            "the offline volume's asset must drop out of the facet count"
        );
    }

    /// Regression test for the `trg_facet_asset_update` trigger's `OR OLD.root_id IS NOT
    /// NEW.root_id` clause: a relink across volumes (ADR-0071's relink tiers) must move the
    /// asset's facet-count bucket even though `model`/`rating` didn't change.
    #[test]
    fn relink_across_volumes_moves_the_asset_facet_bucket() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let v1 = store.upsert_volume("v1", None, None, 0).unwrap();
        let v2 = store.upsert_volume("v2", None, None, 0).unwrap();
        let root1 = store.ensure_root(v1, "").unwrap();
        let root2 = store.ensure_root(v2, "").unwrap();

        let asset_id = store
            .insert_asset(root1, &new_asset("a.NEF", Some("Z8")), None)
            .unwrap();
        assert_eq!(store.facet_count(Some("Z8"), None).unwrap(), 1);

        // Mark v2 offline directly -- no `CatalogStore` method sets this yet (#24's scope).
        store
            .conn
            .lock()
            .unwrap()
            .execute("UPDATE volume SET online = 0 WHERE id = ?1", params![v2])
            .unwrap();

        store
            .relink_asset(asset_id, root2, "a.NEF", "a.nef", 100, 0)
            .unwrap();

        assert_eq!(
            store.facet_count(Some("Z8"), None).unwrap(),
            0,
            "the asset's facet bucket must move to v2 (offline), dropping out of the online sum"
        );
    }
}
