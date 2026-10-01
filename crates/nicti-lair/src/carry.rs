//! Verified folder move (#26, ADR-0026): move a registered root to another location -- typically
//! another drive -- without ever holding the only copy of a file unverified.
//!
//! The protocol, in order (the source is **never touched before the catalog commit**):
//!
//! 1. *Preflight* and open a `root_move` journal row (`begin_root_move`).
//! 2. *Fast path*: `fs::rename` (atomic, no copy) -- works when source and destination share a
//!    volume. Any failure falls through to the copy path.
//! 3. *Copy path*: every file under the source (not just cataloged assets -- XMP sidecars and
//!    anything else in the folder come along) is streamed to `<name>.nicti-partial`, BLAKE3-hashed as it
//!    is read, `sync_all`'d, then **re-read from the destination disk and re-hashed**; only a match
//!    is renamed into place.
//! 4. *Commit*: one catalog transaction re-points the root and records each cataloged asset's
//!    full-file hash (`commit_root_move`).
//! 5. *Cleanup*: verified source files are deleted (skipping any whose size or content hash changed since
//!    they were copied), then now-empty source directories. A file that can't be removed (antivirus,
//!    Explorer) is reported as a leftover, not a failure -- the move already committed.
//!
//! A crash at any point is recoverable from the journal: see [`resume_open_moves`].
//!
//! Like `pounce_jobs::BackupJob`, [`Carry::step`] never returns `Err`: every failure becomes a
//! [`CarryOutcome`], so the job's report slot always fills in.

use std::collections::{HashMap, VecDeque};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use walkdir::WalkDir;

use crate::{CatalogStore, MoveState};

/// Bytes read/written per [`Carry::step`] on a single file (also the hash-verify read size), so a
/// cancel lands within one chunk even on a multi-GB file.
const CHUNK_BYTES: usize = 64 * 1024 * 1024;

/// At most this many leftover paths are named in a report (the count is always exact).
const MAX_LEFTOVERS_LISTED: usize = 20;

/// Suffix of in-progress copies. Deliberately unusual: a source file whose own name ends with it
/// is refused at walk time, and partials are also opened `create_new` and never renamed over an
/// existing file, so a name collision aborts the move (source untouched) instead of clobbering a
/// verified copy.
const PARTIAL_SUFFIX: &str = ".nicti-partial";

#[derive(Debug, Clone, Default)]
pub struct CarryOptions {
    /// Skip the `fs::rename` fast path and always copy + verify -- for tests (CI runs on one
    /// filesystem, where the fast path would otherwise mask the copy path).
    pub force_copy: bool,
    /// Moving onto an archive drive (#72, ADR-0072): before anything moves, write each asset's
    /// catalog T0 thumbnail as a `.thumb.jpg` sidecar in the source folder, so it travels with the
    /// folder. A sidecar that can't be written fails the move before it starts.
    pub export_sidecars: bool,
    /// Set the root's `archived` flag in the commit transaction (`None` leaves it alone).
    pub set_archived: Option<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CarryOutcome {
    /// The catalog now points at the destination.
    Moved {
        files: u64,
        bytes: u64,
        /// `true` if the whole folder moved by rename (no per-file hashes were recorded).
        renamed: bool,
        /// Source files that couldn't be (or shouldn't be) removed; the count is exact, the list
        /// is capped.
        leftovers: Vec<PathBuf>,
        leftover_count: u64,
    },
    /// Rejected before anything was copied.
    Refused(String),
    /// A copy's re-read hash didn't match its source. Nothing was committed; the destination
    /// copy is gone and the source is untouched.
    VerifyFailed { path: PathBuf },
    /// An I/O or catalog error. Nothing was committed unless the message says otherwise.
    Failed(String),
}

struct DoneFile {
    rel: PathBuf,
    hash: String,
    size: u64,
}

/// The file currently being copied / verified.
struct Current {
    rel: PathBuf,
    src_len: u64,
    src_mtime: Option<SystemTime>,
    stage: Stage,
    hasher: blake3::Hasher,
    partial: PathBuf,
    final_path: PathBuf,
}

enum Stage {
    Copy { src: File, dest: File },
    Verify { file: File, expected: String },
}

enum Phase {
    Start,
    /// Writes thumbnail sidecars into the source, one asset per step, then launches the move.
    Export {
        pending: VecDeque<crate::Asset>,
    },
    Copy {
        pending: VecDeque<PathBuf>,
        current: Option<Box<Current>>,
        done: Vec<DoneFile>,
        dirs: Vec<PathBuf>,
    },
    Commit {
        done: Vec<DoneFile>,
        dirs: Vec<PathBuf>,
        renamed: bool,
    },
    Cleanup {
        done: Vec<DoneFile>,
        dirs: Vec<PathBuf>,
        renamed: bool,
    },
    /// After the move is committed and cleaned up: fold thumbnails to the new tier, one asset per
    /// step (`tier::settle_asset`). Skipped when the options ask for no tier change.
    Settle {
        pending: VecDeque<crate::Asset>,
        archived: bool,
        renamed: bool,
    },
    Finished,
}

/// Test-only hook fired after a copy's destination is written, before it's re-read for verify.
#[cfg(test)]
pub(crate) type DestWrittenHook = Box<dyn FnMut(&Path) + Send>;

/// See [`Carry::set_archived_hook`].
pub type ArchivedHook = Box<dyn FnMut(&[i64]) + Send>;

pub struct Carry {
    store: Arc<dyn CatalogStore + Send + Sync>,
    root_id: i64,
    src: PathBuf,
    dest: PathBuf,
    opts: CarryOptions,
    now_unix: i64,
    move_id: Option<i64>,
    committed: bool,
    /// Set once the copy path has started creating files under `dest`: only then is `dest`
    /// ours to delete on cancel/panic. (Before that, `dest` may hold the renamed-over folder.)
    copy_started: bool,
    /// The fast-path `fs::rename` succeeded: the folder is at `dest` even though the catalog
    /// hasn't been told yet (a drop in that window commits it rather than leaving it split).
    renamed_landed: bool,
    leftovers: Vec<PathBuf>,
    leftover_count: u64,
    phase: Phase,
    total_files: Option<u64>,
    files_done: u64,
    bytes_done: u64,
    settle_report: crate::tier::SettleReport,
    /// Called once with the root's asset ids when it lands on an archive drive, so the app can
    /// drop their (now redundant) T2 cache entries.
    on_archived: Option<ArchivedHook>,
    #[cfg(test)]
    pub(crate) after_dest_written: Option<DestWrittenHook>,
}

impl Carry {
    /// Moves `root_id`'s folder to `<dest_parent>/<its own folder name>`.
    pub fn new(
        store: Arc<dyn CatalogStore + Send + Sync>,
        root_id: i64,
        dest_parent: &Path,
        opts: CarryOptions,
        now_unix: i64,
    ) -> Self {
        // Resolved lazily in `Start` (needs the catalog); placeholders until then.
        Carry {
            store,
            root_id,
            src: PathBuf::new(),
            dest: dest_parent.to_path_buf(),
            opts,
            now_unix,
            move_id: None,
            committed: false,
            copy_started: false,
            renamed_landed: false,
            leftovers: Vec::new(),
            leftover_count: 0,
            phase: Phase::Start,
            total_files: None,
            files_done: 0,
            bytes_done: 0,
            settle_report: Default::default(),
            on_archived: None,
            #[cfg(test)]
            after_dest_written: None,
        }
    }

    /// See [`Carry::on_archived`]'s field doc.
    pub fn set_archived_hook(&mut self, hook: ArchivedHook) {
        self.on_archived = Some(hook);
    }

    /// `true` once the catalog points at the destination (a dropped job's message differs).
    pub fn is_committed(&self) -> bool {
        self.committed || self.renamed_landed
    }

    pub fn label(&self) -> String {
        if self.src.as_os_str().is_empty() {
            "Move folder".to_string()
        } else {
            format!(
                "Move: {} \u{2192} {}",
                self.src.display(),
                self.dest.display()
            )
        }
    }

