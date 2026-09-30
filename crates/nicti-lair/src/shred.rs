//! Shred (#32, culling): the "select all, then Delete" engine. A cat shreds what it is finished
//! with -- this removes a batch of assets from the catalog and, on request, moves their RAW files
//! (and `.xmp` sidecars) to the OS Recycle Bin first.
//!
//! Modelled on [`crate::carry`]: a steppable engine ([`Shred::step`], one bounded chunk per call,
//! never `Err`), a journal (`delete_item`) that is written **before** any file is touched, and a
//! startup recovery pass ([`resume_open_deletes`]) that finishes or rolls back whatever a crash
//! left behind. The rule it shares with Carry: never drop catalog knowledge of a file that might
//! still exist -- when the disk can't say for sure (an unreachable folder), the row stays.
//!
//! Per chunk in [`DeleteMode::RecycleBin`]:
//! 1. journal the chunk `pending` (record-before-write);
//! 2. hand the RAW + sidecar paths to the [`Trasher`];
//! 3. decide per asset from what is *on disk afterwards* (a batch Recycle Bin call reports one
//!    error for the whole batch, so the filesystem, not the return value, says what happened);
//! 4. flip the trashed rows to `trashed`, drop the journal rows of the ones that are still there;
//! 5. remove the trashed assets' catalog rows (children and journal row in one transaction).
//!
//! [`DeleteMode::CatalogOnly`] skips 1-4: the files stay on disk, only the rows go.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::{Asset, CatalogError, CatalogStore, DeleteItem};

/// Assets handled per [`Shred::step`]. Big enough to amortize a Recycle Bin call and a catalog
/// transaction, small enough that cancel/progress stay responsive at 90k assets.
pub const CHUNK: usize = 200;

/// What a delete does to the files on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteMode {
    /// Move the RAW file and its `.xmp` sidecar to the OS Recycle Bin, then remove the catalog
    /// rows. Recoverable from the bin.
    RecycleBin,
    /// Remove the catalog rows only; the files stay where they are.
    CatalogOnly,
}

/// Moves files to the OS Recycle Bin. A trait so tests (and non-Windows CI) can fake it; the real
/// one is [`RecycleBin`].
pub trait Trasher: Send {
    /// Trashes every path. May fail as a whole after trashing some of them -- the caller judges
    /// per-file success from the filesystem, not from this `Result`.
    fn trash_all(&self, paths: &[PathBuf]) -> Result<(), String>;
}

/// The real [`Trasher`]: the `trash` crate (Windows Recycle Bin via `IFileOperation`, freedesktop
/// trash elsewhere).
pub struct RecycleBin;

impl Trasher for RecycleBin {
    fn trash_all(&self, paths: &[PathBuf]) -> Result<(), String> {
        trash::delete_all(paths).map_err(|e| e.to_string())
    }
}

/// An asset the delete could not (fully) process, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leftover {
    pub asset_id: i64,
    pub path: PathBuf,
    pub reason: String,
}

/// What a finished (or cancelled) delete did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ShredReport {
    /// Catalog rows removed.
    pub removed: usize,
    /// Of `removed`, how many had their RAW file moved to the Recycle Bin by this run.
    pub trashed: usize,
    /// Of `removed`, how many had a RAW file that was already gone from a reachable folder.
    pub already_missing: usize,
    /// Assets left in the catalog (file locked, folder unreachable, ...), with the reason.
    pub leftovers: Vec<Leftover>,
    /// Sidecars that could not be trashed (their RAW's row is still removed).
    pub sidecars_left: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShredOutcome {
    Done(ShredReport),
    /// A catalog error stopped the run. Everything already removed stays removed; the journal
    /// (if any chunk was mid-flight) is settled by [`resume_open_deletes`] at the next startup.
    Failed {
        message: String,
        report: ShredReport,
    },
}

/// Called with each chunk's removed ids (see [`Shred::on_removed`]).
type RemovedHook = Box<dyn FnMut(&[i64]) + Send>;

/// One asset queued for deletion, resolved to an absolute path.
struct Entry {
    id: i64,
    raw: PathBuf,
}

enum Phase {
    Prepare,
    Chunks { entries: Vec<Entry>, next: usize },
    Finished,
}

/// The steppable delete engine. Build with [`Shred::new`], call [`Shred::step`] until it returns
/// `Some`.
pub struct Shred {
    store: Arc<dyn CatalogStore + Send + Sync>,
    mode: DeleteMode,
    trasher: Box<dyn Trasher>,
    ids: Vec<i64>,
    on_removed: Option<RemovedHook>,
    phase: Phase,
    report: ShredReport,
    total: u64,
    /// Each folder is listed once for the whole run (a 100k-photo folder must not be re-read, or
    /// re-scanned per photo, for every chunk).
    listings: Listings,
}

impl Shred {
    pub fn new(
        store: Arc<dyn CatalogStore + Send + Sync>,
        mode: DeleteMode,
        ids: Vec<i64>,
        trasher: Box<dyn Trasher>,
    ) -> Self {
        // A repeated id would be double-counted and hand the same file to the bin twice.
        let mut seen = HashSet::new();
        let ids: Vec<i64> = ids.into_iter().filter(|id| seen.insert(*id)).collect();
        let total = ids.len() as u64;
        Shred {
            store,
            mode,
            trasher,
            ids,
            on_removed: None,
            phase: Phase::Prepare,
            report: ShredReport::default(),
            total,
            listings: Listings::default(),
        }
    }

    /// Called with each chunk's removed asset ids right after their catalog rows are gone -- the
    /// hook the UI uses to purge the T2 preview cache (`Larder::purge_asset`), which lives outside
    /// the catalog.
    pub fn on_removed(mut self, hook: impl FnMut(&[i64]) + Send + 'static) -> Self {
        self.on_removed = Some(Box::new(hook));
        self
    }

    pub fn label(&self) -> String {
        match self.mode {
            DeleteMode::RecycleBin => format!("Moving {} photos to the Recycle Bin", self.total),
            DeleteMode::CatalogOnly => format!("Removing {} photos from the catalog", self.total),
        }
    }

    /// `(done, total)` in assets.
    pub fn progress(&self) -> (u64, Option<u64>) {
        let done = (self.report.removed + self.report.leftovers.len()) as u64;
        (done, Some(self.total))
    }

    /// What has been done so far (the final report once `step` returned `Some`).
    pub fn partial_report(&self) -> &ShredReport {
        &self.report
    }

    /// Runs one bounded chunk of work. `None` = more to do; `Some` = finished, never call again.
    /// Never returns `Err`: a catalog error becomes [`ShredOutcome::Failed`].
    pub fn step(&mut self) -> Option<ShredOutcome> {
        match self.step_inner() {
            Ok(None) => None,
            Ok(Some(())) => Some(ShredOutcome::Done(self.report.clone())),
            Err(e) => {
                self.phase = Phase::Finished;
                Some(ShredOutcome::Failed {
                    message: e.to_string(),
                    report: self.report.clone(),
                })
            }
        }
    }

