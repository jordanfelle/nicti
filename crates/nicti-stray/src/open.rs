//! Opens a `.lrcat` read-only, refusing anything that looks like a live catalog (promoted from
//! `spikes/shed/src/open.rs`, #61). Every importer entry point routes through [`open_backup`] --
//! the user's working catalog is usually open in Lightroom right now, and an import must only ever
//! read an already-closed backup copy.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

use crate::StrayError;

/// Catalog version this importer was written against (`.lrcat` v13, ADR-0061 Q7). Older/newer
/// versions are a follow-up: the importer refuses rather than guessing at a different schema.
pub const SUPPORTED_CATALOG_VERSION: i64 = 13;

/// Tables and columns the import cannot work without. A missing one is a clear
/// [`StrayError::Refused`] naming it, not a SQL error halfway through a run.
const REQUIRED: &[(&str, &[&str])] = &[
    ("AgLibraryRootFolder", &["id_local", "absolutePath"]),
    (
        "AgLibraryFolder",
        &["id_local", "rootFolder", "pathFromRoot"],
    ),
    (
        "AgLibraryFile",
        &["id_local", "folder", "baseName", "extension"],
    ),
    (
        "Adobe_images",
        &["id_local", "rootFile", "rating", "pick", "colorLabels"],
    ),
    ("AgLibraryKeyword", &["id_local", "name", "genealogy"]),
    ("AgLibraryKeywordImage", &["image", "tag"]),
    (
        "AgLibraryCollection",
        &["id_local", "name", "parent", "creationId"],
    ),
    ("AgLibraryCollectionImage", &["collection", "image"]),
];

/// Opens `path` read-only, refusing beforehand when either sibling shows a live process still has
/// the catalog open: a non-empty `.lock` (Lightroom's own advisory lock) or a non-empty `-wal`
/// (uncheckpointed write-ahead-log frames). Deliberately **not** "the sibling merely exists" (a
/// closed WAL-mode backup that was ever read leaves harmless zero-byte siblings), **not**
/// `immutable=1` (it would turn the gap between this check and the open into silent torn reads),
/// and **not** a `file:` URI (a `?`/`#` in the filename would be reparsed as URI syntax) -- the
/// reasoning `spikes/shed` measured against a real catalog.
pub fn open_backup(path: &Path) -> Result<Connection, StrayError> {
    let lock = sibling_with_suffix(path, ".lock");
    if non_empty(&lock)? {
        return Err(StrayError::Refused(format!(
            "{} has a non-empty lock file {} -- this looks like a live/open catalog (Lightroom \
             itself, most likely). Close Lightroom or use a catalog backup, then retry.",
            path.display(),
            lock.display()
        )));
    }
    let wal = sibling_with_suffix(path, "-wal");
    if non_empty(&wal)? {
        return Err(StrayError::Refused(format!(
            "{} has a non-empty WAL file {} -- real uncommitted writes are pending. Close \
             whatever has this catalog open and retry.",
            path.display(),
            wal.display()
        )));
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    Ok(conn)
}

/// [`open_backup`] plus a schema check: every [`REQUIRED`] table/column must exist. Returns the
/// connection ready for [`crate::read`].
pub fn open_validated(path: &Path) -> Result<Connection, StrayError> {
    let conn = open_backup(path)?;
    validate_schema(&conn)?;
    Ok(conn)
}

/// Checks [`REQUIRED`] against an open catalog.
pub fn validate_schema(conn: &Connection) -> Result<(), StrayError> {
    for (table, columns) in REQUIRED {
        let have = table_columns(conn, table)?;
        if have.is_empty() {
            return Err(StrayError::Refused(format!(
                "not a Lightroom Classic v{SUPPORTED_CATALOG_VERSION} catalog: no {table} table"
            )));
        }
        for col in *columns {
            if !have.iter().any(|c| c == col) {
                return Err(StrayError::Refused(format!(
                    "not a Lightroom Classic v{SUPPORTED_CATALOG_VERSION} catalog: \
                     {table}.{col} is missing"
                )));
            }
        }
    }
    Ok(())
}

/// Column names of `table` (empty when the table doesn't exist).
pub fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>, StrayError> {
    // `table` is always one of this crate's own literals, never user input.
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let cols = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(cols)
}

fn non_empty(path: &Path) -> Result<bool, StrayError> {
    match std::fs::metadata(path) {
        Ok(meta) => Ok(meta.len() > 0),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(StrayError::Io(format!("statting {}: {e}", path.display()))),
    }
}

/// `path` with `suffix` appended to its file name (SQLite's `<name>-wal`, Lightroom's
/// `<name>.lock`), not its extension replaced.
fn sibling_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_catalog(dir: &Path) -> PathBuf {
        let path = dir.join("test.lrcat");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE t (x INTEGER);").unwrap();
        path
    }

    #[test]
    fn opens_a_plain_backup_copy() {
        let dir = TempDir::new().unwrap();
        assert!(open_backup(&make_catalog(dir.path())).is_ok());
    }

    #[test]
    fn opens_a_backup_with_empty_wal_shm_and_lock_siblings() {
        let dir = TempDir::new().unwrap();
        let path = make_catalog(dir.path());
        for suffix in ["-wal", "-shm", ".lock"] {
            fs::write(sibling_with_suffix(&path, suffix), b"").unwrap();
        }
        assert!(open_backup(&path).is_ok());
    }

    #[test]
    fn refuses_a_wal_or_lock_with_real_content() {
        for suffix in ["-wal", ".lock"] {
            let dir = TempDir::new().unwrap();
            let path = make_catalog(dir.path());
            fs::write(sibling_with_suffix(&path, suffix), b"not empty").unwrap();
            assert!(
                matches!(open_backup(&path), Err(StrayError::Refused(_))),
                "{suffix}"
            );
        }
    }

    #[test]
    fn validate_names_the_missing_table_or_column() {
        let dir = TempDir::new().unwrap();
        let conn = open_backup(&make_catalog(dir.path())).unwrap();
        let err = validate_schema(&conn).unwrap_err().to_string();
        assert!(err.contains("AgLibraryRootFolder"), "{err}");

        let conn = Connection::open_in_memory().unwrap();
        crate::test_fixture::create_schema(&conn);
        assert!(validate_schema(&conn).is_ok());
        conn.execute_batch("ALTER TABLE AgLibraryFolder DROP COLUMN pathFromRoot;")
            .unwrap();
        let err = validate_schema(&conn).unwrap_err().to_string();
        assert!(err.contains("AgLibraryFolder.pathFromRoot"), "{err}");
    }
}