    /// `(files finished, total files)`; total is `None` until the source has been walked.
    pub fn progress(&self) -> (u64, Option<u64>) {
        (self.files_done, self.total_files)
    }

    /// Advances one chunk. `Some(outcome)` means the move is over; never call `step` after that.
    pub fn step(&mut self) -> Option<CarryOutcome> {
        let phase = std::mem::replace(&mut self.phase, Phase::Finished);
        match phase {
            Phase::Start => self.start(),
            Phase::Export { pending } => self.export_step(pending),
            Phase::Copy {
                pending,
                current,
                done,
                dirs,
            } => self.copy_step(pending, current, done, dirs),
            Phase::Commit {
                done,
                dirs,
                renamed,
            } => self.commit(done, dirs, renamed),
            Phase::Cleanup {
                done,
                dirs,
                renamed,
            } => self.cleanup(done, dirs, renamed),
            Phase::Settle {
                pending,
                archived,
                renamed,
            } => self.settle_step(pending, archived, renamed),
            Phase::Finished => panic!("Carry::step called again after it already finished"),
        }
    }

    // ---- start: preflight, journal, fast path, walk ----

    fn start(&mut self) -> Option<CarryOutcome> {
        let dest_parent = self.dest.clone();
        let src = match self.store.get_root_path(self.root_id) {
            Ok(Some(p)) => PathBuf::from(p),
            Ok(None) => return Some(CarryOutcome::Refused("folder is not registered".into())),
            Err(e) => return Some(CarryOutcome::Failed(e.to_string())),
        };
        let Some(name) = src.file_name() else {
            return Some(CarryOutcome::Refused(
                "can't move a drive root; pick a folder".into(),
            ));
        };
        self.src = src.clone();
        self.dest = dest_parent.join(name);
        if let Err(reason) = preflight(&self.src, &dest_parent, &self.dest) {
            return Some(CarryOutcome::Refused(reason));
        }

        if let Err(reason) = self.check_no_nested_roots() {
            return Some(CarryOutcome::Refused(reason));
        }

        let move_id = match self.store.begin_root_move(
            self.root_id,
            &self.dest.to_string_lossy(),
            self.now_unix,
        ) {
            Ok(id) => id,
            Err(e) => return Some(CarryOutcome::Refused(e.to_string())),
        };
        self.move_id = Some(move_id);

        if self.opts.export_sidecars {
            match self.store.list_assets_by_root(self.root_id) {
                Ok(assets) => {
                    self.total_files = None;
                    self.phase = Phase::Export {
                        pending: assets.into(),
                    };
                    return None;
                }
                Err(e) => return Some(self.abort_failed(e.to_string())),
            }
        }
        self.launch(move_id)
    }

    fn export_step(&mut self, mut pending: VecDeque<crate::Asset>) -> Option<CarryOutcome> {
        let move_id = self.move_id.expect("journal is open before export");
        for _ in 0..16 {
            let Some(asset) = pending.pop_front() else {
                return self.launch(move_id);
            };
            if let Err(e) = crate::tier::export_asset_sidecar(&*self.store, &self.src, &asset) {
                // Nothing has been copied, so `dest` (possibly a pre-existing empty folder the
                // user picked) is not ours to remove -- just close the journal.
                if let Some(id) = self.move_id.take() {
                    let _ = self.store.finish_root_move(id);
                }
                return Some(CarryOutcome::Failed(format!(
                    "writing thumbnail sidecar: {e}"
                )));
            }
        }
        self.phase = Phase::Export { pending };
        None
    }

    /// Rename fast path, else walk the source for the copy phase.
    fn launch(&mut self, move_id: i64) -> Option<CarryOutcome> {
        if !self.opts.force_copy {
            match self.try_rename(move_id) {
                Ok(true) => {
                    self.renamed_landed = true;
                    self.phase = Phase::Commit {
                        done: Vec::new(),
                        dirs: Vec::new(),
                        renamed: true,
                    };
                    return None;
                }
                Ok(false) => {}
                Err(msg) => return Some(self.abort_failed(msg)),
            }
        }
        self.copy_started = true;

        // Walk: every file, every directory. Symlinks are refused rather than followed or copied.
        let mut pending = VecDeque::new();
        let mut dirs = Vec::new();
        for entry in WalkDir::new(&self.src).min_depth(1) {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => return Some(self.abort_failed(format!("walking source: {e}"))),
            };
            let rel = entry
                .path()
                .strip_prefix(&self.src)
                .expect("walkdir entries are under the walked root")
                .to_path_buf();
            let ft = entry.file_type();
            if ft.is_symlink() {
                return Some(self.abort_refused(format!(
                    "{} is a symbolic link; moving links isn't supported",
                    rel.display()
                )));
            } else if ft.is_dir() {
                dirs.push(rel);
            } else if entry
                .file_name()
                .to_string_lossy()
                .ends_with(PARTIAL_SUFFIX)
            {
                return Some(self.abort_refused(format!(
                    "{} looks like an interrupted copy (ends in {PARTIAL_SUFFIX}); remove it first",
                    rel.display()
                )));
            } else {
                pending.push_back(rel);
            }
        }
        if let Err(e) = fs::create_dir_all(&self.dest) {
            return Some(self.abort_failed(format!("creating {}: {e}", self.dest.display())));
        }
        for d in &dirs {
            if let Err(e) = fs::create_dir_all(self.dest.join(d)) {
                return Some(self.abort_failed(format!("creating {}: {e}", d.display())));
            }
        }
        self.total_files = Some(pending.len() as u64);
        self.phase = Phase::Copy {
            pending,
            current: None,
            done: Vec::new(),
            dirs,
        };
        None
    }

    /// `true` if the whole folder moved by rename. The journal is flipped to `renaming` first so
    /// a crash right after the rename lands is recoverable without ever guessing which side holds
    /// the folder, and back to `copying` if the rename didn't happen.
    fn try_rename(&self, move_id: i64) -> Result<bool, String> {
        if self
            .store
            .set_root_move_state(move_id, MoveState::Renaming)
            .is_err()
        {
            return Ok(false);
        }
        // Windows refuses to rename onto an existing directory, even an empty one.
        let ok = !(self.dest.is_dir() && fs::remove_dir(&self.dest).is_err())
            && fs::rename(&self.src, &self.dest).is_ok();
        if !ok {
            // The copy path must run under a `copying` journal (a crash mid-copy under a stale
            // `renaming` row would strand the root); if we can't restore it, don't start.
            self.store
                .set_root_move_state(move_id, MoveState::Copying)
                .map_err(|e| format!("couldn't update the move journal: {e}"))?;
        }
        Ok(ok)
    }

    /// A move rewrites only the moved root's own path, so a registered root nested inside (or
    /// containing) the source or destination would be orphaned or double-imported. Compared both
    /// as-is and case-folded (NTFS).
    fn check_no_nested_roots(&self) -> Result<(), String> {
        let roots = self.store.list_roots().map_err(|e| e.to_string())?;
        let fold = |p: &Path| PathBuf::from(p.to_string_lossy().to_lowercase());
        // Canonicalize (resolving `..`, symlinks/junctions, `\\?\` prefixes) with the raw path
        // as fallback; the destination doesn't exist yet, so canonicalize its parent.
        let canon = |p: &Path| fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let dest_canon = match (self.dest.parent(), self.dest.file_name()) {
            (Some(parent), Some(name)) => canon(parent).join(name),
            _ => self.dest.clone(),
        };
        let mine_paths = [canon(&self.src), dest_canon];
        for root in roots.into_iter().filter(|r| r.id != self.root_id) {
            let other = canon(Path::new(&root.path));
            for mine in &mine_paths {
                let nested = other.starts_with(mine)
                    || mine.starts_with(&other)
                    || fold(&other).starts_with(fold(mine))
                    || fold(mine).starts_with(fold(&other));
                if nested {
                    return Err(format!(
                        "{} is registered separately and overlaps {}; move or remove it first",
                        other.display(),
                        mine.display()
                    ));
                }
            }
        }
        Ok(())
    }

    // ---- copy + verify ----

