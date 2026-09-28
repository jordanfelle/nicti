//! Patrol: #24's manual, user-triggered catalog sync, matching Lightroom Classic's "Synchronize
//! Folder" rather than a continuous background filesystem watcher (the original #24 scope, dropped
//! 2026-09-27 — see `docs/adr/0024-manual-catalog-sync.md`). Layered on top of Scruff: Scruff's
//! `ingest_root` only ever walks the disk, so it can add, update, and relink an in-root move, but
//! it has no way to notice a path it already knows about that's no longer there. Patrol adds that
//! second, catalog-side pass.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use unicode_normalization::UnicodeNormalization;

use crate::scruff::{Ingest, IngestReport};
use crate::{Asset, CatalogError, CatalogStore};

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
    let mut sync = Sync::new(root_id, root_path, *opts);
    while sync.step(store)? {}
    Ok(sync.into_report())
}

/// `root_path.try_exists() == Ok(true) && root_path.is_dir()` -- the exact reachability check
/// `sync_root` ran twice (once before ingest, once again right after) as a bare `match`; pulled
/// into a helper since [`Sync::step`] now needs the same check at two separate phase
/// transitions.
fn root_reachable(root_path: &Path) -> bool {
    matches!(root_path.try_exists(), Ok(true)) && root_path.is_dir()
}

enum Phase {
    /// Scruff's disk-side pass, one file per [`Ingest::step`] call.
    Ingesting(Ingest),
    /// A one-shot transition: lists every cataloged asset under this root and seeds the
    /// catalog-side pass's working state. Not further chunked -- `list_assets_by_root` is one
    /// SQL query, not a disk walk.
    Listing,
    /// The catalog-side pass: one already-cataloged asset checked per `step()` call, matching
    /// `Ingest`'s own one-file-per-chunk granularity.
    Checking {
        assets: Vec<Asset>,
        idx: usize,
        unreadable_dirs: Vec<PathBuf>,
        present_dirs: HashSet<String>,
        missing_dirs: HashSet<String>,
        now: i64,
    },
    Done,
}

/// A steppable manual sync run: Scruff's disk-side ingest pass, then Patrol's own catalog-side
/// pass, one file/asset at a time. [`sync_root`] is a thin loop over this -- see that function's
/// own doc comment for the overall two-pass design this promotes.
pub struct Sync {
    root_id: i64,
    root_path: PathBuf,
    opts: SyncOptions,
    phase: Phase,
    report: SyncReport,
}

impl Sync {
    pub fn new(root_id: i64, root_path: &Path, opts: SyncOptions) -> Self {
        Sync {
            root_id,
            root_path: root_path.to_path_buf(),
            opts,
            phase: Phase::Ingesting(Ingest::new(root_id, root_path)),
            report: SyncReport::default(),
        }
    }