    fn step_inner(&mut self) -> Result<Option<()>, CatalogError> {
        match std::mem::replace(&mut self.phase, Phase::Finished) {
            Phase::Prepare => {
                let entries = self.resolve_entries()?;
                self.phase = Phase::Chunks { entries, next: 0 };
                Ok(None)
            }
            Phase::Chunks { entries, next } => {
                if next >= entries.len() {
                    return Ok(Some(()));
                }
                let end = (next + CHUNK).min(entries.len());
                self.process_chunk(&entries[next..end])?;
                self.phase = Phase::Chunks { entries, next: end };
                Ok(None)
            }
            Phase::Finished => Ok(Some(())),
        }
    }

    /// Ids -> absolute RAW paths. Ids whose row is already gone are skipped silently; a root that
    /// can't be resolved leaves its assets alone (a leftover, never a removal).
    fn resolve_entries(&mut self) -> Result<Vec<Entry>, CatalogError> {
        let mut entries = Vec::with_capacity(self.ids.len());
        let mut root_paths: std::collections::HashMap<i64, Option<PathBuf>> = Default::default();
        // Roots with an unfinished folder move (`root_move` journal row, e.g. a destination drive
        // that went offline): their files are mid-copy or mid-cleanup, and `Carry`'s recovery
        // reasons about exactly which copies exist. Trashing files under it would change that
        // picture behind its back, so those photos wait until the move is resolved.
        let moving_roots: HashSet<i64> = self
            .store
            .open_root_moves()?
            .iter()
            .map(|m| m.root_id)
            .collect();
        for &id in &self.ids {
            let Some(asset) = self.store.get_asset(id)? else {
                continue;
            };
            if moving_roots.contains(&asset.root_id) {
                self.report.leftovers.push(Leftover {
                    asset_id: id,
                    path: PathBuf::from(&asset.rel_path),
                    reason:
                        "its folder has an unfinished move (see the folder panel); restart Nicti \
with the drives connected so the move can finish, then delete again"
                            .into(),
                });
                continue;
            }
            let root = match root_paths.get(&asset.root_id) {
                Some(p) => p.clone(),
                None => {
                    let p = self.store.get_root_path(asset.root_id)?.map(PathBuf::from);
                    root_paths.insert(asset.root_id, p.clone());
                    p
                }
            };
            match root {
                Some(_)
                    if self.mode == DeleteMode::RecycleBin
                        && !stays_inside_root(&asset.rel_path) =>
                {
                    self.report.leftovers.push(Leftover {
                        asset_id: id,
                        path: PathBuf::from(&asset.rel_path),
                        reason:
                            "its catalog path points outside its folder, so nothing was touched"
                                .into(),
                    })
                }
                Some(root) => entries.push(Entry {
                    id,
                    raw: asset_path(&root, &asset),
                }),
                None => self.report.leftovers.push(Leftover {
                    asset_id: id,
                    path: PathBuf::from(&asset.rel_path),
                    reason: "its folder is no longer registered".into(),
                }),
            }
        }
        Ok(entries)
    }

    fn process_chunk(&mut self, chunk: &[Entry]) -> Result<(), CatalogError> {
        let mut to_remove: Vec<i64> = Vec::with_capacity(chunk.len());

        match self.mode {
            DeleteMode::CatalogOnly => to_remove.extend(chunk.iter().map(|e| e.id)),
            DeleteMode::RecycleBin => {
                // Files already gone from a reachable folder need no trashing. Anything the disk
                // can't answer for -- an unreachable folder, or a stat that *errored* (permission
                // denied, a flaky network drive; `Path::exists` would call that "missing") -- is
                // left completely alone.
                let mut live: Vec<&Entry> = Vec::new();
                for e in chunk {
                    match probe(&e.raw) {
                        Probe::Present => live.push(e),
                        Probe::Absent => {
                            self.report.already_missing += 1;
                            to_remove.push(e.id);
                        }
                        Probe::Unknown => self.report.leftovers.push(Leftover {
                            asset_id: e.id,
                            path: e.raw.clone(),
                            reason: UNKNOWN_REASON.into(),
                        }),
                    }
                }

                if !live.is_empty() {
                    // 1. record before write
                    let journal: Vec<(i64, String)> = live
                        .iter()
                        .map(|e| (e.id, e.raw.to_string_lossy().into_owned()))
                        .collect();
                    self.store.begin_delete_items(&journal)?;

                    // 2. trash the RAWs only. Sidecars are decided in step 3b, from what is
                    // actually left on disk, so a RAW that could not be trashed (locked) -- or a
                    // sibling that wasn't -- keeps its metadata.
                    let raws: Vec<PathBuf> = live.iter().map(|e| e.raw.clone()).collect();
                    let err = self.trasher.trash_all(&raws).err();

                    // 3. judge from the filesystem, never from the bin call's own Result
                    let mut trashed_ids = Vec::new();
                    let mut kept_ids = Vec::new();
                    let mut gone: Vec<&Path> = Vec::new();
                    for e in &live {
                        match probe(&e.raw) {
                            Probe::Present => {
                                kept_ids.push(e.id);
                                self.report.leftovers.push(Leftover {
                                    asset_id: e.id,
                                    path: e.raw.clone(),
                                    reason: err
                                        .clone()
                                        .unwrap_or_else(|| "the file is still on disk".into()),
                                });
                            }
                            Probe::Absent => {
                                trashed_ids.push(e.id);
                                self.report.trashed += 1;
                                gone.push(&e.raw);
                            }
                            // The folder vanished or the stat failed *during* the delete (the
                            // drive was pulled mid-trash). The file may or may not have gone:
                            // keep the row AND leave the journal `pending`, so startup recovery
                            // decides once the disk can answer.
                            Probe::Unknown => self.report.leftovers.push(Leftover {
                                asset_id: e.id,
                                path: e.raw.clone(),
                                reason: UNKNOWN_REASON.into(),
                            }),
                        }
                    }
                    // 3b. only now the sidecars of the RAWs that are really gone, and only those no
                    // other photo still needs: a sibling that still exists on disk (locked, not
                    // selected, or one we can't check) keeps the shared sidecar. Two selected
                    // photos can share one, so it is handed over once. Best effort: a sidecar the
                    // bin can't take is reported, and its RAW's row is removed anyway.
                    let mut asked_sidecars: Vec<PathBuf> = Vec::new();
                    for raw in gone {
                        if let Some(sc) = sidecar_to_trash(raw, &mut self.listings) {
                            if !asked_sidecars.contains(&sc) {
                                asked_sidecars.push(sc);
                            }
                        }
                    }
                    if !asked_sidecars.is_empty() {
                        let _ = self.trasher.trash_all(&asked_sidecars);
                        for sc in asked_sidecars {
                            // Anything not positively gone -- still there, or the disk can't say
                            // (the drive was pulled between the two bin calls) -- is reported.
                            if !matches!(probe(&sc), Probe::Absent) {
                                self.report.sidecars_left.push(sc);
                            }
                        }
                    }

                    // 4. settle the journal
                    self.store.mark_delete_items_trashed(&trashed_ids)?;
                    self.store.abandon_delete_items(&kept_ids)?;
                    to_remove.extend(&trashed_ids);
                }
            }
        }

        // 5. remove the rows
        self.store.remove_assets(&to_remove)?;
        self.report.removed += to_remove.len();
        if let Some(hook) = self.on_removed.as_mut() {
            hook(&to_remove);
        }
        Ok(())
    }
}