    fn copy_step(
        &mut self,
        mut pending: VecDeque<PathBuf>,
        current: Option<Box<Current>>,
        mut done: Vec<DoneFile>,
        dirs: Vec<PathBuf>,
    ) -> Option<CarryOutcome> {
        let mut cur = match current {
            Some(c) => c,
            None => match pending.pop_front() {
                Some(rel) => match self.open_current(rel) {
                    Ok(c) => Box::new(c),
                    Err(msg) => return Some(self.abort_failed(msg)),
                },
                None => {
                    // Durability barrier: the renames into place must survive a power cut
                    // before the commit lets cleanup start deleting source files.
                    if let Err(e) = sync_dirs(&self.dest, &dirs) {
                        return Some(self.abort_failed(format!("syncing destination: {e}")));
                    }
                    self.phase = Phase::Commit {
                        done,
                        dirs,
                        renamed: false,
                    };
                    return None;
                }
            },
        };

        match self.advance(&mut cur) {
            Ok(Advance::More) => {}
            Ok(Advance::Finished(size)) => {
                self.files_done += 1;
                self.bytes_done += size;
                done.push(DoneFile {
                    rel: cur.rel.clone(),
                    hash: cur.hasher.finalize().to_hex().to_string(),
                    size,
                });
                self.phase = Phase::Copy {
                    pending,
                    current: None,
                    done,
                    dirs,
                };
                return None;
            }
            Ok(Advance::Mismatch) => {
                let path = self.src.join(&cur.rel);
                let _ = fs::remove_file(&cur.partial);
                self.discard_dest();
                return Some(CarryOutcome::VerifyFailed { path });
            }
            Err(msg) => {
                let _ = fs::remove_file(&cur.partial);
                return Some(self.abort_failed(msg));
            }
        }
        self.phase = Phase::Copy {
            pending,
            current: Some(cur),
            done,
            dirs,
        };
        None
    }

    fn open_current(&self, rel: PathBuf) -> Result<Current, String> {
        let src_path = self.src.join(&rel);
        let src = File::open(&src_path).map_err(|e| format!("opening {}: {e}", rel.display()))?;
        let meta = src
            .metadata()
            .map_err(|e| format!("stat {}: {e}", rel.display()))?;
        let final_path = self.dest.join(&rel);
        let mut partial_name = final_path
            .file_name()
            .expect("a walked file has a name")
            .to_os_string();
        partial_name.push(PARTIAL_SUFFIX);
        let partial = final_path.with_file_name(partial_name);
        // `create_new`: never truncate something already there (see `PARTIAL_SUFFIX`).
        let dest = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)
            .map_err(|e| format!("creating {}: {e}", partial.display()))?;
        Ok(Current {
            rel,
            src_len: meta.len(),
            src_mtime: meta.modified().ok(),
            stage: Stage::Copy { src, dest },
            hasher: blake3::Hasher::new(),
            partial,
            final_path,
        })
    }

    /// One chunk of copy or verify work on `cur`.
    fn advance(&mut self, cur: &mut Current) -> Result<Advance, String> {
        let mut buf = vec![0u8; 1024 * 1024];
        match &mut cur.stage {
            Stage::Copy { src, dest } => {
                let mut budget = CHUNK_BYTES;
                while budget > 0 {
                    let want = buf.len().min(budget);
                    let n = src
                        .read(&mut buf[..want])
                        .map_err(|e| format!("reading {}: {e}", cur.rel.display()))?;
                    if n == 0 {
                        // EOF: finish the write, then flip to verifying from the disk.
                        if let Some(m) = cur.src_mtime {
                            let _ = dest.set_modified(m);
                        }
                        dest.flush()
                            .and_then(|_| dest.sync_all())
                            .map_err(|e| format!("writing {}: {e}", cur.partial.display()))?;
                        #[cfg(test)]
                        if let Some(hook) = self.after_dest_written.as_mut() {
                            hook(&cur.partial);
                        }
                        let expected = cur.hasher.finalize().to_hex().to_string();
                        let file = File::open(&cur.partial)
                            .map_err(|e| format!("re-opening {}: {e}", cur.partial.display()))?;
                        cur.stage = Stage::Verify { file, expected };
                        // `hasher` now re-hashes the destination bytes; keep the source's hash in
                        // `expected` and restart the hasher.
                        cur.hasher = blake3::Hasher::new();
                        return Ok(Advance::More);
                    }
                    cur.hasher.update(&buf[..n]);
                    dest.write_all(&buf[..n])
                        .map_err(|e| format!("writing {}: {e}", cur.partial.display()))?;
                    budget -= n;
                }
                Ok(Advance::More)
            }
            Stage::Verify { file, expected } => {
                let mut budget = CHUNK_BYTES;
                while budget > 0 {
                    let want = buf.len().min(budget);
                    let n = file
                        .read(&mut buf[..want])
                        .map_err(|e| format!("re-reading {}: {e}", cur.partial.display()))?;
                    if n == 0 {
                        let actual = cur.hasher.finalize().to_hex().to_string();
                        if actual != *expected {
                            return Ok(Advance::Mismatch);
                        }
                        // Keep the verified hash in the hasher slot for the caller's `finalize`.
                        let dest_len = fs::metadata(&cur.partial)
                            .map_err(|e| format!("stat {}: {e}", cur.partial.display()))?
                            .len();
                        if dest_len != cur.src_len {
                            // The source changed size mid-copy; the copy is internally consistent
                            // but not a faithful copy of the file we walked.
                            return Ok(Advance::Mismatch);
                        }
                        // `rename` silently replaces an existing file: a same-name (or, on a
                        // case-insensitive destination, differently-cased) sibling already copied
                        // would be destroyed. Abort instead; the source is untouched.
                        if fs::symlink_metadata(&cur.final_path).is_ok() {
                            return Err(format!(
                                "{} already exists in the destination (name collision)",
                                cur.final_path.display()
                            ));
                        }
                        fs::rename(&cur.partial, &cur.final_path).map_err(|e| {
                            format!("renaming into {}: {e}", cur.final_path.display())
                        })?;
                        return Ok(Advance::Finished(dest_len));
                    }
                    cur.hasher.update(&buf[..n]);
                    budget -= n;
                }
                Ok(Advance::More)
            }
        }
    }

    // ---- commit + cleanup ----

    fn commit(
        &mut self,
        done: Vec<DoneFile>,
        dirs: Vec<PathBuf>,
        renamed: bool,
    ) -> Option<CarryOutcome> {
        let move_id = self.move_id.expect("journal is open before commit");
        let mut hashes = Vec::new();
        if !renamed {
            let by_rel: HashMap<String, &str> = done
                .iter()
                .map(|d| (rel_key(&d.rel), d.hash.as_str()))
                .collect();
            match self.store.list_assets_by_root(self.root_id) {
                Ok(assets) => {
                    for a in assets {
                        if let Some(h) = by_rel.get(&a.rel_path) {
                            hashes.push((a.id, (*h).to_string()));
                        }
                    }
                }
                Err(e) => return Some(self.abort_failed(e.to_string())),
            }
        }
        if let Err(e) =
            self.store
                .commit_root_move_archived(move_id, &hashes, self.opts.set_archived)
        {
            if renamed {
                // The folder already moved on disk but the catalog didn't follow: leave the
                // journal open so `resume_open_moves` finishes the commit on next start.
                self.move_id = None;
                return Some(CarryOutcome::Failed(format!(
                    "folder moved but catalog update failed (will retry on next start): {e}"
                )));
            }
            return Some(self.abort_failed(e.to_string()));
        }
        self.committed = true;
        self.phase = Phase::Cleanup {
            done,
            dirs,
            renamed,
        };
        None
    }

    /// One source file per step (each is re-read and re-hashed, so a whole-folder cleanup must not
    /// be a single uncancellable step); the last step prunes directories and closes the journal.
    fn cleanup(
        &mut self,
        mut done: Vec<DoneFile>,
        dirs: Vec<PathBuf>,
        renamed: bool,
    ) -> Option<CarryOutcome> {
        if !renamed {
            if let Some(d) = done.pop() {
                let p = self.src.join(&d.rel);
                // Only delete what is still exactly what we copied, and only while the verified
                // copy is still there: size is a cheap pre-filter, the re-hash is the real guard
                // (mtime is too coarse on FAT/exFAT to see a same-size rewrite).
                let twin_ok = fs::metadata(self.dest.join(&d.rel))
                    .map(|m| m.len() == d.size)
                    .unwrap_or(false);
                let unchanged = twin_ok
                    && fs::metadata(&p).map(|m| m.len() == d.size).unwrap_or(false)
                    && hash_file(&p).as_deref() == Some(d.hash.as_str());
                if !unchanged || fs::remove_file(&p).is_err() {
                    self.leftover_count += 1;
                    if self.leftovers.len() < MAX_LEFTOVERS_LISTED {
                        self.leftovers.push(p);
                    }
                }
                self.phase = Phase::Cleanup {
                    done,
                    dirs,
                    renamed,
                };
                return None;
            }
            let mut dirs = dirs;
            dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
            for d in dirs {
                let _ = fs::remove_dir(self.src.join(d));
            }
            let _ = fs::remove_dir(&self.src);
        }
        if let Some(archived) = self.opts.set_archived {
            // Keep the journal open through settling: a crash here resumes as "committed, clean
            // up", and the startup reconcile re-runs the (idempotent) settle.
            // Best effort: on a catalog error skip settling; the startup reconcile covers it.
            if let Ok(assets) = self.store.list_assets_by_root(self.root_id) {
                if archived {
                    if let Some(hook) = self.on_archived.as_mut() {
                        let ids: Vec<i64> = assets.iter().map(|a| a.id).collect();
                        hook(&ids);
                    }
                }
                self.phase = Phase::Settle {
                    pending: assets.into(),
                    archived,
                    renamed,
                };
                return None;
            }
        }
        Some(self.finish_moved(renamed))
    }

    fn settle_step(
        &mut self,
        mut pending: VecDeque<crate::Asset>,
        archived: bool,
        renamed: bool,
    ) -> Option<CarryOutcome> {
        for _ in 0..16 {
            let Some(asset) = pending.pop_front() else {
                return Some(self.finish_moved(renamed));
            };
            // Best effort: a failure leaves the other tier in place, which the read path
            // still serves; the startup reconcile retries.
            let _ = crate::tier::settle_asset(
                &*self.store,
                &self.dest,
                archived,
                &asset,
                &mut self.settle_report,
            );
        }
        self.phase = Phase::Settle {
            pending,
            archived,
            renamed,
        };
        None
    }

    fn finish_moved(&mut self, renamed: bool) -> CarryOutcome {
        if let Some(id) = self.move_id.take() {
            let _ = self.store.finish_root_move(id);
        }
        self.phase = Phase::Finished;
        CarryOutcome::Moved {
            files: self.files_done,
            bytes: self.bytes_done,
            renamed,
            leftovers: std::mem::take(&mut self.leftovers),
            leftover_count: self.leftover_count,
        }
    }

    // ---- abort paths (before commit only) ----

    fn discard_dest(&mut self) {
        let _ = fs::remove_dir_all(&self.dest);
        if let Some(id) = self.move_id.take() {
            let _ = self.store.finish_root_move(id);
        }
    }

    fn abort_failed(&mut self, msg: String) -> CarryOutcome {
        self.discard_dest();
        CarryOutcome::Failed(msg)
    }

    fn abort_refused(&mut self, msg: String) -> CarryOutcome {
        self.discard_dest();
        CarryOutcome::Refused(msg)
    }
}

