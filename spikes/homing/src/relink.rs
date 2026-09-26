//! #71's end-to-end scenarios: a folder tree moved between volumes (a `root` row update, no
//! per-asset touch), a tree moved onto an unrecognized volume (fingerprint search), and an
//! offline volume (queries filter it out, `schema::resolve` returns `None`). This module wires
//! `volume`/`schema`/`fingerprint`/`path` together into the operations `main.rs`'s CLI exposes.

use crate::fingerprint;
use crate::path::{fold, normalize_rel_path};
use crate::schema::{self, NewAsset};
use anyhow::Result;
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

/// Walks `root_dir`, registering it as a `root` under `volume_id` and inserting one `asset` row
/// per file found. `fingerprint_tier` controls how expensive the per-file identity computation is
/// -- `None` skips fingerprinting entirely (fastest import), `Some(Tier::Partial)` computes tier
/// (b) at build time (this spike's default), `Some(Tier::Full)` computes tier (c).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Partial,
    Full,
}

pub struct BuildStats {
    pub files_indexed: usize,
    pub errors: usize,
    /// Files indexed successfully but whose fingerprint hash could not be computed (a real I/O
    /// error -- locked/corrupt file -- not a deliberate `Tier::None` skip). Tracked separately
    /// from `errors` (which are metadata/insert failures that drop the file entirely) so a
    /// caller can tell "no fingerprint by choice" apart from "no fingerprint because reading the
    /// file failed" -- these assets can never be relinked by fingerprint later.
    pub fingerprint_failures: usize,
}

pub fn build(
    conn: &Connection,
    volume_id: i64,
    root_rel_path: &str,
    root_dir: &Path,
    tier: Option<Tier>,
) -> Result<BuildStats> {
    let root_id = schema::insert_root(conn, volume_id, root_rel_path)?;
    let mut files_indexed = 0;
    let mut errors = 0;
    let mut fingerprint_failures = 0;

    for entry in WalkDir::new(root_dir).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let abs_path = entry.path();
        let rel_to_root = match abs_path.strip_prefix(root_dir) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let rel_path = normalize_rel_path(rel_to_root);
        let rel_path_fold = fold(&rel_path);

        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => {
                errors += 1;
                continue;
            }
        };
        let size_bytes = metadata.len();
        let mtime_unix = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let fingerprint = match tier {
            Some(Tier::Partial) => match fingerprint::partial_hash(abs_path) {
                Ok(h) => Some(h),
                Err(_) => {
                    fingerprint_failures += 1;
                    None
                }
            },
            Some(Tier::Full) => match fingerprint::full_hash(abs_path) {
                Ok(h) => Some(h),
                Err(_) => {
                    fingerprint_failures += 1;
                    None
                }
            },
            None => None,
        };
        let natural_key = fingerprint::natural_key(abs_path)
            .ok()
            .flatten()
            .map(|k| k.to_string());

        match schema::insert_asset(
            conn,
            root_id,
            &NewAsset {
                rel_path: &rel_path,
                rel_path_fold: &rel_path_fold,
                size_bytes,
                mtime_unix,
                fingerprint: fingerprint.as_deref(),
                natural_key: natural_key.as_deref(),
            },
        ) {
            Ok(_) => files_indexed += 1,
            Err(_) => errors += 1,
        }
    }

    Ok(BuildStats {
        files_indexed,
        errors,
        fingerprint_failures,
    })
}

/// Re-registers `root_id` under a different volume -- the "tree moved between two *known*
/// volumes" scenario (e.g. SSD to archive drive, #72). A single row update; every `asset` row
/// underneath is untouched, which is the whole point of the three-level schema.
pub fn move_root_to_volume(conn: &Connection, root_id: i64, new_volume_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE root SET volume_id = ?1 WHERE id = ?2",
        rusqlite::params![new_volume_id, root_id],
    )?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveOutcome {
    /// Resolved via the volume/root/rel_path chain -- the fast path, no fingerprinting needed.
    Direct(String),
    /// The registered volume is offline; the asset is filtered out per ADR-0020.
    Offline,
    /// Resolved by matching a fingerprint against an unrecognized volume's freshly-scanned files.
    RelinkedByFingerprint(String),
    /// Resolved via tier (a) (size + filename) only -- the asset had no fingerprint to check
    /// (never computed, or hashing failed at import time), so this is the weakest available
    /// signal, not a fingerprint-confirmed match.
    RelinkedBySizeName(String),
    /// No match at any tier.
    Lost,
}

