//! Patrol: #24's manual, user-triggered catalog sync, matching Lightroom Classic's "Synchronize
//! Folder" rather than a continuous background filesystem watcher (the original #24 scope, dropped
//! 2026-09-27 — see `docs/adr/0024-manual-catalog-sync.md`). Layered on top of Scruff: Scruff's
//! `ingest_root` only ever walks the disk, so it can add, update, and relink an in-root move, but
//! it has no way to notice a path it already knows about that's no longer there. Patrol adds that
//! second, catalog-side pass.

use std::path::Path;

use crate::scruff::{ingest_root, IngestReport};
use crate::{CatalogError, CatalogStore};

/// Options for one `sync_root` call.
#[derive(Debug, Clone, Copy, Default)]
pub struct SyncOptions {
    /// When `true`, an asset still missing after this sync's disk pass is deleted outright rather
    /// than just flagged. Off by default, matching LRC's "Remove missing photos from catalog"
    /// checkbox, which a user opts into per-run rather than having on by default.
    pub remove_missing: bool,
}

/// Outcome of one `sync_root` run.
#[derive(Debug, Default)]
pub struct SyncReport {
    /// Scruff's own report from this run's disk-side pass (added/updated/skipped/moved/failed).
    pub ingest: IngestReport,
    /// Assets newly flagged `missing_since` this run (were present last sync, gone now).
    pub newly_missing: u64,
    /// Assets whose `missing_since` was cleared this run (were missing, found again at their
    /// cataloged path). A file found at a *different* path is a `moved` relink instead, counted in
    /// `ingest`, not here.
    pub found_again: u64,
    /// Assets deleted this run because they were still missing and `remove_missing` was set.
    pub removed: u64,
    /// Directories (as `rel_path` prefixes, relative to the root) where every asset under them is
    /// now missing. Computed fresh each run; #24 doesn't add a folder table (ADR-0061 already
    /// treats folders as `asset.rel_path` prefixes, not a separate row).
    pub missing_folders: Vec<String>,
    /// `true` if `root_path` itself couldn't be resolved as a directory (unplugged drive, changed
    /// drive letter, deleted folder). When this is `true`, every other field is left at its default
    /// and nothing else in this report happened -- no asset was touched. An unreachable root is
    /// ADR-0071's offline-volume/remap territory, not something #24 should ever act on by marking
    /// or removing every asset underneath it.
    pub root_unreachable: bool,
}

/// Runs a manual sync against `root_path` (already registered as `root_id`): first Scruff's
/// disk-side ingest pass (add/update/relink), then a catalog-side pass that checks every already-
/// cataloged asset for whether its file is still there, flagging (or, if `opts.remove_missing`,
/// removing) any that aren't.
///
/// Returns immediately with `root_unreachable: true` if `root_path` itself doesn't resolve to a
/// directory right now, touching no rows at all -- see [`SyncReport::root_unreachable`].
pub fn sync_root(
    store: &dyn CatalogStore,
    root_id: i64,
    root_path: &Path,
    opts: &SyncOptions,
) -> Result<SyncReport, CatalogError> {
    let mut report = SyncReport::default();

    match root_path.try_exists() {
        Ok(true) if root_path.is_dir() => {}
        _ => {
            report.root_unreachable = true;
            return Ok(report);
        }
    }

    report.ingest = ingest_root(store, root_id, root_path)?;

    // Any path Scruff couldn't even walk (a permission-denied subdirectory) can't be trusted to
    // report an accurate "gone from disk" either way -- an asset under it is left alone rather than
    // risk flagging it missing (or, worse under `remove_missing`, deleting it) based on a read
    // failure rather than a genuine absence.
    let unreadable_dirs: Vec<&Path> = report
        .ingest
        .failed
        .iter()
        .map(|(path, _)| path.as_path())
        .collect();

    let now = now_unix();
    let assets = store.list_assets_by_root(root_id)?;
    let mut present_dirs: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut missing_dirs: std::collections::HashSet<String> = std::collections::HashSet::new();

    for asset in &assets {
        let dir = dir_prefix(&asset.rel_path);
        let asset_path = root_path.join(&asset.rel_path);

        if unreadable_dirs.iter().any(|d| asset_path.starts_with(d)) {
            continue;
        }

        match asset_path.try_exists() {
            Ok(true) => {
                if !dir.is_empty() {
                    present_dirs.insert(dir);
                }
                if asset.missing_since.is_some() {
                    store.set_asset_missing(asset.id, None)?;
                    report.found_again += 1;
                }
            }
            Ok(false) => {
                if !dir.is_empty() {
                    missing_dirs.insert(dir);
                }
                if opts.remove_missing {
                    store.remove_asset(asset.id)?;
                    report.removed += 1;
                } else if asset.missing_since.is_none() {
                    store.set_asset_missing(asset.id, Some(now))?;
                    report.newly_missing += 1;
                }
                // else: already flagged missing from an earlier sync and not removing this run --
                // left alone, so re-running a plain sync is idempotent (not re-counted every time).
            }
            // A permission or I/O error stat-ing the exact path is treated the same as an
            // unreadable parent directory -- leave the row alone rather than guess.
            Err(_) => {}
        }
    }

    report.missing_folders = missing_dirs.difference(&present_dirs).cloned().collect();
    report.missing_folders.sort();

    Ok(report)
}

/// The directory portion of a normalized `rel_path` (forward-slash separated, no leading slash --
/// `scruff::normalize_rel_path`'s output shape). Empty for a file directly under the root.
fn dir_prefix(rel_path: &str) -> String {
    match rel_path.rfind('/') {
        Some(idx) => rel_path[..idx].to_string(),
        None => String::new(),
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    #[test]
    fn dir_prefix_is_empty_for_a_top_level_file() {
        assert_eq!(super::dir_prefix("a.nef"), "");
    }

    #[test]
    fn dir_prefix_returns_the_directory_portion() {
        assert_eq!(super::dir_prefix("2026/09/a.nef"), "2026/09");
    }
}