/// A cancelled job (Pounce just stops stepping it), or one that panicked mid-`step`, is dropped
/// with the move unfinished (`move_id` still set -- every normal exit clears it):
/// - mid-copy, before the commit: discard the half-built destination and close the journal;
/// - after the commit: finish what cleanup would have done (delete only source files whose bytes
///   match the destination) and close the journal;
/// - mid-`fs::rename` window: leave the journal open for [`resume_open_moves`], which never
///   guesses which side holds the folder.
impl Drop for Carry {
    fn drop(&mut self) {
        let Some(id) = self.move_id else { return };
        if !self.committed && self.renamed_landed {
            // The folder already moved by rename: point the catalog at it now rather than leave
            // the two split until the next start. On failure the `renaming` row stays for resume.
            if self
                .store
                .commit_root_move_archived(id, &[], self.opts.set_archived)
                .is_ok()
            {
                let _ = self.store.finish_root_move(id);
            }
        } else if self.committed {
            cleanup_matching(&self.src, &self.dest);
            // Cancelled mid-settle: leave the journal open so the next start's recovery reports
            // the root and re-runs the (idempotent) thumbnail settle for it.
            if !matches!(self.phase, Phase::Settle { .. }) {
                let _ = self.store.finish_root_move(id);
            }
        } else if self.copy_started {
            self.discard_dest();
        } else {
            // Cancelled while exporting sidecars (nothing moved yet): close the journal. The
            // sidecars stay; `tier::settle_root` folds them back into the catalog.
            let _ = self.store.finish_root_move(id);
        }
    }
}

enum Advance {
    More,
    Finished(u64),
    Mismatch,
}

fn rel_key(rel: &Path) -> String {
    use unicode_normalization::UnicodeNormalization;
    let s: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    s.join("/").nfc().collect()
}

fn preflight(src: &Path, dest_parent: &Path, dest: &Path) -> Result<(), String> {
    if !src.is_dir() {
        return Err(format!("{} doesn't exist or isn't a folder", src.display()));
    }
    if !dest_parent.is_dir() {
        return Err(format!(
            "destination {} doesn't exist or isn't a folder",
            dest_parent.display()
        ));
    }
    let canon_src = fs::canonicalize(src).map_err(|e| format!("resolving source: {e}"))?;
    let canon_parent =
        fs::canonicalize(dest_parent).map_err(|e| format!("resolving destination: {e}"))?;
    if canon_parent.starts_with(&canon_src) {
        return Err("can't move a folder into itself".into());
    }
    if canon_parent == fs::canonicalize(src.parent().unwrap_or(src)).unwrap_or_default() {
        return Err("that's where the folder already is".into());
    }
    if dest.exists() {
        let empty = dest.is_dir()
            && fs::read_dir(dest)
                .map(|mut d| d.next().is_none())
                .unwrap_or(false);
        if !empty {
            return Err(format!("{} already exists and isn't empty", dest.display()));
        }
    }
    Ok(())
}

/// What [`resume_open_moves`] did for one journal row.
#[derive(Debug, Clone, PartialEq)]
pub enum Resumed {
    /// Crashed mid-copy: the half-built destination was discarded; the source was never touched.
    RolledBack { root_id: i64 },
    /// The folder had already moved by rename; the catalog commit was completed.
    Committed { root_id: i64 },
    /// The catalog was already re-pointed; leftover source files that match the destination
    /// byte-for-byte were removed.
    CleanedUp { root_id: i64, leftover_count: u64 },
    /// Neither location holds the folder; the journal was left open for a human to look at.
    Stuck { root_id: i64, reason: String },
}

