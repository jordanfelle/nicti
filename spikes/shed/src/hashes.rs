//! #158: whether `AgLibraryFile.md5`/`importHash` are useful inputs to `spikes/homing`'s relink
//! fingerprints (ADR-0071), without recomputing anything at import time. Two subcommands:
//!
//! - [`stats`] reports only shape (NULL rate, length, character class, distinctness) -- never a
//!   raw value. Answers "what kind of thing is this column" without touching file content at all.
//! - [`verify_sample`] resolves a random sample of real files from the catalog's own root/folder
//!   paths and recomputes both a full-file MD5 and homing's tier-(b) partial BLAKE3 against them,
//!   to settle two things `stats` alone can't: whether LRC's `md5` is genuinely a full-file hash
//!   (not, say, a hash of only image data excluding metadata), and how it and `partial_hash`
//!   actually compare on the same real files. Never prints a path, filename, or hash value --
//!   `check_files`-style privacy-check has no way to redact a hash's own hex digits from a JSON
//!   report the way it redacts prose, so this module holds the raw-value ban structurally instead:
//!   no function here returns anything but a count, unlike `open`/`inventory`, which return real
//!   values because those don't originate from this catalog's sensitive keyword/collection/path
//!   data at all (a table's row count is not a leak the way one of its values would be).

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// A deliberate duplicate of `spikes/homing/src/fingerprint.rs`'s tier-(b) `partial_hash`, not a
/// dependency on it -- CONTRIBUTING.md's spike policy ("don't build on top of a spike... expected
/// to be deleted once its own ticket promotes the working logic elsewhere") rules out `shed`
/// taking a path dependency on `homing`, since `verify-md5` would then silently break the moment
/// `homing` is deleted or promoted. This ~15-line BLAKE3 tier is small and stable enough that
/// duplicating it here is cheaper than coupling two throwaway spikes together; if it ever drifts
/// from homing's own copy, that's this module's problem to catch, not homing's.
const PARTIAL_HASH_WINDOW: u64 = 64 * 1024;

fn partial_hash(path: &Path) -> Result<String> {
    let mut file = File::open(path).context("opening sampled file")?;
    let len = file.metadata()?.len();
    let mut hasher = blake3::Hasher::new();
    hasher.update(&len.to_le_bytes());

    let head_len = len.min(PARTIAL_HASH_WINDOW);
    let mut head = vec![0u8; head_len as usize];
    file.read_exact(&mut head)?;
    hasher.update(&head);

    if len > PARTIAL_HASH_WINDOW {
        let tail_len = len.min(PARTIAL_HASH_WINDOW);
        file.seek(SeekFrom::End(-(tail_len as i64)))?;
        let mut tail = vec![0u8; tail_len as usize];
        file.read_exact(&mut tail)?;
        hasher.update(&tail);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

#[derive(Debug, Clone, Serialize)]
pub struct ColumnShape {
    pub row_count: i64,
    pub null_count: i64,
    pub empty_count: i64,
    pub non_empty_count: i64,
    /// `(length, count)`, only for non-NULL, non-empty values.
    pub length_histogram: Vec<(i64, i64)>,
    /// A masked template per distinct length-and-shape combination, e.g. `"32 lowercase hex"` or
    /// `"36 mixed, incl. '-'"` -- never the value itself. `(shape, count)`.
    pub shape_histogram: Vec<(String, i64)>,
    pub distinct_count: i64,
    /// How many non-NULL, non-empty values are shared by more than one row, and the single
    /// largest such group's size (0/0 if every value is unique).
    pub duplicate_group_count: i64,
    pub largest_duplicate_group_size: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct HashStats {
    pub md5: ColumnShape,
    pub import_hash: ColumnShape,
    /// `md5` shape split by `AgLibraryFile.extension` (lowercased) -- ADR-0071's stated reason
    /// for excluding tier (c) from routine import is DNG's in-place-rewrite instability, so
    /// whether `md5` behaves differently on DNG rows than on NEF/JPEG rows is a direct input to
    /// whether it's usable even as a hint.
    pub md5_by_extension: Vec<(String, ColumnShape)>,
    /// Rows where two different `(name, size)` files share the same non-NULL `md5` -- real
    /// probable duplicates, as distinct from a hash collision with no other resemblance.
    pub md5_shared_across_different_name_or_size: i64,
    /// Distinct `importHash` values and the row-count distribution across them, capped at the 20
    /// largest groups -- a proxy for whether `importHash` looks like a per-import-session id
    /// (many rows per value) or a per-file id (every value used once).
    pub import_hash_group_sizes: Vec<i64>,
}

pub fn stats(conn: &Connection) -> Result<HashStats> {
    let md5 = column_shape(conn, "AgLibraryFile", "md5", None)?;
    let import_hash = column_shape(conn, "AgLibraryFile", "importHash", None)?;

    let extensions: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT DISTINCT LOWER(extension) FROM AgLibraryFile \
             WHERE extension IS NOT NULL AND extension != ''",
        )?;
        let result = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<_, _>>()?;
        result
    };
    let mut md5_by_extension = Vec::new();
    for ext in extensions {
        let shape = column_shape(conn, "AgLibraryFile", "md5", Some(&ext))?;
        md5_by_extension.push((ext, shape));
    }

    let md5_shared_across_different_name_or_size: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM (
            SELECT md5 FROM AgLibraryFile
            WHERE md5 IS NOT NULL AND md5 != ''
            GROUP BY md5
            HAVING COUNT(DISTINCT baseName) > 1 OR COUNT(DISTINCT idx_filename) > 1
         )",
            [],
            |r| r.get(0),
        )
        .or_else(|_| {
            // `idx_filename` may not exist on every schema version this spike is pointed at; fall
            // back to name-only grouping rather than failing the whole command over one column.
            conn.query_row(
                "SELECT COUNT(*) FROM (
                SELECT md5 FROM AgLibraryFile
                WHERE md5 IS NOT NULL AND md5 != ''
                GROUP BY md5
                HAVING COUNT(DISTINCT baseName) > 1
             )",
                [],
                |r| r.get(0),
            )
        })?;

    let import_hash_group_sizes: Vec<i64> = {
        let mut stmt = conn.prepare(
            "SELECT COUNT(*) AS c FROM AgLibraryFile \
             WHERE importHash IS NOT NULL AND importHash != '' \
             GROUP BY importHash ORDER BY c DESC LIMIT 20",
        )?;
        let result = stmt
            .query_map([], |r| r.get::<_, i64>(0))?
            .collect::<Result<_, _>>()?;
        result
    };

    Ok(HashStats {
        md5,
        import_hash,
        md5_by_extension,
        md5_shared_across_different_name_or_size,
        import_hash_group_sizes,
    })
}

