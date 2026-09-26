//! Opens a `.lrcat` file read-only, refusing anything that looks like it could still be a live
//! catalog. This is the one guard every `shed` subcommand routes through -- #61's research must
//! never risk opening the user's actual working catalog (open in Lightroom right now, on this
//! machine, while this research runs), only an already-closed backup copy.

use anyhow::{bail, Context, Result};
use rusqlite::{Connection, OpenFlags};
use std::path::Path;

/// Opens `path` read-only via SQLite's `immutable=1` URI parameter (tells SQLite the file will
/// never be modified by anyone -- including by another process -- for the lifetime of this
/// connection, which also lets it skip locking entirely). Refuses when either sibling looks like a
/// real, live process still has this catalog open:
///
/// - a non-empty `.lock` file (Lightroom's own advisory lock -- confirmed against the user's real,
///   currently-open catalog: 70 bytes, `Lightroom.exe`'s path + PID)
/// - a non-empty `-wal` file (real, uncheckpointed write-ahead-log frames)
///
/// **Deliberately not** "the sibling merely exists": a `.lrcat` file is itself a WAL-mode SQLite
/// database, so a plain, already-closed backup copy that anyone has ever opened with an ordinary
/// read-only `sqlite3` connection picks up empty (zero-byte) `-wal`/`-shm` siblings just from that
/// read -- confirmed while building this spike, when the very first `inventory` run against the
/// real backup copy refused with a false "live catalog" error, because an earlier `sqlite3
/// -readonly` exploration session had already left zero-byte `-wal`/`-shm` files next to it.
/// Checking size, not mere presence, is what tells a closed backup's harmless leftover apart from
/// an actually-open catalog's real pending writes.
pub fn open_backup(path: &Path) -> Result<Connection> {
    let lock = sibling_with_suffix(path, ".lock");
    if non_empty(&lock)? {
        bail!(
            "{} has a non-empty lock file {} -- this looks like a live/open catalog (Lightroom \
             itself, most likely), refusing to open it. Close Lightroom (or use a catalog backup) \
             and retry.",
            path.display(),
            lock.display()
        );
    }
    let wal = sibling_with_suffix(path, "-wal");
    if non_empty(&wal)? {
        bail!(
            "{} has a non-empty WAL file {} -- real uncommitted writes are pending, refusing to \
             open it. Close whatever has this catalog open and retry.",
            path.display(),
            wal.display()
        );
    }

    let uri = format!("file:{}?mode=ro&immutable=1", path.display());
    let conn = Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("opening {} read-only", path.display()))?;
    Ok(conn)
}

fn non_empty(path: &Path) -> Result<bool> {
    match std::fs::metadata(path) {
        Ok(meta) => Ok(meta.len() > 0),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("statting {}", path.display())),
    }
}

/// `path` with `suffix` appended to its file name (not its extension replaced) -- SQLite's own
/// WAL/SHM sibling naming convention is `<full-name>-wal`/`<full-name>-shm`, not
/// `<stem>-wal.<ext>`, and Lightroom's own lock file is `<full-name>.lock`.
fn sibling_with_suffix(path: &Path, suffix: &str) -> std::path::PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_catalog(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("test.lrcat");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE t (x INTEGER);").unwrap();
        drop(conn);
        path
    }

    #[test]
    fn opens_a_plain_backup_copy() {
        let dir = TempDir::new().unwrap();
        let path = make_catalog(dir.path());
        assert!(open_backup(&path).is_ok());
    }

    /// Regression test for the false-positive this guard's first draft had: reading an
    /// already-closed catalog with a plain connection leaves zero-byte `-wal`/`-shm` siblings
    /// behind, and that alone must not be treated as "still live."
    #[test]
    fn opens_a_backup_with_empty_wal_and_shm_siblings() {
        let dir = TempDir::new().unwrap();
        let path = make_catalog(dir.path());
        fs::write(sibling_with_suffix(&path, "-wal"), b"").unwrap();
        fs::write(sibling_with_suffix(&path, "-shm"), b"").unwrap();
        assert!(open_backup(&path).is_ok());
    }

    #[test]
    fn opens_a_backup_with_an_empty_lock_sibling() {
        let dir = TempDir::new().unwrap();
        let path = make_catalog(dir.path());
        fs::write(sibling_with_suffix(&path, ".lock"), b"").unwrap();
        assert!(open_backup(&path).is_ok());
    }

    #[test]
    fn refuses_when_the_wal_sibling_has_real_content() {
        let dir = TempDir::new().unwrap();
        let path = make_catalog(dir.path());
        fs::write(sibling_with_suffix(&path, "-wal"), b"not empty").unwrap();
        assert!(open_backup(&path).is_err());
    }

    #[test]
    fn refuses_when_the_lock_sibling_has_real_content() {
        let dir = TempDir::new().unwrap();
        let path = make_catalog(dir.path());
        fs::write(
            sibling_with_suffix(&path, ".lock"),
            b"C:\\Program Files\\Adobe\\Adobe Lightroom Classic\\Lightroom.exe\n12345",
        )
        .unwrap();
        assert!(open_backup(&path).is_err());
    }
}