    /// Runs one chunk of work (one ingest candidate, or one already-cataloged asset check).
    /// Returns `Ok(true)` if more remains, `Ok(false)` once the whole sync is done (including the
    /// `root_unreachable` early-out, which this reports as `false` -- there's nothing left to run
    /// either way).
    pub fn step(&mut self, store: &dyn CatalogStore) -> Result<bool, CatalogError> {
        let phase = std::mem::replace(&mut self.phase, Phase::Done);
        match phase {
            Phase::Done => Ok(false),

            Phase::Ingesting(mut ingest) => {
                if !root_reachable(&self.root_path) {
                    self.report.root_unreachable = true;
                    return Ok(false);
                }
                if ingest.step(store)? {
                    self.phase = Phase::Ingesting(ingest);
                    return Ok(true);
                }
                self.report.ingest = ingest.into_report();

                // Re-check reachability after ingest's own (potentially long, on a large library)
                // disk walk -- narrows, though doesn't fully close, the window where a drive could
                // be unplugged mid-sync: a disconnect that surfaces as a plain "not found" (rather
                // than an I/O error) on the catalog-side per-asset checks below would otherwise
                // look identical to every one of this root's files actually having vanished, which
                // is exactly the mass-flag/mass-delete outcome this guard exists to prevent. Found
                // by adversarial review.
                if !root_reachable(&self.root_path) {
                    self.report.root_unreachable = true;
                    return Ok(false);
                }
                self.phase = Phase::Listing;
                Ok(true)
            }

            Phase::Listing => {
                // Any path Scruff couldn't even walk (a permission-denied subdirectory) can't be
                // trusted to report an accurate "gone from disk" either way -- an asset under it
                // is left alone rather than risk flagging it missing (or, worse under
                // `remove_missing`, deleting it) based on a read failure rather than a genuine
                // absence.
                //
                // NFC-normalized for comparison (found by CodeRabbit's review): `IngestReport::
                // failed` carries the literal filesystem path Scruff's `WalkDir` returned, which
                // is whatever normalization form the underlying directory entries actually use,
                // while `asset.rel_path` is always NFC-composed (`scruff::normalize_rel_path`).
                // On a filesystem that preserves decomposed (NFD) names as-given rather than
                // normalizing them (ext4, unlike NTFS/HFS+/APFS), comparing the two forms directly
                // via `Path::starts_with` -- a byte/component comparison with no Unicode
                // awareness -- could silently fail to recognize an asset as living under an
                // unreadable directory. Normalizing both sides the same way before comparing
                // closes that gap.
                let unreadable_dirs: Vec<PathBuf> = self
                    .report
                    .ingest
                    .failed
                    .iter()
                    .map(|(path, _)| normalize_path_for_compare(path))
                    .collect();
                let assets = store.list_assets_by_root(self.root_id)?;
                self.phase = Phase::Checking {
                    assets,
                    idx: 0,
                    unreadable_dirs,
                    present_dirs: HashSet::new(),
                    missing_dirs: HashSet::new(),
                    now: now_unix(),
                };
                Ok(true)
            }

            Phase::Checking {
                assets,
                mut idx,
                unreadable_dirs,
                mut present_dirs,
                mut missing_dirs,
                now,
            } => {
                if idx >= assets.len() {
                    self.report.missing_folders =
                        missing_dirs.difference(&present_dirs).cloned().collect();
                    self.report.missing_folders.sort();
                    return Ok(false);
                }

                let asset = &assets[idx];
                let dir = dir_prefix(&asset.rel_path);
                let asset_path = self.root_path.join(&asset.rel_path);
                let comparable_asset_path = normalize_path_for_compare(&asset_path);

                if unreadable_dirs
                    .iter()
                    .any(|d| comparable_asset_path.starts_with(d))
                {
                    // left alone, see this phase's own doc comment above
                } else {
                    // NOT fixed here (flagged by CodeRabbit's review, deferred): `asset_path`
                    // itself is built from the NFC-composed `rel_path` the catalog stores, so on
                    // the same kind of normalization-preserving filesystem described above,
                    // `try_exists()` below could return `false` for a file that is genuinely
                    // present under an NFD-spelled directory entry -- the OS resolves a path by
                    // exact bytes, so no amount of in-memory Rust-side normalization changes what
                    // the syscall actually finds. A real fix needs the catalog to retain each
                    // asset's original filesystem-spelling path (not just its normalized
                    // `rel_path`) and resolve against that -- a schema/API change, tracked as a
                    // follow-up rather than expanding this PR's scope; low real-world risk for
                    // v1's actual target (Windows/NTFS doesn't split precomposed characters into
                    // decomposed form on its own).
                    match asset_path.try_exists() {
                        Ok(true) => {
                            present_dirs.extend(ancestor_prefixes(&dir));
                            if asset.missing_since.is_some() {
                                store.set_asset_missing(asset.id, None)?;
                                self.report.found_again += 1;
                            }
                        }
                        Ok(false) => {
                            if !dir.is_empty() {
                                missing_dirs.insert(dir);
                            }
                            if self.opts.remove_missing {
                                store.remove_asset(asset.id)?;
                                self.report.removed += 1;
                            } else if asset.missing_since.is_none() {
                                store.set_asset_missing(asset.id, Some(now))?;
                                self.report.newly_missing += 1;
                            }
                            // else: already flagged missing from an earlier sync and not removing
                            // this run -- left alone, so re-running a plain sync is idempotent.
                        }
                        // A permission or I/O error stat-ing the exact path is treated the same
                        // as an unreadable parent directory -- leave the row alone rather than
                        // guess.
                        Err(_) => {}
                    }
                }

                idx += 1;
                self.phase = Phase::Checking {
                    assets,
                    idx,
                    unreadable_dirs,
                    present_dirs,
                    missing_dirs,
                    now,
                };
                Ok(true)
            }
        }
    }

    /// Total chunks this run will process, once known -- `None` while still in the ingest phase
    /// (Scruff's walk is lazy), `Some(assets.len())` from the catalog-side pass onward.
    pub fn total_hint(&self) -> Option<u64> {
        match &self.phase {
            Phase::Checking { assets, .. } => Some(assets.len() as u64),
            _ => None,
        }
    }

    pub fn into_report(self) -> SyncReport {
        self.report
    }
}

/// NFC-composes `path`'s string form for comparison purposes only -- this never touches disk or
/// changes what bytes a later `try_exists()`/`starts_with()` call actually resolves against, it
/// only makes two in-memory `PathBuf`s built from differently-normalized source strings compare
/// equal the way `scruff::normalize_rel_path`'s own NFC convention already assumes. Found by
/// CodeRabbit's review.
fn normalize_path_for_compare(path: &Path) -> PathBuf {
    PathBuf::from(path.to_string_lossy().nfc().collect::<String>())
}

/// The directory portion of a normalized `rel_path` (forward-slash separated, no leading slash --
/// `scruff::normalize_rel_path`'s output shape). Empty for a file directly under the root.
fn dir_prefix(rel_path: &str) -> String {
    match rel_path.rfind('/') {
        Some(idx) => rel_path[..idx].to_string(),
        None => String::new(),
    }
}

/// `dir` itself, plus every one of its own ancestor directory prefixes (`"2026/09"` yields
/// `["2026/09", "2026"]`) -- used to roll a present asset's directory up through every level above
/// it, so a live file nested several directories deep still marks all of its ancestors as "has
/// present content," not just its own immediate parent. Empty input yields nothing (a top-level
/// file's empty `dir_prefix` has no directory to roll up into).
fn ancestor_prefixes(dir: &str) -> impl Iterator<Item = String> + '_ {
    let mut current = Some(dir).filter(|d| !d.is_empty());
    std::iter::from_fn(move || {
        let this = current.take()?;
        current = this.rfind('/').map(|idx| &this[..idx]);
        Some(this.to_string())
    })
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

    #[test]
    fn ancestor_prefixes_of_empty_dir_is_empty() {
        assert_eq!(
            super::ancestor_prefixes("").collect::<Vec<_>>(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn ancestor_prefixes_includes_every_level_up_to_the_root() {
        assert_eq!(
            super::ancestor_prefixes("2026/09").collect::<Vec<_>>(),
            vec!["2026/09".to_string(), "2026".to_string()]
        );
    }
}
