//! Reads LRC's own "Auto Settings" PV2012 slider values back out of a `.lrcat` (SQLite) --
//! ground truth for `eval`'s comparison against `heuristic`/`fit`. Opened read-only + immutable,
//! same convention as `spikes/shed`'s `open.rs`, since this must never touch a live/locked
//! catalog. A small independent copy of the `agprefs`/`Adobe_imageDevelopSettings` parsing shed
//! already does, not a path dependency -- see `input.rs`'s doc comment for why.

use std::path::Path;

use agprefs::{Agpref, Value};
use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::sliders::Sliders;

pub fn open_readonly(lrcat_path: &Path) -> anyhow::Result<Connection> {
    let uri = format!("file:{}?immutable=1", lrcat_path.display());
    Ok(Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?)
}

/// Looks up develop-settings text for a NEF by base file name (no extension), matching
/// `AgLibraryFile.baseName` case-sensitively and `extension` case-insensitively -- LRC always
/// lowercases `extension` on import but a caller might not know that, so this normalizes it.
pub fn develop_settings_text(
    conn: &Connection,
    base_name: &str,
    extension: &str,
) -> anyhow::Result<Option<String>> {
    conn.query_row(
        "SELECT d.text
         FROM Adobe_imageDevelopSettings d
         JOIN Adobe_images i ON i.id_local = d.image
         JOIN AgLibraryFile f ON f.id_local = i.rootFile
         WHERE f.baseName = ?1 AND LOWER(f.extension) = LOWER(?2)",
        rusqlite::params![base_name, extension],
        |r| r.get(0),
    )
    .optional()
    .map_err(Into::into)
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
}