fn column_shape(
    conn: &Connection,
    table: &str,
    column: &str,
    extension_filter: Option<&str>,
) -> Result<ColumnShape> {
    let where_ext = extension_filter
        .map(|_| " AND LOWER(extension) = ?1".to_string())
        .unwrap_or_default();
    let params: Vec<&dyn rusqlite::ToSql> = match &extension_filter {
        Some(ext) => vec![ext],
        None => vec![],
    };

    let row_count: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM {table} WHERE 1=1{where_ext}"),
        params.as_slice(),
        |r| r.get(0),
    )?;
    let null_count: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM {table} WHERE {column} IS NULL{where_ext}"),
        params.as_slice(),
        |r| r.get(0),
    )?;
    let empty_count: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM {table} WHERE {column} = ''{where_ext}"),
        params.as_slice(),
        |r| r.get(0),
    )?;
    let non_empty_count = row_count - null_count - empty_count;

    let values: Vec<String> = {
        let mut stmt = conn.prepare(&format!(
            "SELECT {column} FROM {table} \
             WHERE {column} IS NOT NULL AND {column} != ''{where_ext}"
        ))?;
        let result = stmt
            .query_map(params.as_slice(), |r| r.get::<_, String>(0))?
            .collect::<Result<_, _>>()?;
        result
    };

    let mut length_counts: BTreeMap<i64, i64> = BTreeMap::new();
    let mut shape_counts: BTreeMap<String, i64> = BTreeMap::new();
    let mut value_counts: BTreeMap<&str, i64> = BTreeMap::new();
    for v in &values {
        *length_counts.entry(v.chars().count() as i64).or_insert(0) += 1;
        *shape_counts.entry(shape_of(v)).or_insert(0) += 1;
        *value_counts.entry(v.as_str()).or_insert(0) += 1;
    }

    let distinct_count = value_counts.len() as i64;
    let duplicate_group_count = value_counts.values().filter(|&&c| c > 1).count() as i64;
    let largest_duplicate_group_size = value_counts.values().copied().max().unwrap_or(0);

    Ok(ColumnShape {
        row_count,
        null_count,
        empty_count,
        non_empty_count,
        length_histogram: length_counts.into_iter().collect(),
        shape_histogram: shape_counts.into_iter().collect(),
        distinct_count,
        duplicate_group_count,
        largest_duplicate_group_size,
    })
}

