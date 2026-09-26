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
            Some(Tier::Partial) => fingerprint::partial_hash(abs_path).ok(),
            Some(Tier::Full) => fingerprint::full_hash(abs_path).ok(),
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
    /// No match at any tier.
    Lost,
}

/// The "tree moved onto an unrecognized volume" scenario: given a currently-mounted, previously
/// unknown volume's file listing (path -> (size, partial_hash)), tries to relink every `asset`
/// row whose owning volume is offline, cheapest tier first.
pub fn relink_against_unknown_volume(
    conn: &Connection,
    candidate_files: &HashMap<String, (u64, String)>,
) -> Result<Vec<(i64, ResolveOutcome)>> {
    let mut stmt = conn.prepare(
        "SELECT a.id, a.rel_path, a.size_bytes, a.fingerprint
         FROM asset a
         JOIN root r ON r.id = a.root_id
         JOIN volume v ON v.id = r.volume_id
         WHERE v.online = 0",
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

    let mut results = Vec::new();
    for (asset_id, rel_path, size_bytes, fingerprint) in offline_assets {
        let by_path = candidate_files.get(&rel_path);
        let matched = match (fingerprint.as_deref(), by_path) {
            (Some(fp), Some((candidate_size, candidate_fp)))
                if fp == candidate_fp && size_bytes == *candidate_size =>
            {
                Some(rel_path.clone())
            }
            _ => candidate_files
                .iter()
                .find(|(_, (candidate_size, candidate_fp))| {
                    fingerprint.as_deref() == Some(candidate_fp.as_str())
                        && *candidate_size == size_bytes
                })
                .map(|(path, _)| path.clone()),
        };

        results.push((
            asset_id,
            match matched {
                Some(path) => ResolveOutcome::RelinkedByFingerprint(path),
                None => ResolveOutcome::Lost,
            },
        ));
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
}
