//! #71's candidate volume/root/asset schema (rusqlite, matching ADR-0008's SQLite choice). Three
//! levels rather than a flat `asset(absolute_path)` table: a folder registered for tracking
//! (`root`) can move from the active SSD to the archive drive (#72) as a single `root` row
//! update, without touching every `asset` row underneath it.
//!
//! Not #22's real catalog schema -- this is the identity/volume-resolution slice ADR-0020 hands
//! to #22, exercised here in isolation so the spike's scenario tests don't need the rest of the
//! catalog.

use anyhow::Result;
use rusqlite::Connection;

pub fn open_in_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    migrate(&conn)?;
    Ok(conn)
}

/// Idempotent: every statement uses `IF NOT EXISTS`, since `cmd_build` calls this on every
/// `homing build` invocation regardless of whether `db_path` already exists -- a plain
/// `CREATE TABLE` would fail with "table volume already exists" on the second call against the
/// same catalog file, which the remap-test script's own workflow (repeated `build`/`resolve`
/// runs against one `homing.sqlite3`) depends on not happening.
pub fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        PRAGMA foreign_keys = ON;

        CREATE TABLE IF NOT EXISTS volume (
            id              INTEGER PRIMARY KEY,
            identity_key    TEXT NOT NULL UNIQUE,
            label           TEXT,
            total_bytes     INTEGER,
            removable       INTEGER NOT NULL DEFAULT 0,
            last_mount_point TEXT,
            last_seen_at    INTEGER NOT NULL,
            online          INTEGER NOT NULL DEFAULT 1
        );

        -- A registered folder tracked for assets, scoped to one volume. #72's archive-transition
        -- toggles `archived` here, not per-asset.
        CREATE TABLE IF NOT EXISTS root (
            id          INTEGER PRIMARY KEY,
            volume_id   INTEGER NOT NULL REFERENCES volume(id),
            rel_path    TEXT NOT NULL,
            archived    INTEGER NOT NULL DEFAULT 0,
            UNIQUE(volume_id, rel_path)
        );

        CREATE TABLE IF NOT EXISTS asset (
            id              INTEGER PRIMARY KEY,
            root_id         INTEGER NOT NULL REFERENCES root(id),
            rel_path        TEXT NOT NULL,
            rel_path_fold   TEXT NOT NULL,
            size_bytes      INTEGER NOT NULL,
            mtime_unix      INTEGER NOT NULL,
            fingerprint     TEXT,
            natural_key     TEXT,
            UNIQUE(root_id, rel_path)
        );

        CREATE INDEX IF NOT EXISTS idx_asset_root_fold ON asset(root_id, rel_path_fold);
        CREATE INDEX IF NOT EXISTS idx_asset_fingerprint ON asset(fingerprint) WHERE fingerprint IS NOT NULL;
        CREATE INDEX IF NOT EXISTS idx_asset_natural_key ON asset(natural_key) WHERE natural_key IS NOT NULL;
        "#,
    )?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
pub struct Volume {
    pub id: i64,
    pub identity_key: String,
    pub online: bool,
}

pub fn upsert_volume(
    conn: &Connection,
    identity_key: &str,
    label: Option<&str>,
    total_bytes: Option<u64>,
    removable: bool,
    mount_point: &str,
    now_unix: i64,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO volume (identity_key, label, total_bytes, removable, last_mount_point, last_seen_at, online)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1)
         ON CONFLICT(identity_key) DO UPDATE SET
            label = excluded.label,
            total_bytes = excluded.total_bytes,
            last_mount_point = excluded.last_mount_point,
            last_seen_at = excluded.last_seen_at,
            online = 1",
        rusqlite::params![identity_key, label, total_bytes.map(|b| b as i64), removable as i64, mount_point, now_unix],
    )?;
    let id: i64 = conn.query_row(
        "SELECT id FROM volume WHERE identity_key = ?1",
        [identity_key],
        |row| row.get(0),
    )?;
    Ok(id)
}

/// Marks every volume not present in `seen_identity_keys` as offline **and** every volume that
/// *is* present as online -- this is the reconnect path, not just the disconnect path. An
/// earlier version of this function only ever cleared `online`, never set it back: a volume that
/// went offline once stayed stuck at `online = 0` forever after, even after a real reconnect,
/// since `upsert_volume` (called only from `cmd_build`) was the sole path that set it back to 1.
/// `cmd_resolve` calls this on every run without rebuilding, so it must be able to bring a volume
/// back online on its own. Their `root`/`asset` rows are untouched either way -- catalog data
/// (ratings, keywords, edits) is never deleted just because a drive is unplugged, per ADR-0020's
/// offline-UX decision.
pub fn mark_offline_except(conn: &Connection, seen_identity_keys: &[String]) -> Result<usize> {
    if seen_identity_keys.is_empty() {
        return Ok(conn.execute("UPDATE volume SET online = 0", [])?);
    }
    let placeholders = seen_identity_keys
        .iter()
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(",");
    let params: Vec<&dyn rusqlite::ToSql> = seen_identity_keys
        .iter()
        .map(|s| s as &dyn rusqlite::ToSql)
        .collect();

    let offline_sql =
        format!("UPDATE volume SET online = 0 WHERE identity_key NOT IN ({placeholders})");
    let mut changed = conn.execute(&offline_sql, params.as_slice())?;

    let online_sql = format!("UPDATE volume SET online = 1 WHERE identity_key IN ({placeholders})");
    changed += conn.execute(&online_sql, params.as_slice())?;

    Ok(changed)
}

