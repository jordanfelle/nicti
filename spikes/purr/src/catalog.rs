//! Reads keeper rows (picked or rated NEFs with a real, non-default edit) out of a `.lrcat` --
//! training labels for #53's model. A small independent copy of `spikes/pupil::truth`'s
//! `open_readonly` lock/WAL guard and `agprefs`/`Adobe_imageDevelopSettings` parsing (itself a
//! copy of `spikes/shed`'s own convention) -- this repo's spikes stay self-contained, never a path
//! dependency on another spike (see `pupil::input`'s doc comment for why).

use std::path::{Path, PathBuf};

use agprefs::{Agpref, Value};
use rusqlite::{Connection, OpenFlags};

use crate::sliders::Sliders;

/// Opens `lrcat_path` read-only, refusing beforehand if a non-empty `.lock` or `-wal` sibling
/// suggests a real process still has it open. See `pupil::truth::open_readonly`'s doc comment for
/// why this is plain read-only mode, not `immutable=1`.
pub fn open_readonly(lrcat_path: &Path) -> anyhow::Result<Connection> {
    let lock = sibling_with_suffix(lrcat_path, ".lock");
    if non_empty(&lock)? {
        anyhow::bail!(
            "{} has a non-empty lock file {} -- this looks like a live/open catalog, refusing to \
             open it. Close Lightroom and retry.",
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

fn sibling_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

fn as_number(value: &Value) -> Option<f64> {
    match value {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    }
}

/// Parses one `Adobe_imageDevelopSettings.text` Lua-table payload into all eight targets. Missing
/// keys default to `0.0` -- LRC omits a key entirely when it's at its default value.
pub fn parse_sliders(text: &str) -> anyhow::Result<Sliders> {
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
        saturation: get("Saturation"),
        vibrance: get("Vibrance"),
    })
}

/// One candidate training row: everything needed to locate the file on disk, its label, and the
/// context fields used for the split and the EXIF ablation.
#[derive(Debug, Clone)]
pub struct KeeperRow {
    pub image_id: i64,
    /// LRC's `AgLibraryFolder.id_local` -- the split unit (`split::by_folder`), so synced batch
    /// edits across one event never leak across train/holdout.
    pub folder_id: i64,
    pub capture_time: Option<String>,
    /// Absolute path as recorded by LRC (`root.absolutePath` + `folder.pathFromRoot` +
    /// `file.baseName`/`extension`), still drive-letter form (`C:/...`) -- `resolve_path` maps it
    /// to this sandbox's `/mnt/<letter>/...` mount.
    pub lrc_path: String,
    pub sliders: Sliders,
    pub iso: Option<f64>,
    pub shutter_speed: Option<f64>,
    pub aperture: Option<f64>,
}

/// Queries every NEF that's a real keeper: picked (`pick = 1`) or rated (`rating >= 1`), with
/// develop settings present and at least one of the eight target sliders non-default. `pick`/
/// `rating` are SQLite `REAL` columns, not `INTEGER` (see the lrc-migration topic). A row whose
/// `Adobe_imageDevelopSettings.text` fails to parse is skipped, not fatal to the whole query --
/// e.g. an all-default-empty `s = { }` payload agprefs doesn't accept as a struct (rare, but real
/// catalogs can carry it), the same "one bad row doesn't abort the batch" stance
/// `dataset::extract_all` already takes for a bad file on disk.
pub fn keepers(conn: &Connection) -> anyhow::Result<Vec<KeeperRow>> {
    let mut stmt = conn.prepare(
        "SELECT i.id_local, fo.id_local, i.captureTime,
                r.absolutePath, fo.pathFromRoot, f.baseName, f.extension,
                d.text,
                x.isoSpeedRating, x.shutterSpeed, x.aperture
         FROM Adobe_images i
         JOIN AgLibraryFile f ON f.id_local = i.rootFile
         JOIN AgLibraryFolder fo ON fo.id_local = f.folder
         JOIN AgLibraryRootFolder r ON r.id_local = fo.rootFolder
         JOIN Adobe_imageDevelopSettings d ON d.image = i.id_local
         LEFT JOIN AgHarvestedExifMetadata x ON x.image = i.id_local
         WHERE (i.pick = 1 OR i.rating >= 1)
           AND LOWER(f.extension) = 'nef'
         ORDER BY i.id_local",
    )?;

    let mut rows = Vec::new();
    let mut query_rows = stmt.query([])?;
    while let Some(row) = query_rows.next()? {
        let image_id: i64 = row.get(0)?;
        let folder_id: i64 = row.get(1)?;
        let capture_time: Option<String> = row.get(2)?;
        let root: String = row.get(3)?;
        let path_from_root: String = row.get(4)?;
        let base_name: String = row.get(5)?;
        let extension: String = row.get(6)?;
        let text: String = row.get(7)?;
        let iso: Option<f64> = row.get(8)?;
        let shutter_speed: Option<f64> = row.get(9)?;
        let aperture: Option<f64> = row.get(10)?;

        let Ok(sliders) = parse_sliders(&text) else {
            continue;
        };
        if !sliders.any_non_default(1e-6) {
            continue;
        }

        let lrc_path = join_lrc_path(&root, &path_from_root, &base_name, &extension);
        rows.push(KeeperRow {
            image_id,
            folder_id,
            capture_time,
            lrc_path,
            sliders,
            iso,
            shutter_speed,
            aperture,
        });
    }
    Ok(rows)
}

/// Joins `root`/`pathFromRoot`/`baseName.extension` into one LRC-style path string. LRC's own
/// `absolutePath`/`pathFromRoot` values already carry a trailing slash in every real row this
/// pass observed, but that convention isn't documented anywhere -- normalize with `Path::join`
/// -style separators instead of relying on it.
fn join_lrc_path(root: &str, path_from_root: &str, base_name: &str, extension: &str) -> String {
    let root = root.trim_end_matches(['/', '\\']);
    let path_from_root = path_from_root.trim_matches(['/', '\\']);
    if path_from_root.is_empty() {
        format!("{root}/{base_name}.{extension}")
    } else {
        format!("{root}/{path_from_root}/{base_name}.{extension}")
    }
}

/// Maps an LRC drive-letter absolute path (`C:/Photos/...`) onto this WSL sandbox's `/mnt/<letter>`
/// mount. Returns `None` for a path that isn't drive-letter form (a UNC or relative-fallback root
/// -- #62's own scope, unhandled here since #53 only needs *a* real sample, not every real file).
pub fn resolve_path(lrc_path: &str) -> Option<PathBuf> {
    let bytes = lrc_path.as_bytes();
    if bytes.len() < 3 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' {
        return None;
    }
    let drive = (bytes[0] as char).to_ascii_lowercase();
    let rest = &lrc_path[2..].trim_start_matches(['/', '\\']);
    Some(PathBuf::from(format!("/mnt/{drive}/{rest}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE AgLibraryRootFolder (id_local INTEGER PRIMARY KEY, absolutePath TEXT);
             CREATE TABLE AgLibraryFolder (id_local INTEGER PRIMARY KEY, pathFromRoot TEXT, rootFolder INTEGER);
             CREATE TABLE AgLibraryFile (id_local INTEGER PRIMARY KEY, baseName TEXT, extension TEXT, folder INTEGER);
             CREATE TABLE Adobe_images (id_local INTEGER PRIMARY KEY, rootFile INTEGER, captureTime TEXT, pick REAL, rating REAL);
             CREATE TABLE Adobe_imageDevelopSettings (id_local INTEGER PRIMARY KEY, image INTEGER, text TEXT);
             CREATE TABLE AgHarvestedExifMetadata (id_local INTEGER PRIMARY KEY, image INTEGER, isoSpeedRating REAL, shutterSpeed REAL, aperture REAL);

             INSERT INTO AgLibraryRootFolder (id_local, absolutePath) VALUES (1, 'C:/Photos/');
             INSERT INTO AgLibraryFolder (id_local, pathFromRoot, rootFolder) VALUES (10, '2026/Event/', 1);
             -- Picked, real edit, NEF: a real keeper.
             INSERT INTO AgLibraryFile (id_local, baseName, extension, folder) VALUES (1, 'DSC_0001', 'NEF', 10);
             INSERT INTO Adobe_images (id_local, rootFile, captureTime, pick, rating) VALUES (100, 1, '2026-01-01T00:00:00', 1.0, 0.0);
             INSERT INTO Adobe_imageDevelopSettings (image, text) VALUES (100, 's = { Exposure2012 = 0.5, Vibrance = 10 }');
             INSERT INTO AgHarvestedExifMetadata (image, isoSpeedRating, shutterSpeed, aperture) VALUES (100, 400.0, 8.0, 4.0);
             -- Picked but every slider at default: not a keeper (all-default filter).
             INSERT INTO AgLibraryFile (id_local, baseName, extension, folder) VALUES (2, 'DSC_0002', 'NEF', 10);
             INSERT INTO Adobe_images (id_local, rootFile, captureTime, pick, rating) VALUES (101, 2, '2026-01-01T00:00:00', 1.0, 0.0);
             INSERT INTO Adobe_imageDevelopSettings (image, text) VALUES (101, 's = { }');
             -- Unpicked and unrated: not a keeper (pick/rating filter).
             INSERT INTO AgLibraryFile (id_local, baseName, extension, folder) VALUES (3, 'DSC_0003', 'NEF', 10);
             INSERT INTO Adobe_images (id_local, rootFile, captureTime, pick, rating) VALUES (102, 3, '2026-01-01T00:00:00', 0.0, 0.0);
             INSERT INTO Adobe_imageDevelopSettings (image, text) VALUES (102, 's = { Exposure2012 = 1.0 }');
             -- Rated, real edit, but a JPG: not a keeper (extension filter).
             INSERT INTO AgLibraryFile (id_local, baseName, extension, folder) VALUES (4, 'DSC_0004', 'JPG', 10);
             INSERT INTO Adobe_images (id_local, rootFile, captureTime, pick, rating) VALUES (103, 4, '2026-01-01T00:00:00', 0.0, 3.0);
             INSERT INTO Adobe_imageDevelopSettings (image, text) VALUES (103, 's = { Exposure2012 = 1.0 }');",
        )
        .unwrap();
        conn
    }

    #[test]
    fn parses_all_eight_targets_and_defaults_missing_ones() {
        let s =
            parse_sliders("s = { Exposure2012 = 0.75, Saturation = -20, Vibrance = 15 }").unwrap();
        assert!((s.exposure2012 - 0.75).abs() < 1e-9);
        assert!((s.saturation - (-20.0)).abs() < 1e-9);
        assert!((s.vibrance - 15.0).abs() < 1e-9);
        assert_eq!(s.contrast2012, 0.0);
    }

    #[test]
    fn keepers_filters_to_exactly_the_one_real_picked_nef_edit() {
        let conn = fixture_conn();
        let rows = keepers(&conn).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].image_id, 100);
        assert_eq!(rows[0].folder_id, 10);
        assert_eq!(rows[0].lrc_path, "C:/Photos/2026/Event/DSC_0001.NEF");
        assert_eq!(rows[0].iso, Some(400.0));
    }

    #[test]
    fn resolve_path_maps_a_drive_letter_path_into_mnt() {
        let p = resolve_path("C:/Photos/2026/Event/DSC_0001.NEF").unwrap();
        assert_eq!(p, PathBuf::from("/mnt/c/Photos/2026/Event/DSC_0001.NEF"));
    }

    #[test]
    fn resolve_path_returns_none_for_a_non_drive_letter_path() {
        assert!(resolve_path("//unc-share/Photos/DSC_0001.NEF").is_none());
        assert!(resolve_path("relative/path.NEF").is_none());
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
}
