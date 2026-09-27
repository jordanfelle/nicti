//! Reads LRC's own "Auto Settings" PV2012 slider values back out of a `.lrcat` (SQLite) --
//! ground truth for `eval`'s comparison against `heuristic`/`fit`. Opened read-only, refusing
//! anything that looks like it could still be a live catalog -- the same guard `spikes/shed`'s
//! `open.rs` already established (`.lock`/`-wal` sibling checks). A small independent copy of
//! that guard and of the `agprefs`/`Adobe_imageDevelopSettings` parsing shed already does, not a
//! path dependency -- see `input.rs`'s doc comment for why.

use std::path::Path;

use agprefs::{Agpref, Value};
use rusqlite::{Connection, OpenFlags};

use crate::sliders::Sliders;

/// Opens `lrcat_path` read-only, refusing beforehand if a non-empty `.lock` or `-wal` sibling
/// suggests a real process (Lightroom itself, most likely) still has it open. **Deliberately not
/// `immutable=1`/a `file:` URI**: `spikes/shed::open`'s own doc comment explains why both are
/// unsafe here -- `immutable=1` can silently read stale or torn pages past this guard's own
/// check-then-open gap instead of surfacing a clean error, and an unescaped `?`/`#` in `path`
/// would corrupt the URI's own parsing. Plain read-only mode keeps SQLite's normal WAL-aware read
/// path active. This means `#202`'s workflow must quit Lightroom (or otherwise let it checkpoint
/// and release the catalog) before running `pupil` against the resulting `.lrcat` -- see
/// `bench/lrc/auto-tone.ahk`'s closing prompt.
pub fn open_readonly(lrcat_path: &Path) -> anyhow::Result<Connection> {
    let lock = sibling_with_suffix(lrcat_path, ".lock");
    if non_empty(&lock)? {
        anyhow::bail!(
            "{} has a non-empty lock file {} -- this looks like a live/open catalog (Lightroom \
             itself, most likely), refusing to open it. Close Lightroom and retry.",
            lrcat_path.display(),
            lock.display()
        );
    }
    let wal = sibling_with_suffix(lrcat_path, "-wal");
    if non_empty(&wal)? {
        anyhow::bail!(
            "{} has a non-empty WAL file {} -- real uncommitted writes are pending, refusing to \
             open it. Close Lightroom and retry.",
            lrcat_path.display(),
            wal.display()
        );
    }
    Ok(Connection::open_with_flags(
        lrcat_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?)
}

fn non_empty(path: &Path) -> anyhow::Result<bool> {
    match std::fs::metadata(path) {
        Ok(meta) => Ok(meta.len() > 0),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).map_err(|e| anyhow::anyhow!("statting {}: {e}", path.display())),
    }
}

/// `path` with `suffix` appended to its file name, matching SQLite's own WAL sibling naming
/// convention (`<full-name>-wal`, not `<stem>-wal.<ext>`) and Lightroom's own `<full-name>.lock`.
fn sibling_with_suffix(path: &Path, suffix: &str) -> std::path::PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

/// Looks up develop-settings text for a NEF by base file name (no extension), matching
/// `AgLibraryFile.baseName` case-sensitively and `extension` case-insensitively -- LRC always
/// lowercases `extension` on import but a caller might not know that, so this normalizes it.
/// Errors (rather than silently picking one) if more than one row matches -- `baseName` +
/// `extension` isn't guaranteed globally unique in a real catalog (e.g. the same-named file
/// imported from two different folders), and silently attributing the wrong develop settings to
/// a file would corrupt ground truth without any error at all.
pub fn develop_settings_text(
    conn: &Connection,
    base_name: &str,
    extension: &str,
) -> anyhow::Result<Option<String>> {
    let mut stmt = conn.prepare(
        "SELECT d.text
         FROM Adobe_imageDevelopSettings d
         JOIN Adobe_images i ON i.id_local = d.image
         JOIN AgLibraryFile f ON f.id_local = i.rootFile
         WHERE f.baseName = ?1 AND LOWER(f.extension) = LOWER(?2)
         LIMIT 2",
    )?;
    let mut rows = stmt.query(rusqlite::params![base_name, extension])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let text: String = row.get(0)?;
    anyhow::ensure!(
        rows.next()?.is_none(),
        "multiple develop-settings rows match {base_name}.{extension} -- baseName+extension isn't \
         unique in this catalog, refusing to guess which one is right"
    );
    Ok(Some(text))
}

fn as_number(value: &Value) -> Option<f64> {
    match value {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    }
}

/// Extracts the six PV2012 sliders from one `Adobe_imageDevelopSettings.text` payload (a Lua
/// table literal). Missing keys default to `0.0` -- LRC omits a key entirely when it's at its
/// default value (see `shed::develop`'s module doc comment on this same convention).
pub fn pv2012_sliders(text: &str) -> anyhow::Result<Sliders> {
    let pref = Agpref::parse(text).map_err(|e| anyhow::anyhow!("agprefs parse failed: {e}"))?;
    let fields = pref
        .get_struct()
        .ok_or_else(|| anyhow::anyhow!("develop settings root is not a struct"))?;
    let get = |key: &str| fields.get(key).and_then(as_number).unwrap_or(0.0);
    Ok(Sliders {
        exposure2012: get("Exposure2012"),
        contrast2012: get("Contrast2012"),
        highlights2012: get("Highlights2012"),
        shadows2012: get("Shadows2012"),
        whites2012: get("Whites2012"),
        blacks2012: get("Blacks2012"),
    })
}