/// Crash recovery: call once at startup, before any import/sync. Idempotent.
pub fn resume_open_moves(store: &dyn CatalogStore) -> Vec<Resumed> {
    let moves = match store.open_root_moves() {
        Ok(m) => m,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for m in moves {
        let src = PathBuf::from(&m.src_path);
        let dest = PathBuf::from(&m.dest_path);
        match m.state {
            MoveState::Copying => {
                // `Copying` means the fast-path rename was never attempted (or was reverted), so
                // `dest` only ever holds our own half-built copy.
                if src.is_dir() && !dest_covered_by_src(&src, &dest) {
                    // The destination holds something the source doesn't: e.g. the commit and the
                    // source cleanup both happened but the (WAL, non-fsynced) commit was lost to a
                    // power cut. Deleting `dest` could delete the only copy.
                    out.push(Resumed::Stuck {
                        root_id: m.root_id,
                        reason: "the destination has files the source no longer has; not \
                                 deleting it -- check both folders"
                            .into(),
                    });
                } else if src.is_dir() {
                    let _ = fs::remove_dir_all(&dest);
                    let _ = store.finish_root_move(m.id);
                    out.push(Resumed::RolledBack { root_id: m.root_id });
                } else {
                    out.push(Resumed::Stuck {
                        root_id: m.root_id,
                        reason: "neither the source nor the destination folder exists".into(),
                    });
                }
            }
            MoveState::Renaming => {
                // Never delete either side here: the rename may or may not have landed, and the
                // source path may have been recreated since.
                match (src.is_dir(), dest.is_dir()) {
                    (false, true) if dir_is_empty(&dest) => out.push(Resumed::Stuck {
                        root_id: m.root_id,
                        reason: "the source is missing and the destination is empty; not \
                                 re-pointing the catalog at an empty folder"
                            .into(),
                    }),
                    (false, true) => {
                        match store
                            .commit_root_move(m.id, &[])
                            .and_then(|_| store.finish_root_move(m.id))
                        {
                            Ok(()) => out.push(Resumed::Committed { root_id: m.root_id }),
                            Err(e) => out.push(Resumed::Stuck {
                                root_id: m.root_id,
                                reason: e.to_string(),
                            }),
                        }
                    }
                    (true, false) => {
                        // The rename never happened; nothing moved.
                        let _ = store.finish_root_move(m.id);
                        out.push(Resumed::RolledBack { root_id: m.root_id });
                    }
                    (true, true) if dir_is_empty(&dest) => {
                        // The rename never landed; `dest` is just the empty folder we emptied.
                        let _ = store.finish_root_move(m.id);
                        out.push(Resumed::RolledBack { root_id: m.root_id });
                    }
                    (true, true) => out.push(Resumed::Stuck {
                        root_id: m.root_id,
                        reason: "the folder exists at both locations after an interrupted \
                                 rename; check both before continuing"
                            .into(),
                    }),
                    (false, false) => out.push(Resumed::Stuck {
                        root_id: m.root_id,
                        reason: "neither the source nor the destination folder exists".into(),
                    }),
                }
            }
            MoveState::Committed => {
                let leftover_count = cleanup_matching(&src, &dest);
                let _ = store.finish_root_move(m.id);
                out.push(Resumed::CleanedUp {
                    root_id: m.root_id,
                    leftover_count,
                });
            }
        }
    }
    out
}

/// Deletes every file under `src` whose counterpart under `dest` has identical content, then
/// prunes empty directories. Returns how many source files remain.
fn cleanup_matching(src: &Path, dest: &Path) -> u64 {
    if !src.is_dir() {
        return 0;
    }
    let mut remaining = 0u64;
    let mut dirs = Vec::new();
    for entry in WalkDir::new(src).min_depth(1).into_iter().flatten() {
        if entry.file_type().is_dir() {
            dirs.push(entry.path().to_path_buf());
            continue;
        }
        let rel = entry.path().strip_prefix(src).expect("under src");
        let twin = dest.join(rel);
        let same = match (hash_file(entry.path()), hash_file(&twin)) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        };
        if !(same && fs::remove_file(entry.path()).is_ok()) {
            remaining += 1;
        }
    }
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    for d in dirs {
        let _ = fs::remove_dir(d);
    }
    let _ = fs::remove_dir(src);
    remaining
}

fn hash_file(p: &Path) -> Option<String> {
    let mut f = File::open(p).ok()?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            return Some(h.finalize().to_hex().to_string());
        }
        h.update(&buf[..n]);
    }
}

/// fsyncs `dest` and every directory under it, so the directory entries created by the renames
/// into place are durable (a file's own `sync_all` doesn't persist its directory entry on POSIX).
/// Windows can't open a directory as a plain `File`; NTFS journals metadata, so this is a no-op
/// there (documented limit).
fn sync_dirs(dest: &Path, dirs: &[PathBuf]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        for d in dirs
            .iter()
            .map(|d| dest.join(d))
            .chain(std::iter::once(dest.to_path_buf()))
        {
            File::open(&d)?.sync_all()?;
        }
    }
    #[cfg(not(unix))]
    let _ = (dest, dirs);
    Ok(())
}

fn dir_is_empty(p: &Path) -> bool {
    fs::read_dir(p)
        .map(|mut d| d.next().is_none())
        .unwrap_or(false)
}