pub fn is_volume_online(conn: &Connection, identity_key: &str) -> Result<bool> {
    let online: i64 = conn.query_row(
        "SELECT online FROM volume WHERE identity_key = ?1",
        [identity_key],
        |row| row.get(0),
    )?;
    Ok(online != 0)
}

pub fn insert_root(conn: &Connection, volume_id: i64, rel_path: &str) -> Result<i64> {
    conn.execute(
        "INSERT INTO root (volume_id, rel_path) VALUES (?1, ?2)
         ON CONFLICT(volume_id, rel_path) DO NOTHING",
        rusqlite::params![volume_id, rel_path],
    )?;
    Ok(conn.query_row(
        "SELECT id FROM root WHERE volume_id = ?1 AND rel_path = ?2",
        rusqlite::params![volume_id, rel_path],
        |row| row.get(0),
    )?)
}

pub struct NewAsset<'a> {
    pub rel_path: &'a str,
    pub rel_path_fold: &'a str,
    pub size_bytes: u64,
    pub mtime_unix: i64,
    pub fingerprint: Option<&'a str>,
    pub natural_key: Option<&'a str>,
}

pub fn insert_asset(conn: &Connection, root_id: i64, asset: &NewAsset) -> Result<i64> {
    conn.execute(
        "INSERT INTO asset (root_id, rel_path, rel_path_fold, size_bytes, mtime_unix, fingerprint, natural_key)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            root_id,
            asset.rel_path,
            asset.rel_path_fold,
            asset.size_bytes as i64,
            asset.mtime_unix,
            asset.fingerprint,
            asset.natural_key,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Re-points an existing `asset` row at a new `root`/`rel_path` -- the persistence step
/// `relink_against_unknown_volume`'s match result needs, once a match is confirmed, so a later
/// `resolve()` actually finds it under its new location instead of still reading the stale
/// (offline volume's) `root_id`/`rel_path` forever. Without this, `homing relink` only ever
/// reported a match to stdout and never updated the catalog, so the asset stayed unresolved on
/// every subsequent `homing resolve` run.
pub fn relink_asset(
    conn: &Connection,
    asset_id: i64,
    new_root_id: i64,
    new_rel_path: &str,
    new_rel_path_fold: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE asset SET root_id = ?1, rel_path = ?2, rel_path_fold = ?3 WHERE id = ?4",
        rusqlite::params![new_root_id, new_rel_path, new_rel_path_fold, asset_id],
    )?;
    Ok(())
}

/// Resolves `(volume_identity_key, root_rel_path, asset_rel_path)` to a live absolute path, given
/// the currently-mounted volumes' identity->mount_point map. Returns `None` when the owning
/// volume is offline -- callers (search/facet counts/the folder panel) filter these out rather
/// than showing a stale path, per ADR-0020's offline-UX decision.
pub fn resolve(
    conn: &Connection,
    asset_id: i64,
    mounted: &std::collections::HashMap<String, String>,
) -> Result<Option<String>> {
    // `.ok()` here would conflate "this asset id doesn't exist" (a real, expected `NoRows` case)
    // with a genuine DB error (corruption, I/O failure) -- both would collapse to `Ok(None)`,
    // and a caller (e.g. `cmd_resolve`) would count a real DB failure as an ordinary "offline"
    // asset instead of surfacing it. Match on the specific "no rows" variant instead, and
    // propagate everything else.
    let row: Option<(String, bool, String, String)> = match conn.query_row(
        "SELECT v.identity_key, v.online, r.rel_path, a.rel_path
         FROM asset a
         JOIN root r ON r.id = a.root_id
         JOIN volume v ON v.id = r.volume_id
         WHERE a.id = ?1",
        [asset_id],
        |row| {
            Ok((
                row.get(0)?,
                row.get::<_, i64>(1)? != 0,
                row.get(2)?,
                row.get(3)?,
            ))
        },
    ) {
        Ok(row) => Some(row),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(e) => return Err(e.into()),
    };
    let Some((identity_key, online, root_rel, asset_rel)) = row else {
        return Ok(None);
    };
    if !online {
        return Ok(None);
    }
    let Some(mount_point) = mounted.get(&identity_key) else {
        return Ok(None);
    };
    let mut path = mount_point.trim_end_matches(['/', '\\']).to_string();
    if !root_rel.is_empty() {
        path.push('/');
        path.push_str(&root_rel);
    }
    path.push('/');
    path.push_str(&asset_rel);
    Ok(Some(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_is_idempotent_against_an_existing_database() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();
    }

    #[test]
    fn upsert_volume_is_idempotent_and_updates_last_seen() {
        let conn = open_in_memory().unwrap();
        let id1 = upsert_volume(
            &conn,
            "ntfs64:aaa",
            Some("Archive"),
            Some(1_000),
            true,
            "H:\\",
            100,
        )
        .unwrap();
        let id2 = upsert_volume(
            &conn,
            "ntfs64:aaa",
            Some("Archive"),
            Some(1_000),
            true,
            "Z:\\",
            200,
        )
        .unwrap();
        assert_eq!(
            id1, id2,
            "same identity key must map to the same volume row across a letter change"
        );
        let mount: String = conn
            .query_row(
                "SELECT last_mount_point FROM volume WHERE id = ?1",
                [id1],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(mount, "Z:\\");
    }

    #[test]
    fn mark_offline_except_preserves_asset_rows() {
        let conn = open_in_memory().unwrap();
        let vid = upsert_volume(&conn, "ntfs64:aaa", None, None, false, "H:\\", 100).unwrap();
        let rid = insert_root(&conn, vid, "Photos/2026").unwrap();
        insert_asset(
            &conn,
            rid,
            &NewAsset {
                rel_path: "IMG_0001.NEF",
                rel_path_fold: "img_0001.nef",
                size_bytes: 12_345,
                mtime_unix: 1_000,
                fingerprint: None,
                natural_key: None,
            },
        )
        .unwrap();

        mark_offline_except(&conn, &[]).unwrap();
        assert!(!is_volume_online(&conn, "ntfs64:aaa").unwrap());

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM asset", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            count, 1,
            "offline volumes keep their catalog rows, per ADR-0020's offline-UX decision"
        );
    }

    #[test]
    fn mark_offline_except_brings_a_reconnected_volume_back_online() {
        let conn = open_in_memory().unwrap();
        upsert_volume(&conn, "ntfs64:aaa", None, None, false, "H:\\", 100).unwrap();

        // Drive unplugged: not in this round's seen set.
        mark_offline_except(&conn, &[]).unwrap();
        assert!(!is_volume_online(&conn, "ntfs64:aaa").unwrap());

        // Drive reconnected: it's now in the seen set, and must come back online without
        // needing `homing build` to be rerun -- `cmd_resolve` only ever calls
        // `mark_offline_except`, never `upsert_volume`.
        mark_offline_except(&conn, &["ntfs64:aaa".to_string()]).unwrap();
        assert!(
            is_volume_online(&conn, "ntfs64:aaa").unwrap(),
            "a volume present in the seen set must be marked back online, not left stuck offline"
        );
    }

    #[test]
    fn resolve_returns_none_for_offline_volume() {
        let conn = open_in_memory().unwrap();
        let vid = upsert_volume(&conn, "ntfs64:aaa", None, None, false, "H:\\", 100).unwrap();
        let rid = insert_root(&conn, vid, "").unwrap();
        let aid = insert_asset(
            &conn,
            rid,
            &NewAsset {
                rel_path: "IMG_0001.NEF",
                rel_path_fold: "img_0001.nef",
                size_bytes: 1,
                mtime_unix: 1,
                fingerprint: None,
                natural_key: None,
            },
        )
        .unwrap();

        mark_offline_except(&conn, &[]).unwrap();
        let mounted = std::collections::HashMap::new();
        assert_eq!(resolve(&conn, aid, &mounted).unwrap(), None);
    }

    #[test]
    fn resolve_follows_a_remapped_mount_point() {
        let conn = open_in_memory().unwrap();
        let vid = upsert_volume(&conn, "ntfs64:aaa", None, None, false, "H:\\", 100).unwrap();
        let rid = insert_root(&conn, vid, "Photos").unwrap();
        let aid = insert_asset(
            &conn,
            rid,
            &NewAsset {
                rel_path: "IMG_0001.NEF",
                rel_path_fold: "img_0001.nef",
                size_bytes: 1,
                mtime_unix: 1,
                fingerprint: None,
                natural_key: None,
            },
        )
        .unwrap();

        let mut mounted = std::collections::HashMap::new();
        // Simulate the drive coming back on a *different* letter than it was registered under.
        mounted.insert("ntfs64:aaa".to_string(), "Z:\\".to_string());
        assert_eq!(
            resolve(&conn, aid, &mounted).unwrap(),
            Some("Z:/Photos/IMG_0001.NEF".to_string())
        );
    }
}