/// Looks up and parses one file's ground-truth sliders in a single call.
pub fn truth_for_file(
    conn: &Connection,
    base_name: &str,
    extension: &str,
) -> anyhow::Result<Option<Sliders>> {
    match develop_settings_text(conn, base_name, extension)? {
        Some(text) => Ok(Some(pv2012_sliders(&text)?)),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE AgLibraryFile (id_local INTEGER PRIMARY KEY, baseName TEXT, extension TEXT);
             CREATE TABLE Adobe_images (id_local INTEGER PRIMARY KEY, rootFile INTEGER);
             CREATE TABLE Adobe_imageDevelopSettings (id_local INTEGER PRIMARY KEY, image INTEGER, text TEXT);
             INSERT INTO AgLibraryFile (id_local, baseName, extension) VALUES (1, 'DSC_0001', 'NEF');
             INSERT INTO Adobe_images (id_local, rootFile) VALUES (10, 1);
             INSERT INTO Adobe_imageDevelopSettings (image, text) VALUES (10, 's = {
                Exposure2012 = 0.75,
                Contrast2012 = 12,
                Highlights2012 = -30,
             }');",
        )
        .unwrap();
        conn
    }

    #[test]
    fn finds_develop_settings_by_base_name_and_extension() {
        let conn = fixture_conn();
        let text = develop_settings_text(&conn, "DSC_0001", "nef").unwrap();
        assert!(text.is_some());
    }

    #[test]
    fn returns_none_for_an_unknown_file() {
        let conn = fixture_conn();
        assert!(develop_settings_text(&conn, "DSC_9999", "nef")
            .unwrap()
            .is_none());
    }

    #[test]
    fn parses_present_keys_and_defaults_missing_ones_to_zero() {
        let sliders = pv2012_sliders(
            "s = {
                Exposure2012 = 0.75,
                Contrast2012 = 12,
                Highlights2012 = -30,
             }",
        )
        .unwrap();
        assert!((sliders.exposure2012 - 0.75).abs() < 1e-9);
        assert!((sliders.contrast2012 - 12.0).abs() < 1e-9);
        assert!((sliders.highlights2012 - (-30.0)).abs() < 1e-9);
        assert_eq!(sliders.shadows2012, 0.0);
        assert_eq!(sliders.whites2012, 0.0);
        assert_eq!(sliders.blacks2012, 0.0);
    }

    #[test]
    fn parse_failure_on_malformed_text_is_an_error_not_a_panic() {
        assert!(pv2012_sliders("this is not valid lua").is_err());
    }

    #[test]
    fn truth_for_file_end_to_end() {
        let conn = fixture_conn();
        let sliders = truth_for_file(&conn, "DSC_0001", "NEF").unwrap().unwrap();
        assert!((sliders.exposure2012 - 0.75).abs() < 1e-9);
    }

    #[test]
    fn truth_for_file_returns_none_when_no_match() {
        let conn = fixture_conn();
        assert!(truth_for_file(&conn, "nonexistent", "nef")
            .unwrap()
            .is_none());
    }

    #[test]
    fn develop_settings_text_errors_on_an_ambiguous_duplicate_base_name() {
        let conn = fixture_conn();
        // A second file with the same base name + extension, imported into a different folder --
        // baseName+extension isn't unique in a real catalog.
        conn.execute_batch(
            "INSERT INTO AgLibraryFile (id_local, baseName, extension) VALUES (2, 'DSC_0001', 'NEF');
             INSERT INTO Adobe_images (id_local, rootFile) VALUES (11, 2);
             INSERT INTO Adobe_imageDevelopSettings (image, text) VALUES (11, 's = { Exposure2012 = -1.0 }');",
        )
        .unwrap();
        assert!(develop_settings_text(&conn, "DSC_0001", "nef").is_err());
    }

    #[test]
    fn open_readonly_opens_a_plain_closed_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("closed.lrcat");
        Connection::open(&path).unwrap();
        assert!(open_readonly(&path).is_ok());
    }

    #[test]
    fn open_readonly_refuses_a_non_empty_lock_sibling() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live.lrcat");
        Connection::open(&path).unwrap();
        std::fs::write(dir.path().join("live.lrcat.lock"), b"Lightroom.exe:1234").unwrap();
        let err = open_readonly(&path).unwrap_err();
        assert!(err.to_string().contains("lock file"));
    }

    #[test]
    fn open_readonly_refuses_a_non_empty_wal_sibling() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live.lrcat");
        Connection::open(&path).unwrap();
        std::fs::write(dir.path().join("live.lrcat-wal"), b"pending frames").unwrap();
        let err = open_readonly(&path).unwrap_err();
        assert!(err.to_string().contains("WAL file"));
    }

    #[test]
    fn open_readonly_tolerates_an_empty_wal_sibling() {
        // A closed backup that anyone has ever opened with a plain read-only connection picks up
        // empty (zero-byte) -wal/-shm siblings just from that read -- must not be mistaken for a
        // live catalog (same gotcha shed's own open.rs documents).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("closed.lrcat");
        Connection::open(&path).unwrap();
        std::fs::write(dir.path().join("closed.lrcat-wal"), b"").unwrap();
        assert!(open_readonly(&path).is_ok());
    }
}