/// `true` if every file under `dest` (other than our own temp copies) also exists under `src`
/// with the same size -- i.e. discarding `dest` can't lose anything `src` doesn't still have.
fn dest_covered_by_src(src: &Path, dest: &Path) -> bool {
    if !dest.is_dir() {
        return true;
    }
    for entry in WalkDir::new(dest).min_depth(1).into_iter().flatten() {
        if !entry.file_type().is_file()
            || entry
                .file_name()
                .to_string_lossy()
                .ends_with(PARTIAL_SUFFIX)
        {
            continue;
        }
        let rel = entry.path().strip_prefix(dest).expect("under dest");
        let same = match (fs::metadata(entry.path()), fs::metadata(src.join(rel))) {
            (Ok(a), Ok(b)) => a.len() == b.len(),
            _ => false,
        };
        if !same {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NewAsset, SqliteCatalog};

    struct Fixture {
        _tmp: tempfile::TempDir,
        src_parent: PathBuf,
        dest_parent: PathBuf,
        src: PathBuf,
        cat: Arc<SqliteCatalog>,
        root_id: i64,
        asset_id: i64,
    }

    fn new_asset(rel: &str) -> NewAsset {
        NewAsset {
            rel_path: rel.to_string(),
            rel_path_fold: rel.to_lowercase(),
            size_bytes: 0,
            mtime_unix: 0,
            fingerprint: None,
            natural_key: None,
            make: Some("NIKON".into()),
            model: Some("Z8".into()),
            captured_at: None,
            width: None,
            height: None,
            imported_at: 0,
        }
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let src_parent = tmp.path().join("ssd");
        let dest_parent = tmp.path().join("archive");
        let src = src_parent.join("event-2026");
        fs::create_dir_all(src.join("day1")).unwrap();
        fs::create_dir_all(src.join("empty-dir")).unwrap();
        fs::create_dir_all(&dest_parent).unwrap();
        fs::write(src.join("a.NEF"), vec![7u8; 3_000_000]).unwrap();
        fs::write(src.join("a.xmp"), b"<xmp/>").unwrap();
        fs::write(src.join("day1/b.NEF"), vec![9u8; 100]).unwrap();
        let cat = Arc::new(SqliteCatalog::open_in_memory().unwrap());
        let vol = cat.upsert_volume("test-vol", None, None, 0).unwrap();
        let root_id = cat.ensure_root(vol, &src.to_string_lossy()).unwrap();
        let asset_id = cat
            .insert_asset(root_id, &new_asset("a.NEF"), None)
            .unwrap();
        cat.insert_asset(root_id, &new_asset("day1/b.NEF"), None)
            .unwrap();
        cat.set_rating(&[asset_id], Some(4)).unwrap();
        Fixture {
            _tmp: tmp,
            src_parent,
            dest_parent,
            src,
            cat,
            root_id,
            asset_id,
        }
    }

    fn carry(f: &Fixture, force_copy: bool) -> Carry {
        Carry::new(
            f.cat.clone(),
            f.root_id,
            &f.dest_parent,
            CarryOptions {
                force_copy,
                ..Default::default()
            },
            1,
        )
    }

    fn run(c: &mut Carry) -> CarryOutcome {
        loop {
            if let Some(o) = c.step() {
                return o;
            }
        }
    }

    fn no_journal(f: &Fixture) {
        assert!(f.cat.open_root_moves().unwrap().is_empty());
    }

    fn fake_jpeg(tag: u8) -> crate::Preview {
        let mut b = vec![
            0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x0B, 8, 0, 30, 0, 40, 1, 1, 0x11, 0,
        ];
        b.extend([tag; 64]);
        b.extend([0xFF, 0xD9]);
        crate::Preview {
            width: Some(40),
            height: Some(30),
            bytes: b,
        }
    }

    /// The #72 exit criterion, both move paths: archive exports sidecars that travel with the
    /// folder and drops the catalog blobs; moving back restores byte-identical blobs and removes
    /// the sidecars.
    #[test]
    fn archive_then_reactivate_round_trips_thumbnails() {
        for force_copy in [true, false] {
            let f = fixture();
            let pv_a = fake_jpeg(1);
            let pv_b = fake_jpeg(2);
            let b_id = f
                .cat
                .find_asset_by_path(f.root_id, "day1/b.NEF")
                .unwrap()
                .unwrap()
                .id;
            f.cat
                .put_preview(f.asset_id, crate::PreviewTier::T0, &pv_a)
                .unwrap();
            f.cat
                .put_preview(b_id, crate::PreviewTier::T0, &pv_b)
                .unwrap();

            let mut c = Carry::new(
                f.cat.clone(),
                f.root_id,
                &f.dest_parent,
                CarryOptions {
                    force_copy,
                    export_sidecars: true,
                    set_archived: Some(true),
                },
                1,
            );
            let CarryOutcome::Moved { renamed, .. } = run(&mut c) else {
                panic!("archive move failed");
            };
            assert_eq!(renamed, !force_copy, "expected path taken");
            let dest = f.dest_parent.join("event-2026");
            assert_eq!(
                fs::read(dest.join("a.NEF.thumb.jpg")).unwrap(),
                pv_a.bytes,
                "force_copy={force_copy}"
            );
            assert!(dest.join("day1/b.NEF.thumb.jpg").exists());
            let root = f.cat.list_roots().unwrap().remove(0);
            assert!(root.archived);

            // Carry settled the tier itself: blobs gone, sidecars kept.
            assert!(f
                .cat
                .get_preview(f.asset_id, crate::PreviewTier::T0)
                .unwrap()
                .is_none());
            // The read path serves the sidecar with the catalog blob gone.
            let a = f.cat.get_asset(f.asset_id).unwrap().unwrap();
            let got = crate::tier::load_t0(&*f.cat, Path::new(&root.path), true, &a)
                .unwrap()
                .unwrap();
            assert_eq!(got.bytes, pv_a.bytes);
            let by_id = crate::tier::load_t0_by_id(&*f.cat, f.asset_id)
                .unwrap()
                .unwrap();
            assert_eq!(
                by_id.bytes, pv_a.bytes,
                "id-based read path serves the sidecar"
            );

            // Back to the SSD.
            let mut c = Carry::new(
                f.cat.clone(),
                f.root_id,
                &f.src_parent,
                CarryOptions {
                    force_copy,
                    export_sidecars: false,
                    set_archived: Some(false),
                },
                2,
            );
            assert!(matches!(run(&mut c), CarryOutcome::Moved { .. }));
            let back = f
                .cat
                .get_preview(f.asset_id, crate::PreviewTier::T0)
                .unwrap()
                .unwrap();
            assert_eq!(back.bytes, pv_a.bytes);
            assert_eq!((back.width, back.height), (Some(40), Some(30)));
            assert!(!f.src.join("a.NEF.thumb.jpg").exists());
            assert!(!f.cat.list_roots().unwrap()[0].archived);
            no_journal(&f);
        }
    }

    #[test]
    fn settle_never_drops_a_blob_without_its_sidecar() {
        let f = fixture();
        f.cat
            .put_preview(f.asset_id, crate::PreviewTier::T0, &fake_jpeg(3))
            .unwrap();
        f.cat.set_root_archived(f.root_id, true).unwrap();
        let rep = crate::tier::settle_root(&*f.cat, f.root_id).unwrap();
        assert_eq!((rep.blobs_dropped, rep.kept), (0, 1));
        assert!(f
            .cat
            .get_preview(f.asset_id, crate::PreviewTier::T0)
            .unwrap()
            .is_some());
    }

    #[test]
    fn settle_keeps_a_blob_that_differs_from_its_sidecar() {
        let f = fixture();
        f.cat
            .put_preview(f.asset_id, crate::PreviewTier::T0, &fake_jpeg(5))
            .unwrap();
        f.cat.set_root_archived(f.root_id, true).unwrap();
        crate::thumb_sidecar::export(
            &f.src.join("a.NEF"),
            &fake_jpeg(6),
            crate::thumb_sidecar::SidecarCodec::Jpeg,
        )
        .unwrap();
        let rep = crate::tier::settle_root(&*f.cat, f.root_id).unwrap();
        assert_eq!((rep.blobs_dropped, rep.kept), (0, 1));
    }

    #[test]
    fn a_missing_raw_does_not_block_an_archive_move() {
        let f = fixture();
        f.cat
            .put_preview(f.asset_id, crate::PreviewTier::T0, &fake_jpeg(7))
            .unwrap();
        fs::remove_file(f.src.join("a.NEF")).unwrap();
        let mut c = Carry::new(
            f.cat.clone(),
            f.root_id,
            &f.dest_parent,
            CarryOptions {
                force_copy: true,
                export_sidecars: true,
                set_archived: Some(true),
            },
            1,
        );
        assert!(matches!(run(&mut c), CarryOutcome::Moved { .. }));
        // No sidecar was possible, so the catalog keeps the only copy.
        assert!(f
            .cat
            .get_preview(f.asset_id, crate::PreviewTier::T0)
            .unwrap()
            .is_some());
    }

    #[test]
    fn cancelling_during_settle_leaves_the_journal_open_for_recovery() {
        let f = fixture();
        f.cat
            .put_preview(f.asset_id, crate::PreviewTier::T0, &fake_jpeg(8))
            .unwrap();
        let mut c = Carry::new(
            f.cat.clone(),
            f.root_id,
            &f.dest_parent,
            CarryOptions {
                force_copy: true,
                export_sidecars: true,
                set_archived: Some(true),
            },
            1,
        );
        loop {
            if matches!(c.phase, Phase::Settle { .. }) {
                break;
            }
            assert!(c.step().is_none());
        }
        drop(c);
        assert_eq!(f.cat.open_root_moves().unwrap().len(), 1);
        let resumed = resume_open_moves(&*f.cat);
        assert!(matches!(
            resumed[0],
            Resumed::CleanedUp { .. } | Resumed::Committed { .. }
        ));
        // Recovery's follow-up: settle finishes the job.
        crate::tier::settle_root(&*f.cat, f.root_id).unwrap();
        assert!(f
            .cat
            .get_preview(f.asset_id, crate::PreviewTier::T0)
            .unwrap()
            .is_none());
        no_journal(&f);
    }

    #[test]
    fn a_failed_sidecar_write_aborts_the_move_before_anything_moves() {
        let f = fixture();
        f.cat
            .put_preview(f.asset_id, crate::PreviewTier::T0, &fake_jpeg(4))
            .unwrap();
        // A directory squatting on the sidecar path makes the rename-into-place fail.
        fs::create_dir(f.src.join("a.NEF.thumb.jpg")).unwrap();
        let mut c = Carry::new(
            f.cat.clone(),
            f.root_id,
            &f.dest_parent,
            CarryOptions {
                force_copy: true,
                export_sidecars: true,
                set_archived: Some(true),
            },
            1,
        );
        assert!(matches!(run(&mut c), CarryOutcome::Failed(_)));
        assert!(f.src.join("a.NEF").exists());
        assert!(!f.cat.list_roots().unwrap()[0].archived);
        no_journal(&f);
    }

    #[test]
    fn copy_path_moves_verifies_and_repoints_the_catalog() {
        let f = fixture();
        let dest = f.dest_parent.join("event-2026");
        let src_mtime = fs::metadata(f.src.join("a.NEF"))
            .unwrap()
            .modified()
            .unwrap();
        let mut c = carry(&f, true);
        let out = run(&mut c);
        let CarryOutcome::Moved {
            files,
            renamed,
            leftover_count,
            ..
        } = out
        else {
            panic!("expected Moved, got {out:?}");
        };
        assert_eq!((files, renamed, leftover_count), (3, false, 0));

        // Everything (sidecar, nested file, empty dir) landed; the source is gone.
        assert_eq!(fs::read(dest.join("a.xmp")).unwrap(), b"<xmp/>");
        assert_eq!(fs::read(dest.join("day1/b.NEF")).unwrap(), vec![9u8; 100]);
        assert!(dest.join("empty-dir").is_dir());
        assert!(!f.src.exists());
        assert!(!dest.join("a.NEF.nicti-partial").exists());
        assert_eq!(
            fs::metadata(dest.join("a.NEF"))
                .unwrap()
                .modified()
                .unwrap(),
            src_mtime
        );

        // The catalog follows: same asset id and rating, root re-pointed, full hash recorded.
        assert_eq!(
            f.cat.get_root_path(f.root_id).unwrap().unwrap(),
            dest.to_string_lossy()
        );
        let a = f.cat.get_asset(f.asset_id).unwrap().unwrap();
        assert_eq!((a.root_id, a.rating), (f.root_id, Some(4)));
        assert_eq!(
            f.cat.content_hash(f.asset_id).unwrap().unwrap(),
            blake3::hash(&vec![7u8; 3_000_000]).to_hex().to_string()
        );
        no_journal(&f);
    }

    #[test]
    fn same_volume_move_takes_the_rename_fast_path() {
        let f = fixture();
        let mut c = carry(&f, false);
        let out = run(&mut c);
        assert!(
            matches!(out, CarryOutcome::Moved { renamed: true, .. }),
            "{out:?}"
        );
        assert!(!f.src.exists());
        assert!(f.dest_parent.join("event-2026/a.NEF").is_file());
        assert_eq!(
            f.cat.get_root_path(f.root_id).unwrap().unwrap(),
            f.dest_parent.join("event-2026").to_string_lossy()
        );
        no_journal(&f);
    }

    #[test]
    fn preflight_refusals_touch_nothing() {
        let f = fixture();
        let refused = |c: &mut Carry| matches!(run(c), CarryOutcome::Refused(_));

        // Into itself.
        let mut c = Carry::new(f.cat.clone(), f.root_id, &f.src, CarryOptions::default(), 1);
        assert!(refused(&mut c));
        // Where it already is.
        let mut c = Carry::new(
            f.cat.clone(),
            f.root_id,
            &f.src_parent,
            CarryOptions::default(),
            1,
        );
        assert!(refused(&mut c));
        // Missing destination folder.
        let mut c = Carry::new(
            f.cat.clone(),
            f.root_id,
            &f.dest_parent.join("nope"),
            CarryOptions::default(),
            1,
        );
        assert!(refused(&mut c));
        // Non-empty destination of the same name.
        fs::create_dir_all(f.dest_parent.join("event-2026")).unwrap();
        fs::write(f.dest_parent.join("event-2026/x"), b"x").unwrap();
        let mut c = carry(&f, true);
        assert!(refused(&mut c));

        assert!(f.src.join("a.NEF").is_file());
        assert_eq!(
            f.cat.get_root_path(f.root_id).unwrap().unwrap(),
            f.src.to_string_lossy()
        );
        no_journal(&f);
    }

    #[test]
    fn a_second_move_of_the_same_root_is_refused_while_one_is_open() {
        let f = fixture();
        let mut first = carry(&f, true);
        assert!(first.step().is_none()); // Start: journal opened, dest created.
        let mut second = carry(&f, true);
        assert!(matches!(second.step(), Some(CarryOutcome::Refused(_))));
        drop(first);
        no_journal(&f);
    }

    #[test]
    fn a_hash_mismatch_commits_nothing_and_cleans_up() {
        let f = fixture();
        let mut c = carry(&f, true);
        c.after_dest_written = Some(Box::new(|partial| {
            // Bit rot between write and re-read.
            let mut bytes = fs::read(partial).unwrap();
            bytes[0] ^= 0xff;
            fs::write(partial, bytes).unwrap();
        }));
        let out = run(&mut c);
        assert!(matches!(out, CarryOutcome::VerifyFailed { .. }), "{out:?}");
        assert!(!f.dest_parent.join("event-2026").exists());
        assert!(f.src.join("a.NEF").is_file() && f.src.join("day1/b.NEF").is_file());
        assert_eq!(
            f.cat.get_root_path(f.root_id).unwrap().unwrap(),
            f.src.to_string_lossy()
        );
        assert_eq!(f.cat.content_hash(f.asset_id).unwrap(), None);
        no_journal(&f);
    }

    #[test]
    fn cancelling_mid_copy_discards_the_destination() {
        let f = fixture();
        let mut c = carry(&f, true);
        assert!(c.step().is_none()); // Start
        assert!(c.step().is_none()); // first chunk of the first file
        drop(c); // what Pounce does to a cancelled job
        assert!(!f.dest_parent.join("event-2026").exists());
        assert!(f.src.join("a.NEF").is_file());
        no_journal(&f);
    }

    #[test]
    fn a_source_file_changed_after_copy_is_kept_as_a_leftover() {
        let f = fixture();
        let src_a = f.src.join("a.NEF");
        let mut c = carry(&f, true);
        c.after_dest_written = Some(Box::new(move |partial| {
            if partial
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("a.NEF")
            {
                let mut f = fs::OpenOptions::new().append(true).open(&src_a).unwrap();
                f.write_all(b"edited while copying").unwrap();
            }
        }));
        let out = run(&mut c);
        let CarryOutcome::Moved {
            leftover_count,
            leftovers,
            ..
        } = out
        else {
            panic!("expected Moved, got {out:?}");
        };
        assert_eq!(leftover_count, 1);
        assert_eq!(leftovers, vec![f.src.join("a.NEF")]);
        assert!(f.src.join("a.NEF").is_file());
        assert!(!f.src.join("day1/b.NEF").exists());
    }

    #[test]
    fn resume_rolls_back_a_crash_mid_copy() {
        let f = fixture();
        let mut c = carry(&f, true);
        assert!(c.step().is_none());
        assert!(c.step().is_none());
        std::mem::forget(c); // a crash: no Drop cleanup runs
        assert!(f.dest_parent.join("event-2026").exists());
        assert_eq!(f.cat.open_root_moves().unwrap().len(), 1);

        let r = resume_open_moves(&*f.cat);
        assert_eq!(r, vec![Resumed::RolledBack { root_id: f.root_id }]);
        assert!(!f.dest_parent.join("event-2026").exists());
        assert!(f.src.join("a.NEF").is_file());
        no_journal(&f);
    }

    #[test]
    fn resume_finishes_a_crash_between_commit_and_cleanup() {
        let f = fixture();
        let mut c = carry(&f, true);
        // Drive to just after the commit, before cleanup deletes the source.
        loop {
            assert!(c.step().is_none());
            if c.committed {
                break;
            }
        }
        std::mem::forget(c);
        assert!(f.src.join("a.NEF").is_file()); // source still fully there

        let r = resume_open_moves(&*f.cat);
        assert_eq!(
            r,
            vec![Resumed::CleanedUp {
                root_id: f.root_id,
                leftover_count: 0
            }]
        );
        assert!(!f.src.exists());
        assert!(f.dest_parent.join("event-2026/a.NEF").is_file());
        no_journal(&f);
    }

    #[test]
    fn resume_completes_a_rename_that_landed_before_the_commit() {
        let f = fixture();
        let dest = f.dest_parent.join("event-2026");
        // Simulate: journal opened, rename happened, crash before commit.
        let id = f
            .cat
            .begin_root_move(f.root_id, &dest.to_string_lossy(), 1)
            .unwrap();
        f.cat.set_root_move_state(id, MoveState::Renaming).unwrap();
        fs::rename(&f.src, &dest).unwrap();

        let r = resume_open_moves(&*f.cat);
        assert_eq!(r, vec![Resumed::Committed { root_id: f.root_id }]);
        assert_eq!(
            f.cat.get_root_path(f.root_id).unwrap().unwrap(),
            dest.to_string_lossy()
        );
        no_journal(&f);
    }

    #[test]
    fn resume_keeps_a_committed_source_file_that_differs_from_the_destination() {
        let f = fixture();
        let mut c = carry(&f, true);
        loop {
            assert!(c.step().is_none());
            if c.committed {
                break;
            }
        }
        std::mem::forget(c);
        // The user edited a source file after the copy: it no longer matches, so keep it.
        fs::write(f.src.join("a.xmp"), b"<edited/>").unwrap();

        let r = resume_open_moves(&*f.cat);
        assert_eq!(
            r,
            vec![Resumed::CleanedUp {
                root_id: f.root_id,
                leftover_count: 1
            }]
        );
        assert_eq!(fs::read(f.src.join("a.xmp")).unwrap(), b"<edited/>");
    }

    #[test]
    fn a_source_file_named_like_a_temp_copy_is_refused_not_clobbered() {
        let f = fixture();
        fs::write(f.src.join("x.NEF"), b"real").unwrap();
        fs::write(f.src.join("x.NEF.nicti-partial"), b"user file").unwrap();
        let mut c = carry(&f, true);
        assert!(matches!(run(&mut c), CarryOutcome::Refused(_)));
        assert_eq!(
            fs::read(f.src.join("x.NEF.nicti-partial")).unwrap(),
            b"user file"
        );
        assert!(!f.dest_parent.join("event-2026").exists());
        no_journal(&f);
    }

    #[test]
    fn a_source_file_ending_dot_partial_survives_the_move() {
        // Regression: the old temp name was `<name>.partial`, so `x.NEF` and `x.NEF.partial`
        // clobbered each other and the real `.partial` file was then deleted from the source.
        let f = fixture();
        for i in 0..30 {
            fs::write(f.src.join(format!("p{i}.NEF")), format!("raw{i}")).unwrap();
            fs::write(f.src.join(format!("p{i}.NEF.partial")), format!("side{i}")).unwrap();
        }
        let mut c = carry(&f, true);
        assert!(matches!(run(&mut c), CarryOutcome::Moved { .. }));
        let dest = f.dest_parent.join("event-2026");
        for i in 0..30 {
            assert_eq!(
                fs::read(dest.join(format!("p{i}.NEF"))).unwrap(),
                format!("raw{i}").as_bytes()
            );
            assert_eq!(
                fs::read(dest.join(format!("p{i}.NEF.partial"))).unwrap(),
                format!("side{i}").as_bytes()
            );
        }
    }

    #[test]
    fn resume_never_deletes_either_side_when_the_source_was_recreated_after_a_rename() {
        // Regression: rename landed, commit never ran, then something recreated the source path.
        // The old rollback saw "source exists" and deleted the moved folder.
        let f = fixture();
        let dest = f.dest_parent.join("event-2026");
        let id = f
            .cat
            .begin_root_move(f.root_id, &dest.to_string_lossy(), 1)
            .unwrap();
        f.cat.set_root_move_state(id, MoveState::Renaming).unwrap();
        fs::rename(&f.src, &dest).unwrap();
        fs::create_dir_all(&f.src).unwrap();

        let r = resume_open_moves(&*f.cat);
        assert!(matches!(r[..], [Resumed::Stuck { .. }]), "{r:?}");
        assert!(dest.join("a.NEF").is_file(), "moved folder must survive");
        assert_eq!(
            f.cat.open_root_moves().unwrap().len(),
            1,
            "journal stays for a human"
        );
    }

    #[test]
    fn nested_roots_are_refused() {
        let f = fixture();
        let vol = f.cat.upsert_volume("test-vol", None, None, 0).unwrap();
        f.cat
            .ensure_root(vol, &f.src.join("day1").to_string_lossy())
            .unwrap();
        let mut c = carry(&f, true);
        assert!(matches!(run(&mut c), CarryOutcome::Refused(_)));
        assert!(f.src.join("a.NEF").is_file());
        no_journal(&f);
    }

    #[test]
    fn dropping_after_commit_finishes_cleanup_and_closes_the_journal() {
        let f = fixture();
        let mut c = carry(&f, true);
        loop {
            assert!(c.step().is_none());
            if c.committed {
                break;
            }
        }
        drop(c); // cancel landed in the Cleanup phase
        assert!(!f.src.exists());
        assert!(f.dest_parent.join("event-2026/a.NEF").is_file());
        no_journal(&f);
    }

    #[test]
    fn a_same_size_rewrite_of_a_source_file_is_kept_not_deleted() {
        // mtime alone can't see this on coarse-granularity filesystems; the re-hash does.
        let f = fixture();
        let src_xmp = f.src.join("a.xmp");
        let mut c = carry(&f, true);
        c.after_dest_written = Some(Box::new(move |partial| {
            if partial
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("a.xmp")
            {
                fs::write(&src_xmp, b"<XMP/>").unwrap(); // same length as "<xmp/>"
            }
        }));
        let out = run(&mut c);
        let CarryOutcome::Moved { leftovers, .. } = out else {
            panic!("expected Moved, got {out:?}");
        };
        assert_eq!(leftovers, vec![f.src.join("a.xmp")]);
        assert_eq!(fs::read(f.src.join("a.xmp")).unwrap(), b"<XMP/>");
    }

    #[test]
    fn dropping_right_after_a_landed_rename_commits_the_catalog() {
        let f = fixture();
        let mut c = carry(&f, false);
        assert!(c.step().is_none()); // Start: rename landed, commit not yet run
        assert!(c.is_committed());
        drop(c);
        let dest = f.dest_parent.join("event-2026");
        assert!(dest.join("a.NEF").is_file());
        assert_eq!(
            f.cat.get_root_path(f.root_id).unwrap().unwrap(),
            dest.to_string_lossy()
        );
        no_journal(&f);
    }

    #[test]
    fn resume_refuses_to_delete_a_destination_holding_files_the_source_lost() {
        // Power-cut shape: commit + source cleanup happened on disk, but the journal reads
        // `copying` (the WAL commit never became durable) and the source dir remnants survive.
        let f = fixture();
        let dest = f.dest_parent.join("event-2026");
        f.cat
            .begin_root_move(f.root_id, &dest.to_string_lossy(), 1)
            .unwrap();
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("a.NEF"), vec![7u8; 3_000_000]).unwrap();
        fs::remove_file(f.src.join("a.NEF")).unwrap(); // source cleanup already ran

        let r = resume_open_moves(&*f.cat);
        assert!(matches!(r[..], [Resumed::Stuck { .. }]), "{r:?}");
        assert!(dest.join("a.NEF").is_file(), "the only copy must survive");
    }

    #[test]
    fn resume_rolls_back_a_renaming_row_whose_destination_is_just_the_empty_folder() {
        let f = fixture();
        let dest = f.dest_parent.join("event-2026");
        fs::create_dir_all(&dest).unwrap();
        let id = f
            .cat
            .begin_root_move(f.root_id, &dest.to_string_lossy(), 1)
            .unwrap();
        f.cat.set_root_move_state(id, MoveState::Renaming).unwrap();
        let r = resume_open_moves(&*f.cat);
        assert_eq!(r, vec![Resumed::RolledBack { root_id: f.root_id }]);
        assert!(f.src.join("a.NEF").is_file());
        no_journal(&f);
    }

    #[test]
    fn resume_will_not_repoint_the_catalog_at_an_empty_destination() {
        let f = fixture();
        let dest = f.dest_parent.join("event-2026");
        fs::create_dir_all(&dest).unwrap();
        let id = f
            .cat
            .begin_root_move(f.root_id, &dest.to_string_lossy(), 1)
            .unwrap();
        f.cat.set_root_move_state(id, MoveState::Renaming).unwrap();
        fs::remove_dir_all(&f.src).unwrap(); // e.g. the source drive went away
        let r = resume_open_moves(&*f.cat);
        assert!(matches!(r[..], [Resumed::Stuck { .. }]), "{r:?}");
        assert_eq!(
            f.cat.get_root_path(f.root_id).unwrap().unwrap(),
            f.src.to_string_lossy()
        );
    }

    #[test]
    fn nested_roots_are_found_through_dotdot_segments() {
        let f = fixture();
        let vol = f.cat.upsert_volume("test-vol", None, None, 0).unwrap();
        let sneaky = f.src_parent.join("elsewhere/../event-2026/day1");
        fs::create_dir_all(f.src_parent.join("elsewhere")).unwrap();
        f.cat.ensure_root(vol, &sneaky.to_string_lossy()).unwrap();
        let mut c = carry(&f, true);
        assert!(matches!(run(&mut c), CarryOutcome::Refused(_)));
        no_journal(&f);
    }
}
