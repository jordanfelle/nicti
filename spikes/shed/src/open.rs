//! Opens a `.lrcat` file read-only, refusing anything that looks like it could still be a live
//! catalog. This is the one guard every `shed` subcommand routes through -- #61's research must
//! never risk opening the user's actual working catalog (open in Lightroom right now, on this
//! machine, while this research runs), only an already-closed backup copy.

use anyhow::{bail, Context, Result};
use rusqlite::{Connection, OpenFlags};
use std::path::Path;

/// Opens `path` read-only, refusing beforehand when either
/// sibling looks like a real, live process still has this catalog open:
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
///
/// **Deliberately not `immutable=1`**: an earlier draft added it (it tells SQLite the file will
/// never be modified by anyone, including another process, letting it skip its own locking
/// protocol entirely) alongside this pre-check, on the theory that the two together were enough.
/// They aren't -- the pre-check and the actual `open_with_flags` call below are two separate
/// steps with a real gap between them (a live process could acquire the lock and start writing in
/// that window), and `immutable=1` is exactly the flag that turns that gap from "SQLite would
/// notice and error" into "SQLite has no mechanism to notice at all," risking a silent read of
/// torn/inconsistent pages instead of a clean failure. Plain read-only mode keeps SQLite's normal
/// shared-lock/WAL-aware read path active, so a real concurrent writer is still safely serialized
/// against (or surfaced as a busy/lock error) rather than silently ignored -- the pre-check above
/// is a fast, informative up-front rejection for the common case, not the only safety net.
///
/// **Deliberately not a `file:` URI at all**: an earlier draft passed `path` through
/// `format!("file:{}?mode=ro", ...)`, which needed `SQLITE_OPEN_URI` to parse. That's a second,
/// independent bug on top of the `immutable=1` one above -- SQLite's URI filename syntax treats an
/// unescaped `?`/`#` in the path as the start of its own query string/fragment, so a `path`
/// containing one (e.g. `catalog.lrcat?immutable=1`, however such a name arose) would silently
/// reinterpret part of the *filename* as a URI parameter, potentially re-adding `immutable=1`
/// behind this guard's back -- while `sibling_with_suffix`'s own sibling checks above still operate
/// on the literal, unparsed `path`, so the two would disagree about which file is actually being
/// opened. Passing `path` directly with no `SQLITE_OPEN_URI` flag needs no escaping and has no URI
/// parser in the loop to disagree with the sibling checks in the first place.
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

    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
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

    /// Regression test: an earlier draft opened `path` via a hand-built `file:{path}?mode=ro` URI,
    /// which needed `SQLITE_OPEN_URI` to parse -- meaning a filename containing a literal `?` or
    /// `#` would have part of its own name reinterpreted as a URI query string/fragment by SQLite,
    /// while the sibling-file checks above kept operating on the real, literal filename, letting
    /// the two disagree about which file is being opened. Passing `path` directly with no URI
    /// parsing in the loop must not care what characters the filename contains.
    ///
    /// **Unix-only**: Windows' filesystem rejects `?`/`#` in a filename outright (`CannotOpen`,
    /// confirmed by this exact test failing CI on `windows-latest` before this `cfg` was added) --
    /// the scenario this test exercises can only occur on a filesystem that permits those
    /// characters at all, so there is nothing to regress-test on Windows.
    #[cfg(unix)]
    #[test]
    fn opens_a_backup_whose_filename_contains_uri_special_characters() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("catalog.lrcat?immutable=1#x");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE t (x INTEGER);").unwrap();
        drop(conn);
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