const UNKNOWN_REASON: &str =
    "the disk could not confirm the file either way (folder unreachable, drive unplugged, or no \
     permission), so nothing was decided";

/// What the disk says about one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Probe {
    /// The file is there.
    Present,
    /// The file is not there, and its folder *is* -- it really is gone.
    Absent,
    /// The disk can't say: the stat errored, or the folder itself is unreachable. Never treated
    /// as "gone" -- an unplugged drive is not a deleted photo.
    Unknown,
}

fn probe(path: &Path) -> Probe {
    match path.try_exists() {
        Ok(true) => Probe::Present,
        Ok(false) if parent_reachable(path) => Probe::Absent,
        _ => Probe::Unknown,
    }
}

/// What [`resume_open_deletes`] did with the journal a crash left behind.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResumeReport {
    /// Catalog rows removed: their files were (or are now known to be) in the Recycle Bin.
    pub finished: usize,
    /// Journal rows dropped, asset kept: the file is still on disk, the trash never happened.
    pub rolled_back: usize,
    /// Left in the journal: the disk can't say whether the file exists (unreachable folder), so
    /// nothing is decided until the folder is back.
    pub stuck: usize,
}

/// Startup crash recovery for the delete journal. A row (`trashed` or `pending`) finishes
/// only if its file is gone from a reachable folder **and** the photo does not live somewhere else
/// now (a move or relink since the journal was written); it rolls back if the file is still there
/// (at the journaled path or at the asset's current one); and if the disk can't say either way it
/// is left alone (`stuck`) rather than guessed at. Idempotent. `on_removed` gets the ids of the
/// rows it removed (same cache-purge hook as [`Shred::on_removed`]).
pub fn resume_open_deletes(
    store: &dyn CatalogStore,
    mut on_removed: impl FnMut(&[i64]),
) -> Result<ResumeReport, CatalogError> {
    let mut report = ResumeReport::default();
    let mut finish: Vec<i64> = Vec::new();
    let mut rollback: Vec<i64> = Vec::new();
    for item in store.open_delete_items()? {
        match classify(store, &item)? {
            Resume::Finish => finish.push(item.asset_id),
            Resume::Rollback => rollback.push(item.asset_id),
            Resume::Stuck => report.stuck += 1,
        }
    }
    store.remove_assets(&finish)?;
    if !finish.is_empty() {
        on_removed(&finish);
    }
    store.abandon_delete_items(&rollback)?;
    report.finished = finish.len();
    report.rolled_back = rollback.len();
    Ok(report)
}

enum Resume {
    Finish,
    Rollback,
    Stuck,
}

fn classify(store: &dyn CatalogStore, item: &DeleteItem) -> Result<Resume, CatalogError> {
    // Only a positive "absent from a reachable folder" ever finishes a delete -- for a `trashed`
    // row too. Its state proves the file reached the bin *at that moment*, not that it is still
    // gone: the user may have restored it since and then unplugged the drive, or the photo may
    // now live at a path on a drive that is offline. So an unknown answer is `Stuck` for both
    // states (the row lingers, harmlessly, until the disk can answer), and a file that is present
    // -- restored from the bin -- keeps its row.
    match probe(Path::new(&item.abs_path)) {
        Probe::Present => return Ok(Resume::Rollback),
        Probe::Unknown => return Ok(Resume::Stuck),
        Probe::Absent => {}
    }
    // The journaled path is empty -- but the journal can be older than the row: a photo moved
    // (Carry) or relinked since would live at a new path while the old folder still exists. Its
    // *current* location decides too; a live file there means this delete must not finish.
    if let Some(asset) = store.get_asset(item.asset_id)? {
        if let Some(root) = store.get_root_path(asset.root_id)? {
            if stays_inside_root(&asset.rel_path) {
                match probe(&asset_path(Path::new(&root), &asset)) {
                    Probe::Present => return Ok(Resume::Rollback),
                    Probe::Unknown => return Ok(Resume::Stuck),
                    Probe::Absent => {}
                }
            }
        }
    }
    Ok(Resume::Finish)
}