/// A description of `value`'s character makeup -- never the value itself. Distinguishes the
/// shapes actually worth telling apart for this research question (hex digest vs. GUID-like vs.
/// something else) without being a general-purpose classifier.
fn shape_of(value: &str) -> String {
    let len = value.chars().count();
    // Pure-digit strings must be checked *before* the hex check: every ASCII digit is also a
    // valid hex digit, so `all(is_ascii_hexdigit)` is true for a pure-digit string too -- checking
    // hex first would make this digit branch permanently unreachable dead code, silently
    // misreporting every all-numeric value (e.g. a plain sequence number) as "N lowercase hex".
    if value.chars().all(|c| c.is_ascii_digit()) {
        format!("{len} digits")
    } else if value.chars().all(|c| c.is_ascii_hexdigit()) {
        let case = if value
            .chars()
            .all(|c| !c.is_ascii_alphabetic() || c.is_ascii_lowercase())
        {
            "lowercase"
        } else if value
            .chars()
            .all(|c| !c.is_ascii_alphabetic() || c.is_ascii_uppercase())
        {
            "uppercase"
        } else {
            "mixed-case"
        };
        format!("{len} {case} hex")
    } else {
        let has_dash = value.contains('-');
        let rest_hex = value
            .chars()
            .filter(|&c| c != '-')
            .all(|c| c.is_ascii_hexdigit());
        if has_dash && rest_hex {
            format!("{len} hex with '-' (GUID-like)")
        } else {
            format!("{len} other")
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct VerifySummary {
    pub requested: usize,
    pub sampled: usize,
    pub md5_matched: i64,
    pub md5_mismatched: i64,
    pub missing_on_disk: i64,
    pub path_resolution_failed: i64,
    /// A read/hash error on a file that does exist (permission denied, deleted in the window
    /// between the `exists()` check and the read, not actually a regular file, non-UTF8 name
    /// tripping something downstream, etc.) -- counted rather than propagated, specifically so a
    /// real sampled file's path never ends up in an error chain a caller might print (this
    /// module's whole point is never surfacing a raw catalog-derived value).
    pub hash_error: i64,
    /// Same four outcomes, split by (lowercased) extension.
    pub by_extension: Vec<(String, ExtensionOutcome)>,
    pub md5_hash_ms: Percentiles,
    pub partial_hash_ms: Percentiles,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ExtensionOutcome {
    pub matched: i64,
    pub mismatched: i64,
    pub missing: i64,
    pub error: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Percentiles {
    pub p50: f64,
    pub p95: f64,
    pub max: f64,
}

fn percentiles(mut samples: Vec<f64>) -> Percentiles {
    if samples.is_empty() {
        return Percentiles {
            p50: 0.0,
            p95: 0.0,
            max: 0.0,
        };
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let at = |q: f64| -> f64 {
        let idx = ((samples.len() - 1) as f64 * q).round() as usize;
        samples[idx]
    };
    Percentiles {
        p50: at(0.50),
        p95: at(0.95),
        max: *samples.last().unwrap(),
    }
}

/// A resolved row: the id and extension are kept only to bucket outcomes and never printed; the
/// path is used solely to open the file on disk, never included in the returned summary.
struct SampleRow {
    path: PathBuf,
    extension: String,
    catalog_md5: String,
}

/// Maps an LRC absolute path (`X:\Photos\...` or `X:/Photos/...`) onto this machine's WSL mount
/// (`/mnt/x/Photos/...`). Returns `None` for anything not drive-letter-rooted -- #62's own
/// importer needs a real cross-platform path story; this is just enough to let this research
/// pass resolve files on the box the catalog's originals happen to be reachable from.
fn wsl_path(drive_letter_path: &str) -> Option<PathBuf> {
    let mut chars = drive_letter_path.chars();
    let letter = chars.next()?.to_ascii_lowercase();
    if !letter.is_ascii_lowercase() {
        return None;
    }
    let rest: String = chars.as_str().strip_prefix(':')?.chars().collect();
    let rest = rest.replace('\\', "/");
    let rest = rest.strip_prefix('/').unwrap_or(&rest);
    Some(PathBuf::from(format!("/mnt/{letter}/{rest}")))
}

/// `wsl_path` if `full` looks like a Windows drive-letter path (every real root in the measured
/// catalog is one, ADR-0061 Q1) -- otherwise, `full` unchanged, as an already-absolute Unix path.
/// The fallback exists so this module's own tests can build a fixture root pointing at a real
/// `TempDir` without going through a Windows-path round-trip; a real `.lrcat`'s roots never hit
/// it, since ADR-0061 confirmed all 13 real root folders are drive-letter-prefixed.
fn resolve_local_path(full: &str) -> Option<PathBuf> {
    wsl_path(full).or_else(|| full.starts_with('/').then(|| PathBuf::from(full)))
}

/// Joins `root` (`AgLibraryRootFolder.absolutePath`), `folder_rel`
/// (`AgLibraryFolder.pathFromRoot`), and `base_name.extension` into one path string, normalizing
/// each segment's own leading/trailing separators away first rather than assuming any of them
/// carries a specific slash convention. `pathFromRoot`'s trailing-slash convention is nowhere
/// documented in this repo (ADR-0158's own real-catalog run never exercised a non-empty
/// `pathFromRoot` in practice, since `md5` was NULL on every real row it could have sampled) --
/// naive concatenation assuming `folder_rel` always ends in `/` would glue a nested folder's last
/// segment onto the file's base name whenever it doesn't (e.g. `folder_rel = "sub/dir"`,
/// `base_name = "test"` naively concatenating to `"sub/dirtest"` instead of `"sub/dir/test"`) --
/// caught and fixed alongside #158's adversarial review, see
/// `join_normalizes_a_pathfromroot_without_a_trailing_separator` below.
fn join_lrc_path(root: &str, folder_rel: &str, base_name: &str, extension: &str) -> String {
    let mut full = root.trim_end_matches(['/', '\\']).to_string();
    let folder_rel = folder_rel.trim_matches(['/', '\\']);
    if !folder_rel.is_empty() {
        full.push('/');
        full.push_str(folder_rel);
    }
    full.push('/');
    full.push_str(&format!("{base_name}.{extension}"));
    full
}

/// Returns the resolved sample plus the number of SQL-matched rows whose path didn't resolve to
/// something on disk-shaped local filesystem at all (as distinct from resolving but not existing
/// -- that's `missing_on_disk`, checked later once a real `Path` exists to check).
fn sample_rows(conn: &Connection, sample_size: usize) -> Result<(Vec<SampleRow>, i64)> {
    let mut stmt = conn.prepare(
        "SELECT f.md5, LOWER(f.extension), r.absolutePath, fo.pathFromRoot, f.baseName
         FROM AgLibraryFile f
         JOIN AgLibraryFolder fo ON fo.id_local = f.folder
         JOIN AgLibraryRootFolder r ON r.id_local = fo.rootFolder
         WHERE f.md5 IS NOT NULL AND f.md5 != ''
         ORDER BY RANDOM()
         LIMIT ?1",
    )?;
    let rows = stmt
        .query_map([sample_size as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let sql_matched = rows.len() as i64;

    let mut out = Vec::new();
    for (md5, extension, root, folder_rel, base_name) in rows {
        let full = join_lrc_path(&root, &folder_rel, &base_name, &extension);
        if let Some(path) = resolve_local_path(&full) {
            out.push(SampleRow {
                path,
                extension,
                catalog_md5: md5,
            });
        }
    }
    Ok((out, sql_matched))
}

pub fn verify_sample(conn: &Connection, requested: usize) -> Result<VerifySummary> {
    let (rows, sql_matched) = sample_rows(conn, requested)?;
    let path_resolution_failed = sql_matched - rows.len() as i64;

    let mut md5_matched = 0i64;
    let mut md5_mismatched = 0i64;
    let mut missing_on_disk = 0i64;
    let mut hash_error = 0i64;
    let mut by_extension: BTreeMap<String, ExtensionOutcome> = BTreeMap::new();
    let mut md5_ms = Vec::new();
    let mut partial_ms = Vec::new();

    for row in &rows {
        let entry = by_extension.entry(row.extension.clone()).or_default();
        if !row.path.exists() {
            missing_on_disk += 1;
            entry.missing += 1;
            continue;
        }

        // Every error here is swallowed into a count, deliberately, never propagated with `?` --
        // any `Result::Err` that reaches this function's own caller risks a real sampled file's
        // path surfacing in an error chain a top-level `main()` would print (anyhow's default
        // `Debug` rendering of a returned `Err` includes every `.context()` layer). See the
        // `hash_error_is_counted_not_propagated_or_leaked` regression test.
        let start = Instant::now();
        let computed = match compute_md5(&row.path) {
            Ok(v) => v,
            Err(_) => {
                hash_error += 1;
                entry.error += 1;
                continue;
            }
        };
        md5_ms.push(start.elapsed().as_secs_f64() * 1000.0);

        let start = Instant::now();
        match partial_hash(&row.path) {
            Ok(_) => partial_ms.push(start.elapsed().as_secs_f64() * 1000.0),
            Err(_) => {
                hash_error += 1;
                entry.error += 1;
                continue;
            }
        }

        if computed.eq_ignore_ascii_case(&row.catalog_md5) {
            md5_matched += 1;
            entry.matched += 1;
        } else {
            md5_mismatched += 1;
            entry.mismatched += 1;
        }
    }

    Ok(VerifySummary {
        requested,
        sampled: rows.len(),
        md5_matched,
        md5_mismatched,
        missing_on_disk,
        path_resolution_failed,
        hash_error,
        by_extension: by_extension.into_iter().collect(),
        md5_hash_ms: percentiles(md5_ms),
        partial_hash_ms: percentiles(partial_ms),
    })
}

/// Deliberately no `.with_context`/`.display()` on `path` -- unlike `open.rs`'s catalog-path
/// errors (an operator-supplied CLI argument, safe to echo back), `path` here is a real photo file
/// path reconstructed from the catalog's own private folder/file data by `sample_rows`, and its
/// only caller (`verify_sample`) must be able to discard this error without any risk of the path
/// riding along in the error chain.
fn compute_md5(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).context("reading sampled file")?;
    let digest = md5::compute(&bytes);
    Ok(format!("{digest:x}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::{NamedTempFile, TempDir};

    fn fixture(dir: &Path) -> Connection {
        let path = dir.join("fixture.lrcat");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE AgLibraryFile (id_local INTEGER PRIMARY KEY, md5 TEXT, importHash TEXT,
                extension TEXT, baseName TEXT, folder INTEGER, idx_filename TEXT);
            INSERT INTO AgLibraryFile (md5, importHash, extension, baseName, folder) VALUES
                ('d41d8cd98f00b204e9800998ecf8427e', 'abc123', 'nef', 'IMG_0001', 1),
                ('d41d8cd98f00b204e9800998ecf8427e', 'abc123', 'nef', 'IMG_0002', 1),
                (NULL, 'def456', 'jpg', 'IMG_0003', 1),
                ('', 'ghi789', 'jpg', 'IMG_0004', 1),
                ('5eb63bbbe01eeed093cb22bb8f5acdc3', NULL, 'dng', 'IMG_0005', 1);

            CREATE TABLE AgLibraryFolder (id_local INTEGER PRIMARY KEY, pathFromRoot TEXT,
                rootFolder INTEGER);
            INSERT INTO AgLibraryFolder (pathFromRoot, rootFolder) VALUES ('2026/09/', 1);

            CREATE TABLE AgLibraryRootFolder (id_local INTEGER PRIMARY KEY, absolutePath TEXT);
            INSERT INTO AgLibraryRootFolder (absolutePath) VALUES ('C:/Photos/');
            "#,
        )
        .unwrap();
        drop(conn);
        crate::open::open_backup(&path).unwrap()
    }

    #[test]
    fn stats_reports_null_empty_and_non_empty_counts() {
        let dir = TempDir::new().unwrap();
        let conn = fixture(dir.path());
        let s = stats(&conn).unwrap();

        assert_eq!(s.md5.row_count, 5);
        assert_eq!(s.md5.null_count, 1);
        assert_eq!(s.md5.empty_count, 1);
        assert_eq!(s.md5.non_empty_count, 3);
        assert_eq!(s.md5.distinct_count, 2);
        assert_eq!(s.md5.duplicate_group_count, 1);
        assert_eq!(s.md5.largest_duplicate_group_size, 2);
    }

    #[test]
    fn stats_never_includes_a_raw_value() {
        let dir = TempDir::new().unwrap();
        let conn = fixture(dir.path());
        let s = stats(&conn).unwrap();
        let json = serde_json::to_string(&s).unwrap();

        // Regression guard: this is what "aggregates only" actually means -- a real catalog value
        // must never round-trip through this report, even accidentally via a debug/display impl.
        assert!(!json.contains("d41d8cd98f00b204e9800998ecf8427e"));
        assert!(!json.contains("abc123"));
        assert!(!json.contains("IMG_0001"));
    }

    #[test]
    fn shared_md5_across_different_names_is_detected() {
        let dir = TempDir::new().unwrap();
        let conn = fixture(dir.path());
        let s = stats(&conn).unwrap();
        assert_eq!(s.md5_shared_across_different_name_or_size, 1);
    }

    #[test]
    fn md5_by_extension_splits_correctly() {
        let dir = TempDir::new().unwrap();
        let conn = fixture(dir.path());
        let s = stats(&conn).unwrap();
        let nef = s
            .md5_by_extension
            .iter()
            .find(|(ext, _)| ext == "nef")
            .unwrap();
        assert_eq!(nef.1.non_empty_count, 2);
        let dng = s
            .md5_by_extension
            .iter()
            .find(|(ext, _)| ext == "dng")
            .unwrap();
        assert_eq!(dng.1.non_empty_count, 1);
    }

    #[test]
    fn shape_of_classifies_lowercase_hex() {
        assert_eq!(
            shape_of("d41d8cd98f00b204e9800998ecf8427e"),
            "32 lowercase hex"
        );
    }

    #[test]
    fn shape_of_classifies_guid_like_values() {
        assert_eq!(
            shape_of("550e8400-e29b-41d4-a716-446655440000"),
            "36 hex with '-' (GUID-like)"
        );
    }

    #[test]
    fn wsl_path_maps_a_drive_letter_path() {
        assert_eq!(
            wsl_path("C:/Photos/2026/IMG_0001.NEF"),
            Some(PathBuf::from("/mnt/c/Photos/2026/IMG_0001.NEF"))
        );
        assert_eq!(
            wsl_path(r"C:\Photos\2026\IMG_0001.NEF"),
            Some(PathBuf::from("/mnt/c/Photos/2026/IMG_0001.NEF"))
        );
    }

    #[test]
    fn wsl_path_rejects_a_non_drive_letter_path() {
        assert_eq!(wsl_path("/already/unix/path"), None);
    }

    #[test]
    fn join_handles_a_pathfromroot_with_a_trailing_separator() {
        assert_eq!(
            join_lrc_path("/root", "2026/09/", "IMG_0001", "nef"),
            "/root/2026/09/IMG_0001.nef"
        );
    }

    #[test]
    fn join_normalizes_a_pathfromroot_without_a_trailing_separator() {
        // Regression test for CONFIRMED finding #3 (adversarial review, #158): naive
        // concatenation of a non-empty `folder_rel` with no assumed separator glued the folder's
        // last segment onto the base name (`"sub/dirIMG_0001.nef"` instead of
        // `"sub/dir/IMG_0001.nef"`) -- this is exactly the untested, undocumented case since the
        // real catalog's `pathFromRoot` convention was never exercised (see ADR-0158's own caveat).
        assert_eq!(
            join_lrc_path("/root", "sub/dir", "IMG_0001", "nef"),
            "/root/sub/dir/IMG_0001.nef"
        );
    }

    #[test]
    fn join_handles_an_empty_pathfromroot() {
        assert_eq!(
            join_lrc_path("/root/", "", "IMG_0001", "nef"),
            "/root/IMG_0001.nef"
        );
    }

    #[test]
    fn compute_md5_matches_a_known_vector() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"").unwrap();
        f.flush().unwrap();
        assert_eq!(
            compute_md5(f.path()).unwrap(),
            "d41d8cd98f00b204e9800998ecf8427e"
        );
    }

    #[test]
    fn verify_sample_reports_a_match_against_a_real_file() {
        let dir = TempDir::new().unwrap();
        let conn = dir.path().join("fixture2.lrcat");
        let sql_conn = Connection::open(&conn).unwrap();

        let file_dir = TempDir::new().unwrap();
        let file_path = file_dir.path().join("test.nef");
        std::fs::write(&file_path, b"hello world").unwrap();
        let real_md5 = compute_md5(&file_path).unwrap();

        // Build a fixture whose resolved path really exists on disk, using a `/`-rooted "root"
        // path this test controls -- `wsl_path` only maps genuine `X:`-prefixed absolute paths,
        // so this exercises `sample_rows`' join/concatenation logic directly against a real file
        // rather than through the WSL mapping layer (that's covered separately above).
        sql_conn
            .execute_batch(&format!(
                r#"
                CREATE TABLE AgLibraryFile (id_local INTEGER PRIMARY KEY, md5 TEXT,
                    importHash TEXT, extension TEXT, baseName TEXT, folder INTEGER);
                INSERT INTO AgLibraryFile (md5, extension, baseName, folder) VALUES
                    ('{real_md5}', 'nef', 'test', 1);

                CREATE TABLE AgLibraryFolder (id_local INTEGER PRIMARY KEY, pathFromRoot TEXT,
                    rootFolder INTEGER);
                INSERT INTO AgLibraryFolder (pathFromRoot, rootFolder) VALUES ('', 1);

                CREATE TABLE AgLibraryRootFolder (id_local INTEGER PRIMARY KEY, absolutePath TEXT);
                INSERT INTO AgLibraryRootFolder (absolutePath) VALUES ('{}/');
                "#,
                file_dir.path().display()
            ))
            .unwrap();
        drop(sql_conn);
        let conn = crate::open::open_backup(&conn).unwrap();

        let summary = verify_sample(&conn, 1).unwrap();
        assert_eq!(summary.sampled, 1);
        assert_eq!(summary.md5_matched, 1);
        assert_eq!(summary.md5_mismatched, 0);
        assert_eq!(summary.missing_on_disk, 0);
        assert_eq!(summary.hash_error, 0);
    }

    #[test]
    fn verify_sample_reports_missing_files_without_hashing() {
        let dir = TempDir::new().unwrap();
        let conn = dir.path().join("fixture3.lrcat");
        let sql_conn = Connection::open(&conn).unwrap();
        sql_conn
            .execute_batch(
                r#"
                CREATE TABLE AgLibraryFile (id_local INTEGER PRIMARY KEY, md5 TEXT,
                    importHash TEXT, extension TEXT, baseName TEXT, folder INTEGER);
                INSERT INTO AgLibraryFile (md5, extension, baseName, folder) VALUES
                    ('d41d8cd98f00b204e9800998ecf8427e', 'nef', 'gone', 1);

                CREATE TABLE AgLibraryFolder (id_local INTEGER PRIMARY KEY, pathFromRoot TEXT,
                    rootFolder INTEGER);
                INSERT INTO AgLibraryFolder (pathFromRoot, rootFolder) VALUES ('', 1);

                CREATE TABLE AgLibraryRootFolder (id_local INTEGER PRIMARY KEY, absolutePath TEXT);
                INSERT INTO AgLibraryRootFolder (absolutePath) VALUES ('/nonexistent-dir-xyz/');
                "#,
            )
            .unwrap();
        drop(sql_conn);
        let conn = crate::open::open_backup(&conn).unwrap();

        let summary = verify_sample(&conn, 1).unwrap();
        assert_eq!(summary.missing_on_disk, 1);
        assert_eq!(summary.md5_matched, 0);
    }

    /// Regression test for CONFIRMED finding #7 (adversarial review, #158): nothing previously
    /// exercised the mismatch branch, so an inverted or misapplied `eq_ignore_ascii_case` could
    /// have passed every existing test while always reporting a match.
    #[test]
    fn verify_sample_reports_a_mismatch_against_a_real_file() {
        let dir = TempDir::new().unwrap();
        let conn = dir.path().join("fixture4.lrcat");
        let sql_conn = Connection::open(&conn).unwrap();

        let file_dir = TempDir::new().unwrap();
        let file_path = file_dir.path().join("test.nef");
        std::fs::write(&file_path, b"hello world").unwrap();

        sql_conn
            .execute_batch(&format!(
                r#"
                CREATE TABLE AgLibraryFile (id_local INTEGER PRIMARY KEY, md5 TEXT,
                    importHash TEXT, extension TEXT, baseName TEXT, folder INTEGER);
                INSERT INTO AgLibraryFile (md5, extension, baseName, folder) VALUES
                    ('00000000000000000000000000000000', 'nef', 'test', 1);

                CREATE TABLE AgLibraryFolder (id_local INTEGER PRIMARY KEY, pathFromRoot TEXT,
                    rootFolder INTEGER);
                INSERT INTO AgLibraryFolder (pathFromRoot, rootFolder) VALUES ('', 1);

                CREATE TABLE AgLibraryRootFolder (id_local INTEGER PRIMARY KEY, absolutePath TEXT);
                INSERT INTO AgLibraryRootFolder (absolutePath) VALUES ('{}/');
                "#,
                file_dir.path().display()
            ))
            .unwrap();
        drop(sql_conn);
        let conn = crate::open::open_backup(&conn).unwrap();

        let summary = verify_sample(&conn, 1).unwrap();
        assert_eq!(summary.sampled, 1);
        assert_eq!(summary.md5_matched, 0);
        assert_eq!(summary.md5_mismatched, 1);
    }

    #[test]
    fn verify_sample_resolves_a_nested_folder_with_no_trailing_separator() {
        // End-to-end companion to `join_normalizes_a_pathfromroot_without_a_trailing_separator`:
        // exercises the real join through `sample_rows`/`verify_sample`, not just the helper in
        // isolation, against a real nested directory on disk.
        let dir = TempDir::new().unwrap();
        let conn = dir.path().join("fixture6.lrcat");
        let sql_conn = Connection::open(&conn).unwrap();

        let file_dir = TempDir::new().unwrap();
        let nested = file_dir.path().join("sub").join("dir");
        std::fs::create_dir_all(&nested).unwrap();
        let file_path = nested.join("test.nef");
        std::fs::write(&file_path, b"hello world").unwrap();
        let real_md5 = compute_md5(&file_path).unwrap();

        sql_conn
            .execute_batch(&format!(
                r#"
                CREATE TABLE AgLibraryFile (id_local INTEGER PRIMARY KEY, md5 TEXT,
                    importHash TEXT, extension TEXT, baseName TEXT, folder INTEGER);
                INSERT INTO AgLibraryFile (md5, extension, baseName, folder) VALUES
                    ('{real_md5}', 'nef', 'test', 1);

                CREATE TABLE AgLibraryFolder (id_local INTEGER PRIMARY KEY, pathFromRoot TEXT,
                    rootFolder INTEGER);
                INSERT INTO AgLibraryFolder (pathFromRoot, rootFolder) VALUES ('sub/dir', 1);

                CREATE TABLE AgLibraryRootFolder (id_local INTEGER PRIMARY KEY, absolutePath TEXT);
                INSERT INTO AgLibraryRootFolder (absolutePath) VALUES ('{}');
                "#,
                file_dir.path().display()
            ))
            .unwrap();
        drop(sql_conn);
        let conn = crate::open::open_backup(&conn).unwrap();

        let summary = verify_sample(&conn, 1).unwrap();
        assert_eq!(summary.sampled, 1);
        assert_eq!(summary.md5_matched, 1);
        assert_eq!(summary.missing_on_disk, 0);
    }

    /// Regression test for CONFIRMED finding #2 (adversarial review, #158): a hashing failure on a
    /// file that exists must be counted, never propagated -- propagating it with `?` would let a
    /// real sampled file's path ride along in the error chain up to a caller that might print it
    /// (this module's whole point is never surfacing a raw catalog-derived value).
    #[test]
    fn hash_error_is_counted_not_propagated_or_leaked() {
        let dir = TempDir::new().unwrap();
        let conn = dir.path().join("fixture5.lrcat");
        let sql_conn = Connection::open(&conn).unwrap();

        // A directory, not a file, at the resolved path -- `exists()` is true (so the code passes
        // the `missing_on_disk` check) but `std::fs::read` on it fails, forcing the error path.
        let file_dir = TempDir::new().unwrap();
        let bogus_dir = file_dir.path().join("test.nef");
        std::fs::create_dir(&bogus_dir).unwrap();

        sql_conn
            .execute_batch(&format!(
                r#"
                CREATE TABLE AgLibraryFile (id_local INTEGER PRIMARY KEY, md5 TEXT,
                    importHash TEXT, extension TEXT, baseName TEXT, folder INTEGER);
                INSERT INTO AgLibraryFile (md5, extension, baseName, folder) VALUES
                    ('00000000000000000000000000000000', 'nef', 'test', 1);

                CREATE TABLE AgLibraryFolder (id_local INTEGER PRIMARY KEY, pathFromRoot TEXT,
                    rootFolder INTEGER);
                INSERT INTO AgLibraryFolder (pathFromRoot, rootFolder) VALUES ('', 1);

                CREATE TABLE AgLibraryRootFolder (id_local INTEGER PRIMARY KEY, absolutePath TEXT);
                INSERT INTO AgLibraryRootFolder (absolutePath) VALUES ('{}/');
                "#,
                file_dir.path().display()
            ))
            .unwrap();
        drop(sql_conn);
        let conn = crate::open::open_backup(&conn).unwrap();

        // The point of this assertion: `verify_sample` returns `Ok`, not `Err` -- if the hashing
        // failure were still propagated with `?`, this would be `Err` instead, and that error's
        // `Debug` chain would contain `bogus_dir`'s real path.
        let summary = verify_sample(&conn, 1).unwrap();
        assert_eq!(summary.sampled, 1);
        assert_eq!(summary.hash_error, 1);
        assert_eq!(summary.md5_matched, 0);
        assert_eq!(summary.md5_mismatched, 0);
        assert_eq!(summary.missing_on_disk, 0);
    }

    #[test]
    fn shape_of_classifies_pure_digit_values_as_digits_not_hex() {
        // Regression test for CONFIRMED finding #1 (adversarial review, #158): every ASCII digit
        // is also a valid hex digit, so checking the hex branch first made this digit branch
        // permanently unreachable dead code -- a plain numeric value like an old-style sequence
        // number would have been silently misreported as "N lowercase hex".
        assert_eq!(shape_of("12345678"), "8 digits");
        assert_eq!(shape_of("0"), "1 digits");
    }
}