/// The "tree moved onto an unrecognized volume" scenario: given a currently-mounted, previously
/// unknown volume's file listing (path -> (size, partial_hash)), tries to relink every `asset`
/// row whose owning volume is offline, cheapest tier first.
///
/// Each candidate path can be claimed by at most one asset: without this, two offline assets
/// that happen to share the same size+fingerprint (a genuine duplicate photo, or a partial-hash
/// collision) would both silently resolve to the *same* candidate file -- a false-positive relink
/// for one of them, reported as a clean success. `claimed` tracks paths already matched to an
/// earlier (lower `asset.id`, per the `ORDER BY`) asset in this same call, so a later asset with
/// an identical fingerprint either finds a different real candidate or, if there truly isn't one,
/// is honestly reported `Lost` instead of double-claiming.
///
/// Candidates are pre-bucketed by size (`by_size`) so the fingerprint fallback only scans
/// same-size candidates, not the full set -- this also gives tier (a)'s `size_name_key` (see
/// `fingerprint.rs`) real use: an asset with no fingerprint at all (no hash was computed at
/// import time, or hashing failed -- see `BuildStats::fingerprint_failures`) still gets a weaker,
/// last-resort shot at a match via size+filename alone, rather than being unconditionally `Lost`.
pub fn relink_against_unknown_volume(
    conn: &Connection,
    candidate_files: &HashMap<String, (u64, String)>,
) -> Result<Vec<(i64, ResolveOutcome)>> {
    let mut stmt = conn.prepare(
        "SELECT a.id, a.rel_path, a.size_bytes, a.fingerprint
         FROM asset a
         JOIN root r ON r.id = a.root_id
         JOIN volume v ON v.id = r.volume_id
         WHERE v.online = 0
         ORDER BY a.id",
    )?;
    let offline_assets: Vec<(i64, String, u64, Option<String>)> = stmt
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get::<_, i64>(2)? as u64,
                row.get(3)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();

    let mut by_size: HashMap<u64, Vec<String>> = HashMap::new();
    let mut by_size_name: HashMap<fingerprint::SizeNameKey, String> = HashMap::new();
    for (path, (size, _fp)) in candidate_files {
        by_size.entry(*size).or_default().push(path.clone());
        let file_name = Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        by_size_name
            .entry(fingerprint::SizeNameKey {
                size_bytes: *size,
                file_name,
            })
            .or_insert_with(|| path.clone());
    }

    let mut claimed: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut results = Vec::new();
    for (asset_id, rel_path, size_bytes, fingerprint) in offline_assets {
        let by_path = candidate_files
            .get(&rel_path)
            .filter(|_| !claimed.contains(&rel_path));
        let matched = match (fingerprint.as_deref(), by_path) {
            (Some(fp), Some((candidate_size, candidate_fp)))
                if fp == candidate_fp && size_bytes == *candidate_size =>
            {
                Some(rel_path.clone())
            }
            (Some(fp), _) => by_size
                .get(&size_bytes)
                .into_iter()
                .flatten()
                .find(|path| {
                    !claimed.contains(*path)
                        && candidate_files
                            .get(*path)
                            .is_some_and(|(_, candidate_fp)| candidate_fp == fp)
                })
                .cloned(),
            (None, _) => None,
        };

        // No fingerprint at all -- last-resort tier (a): size + filename. Weaker evidence than a
        // fingerprint match, only tried when there's no fingerprint to check at all.
        let size_name_matched = if fingerprint.is_none() && matched.is_none() {
            let file_name = Path::new(&rel_path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            by_size_name
                .get(&fingerprint::SizeNameKey {
                    size_bytes,
                    file_name,
                })
                .filter(|path| !claimed.contains(*path))
                .cloned()
        } else {
            None
        };

        let outcome = match (matched, size_name_matched) {
            (Some(path), _) => {
                claimed.insert(path.clone());
                ResolveOutcome::RelinkedByFingerprint(path)
            }
            (None, Some(path)) => {
                claimed.insert(path.clone());
                ResolveOutcome::RelinkedBySizeName(path)
            }
            (None, None) => ResolveOutcome::Lost,
        };

        results.push((asset_id, outcome));
    }
    Ok(results)
}

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::open_in_memory;
    use std::fs;
    use tempfile::TempDir;

    fn write_sample_files(dir: &TempDir) {
        fs::write(dir.path().join("IMG_0001.NEF"), b"fake-raw-bytes-1").unwrap();
        fs::write(
            dir.path().join("IMG_0002.NEF"),
            b"fake-raw-bytes-2-longer-content",
        )
        .unwrap();
    }

    #[test]
    fn build_indexes_every_file_under_root() {
        let conn = open_in_memory().unwrap();
        let vid = schema::upsert_volume(&conn, "ntfs64:aaa", None, None, false, "H:\\", now_unix())
            .unwrap();
        let dir = TempDir::new().unwrap();
        write_sample_files(&dir);

        let stats = build(&conn, vid, "", dir.path(), Some(Tier::Partial)).unwrap();
        assert_eq!(stats.files_indexed, 2);
        assert_eq!(stats.errors, 0);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM asset", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn move_root_to_volume_leaves_asset_rows_untouched() {
        let conn = open_in_memory().unwrap();
        let vid_ssd =
            schema::upsert_volume(&conn, "ntfs64:ssd", None, None, false, "C:\\", now_unix())
                .unwrap();
        let vid_archive = schema::upsert_volume(
            &conn,
            "ntfs64:archive",
            None,
            None,
            false,
            "H:\\",
            now_unix(),
        )
        .unwrap();
        let dir = TempDir::new().unwrap();
        write_sample_files(&dir);
        build(&conn, vid_ssd, "Photos/2026", dir.path(), None).unwrap();

        let root_id: i64 = conn
            .query_row("SELECT id FROM root WHERE volume_id = ?1", [vid_ssd], |r| {
                r.get(0)
            })
            .unwrap();
        let asset_count_before: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM asset WHERE root_id = ?1",
                [root_id],
                |r| r.get(0),
            )
            .unwrap();

        move_root_to_volume(&conn, root_id, vid_archive).unwrap();

        let new_volume_id: i64 = conn
            .query_row("SELECT volume_id FROM root WHERE id = ?1", [root_id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(new_volume_id, vid_archive);
        let asset_count_after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM asset WHERE root_id = ?1",
                [root_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(asset_count_before, asset_count_after);
    }

    #[test]
    fn relink_matches_by_fingerprint_on_unrecognized_volume() {
        let conn = open_in_memory().unwrap();
        let vid = schema::upsert_volume(&conn, "ntfs64:aaa", None, None, false, "H:\\", now_unix())
            .unwrap();
        let dir = TempDir::new().unwrap();
        write_sample_files(&dir);
        build(&conn, vid, "", dir.path(), Some(Tier::Partial)).unwrap();
        schema::mark_offline_except(&conn, &[]).unwrap();

        // Simulate the same files having been copied onto a fresh, previously unknown volume
        // under a *different* relative path -- the scenario a size+name-only match would miss.
        let mut candidates = HashMap::new();
        let path1 = dir.path().join("IMG_0001.NEF");
        let path2 = dir.path().join("IMG_0002.NEF");
        let fp1 = fingerprint::partial_hash(&path1).unwrap();
        let fp2 = fingerprint::partial_hash(&path2).unwrap();
        let size1 = fs::metadata(&path1).unwrap().len();
        let size2 = fs::metadata(&path2).unwrap().len();
        candidates.insert("Renamed/IMG_0001.NEF".to_string(), (size1, fp1));
        candidates.insert("Renamed/IMG_0002.NEF".to_string(), (size2, fp2));

        let results = relink_against_unknown_volume(&conn, &candidates).unwrap();
        assert_eq!(results.len(), 2);
        for (_, outcome) in &results {
            assert!(matches!(outcome, ResolveOutcome::RelinkedByFingerprint(_)));
        }
    }

    #[test]
    fn relink_reports_lost_when_no_fingerprint_matches() {
        let conn = open_in_memory().unwrap();
        let vid = schema::upsert_volume(&conn, "ntfs64:aaa", None, None, false, "H:\\", now_unix())
            .unwrap();
        let dir = TempDir::new().unwrap();
        write_sample_files(&dir);
        build(&conn, vid, "", dir.path(), Some(Tier::Partial)).unwrap();
        schema::mark_offline_except(&conn, &[]).unwrap();

        let candidates = HashMap::new();
        let results = relink_against_unknown_volume(&conn, &candidates).unwrap();
        assert_eq!(results.len(), 2);
        for (_, outcome) in &results {
            assert_eq!(*outcome, ResolveOutcome::Lost);
        }
    }

    #[test]
    fn relink_never_double_claims_a_candidate_for_two_assets_with_the_same_fingerprint() {
        let conn = open_in_memory().unwrap();
        let vid = schema::upsert_volume(&conn, "ntfs64:aaa", None, None, false, "H:\\", now_unix())
            .unwrap();
        let dir = TempDir::new().unwrap();
        // Two genuinely identical files (same bytes -> same fingerprint AND size) at different
        // paths -- the exact case a naive "first fingerprint match wins" scan would double-claim
        // a single candidate for both.
        fs::write(dir.path().join("IMG_A.NEF"), b"identical-bytes-both-files").unwrap();
        fs::write(dir.path().join("IMG_B.NEF"), b"identical-bytes-both-files").unwrap();
        build(&conn, vid, "", dir.path(), Some(Tier::Partial)).unwrap();
        schema::mark_offline_except(&conn, &[]).unwrap();

        // Only ONE candidate file exists on the unrecognized volume with that fingerprint+size --
        // simulating that only one of the two duplicates actually survived the move.
        let mut candidates = HashMap::new();
        let fp = fingerprint::partial_hash(&dir.path().join("IMG_A.NEF")).unwrap();
        let size = fs::metadata(dir.path().join("IMG_A.NEF")).unwrap().len();
        candidates.insert("Recovered/only_copy.NEF".to_string(), (size, fp));

        let results = relink_against_unknown_volume(&conn, &candidates).unwrap();
        assert_eq!(results.len(), 2);
        let relinked_count = results
            .iter()
            .filter(|(_, o)| matches!(o, ResolveOutcome::RelinkedByFingerprint(_)))
            .count();
        let lost_count = results
            .iter()
            .filter(|(_, o)| *o == ResolveOutcome::Lost)
            .count();
        assert_eq!(
            relinked_count, 1,
            "exactly one asset may claim the single available candidate"
        );
        assert_eq!(
            lost_count, 1,
            "the other asset must be honestly reported Lost, not double-matched to the same file"
        );
    }

    #[test]
    fn relink_falls_back_to_size_and_name_when_no_fingerprint_was_computed() {
        let conn = open_in_memory().unwrap();
        let vid = schema::upsert_volume(&conn, "ntfs64:aaa", None, None, false, "H:\\", now_unix())
            .unwrap();
        let dir = TempDir::new().unwrap();
        write_sample_files(&dir);
        // Tier::None -- no fingerprint computed at import time, exercising tier (a)'s last-resort
        // size+name path rather than the fingerprint tiers the other tests already cover.
        build(&conn, vid, "", dir.path(), None).unwrap();
        schema::mark_offline_except(&conn, &[]).unwrap();

        let mut candidates = HashMap::new();
        let size1 = fs::metadata(dir.path().join("IMG_0001.NEF")).unwrap().len();
        let size2 = fs::metadata(dir.path().join("IMG_0002.NEF")).unwrap().len();
        // Different relative path (moved into a dated subfolder), same file name + size --
        // the exact shape the size+name fallback exists for.
        candidates.insert(
            "2026/09/IMG_0001.NEF".to_string(),
            (size1, "unused-fingerprint".to_string()),
        );
        candidates.insert(
            "2026/09/IMG_0002.NEF".to_string(),
            (size2, "unused-fingerprint".to_string()),
        );

        let results = relink_against_unknown_volume(&conn, &candidates).unwrap();
        assert_eq!(results.len(), 2);
        for (_, outcome) in &results {
            assert!(matches!(outcome, ResolveOutcome::RelinkedBySizeName(_)));
        }
    }
}