/// Whether a catalog `rel_path` is really relative to its root: no absolute path (which
/// `Path::join` would let *replace* the root), no drive prefix, and no `..` climbing out. Ingest
/// only ever produces clean relative paths; this is defence in depth for a catalog that came from
/// somewhere else (an LRC import, #62), since Delete is the one place a bad path would trash a
/// file the user never selected.
fn stays_inside_root(rel_path: &str) -> bool {
    use std::path::Component;
    !rel_path.is_empty()
        && Path::new(rel_path)
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

fn asset_path(root: &Path, asset: &Asset) -> PathBuf {
    root.join(&asset.rel_path)
}

/// The RAW's XMP sidecar (`IMG_0001.NEF` -> `IMG_0001.xmp`, the LRC/Adobe convention), if one
/// exists next to it.
fn sidecar(raw: &Path) -> Option<PathBuf> {
    ["xmp", "XMP"]
        .iter()
        .map(|ext| raw.with_extension(ext))
        .find(|p| matches!(p.try_exists(), Ok(true)))
}

/// A folder's non-`.xmp` files indexed by lowercase stem, or `None` if the folder could not be
/// listed *completely* (an unreadable folder, a transient network error, one entry that errored).
type StemIndex = Option<HashMap<String, Vec<std::ffi::OsString>>>;

/// Directory indexes, built once per folder for the whole run.
#[derive(Default)]
struct Listings {
    dirs: HashMap<PathBuf, StemIndex>,
    /// Folders actually read (tests assert this stays at one per folder).
    #[cfg(test)]
    reads: usize,
}

impl Listings {
    /// The stem index of `dir`; `None` means "couldn't tell", which callers must treat as *keep*.
    fn by_stem(&mut self, dir: &Path) -> Option<&HashMap<String, Vec<std::ffi::OsString>>> {
        #[cfg(test)]
        if !self.dirs.contains_key(dir) {
            self.reads += 1;
        }
        self.dirs
            .entry(dir.to_path_buf())
            .or_insert_with(|| index_dir(dir))
            .as_ref()
    }
}

fn index_dir(dir: &Path) -> StemIndex {
    let mut index: HashMap<String, Vec<std::ffi::OsString>> = HashMap::new();
    // Any error -- opening the folder, or any single entry -- makes the whole listing untrusted:
    // a missing sibling in a partial list would look like "nobody shares this sidecar".
    for entry in std::fs::read_dir(dir).ok()? {
        let name = entry.ok()?.file_name();
        let path = Path::new(&name);
        if path
            .extension()
            .is_some_and(|e| e.to_string_lossy().eq_ignore_ascii_case("xmp"))
        {
            continue;
        }
        if let Some(stem) = path.file_stem() {
            index
                .entry(stem.to_string_lossy().to_lowercase())
                .or_default()
                .push(name.clone());
        }
    }
    Some(index)
}

/// The sidecar to trash along with `raw` (already gone from disk), or `None` if there is none *or
/// another photo still needs it*. `IMG_0001.NEF` and `IMG_0001.NRW`/`.JPG` share `IMG_0001.xmp`;
/// deleting one must not take the other's metadata with it. Any other file with the same stem that
/// still exists -- or whose existence can't be checked, or in a folder that can't be listed --
/// keeps the sidecar. Decided from what is on disk *now*, so a sibling that was selected but could
/// not be trashed (locked) still counts, and one already trashed earlier in the run does not (its
/// own turn, or an earlier one, finds no one left).
fn sidecar_to_trash(raw: &Path, listings: &mut Listings) -> Option<PathBuf> {
    let sc = sidecar(raw)?;
    let stem = raw.file_stem()?.to_string_lossy().to_lowercase();
    let dir = raw.parent()?;
    let raw_name = raw.file_name()?.to_os_string();
    // Can't list the folder completely: can't rule out a sibling, so keep the sidecar.
    let index = listings.by_stem(dir)?;
    let shared = index.get(&stem).is_some_and(|names| {
        names
            .iter()
            .any(|n| *n != raw_name && !matches!(dir.join(n).try_exists(), Ok(false)))
    });
    (!shared).then_some(sc)
}

/// Whether the file's containing folder exists -- distinguishes "the file is gone" from "the
/// drive isn't there".
fn parent_reachable(raw: &Path) -> bool {
    raw.parent().is_some_and(Path::is_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DeleteState, NewAsset, SqliteCatalog};
    use std::sync::Mutex;

    /// Moves files into a `bin` directory; can be told to leave some files behind or to report an
    /// error after doing so.
    struct FakeBin {
        bin: PathBuf,
        refuse: Vec<String>,
        calls: Arc<Mutex<Vec<usize>>>,
    }

    impl FakeBin {
        fn new(bin: &Path) -> Self {
            FakeBin {
                bin: bin.to_path_buf(),
                refuse: vec![],
                calls: Arc::new(Mutex::new(vec![])),
            }
        }
    }

    impl Trasher for FakeBin {
        fn trash_all(&self, paths: &[PathBuf]) -> Result<(), String> {
            self.calls.lock().unwrap().push(paths.len());
            let mut failed = None;
            for p in paths {
                let name = p.file_name().unwrap().to_string_lossy().into_owned();
                if self.refuse.iter().any(|r| name.starts_with(r)) {
                    failed = Some(format!("{name} is locked"));
                    continue;
                }
                std::fs::rename(p, self.bin.join(&name)).map_err(|e| e.to_string())?;
            }
            failed.map_or(Ok(()), Err)
        }
    }

    struct Fixture {
        dir: tempfile::TempDir,
        store: Arc<SqliteCatalog>,
        ids: Vec<i64>,
    }

    impl Fixture {
        fn root(&self) -> PathBuf {
            self.dir.path().join("shoot")
        }
        fn bin(&self) -> PathBuf {
            self.dir.path().join("bin")
        }
        fn shred(&self, mode: DeleteMode, ids: &[i64], trasher: FakeBin) -> Shred {
            Shred::new(self.store.clone(), mode, ids.to_vec(), Box::new(trasher))
        }
    }

    fn fixture(n: usize, with_sidecars: bool) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("shoot");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(dir.path().join("bin")).unwrap();
        let store = Arc::new(SqliteCatalog::open_in_memory().unwrap());
        let volume = store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = store.ensure_root(volume, &root.to_string_lossy()).unwrap();
        let ids = (0..n)
            .map(|i| {
                let name = format!("IMG_{i:04}.NEF");
                std::fs::write(root.join(&name), b"raw").unwrap();
                if with_sidecars {
                    std::fs::write(root.join(format!("IMG_{i:04}.xmp")), b"xmp").unwrap();
                }
                store
                    .insert_asset(
                        root_id,
                        &NewAsset {
                            rel_path: name.clone(),
                            rel_path_fold: name.to_lowercase(),
                            size_bytes: 3,
                            mtime_unix: 0,
                            fingerprint: None,
                            natural_key: None,
                            make: None,
                            model: None,
                            captured_at: None,
                            width: None,
                            height: None,
                            imported_at: 0,
                        },
                        None,
                    )
                    .unwrap()
            })
            .collect();
        Fixture { dir, store, ids }
    }

    fn run(mut shred: Shred) -> ShredOutcome {
        loop {
            if let Some(o) = shred.step() {
                return o;
            }
        }
    }

    fn done(o: ShredOutcome) -> ShredReport {
        match o {
            ShredOutcome::Done(r) => r,
            ShredOutcome::Failed { message, .. } => panic!("delete failed: {message}"),
        }
    }

    #[test]
    fn recycle_bin_mode_trashes_raw_and_sidecar_then_removes_the_rows() {
        let fx = fixture(3, true);
        let bin = FakeBin::new(&fx.bin());
        let report = done(run(fx.shred(DeleteMode::RecycleBin, &fx.ids[..2], bin)));

        assert_eq!(report.removed, 2);
        assert_eq!(report.trashed, 2);
        assert!(report.leftovers.is_empty() && report.sidecars_left.is_empty());
        assert_eq!(fx.store.asset_count().unwrap(), 1);
        assert!(fx.store.open_delete_items().unwrap().is_empty());
        // both RAWs and both sidecars are in the bin; the third photo is untouched.
        assert!(fx.bin().join("IMG_0000.NEF").exists() && fx.bin().join("IMG_0001.xmp").exists());
        assert!(fx.root().join("IMG_0002.NEF").exists() && fx.root().join("IMG_0002.xmp").exists());
        assert!(!fx.root().join("IMG_0000.NEF").exists());
    }

    #[test]
    fn catalog_only_mode_leaves_every_file_on_disk() {
        let fx = fixture(2, true);
        let bin = FakeBin::new(&fx.bin());
        let calls = bin.calls.clone();
        let report = done(run(fx.shred(DeleteMode::CatalogOnly, &fx.ids, bin)));

        assert_eq!((report.removed, report.trashed), (2, 0));
        assert_eq!(fx.store.asset_count().unwrap(), 0);
        assert!(calls.lock().unwrap().is_empty(), "the bin is never called");
        assert!(fx.root().join("IMG_0000.NEF").exists() && fx.root().join("IMG_0001.xmp").exists());
    }

    #[test]
    fn a_locked_file_stays_in_the_catalog_and_is_reported() {
        let fx = fixture(3, false);
        let mut bin = FakeBin::new(&fx.bin());
        bin.refuse = vec!["IMG_0001".into()];
        let report = done(run(fx.shred(DeleteMode::RecycleBin, &fx.ids, bin)));

        assert_eq!((report.removed, report.trashed), (2, 2));
        assert_eq!(report.leftovers.len(), 1);
        assert_eq!(report.leftovers[0].asset_id, fx.ids[1]);
        assert!(report.leftovers[0].reason.contains("locked"));
        assert_eq!(fx.store.asset_count().unwrap(), 1);
        assert!(fx.store.get_asset(fx.ids[1]).unwrap().is_some());
        assert!(
            fx.store.open_delete_items().unwrap().is_empty(),
            "the kept asset's journal row is dropped, not left to confuse recovery"
        );
        assert!(fx.root().join("IMG_0001.NEF").exists());
    }

    #[test]
    fn a_sidecar_that_cannot_be_trashed_is_reported_but_the_asset_still_goes() {
        let fx = fixture(1, true);
        let mut bin = FakeBin::new(&fx.bin());
        // refuse only the .xmp: names start with the stem, so match on the full sidecar name.
        bin.refuse = vec!["IMG_0000.xmp".into()];
        let report = done(run(fx.shred(DeleteMode::RecycleBin, &fx.ids, bin)));
        assert_eq!(report.removed, 1);
        assert_eq!(report.sidecars_left, vec![fx.root().join("IMG_0000.xmp")]);
    }

    #[test]
    fn a_file_already_gone_from_a_reachable_folder_just_loses_its_row() {
        let fx = fixture(2, false);
        std::fs::remove_file(fx.root().join("IMG_0000.NEF")).unwrap();
        let report = done(run(fx.shred(
            DeleteMode::RecycleBin,
            &fx.ids,
            FakeBin::new(&fx.bin()),
        )));
        assert_eq!(
            (report.removed, report.already_missing, report.trashed),
            (2, 1, 1)
        );
        assert_eq!(fx.store.asset_count().unwrap(), 0);
    }

    #[test]
    fn an_unreachable_folder_keeps_its_rows_instead_of_guessing() {
        let fx = fixture(2, false);
        std::fs::remove_dir_all(fx.root()).unwrap(); // the "drive" is gone
        let report = done(run(fx.shred(
            DeleteMode::RecycleBin,
            &fx.ids,
            FakeBin::new(&fx.bin()),
        )));
        assert_eq!(report.removed, 0);
        assert_eq!(report.leftovers.len(), 2);
        assert_eq!(fx.store.asset_count().unwrap(), 2);
    }

    #[test]
    fn a_catalog_path_that_escapes_its_folder_is_never_touched() {
        let fx = fixture(1, false);
        // A file OUTSIDE the registered folder, and three ways a bad rel_path could reach it.
        let outside = fx.dir.path().join("precious.NEF");
        std::fs::write(&outside, b"do not delete").unwrap();
        let volume = fx.store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = fx
            .store
            .ensure_root(volume, &fx.root().to_string_lossy())
            .unwrap();
        let new = |rel: &str| NewAsset {
            rel_path: rel.to_string(),
            rel_path_fold: rel.to_lowercase(),
            size_bytes: 1,
            mtime_unix: 0,
            fingerprint: None,
            natural_key: None,
            make: None,
            model: None,
            captured_at: None,
            width: None,
            height: None,
            imported_at: 0,
        };
        let bad: Vec<i64> = [
            outside.to_string_lossy().into_owned(), // absolute: join() would replace the root
            "../precious.NEF".to_string(),          // climbs out
            "sub/../../precious.NEF".to_string(),   // climbs out later
        ]
        .iter()
        .map(|rel| fx.store.insert_asset(root_id, &new(rel), None).unwrap())
        .collect();

        let report = done(run(fx.shred(
            DeleteMode::RecycleBin,
            &bad,
            FakeBin::new(&fx.bin()),
        )));
        assert_eq!(report.removed, 0);
        assert_eq!(report.leftovers.len(), 3);
        assert!(report.leftovers[0].reason.contains("outside its folder"));
        assert!(
            outside.exists(),
            "the file outside the folder was not trashed"
        );
        assert!(!fx.bin().join("precious.NEF").exists());
        for id in bad {
            assert!(
                fx.store.get_asset(id).unwrap().is_some(),
                "and no row was dropped"
            );
        }
    }

    #[test]
    fn only_clean_relative_paths_stay_inside_the_root() {
        for ok in ["a.NEF", "day1/a.NEF", "./a.NEF", "a b/c.NEF"] {
            assert!(stays_inside_root(ok), "{ok}");
        }
        for bad in ["", "/etc/passwd", "../a.NEF", "a/../../b.NEF"] {
            assert!(!stays_inside_root(bad), "{bad:?}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_drive_and_unc_paths_do_not_stay_inside_the_root() {
        for bad in ["C:\\x\\a.NEF", "\\\\srv\\share\\a.NEF", "\\a.NEF"] {
            assert!(!stays_inside_root(bad), "{bad:?}");
        }
    }

    #[test]
    fn catalog_only_mode_does_not_care_about_the_path_since_it_touches_no_file() {
        let fx = fixture(1, false);
        let volume = fx.store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = fx
            .store
            .ensure_root(volume, &fx.root().to_string_lossy())
            .unwrap();
        let odd = fx
            .store
            .insert_asset(
                root_id,
                &NewAsset {
                    rel_path: "../elsewhere.NEF".into(),
                    rel_path_fold: "../elsewhere.nef".into(),
                    size_bytes: 1,
                    mtime_unix: 0,
                    fingerprint: None,
                    natural_key: None,
                    make: None,
                    model: None,
                    captured_at: None,
                    width: None,
                    height: None,
                    imported_at: 0,
                },
                None,
            )
            .unwrap();
        let report = done(run(fx.shred(
            DeleteMode::CatalogOnly,
            &[odd],
            FakeBin::new(&fx.bin()),
        )));
        assert_eq!(
            report.removed, 1,
            "the row can still be removed from the catalog"
        );
    }

    #[test]
    fn unknown_ids_are_skipped_and_work_is_chunked() {
        let fx = fixture(CHUNK + 30, false);
        let bin = FakeBin::new(&fx.bin());
        let calls = bin.calls.clone();
        let mut ids = fx.ids.clone();
        ids.push(987_654);
        let mut shred = fx.shred(DeleteMode::RecycleBin, &ids, bin);
        // Prepare + two chunks; progress is monotonic and ends at the total.
        let mut steps = 0;
        let outcome = loop {
            steps += 1;
            if let Some(o) = shred.step() {
                break o;
            }
        };
        assert_eq!(steps, 4, "prepare, chunk, chunk, finish");
        let report = done(outcome);
        assert_eq!(report.removed, CHUNK + 30);
        assert_eq!(*calls.lock().unwrap(), vec![CHUNK, 30]);
    }

    #[test]
    fn on_removed_hook_sees_exactly_the_removed_ids() {
        let fx = fixture(3, false);
        let mut bin = FakeBin::new(&fx.bin());
        bin.refuse = vec!["IMG_0001".into()];
        let seen = Arc::new(Mutex::new(Vec::<i64>::new()));
        let seen2 = seen.clone();
        let shred = fx
            .shred(DeleteMode::RecycleBin, &fx.ids, bin)
            .on_removed(move |ids| seen2.lock().unwrap().extend_from_slice(ids));
        done(run(shred));
        assert_eq!(*seen.lock().unwrap(), vec![fx.ids[0], fx.ids[2]]);
    }

    #[test]
    fn a_catalog_error_reports_failed_with_the_partial_progress() {
        let fx = fixture(2, false);
        let mut shred = fx.shred(DeleteMode::RecycleBin, &fx.ids, FakeBin::new(&fx.bin()));
        assert!(shred.step().is_none(), "prepare resolves both assets");
        // The rows vanish underneath the run (another window, a sync): journaling a delete for a
        // missing asset violates the journal's foreign key, which must surface as `Failed`, not a
        // panic and not a hung job.
        fx.store.remove_assets(&fx.ids).unwrap();
        match shred.step() {
            Some(ShredOutcome::Failed { report, .. }) => assert_eq!(report.removed, 0),
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(
            fx.root().join("IMG_0000.NEF").exists(),
            "no file was touched"
        );
    }

    #[test]
    fn resume_finishes_trashed_rolls_back_pending_with_a_file_and_finishes_pending_without() {
        let fx = fixture(4, false);
        let raw = |i: usize| fx.root().join(format!("IMG_{i:04}.NEF"));
        let journal: Vec<(i64, String)> = (0..4)
            .map(|i| (fx.ids[i], raw(i).to_string_lossy().into_owned()))
            .collect();
        fx.store.begin_delete_items(&journal).unwrap();
        // 0: trashed (file already in the bin); 1: pending, file still on disk (never trashed);
        // 2: pending, file gone (crash between trash and journal update); 3: unreachable.
        fx.store.mark_delete_items_trashed(&[fx.ids[0]]).unwrap();
        std::fs::remove_file(raw(0)).unwrap();
        std::fs::remove_file(raw(2)).unwrap();
        fx.store.abandon_delete_items(&[fx.ids[3]]).unwrap();
        fx.store
            .begin_delete_items(&[(fx.ids[3], "/no/such/drive/IMG_0003.NEF".into())])
            .unwrap();

        let mut removed = Vec::new();
        let report =
            resume_open_deletes(fx.store.as_ref(), |ids| removed.extend_from_slice(ids)).unwrap();

        assert_eq!(
            report,
            ResumeReport {
                finished: 2,
                rolled_back: 1,
                stuck: 1
            }
        );
        removed.sort_unstable();
        assert_eq!(removed, vec![fx.ids[0], fx.ids[2]]);
        assert!(fx.store.get_asset(fx.ids[0]).unwrap().is_none());
        assert!(fx.store.get_asset(fx.ids[1]).unwrap().is_some());
        assert!(fx.store.get_asset(fx.ids[2]).unwrap().is_none());
        assert!(fx.store.get_asset(fx.ids[3]).unwrap().is_some());
        // Only the stuck row is left in the journal, and a second run changes nothing.
        assert_eq!(fx.store.open_delete_items().unwrap().len(), 1);
        let again = resume_open_deletes(fx.store.as_ref(), |_| {}).unwrap();
        assert_eq!(
            again,
            ResumeReport {
                finished: 0,
                rolled_back: 0,
                stuck: 1
            }
        );
    }

    // ---- regressions from the adversarial review of #32 --------------------------------------

    /// A trasher that "pulls the drive" (deletes the whole folder) and then reports an error,
    /// like a network share dropping mid-operation.
    struct YankDrive {
        root: PathBuf,
    }
    impl Trasher for YankDrive {
        fn trash_all(&self, _paths: &[PathBuf]) -> Result<(), String> {
            std::fs::remove_dir_all(&self.root).map_err(|e| e.to_string())?;
            Err("the device was removed".into())
        }
    }

    #[test]
    fn a_drive_pulled_during_the_trash_keeps_every_row_and_leaves_the_journal_pending() {
        let fx = fixture(3, false);
        let shred = Shred::new(
            fx.store.clone(),
            DeleteMode::RecycleBin,
            fx.ids.clone(),
            Box::new(YankDrive { root: fx.root() }),
        );
        let report = done(run(shred));

        assert_eq!(
            (report.removed, report.trashed),
            (0, 0),
            "nothing may be claimed trashed"
        );
        assert_eq!(report.leftovers.len(), 3);
        assert_eq!(
            fx.store.asset_count().unwrap(),
            3,
            "no catalog row was dropped"
        );
        let open = fx.store.open_delete_items().unwrap();
        assert_eq!(open.len(), 3);
        assert!(open.iter().all(|i| i.state == DeleteState::Pending));

        // Recovery with the drive still gone decides nothing...
        let r = resume_open_deletes(fx.store.as_ref(), |_| {}).unwrap();
        assert_eq!((r.finished, r.rolled_back, r.stuck), (0, 0, 3));
        // ...and when it comes back with one file present, that one rolls back and the two that
        // really are gone finish.
        std::fs::create_dir_all(fx.root()).unwrap();
        std::fs::write(fx.root().join("IMG_0001.NEF"), b"raw").unwrap();
        let r = resume_open_deletes(fx.store.as_ref(), |_| {}).unwrap();
        assert_eq!((r.finished, r.rolled_back, r.stuck), (2, 1, 0));
        assert!(fx.store.get_asset(fx.ids[1]).unwrap().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn a_stat_that_errors_is_not_mistaken_for_a_missing_file() {
        use std::os::unix::fs::PermissionsExt;
        let fx = fixture(2, false);
        let root = fx.root();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Running as root ignores permissions; then there is nothing to test.
        let can_still_read = std::fs::read_dir(&root).is_ok();
        let report = if can_still_read {
            None
        } else {
            Some(done(run(fx.shred(
                DeleteMode::RecycleBin,
                &fx.ids,
                FakeBin::new(&fx.bin()),
            ))))
        };
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        if let Some(report) = report {
            assert_eq!(
                report.removed, 0,
                "an unreadable folder is not 'already deleted'"
            );
            assert_eq!(report.already_missing, 0);
            assert_eq!(report.leftovers.len(), 2);
            assert_eq!(fx.store.asset_count().unwrap(), 2);
            assert!(fx.root().join("IMG_0000.NEF").exists());
        }
    }

    #[test]
    fn recovery_does_not_finish_a_delete_for_a_photo_that_now_lives_elsewhere() {
        let fx = fixture(1, false);
        // A stale `pending` row from an earlier run, pointing at a path in a folder that still
        // exists but no longer holds the file (the photo was moved since)...
        let old_dir = fx.dir.path().join("old_place");
        std::fs::create_dir_all(&old_dir).unwrap();
        fx.store
            .begin_delete_items(&[(
                fx.ids[0],
                old_dir.join("IMG_0000.NEF").to_string_lossy().into_owned(),
            )])
            .unwrap();
        // ...while the asset's current path holds a live file (fixture: shoot/IMG_0000.NEF).
        assert!(fx.root().join("IMG_0000.NEF").exists());

        let r = resume_open_deletes(fx.store.as_ref(), |_| {}).unwrap();
        assert_eq!((r.finished, r.rolled_back), (0, 1));
        assert!(fx.store.get_asset(fx.ids[0]).unwrap().is_some());
        assert!(fx.store.open_delete_items().unwrap().is_empty());
    }

    #[test]
    fn re_journaling_refreshes_a_stale_pending_path_but_never_a_trashed_one() {
        let fx = fixture(2, false);
        fx.store
            .begin_delete_items(&[
                (fx.ids[0], "/stale/a".into()),
                (fx.ids[1], "/stale/b".into()),
            ])
            .unwrap();
        fx.store.mark_delete_items_trashed(&[fx.ids[1]]).unwrap();
        fx.store
            .begin_delete_items(&[
                (fx.ids[0], "/fresh/a".into()),
                (fx.ids[1], "/fresh/b".into()),
            ])
            .unwrap();
        let open = fx.store.open_delete_items().unwrap();
        assert_eq!(open[0].abs_path, "/fresh/a");
        assert_eq!(
            open[1].abs_path, "/stale/b",
            "a trashed row is left exactly as it was"
        );
        assert_eq!(open[1].state, DeleteState::Trashed);
    }

    fn add_asset(fx: &Fixture, rel: &str) -> i64 {
        let volume = fx.store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = fx
            .store
            .ensure_root(volume, &fx.root().to_string_lossy())
            .unwrap();
        fx.store
            .insert_asset(
                root_id,
                &NewAsset {
                    rel_path: rel.to_string(),
                    rel_path_fold: rel.to_lowercase(),
                    size_bytes: 3,
                    mtime_unix: 0,
                    fingerprint: None,
                    natural_key: None,
                    make: None,
                    model: None,
                    captured_at: None,
                    width: None,
                    height: None,
                    imported_at: 0,
                },
                None,
            )
            .unwrap()
    }

    #[test]
    fn a_sidecar_shared_with_an_unselected_sibling_is_left_for_it() {
        let fx = fixture(1, true); // IMG_0000.NEF + IMG_0000.xmp
        std::fs::write(fx.root().join("IMG_0000.NRW"), b"raw2").unwrap();
        let sibling = add_asset(&fx, "IMG_0000.NRW");

        let report = done(run(fx.shred(
            DeleteMode::RecycleBin,
            &fx.ids,
            FakeBin::new(&fx.bin()),
        )));
        assert_eq!(report.trashed, 1);
        assert!(fx.bin().join("IMG_0000.NEF").exists());
        assert!(
            fx.root().join("IMG_0000.xmp").exists(),
            "the sibling still needs its sidecar"
        );
        assert!(
            report.sidecars_left.is_empty(),
            "keeping it is deliberate, not a failure"
        );

        // Once the sibling is deleted too, nobody needs it any more and it goes with it.
        let report = done(run(fx.shred(
            DeleteMode::RecycleBin,
            &[sibling],
            FakeBin::new(&fx.bin()),
        )));
        assert_eq!(report.trashed, 1);
        assert!(!fx.root().join("IMG_0000.xmp").exists());
        assert!(fx.bin().join("IMG_0000.xmp").exists());
    }

    #[test]
    fn a_sidecar_shared_by_two_photos_deleted_together_is_trashed_once() {
        let fx = fixture(1, true);
        std::fs::write(fx.root().join("IMG_0000.NRW"), b"raw2").unwrap();
        let sibling = add_asset(&fx, "IMG_0000.NRW");
        let both = vec![fx.ids[0], sibling];

        let report = done(run(fx.shred(
            DeleteMode::RecycleBin,
            &both,
            FakeBin::new(&fx.bin()),
        )));
        assert_eq!((report.removed, report.trashed), (2, 2));
        assert!(report.sidecars_left.is_empty() && report.leftovers.is_empty());
        assert!(fx.bin().join("IMG_0000.xmp").exists());
        assert!(!fx.root().join("IMG_0000.xmp").exists());
    }

    #[test]
    fn a_repeated_id_is_deleted_once_not_counted_twice_or_handed_to_the_bin_twice() {
        let fx = fixture(2, false);
        let dup = vec![fx.ids[0], fx.ids[0], fx.ids[1], fx.ids[0]];
        let report = done(run(fx.shred(
            DeleteMode::RecycleBin,
            &dup,
            FakeBin::new(&fx.bin()),
        )));
        assert_eq!((report.removed, report.trashed), (2, 2));
        assert!(report.leftovers.is_empty());
    }

    #[test]
    fn a_locked_raw_keeps_its_sidecar_while_an_unlocked_photo_loses_both() {
        let fx = fixture(2, true); // IMG_0000.NEF/.xmp and IMG_0001.NEF/.xmp
        let mut bin = FakeBin::new(&fx.bin());
        // Only the RAW is locked -- its .xmp is not (refuse matches by name prefix).
        bin.refuse = vec!["IMG_0001.NEF".into()];
        let report = done(run(fx.shred(DeleteMode::RecycleBin, &fx.ids, bin)));

        assert_eq!((report.removed, report.trashed), (1, 1));
        assert_eq!(report.leftovers.len(), 1);
        assert!(
            fx.root().join("IMG_0001.NEF").exists(),
            "the locked RAW stays"
        );
        assert!(
            fx.root().join("IMG_0001.xmp").exists(),
            "and its metadata must not be trashed out from under a photo that is still in the catalog"
        );
        assert!(fx.bin().join("IMG_0000.NEF").exists() && fx.bin().join("IMG_0000.xmp").exists());
        assert!(!fx.bin().join("IMG_0001.xmp").exists());
    }

    #[test]
    fn a_trashed_row_whose_file_was_restored_from_the_bin_keeps_its_photo() {
        let fx = fixture(2, false);
        let raw = |i: usize| fx.root().join(format!("IMG_{i:04}.NEF"));
        let journal: Vec<(i64, String)> = (0..2)
            .map(|i| (fx.ids[i], raw(i).to_string_lossy().into_owned()))
            .collect();
        fx.store.begin_delete_items(&journal).unwrap();
        fx.store.mark_delete_items_trashed(&fx.ids).unwrap();
        // Photo 0 really is in the bin; photo 1 was restored before the next startup.
        std::fs::remove_file(raw(0)).unwrap();
        assert!(raw(1).exists());

        let r = resume_open_deletes(fx.store.as_ref(), |_| {}).unwrap();
        assert_eq!((r.finished, r.rolled_back, r.stuck), (1, 1, 0));
        assert!(fx.store.get_asset(fx.ids[0]).unwrap().is_none());
        assert!(
            fx.store.get_asset(fx.ids[1]).unwrap().is_some(),
            "a photo whose file is back on disk must not lose its catalog row"
        );
        assert!(fx.store.open_delete_items().unwrap().is_empty());
    }

    /// Journals `abs_path` for `asset` in the given state and runs recovery once.
    fn recover(fx: &Fixture, asset: i64, abs_path: &str, trashed: bool) -> ResumeReport {
        fx.store
            .begin_delete_items(&[(asset, abs_path.to_string())])
            .unwrap();
        if trashed {
            fx.store.mark_delete_items_trashed(&[asset]).unwrap();
        }
        resume_open_deletes(fx.store.as_ref(), |_| {}).unwrap()
    }

    #[test]
    fn an_unknown_journaled_path_leaves_the_row_for_both_journal_states() {
        // The journaled path's folder is unreachable (the drive is unplugged). A `trashed` row
        // proves the file was binned at that moment, not that it is still gone -- it may have
        // been restored since -- so neither state may remove the row.
        for trashed in [false, true] {
            let fx = fixture(1, false);
            let r = recover(&fx, fx.ids[0], "/no/such/drive/IMG_0000.NEF", trashed);
            assert_eq!(
                (r.finished, r.rolled_back, r.stuck),
                (0, 0, 1),
                "trashed={trashed}"
            );
            assert!(
                fx.store.get_asset(fx.ids[0]).unwrap().is_some(),
                "trashed={trashed}: the photo's row must survive"
            );
            assert_eq!(fx.store.open_delete_items().unwrap().len(), 1);
        }
    }

    #[test]
    fn an_unknown_current_path_leaves_the_row_for_both_journal_states() {
        // The journaled path is reachable and the file is gone from it -- but the photo's
        // *current* location (its root was re-pointed by a move) is on a drive that is offline,
        // so nothing can be concluded about a copy there. Neither state may finish.
        for trashed in [false, true] {
            let fx = fixture(0, false);
            let volume = fx.store.upsert_volume("v", None, None, 0).unwrap();
            let offline = fx
                .store
                .ensure_root(volume, "/no/such/drive/shoot")
                .unwrap();
            let asset = fx
                .store
                .insert_asset(
                    offline,
                    &NewAsset {
                        rel_path: "IMG_0000.NEF".into(),
                        rel_path_fold: "img_0000.nef".into(),
                        size_bytes: 3,
                        mtime_unix: 0,
                        fingerprint: None,
                        natural_key: None,
                        make: None,
                        model: None,
                        captured_at: None,
                        width: None,
                        height: None,
                        imported_at: 0,
                    },
                    None,
                )
                .unwrap();
            let old_place = fx.root().join("IMG_0000.NEF"); // folder exists, file does not
            let r = recover(&fx, asset, &old_place.to_string_lossy(), trashed);
            assert_eq!(
                (r.finished, r.rolled_back, r.stuck),
                (0, 0, 1),
                "trashed={trashed}"
            );
            assert!(
                fx.store.get_asset(asset).unwrap().is_some(),
                "trashed={trashed}"
            );
        }
    }

    // ---- code-review round: sidecars decided from live disk state, one listing per folder ----

    #[test]
    fn a_shared_sidecar_stays_when_its_selected_sibling_could_not_be_trashed() {
        // IMG_0000.NEF and IMG_0000.NRW are BOTH selected and share IMG_0000.xmp; the NRW is
        // locked, so its row stays. The NEF goes -- but must not take the NRW's metadata.
        let fx = fixture(1, true);
        std::fs::write(fx.root().join("IMG_0000.NRW"), b"raw2").unwrap();
        let nrw = add_asset(&fx, "IMG_0000.NRW");
        let mut bin = FakeBin::new(&fx.bin());
        bin.refuse = vec!["IMG_0000.NRW".into()];

        let report = done(run(fx.shred(
            DeleteMode::RecycleBin,
            &[fx.ids[0], nrw],
            bin,
        )));
        assert_eq!((report.removed, report.trashed), (1, 1));
        assert!(fx.bin().join("IMG_0000.NEF").exists());
        assert!(
            fx.root().join("IMG_0000.NRW").exists(),
            "the locked sibling stays"
        );
        assert!(
            fx.root().join("IMG_0000.xmp").exists(),
            "and so does the sidecar it still needs"
        );
        assert!(fx.store.get_asset(nrw).unwrap().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_that_cannot_be_listed_keeps_the_sidecar() {
        use std::os::unix::fs::PermissionsExt;
        let fx = fixture(1, true);
        std::fs::write(fx.root().join("IMG_0000.NRW"), b"raw2").unwrap();
        let _sibling = add_asset(&fx, "IMG_0000.NRW"); // unselected, still in the catalog
        let root = fx.root();
        // Write + search but no read: stat and rename work, listing does not.
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o333)).unwrap();
        let can_list = std::fs::read_dir(&root).is_ok(); // root ignores permissions
        let report = if can_list {
            None
        } else {
            Some(done(run(fx.shred(
                DeleteMode::RecycleBin,
                &fx.ids,
                FakeBin::new(&fx.bin()),
            ))))
        };
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        if let Some(report) = report {
            assert_eq!(report.trashed, 1);
            assert!(
                fx.root().join("IMG_0000.xmp").exists(),
                "an unlistable folder must keep the sidecar: a sibling can't be ruled out"
            );
            assert!(report.sidecars_left.is_empty(), "keeping it is deliberate");
        }
    }

    #[test]
    fn a_folder_is_listed_once_for_the_whole_run_not_once_per_chunk() {
        let fx = fixture(CHUNK * 2 + 10, true);
        let mut shred = fx.shred(DeleteMode::RecycleBin, &fx.ids, FakeBin::new(&fx.bin()));
        let outcome = loop {
            if let Some(o) = shred.step() {
                break o;
            }
        };
        assert_eq!(done(outcome).trashed, CHUNK * 2 + 10);
        assert_eq!(
            shred.listings.reads, 1,
            "three chunks in one folder must share one directory read"
        );
        assert!(!fx.root().join("IMG_0000.xmp").exists());
    }

    #[test]
    fn photos_under_a_root_with_an_unfinished_move_are_left_alone() {
        let fx = fixture(2, true);
        let root_id = fx.store.list_roots().unwrap()[0].id;
        // An open move journal row for the root (a destination drive that went offline).
        fx.store
            .begin_root_move(root_id, "/archive/held", 0)
            .unwrap();

        let report = done(run(fx.shred(
            DeleteMode::RecycleBin,
            &fx.ids,
            FakeBin::new(&fx.bin()),
        )));
        assert_eq!(report.removed, 0);
        assert_eq!(report.leftovers.len(), 2);
        assert!(report.leftovers[0].reason.contains("unfinished move"));
        assert_eq!(fx.store.asset_count().unwrap(), 2);
        assert!(fx.root().join("IMG_0000.NEF").exists());
        // Catalog-only is held too: rows vanishing mid-move would confuse Carry's recovery.
        let report = done(run(fx.shred(
            DeleteMode::CatalogOnly,
            &fx.ids,
            FakeBin::new(&fx.bin()),
        )));
        assert_eq!(report.removed, 0);
    }

    #[test]
    fn a_sibling_trashed_after_the_folder_was_indexed_does_not_orphan_the_shared_sidecar() {
        // Chunk 1 (200 photos) builds the folder's stem index, which already lists both SHARED
        // files. Chunk 2 then deletes SHARED.NEF and SHARED.NRW together: after the trash neither
        // exists, so the shared sidecar must go. If the (stale) index alone decided, each would
        // see the other as a live sibling and the sidecar would be orphaned forever.
        let fx = fixture(CHUNK, true);
        std::fs::write(fx.root().join("SHARED.NEF"), b"a").unwrap();
        std::fs::write(fx.root().join("SHARED.NRW"), b"b").unwrap();
        std::fs::write(fx.root().join("SHARED.xmp"), b"x").unwrap();
        let a = add_asset(&fx, "SHARED.NEF");
        let b = add_asset(&fx, "SHARED.NRW");
        let mut ids = fx.ids.clone();
        ids.extend([a, b]);

        let report = done(run(fx.shred(
            DeleteMode::RecycleBin,
            &ids,
            FakeBin::new(&fx.bin()),
        )));
        assert_eq!(report.trashed, CHUNK + 2);
        assert!(report.sidecars_left.is_empty());
        assert!(
            !fx.root().join("SHARED.xmp").exists(),
            "the shared sidecar was orphaned by a stale folder index"
        );
        assert!(fx.bin().join("SHARED.xmp").exists());
    }
}
