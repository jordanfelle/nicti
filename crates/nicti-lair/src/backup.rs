//! Additive backup copy of a RAW folder to a second drive (#305): "caching" -- a cat stashes food
//! where it can find it again. Every file under a source root is copied, checked, to the same
//! relative place under a backup target, by [`crate::offering::offer_file`]'s verified copy
//! (streamed once, hashed, synced, re-read, published without overwriting).
//!
//! **Additive only.** Nothing here deletes, overwrites, renames or truncates anything, on the
//! source or on the target: a file deleted from the library stays in the backup, and a file whose
//! backed-up copy differs from the source is reported ([`BackedUp::Differs`]) and left alone, never
//! replaced and never duplicated under a new name. A backup that mirrored deletions would turn the
//! accidental deletion it exists to survive into two deletions.
//!
//! **Volume identity (ADR-0071).** A drive letter or mount point can end up on a different drive
//! (unplugged, remapped, a second card reader). The target therefore carries a marker file with a
//! random id, written once by [`init_target`]; [`open_target`] only returns a [`BackupTarget`] when
//! the marker matches the id the caller saved, so a backup never silently lands on the wrong disk.
//! The marker is the secondary signal ADR-0071 describes, not a replacement for the OS-level volume
//! key the catalog uses; a cloned drive carries the same marker (documented, not detectable here).
//!
//! The target and the source must not overlap (either inside the other), or a backup would copy
//! itself. Cloud targets are out of scope (credentials and a dependency/licence audit, see #5).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::offering::{list_card_files, offer_file, read_back, OfferError, Offered};

/// Marker file at the root of a backup target.
pub const MARKER_NAME: &str = ".nicti-backup-target";
const MARKER_HEADER: &str = "nicti-backup-target v1";
/// A marker larger than this is not ours.
const MARKER_MAX_BYTES: u64 = 256;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BackupError {
    #[error("{0}: {1}")]
    Io(PathBuf, String),
    #[error("{0} already carries a backup marker; open it with its saved id instead")]
    AlreadyInitialised(PathBuf),
    #[error(
        "{0} has no backup marker: this is not a backup target (wrong drive, or never set up)"
    )]
    NotATarget(PathBuf),
    #[error("{0} is a different backup target than the one that was saved (wrong drive?)")]
    WrongTarget(PathBuf),
    #[error("{0}: the backup marker is unreadable or not Nicti's")]
    BadMarker(PathBuf),
    #[error("the backup target and the source folder overlap")]
    Overlap,
    #[error("{0}")]
    Offer(OfferError),
}

/// An opened, identity-checked backup target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupTarget {
    root: PathBuf,
    id: String,
}

impl BackupTarget {
    pub fn root(&self) -> &Path {
        &self.root
    }
    /// The id to save with the user's settings and pass to [`open_target`] next time.
    pub fn id(&self) -> &str {
        &self.id
    }
}

fn io_err(path: &Path, e: io::Error) -> BackupError {
    BackupError::Io(path.to_path_buf(), e.to_string())
}

fn new_id() -> String {
    // Unique enough to tell two drives apart; not a secret and not security-relevant.
    let mut h = blake3::Hasher::new();
    h.update(
        &std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
            .to_le_bytes(),
    );
    h.update(&u64::from(std::process::id()).to_le_bytes());
    let mut salt = [0u8; 16];
    let _ = File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut salt));
    h.update(&salt);
    h.finalize().to_hex()[..32].to_string()
}

fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Marks `root` (an existing directory the user chose) as a backup target and returns its id.
/// Refuses a root that already carries a marker: re-initialising would orphan every saved id.
pub fn init_target(root: &Path) -> Result<BackupTarget, BackupError> {
    let root = root.canonicalize().map_err(|e| io_err(root, e))?;
    if !root.is_dir() {
        return Err(BackupError::Io(root, "not a directory".into()));
    }
    if root.parent().is_none() {
        return Err(BackupError::Io(
            root,
            "a drive root is not a backup folder; pick or create a folder on it".into(),
        ));
    }
    let marker = root.join(MARKER_NAME);
    let id = new_id();
    let mut f = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
    {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            return Err(BackupError::AlreadyInitialised(root));
        }
        Err(e) => return Err(io_err(&marker, e)),
    };
    f.write_all(format!("{MARKER_HEADER}\n{id}\n").as_bytes())
        .and_then(|()| f.sync_all())
        .map_err(|e| io_err(&marker, e))?;
    Ok(BackupTarget { root, id })
}

/// Opens `root` as the backup target whose saved id is `expected_id`. A missing marker, an
/// unreadable one or a different id is an error, never a fallback to "back up here anyway".
pub fn open_target(root: &Path, expected_id: &str) -> Result<BackupTarget, BackupError> {
    let root = root.canonicalize().map_err(|e| io_err(root, e))?;
    let marker = root.join(MARKER_NAME);
    match fs::symlink_metadata(&marker) {
        Ok(m) if m.is_file() && m.len() <= MARKER_MAX_BYTES => {}
        Ok(_) => return Err(BackupError::BadMarker(root)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(BackupError::NotATarget(root)),
        Err(e) => return Err(io_err(&marker, e)),
    }
    let mut text = String::new();
    File::open(&marker)
        .and_then(|f| f.take(MARKER_MAX_BYTES).read_to_string(&mut text))
        .map_err(|_| BackupError::BadMarker(root.clone()))?;
    let mut lines = text.lines();
    let id = match (lines.next(), lines.next(), lines.next()) {
        (Some(MARKER_HEADER), Some(id), None) if valid_id(id) => id,
        _ => return Err(BackupError::BadMarker(root)),
    };
    if id != expected_id {
        return Err(BackupError::WrongTarget(root));
    }
    Ok(BackupTarget {
        root,
        id: id.to_owned(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackedUp {
    Copied {
        dest: PathBuf,
        blake3: String,
        size: u64,
    },
    /// The backup already holds identical bytes; nothing was written.
    Unchanged {
        dest: PathBuf,
    },
    /// The backup holds a *different* file at this path. Left untouched; the caller reports it.
    Differs {
        dest: PathBuf,
    },
    Cancelled,
}

/// Whether two directories overlap (either contains the other), by canonical path.
fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// Backs up one file. `src` must lie under `source_root` (as returned by listing it); its path
/// relative to the root is reproduced under the target.
pub fn back_up_file(
    target: &BackupTarget,
    source_root: &Path,
    src: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Result<BackedUp, BackupError> {
    let source_root = source_root
        .canonicalize()
        .map_err(|e| io_err(source_root, e))?;
    if overlaps(&source_root, &target.root) {
        return Err(BackupError::Overlap);
    }
    let Ok(rel) = src.strip_prefix(&source_root).map(Path::to_path_buf) else {
        return Err(BackupError::Offer(OfferError::Refused(
            src.to_path_buf(),
            "not under the source folder".into(),
        )));
    };
    // The OS follows a link in any *intermediate* folder, so a link inside the target would make
    // `existing` resolve to some other copy and be reported Unchanged while nothing was backed up
    // here. Refuse a link or non-folder among the folders that already exist.
    let mut walk = target.root.clone();
    for part in rel.parent().into_iter().flat_map(Path::components) {
        walk.push(part);
        match fs::symlink_metadata(&walk) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => {
                return Err(BackupError::Offer(OfferError::Refused(
                    src.to_path_buf(),
                    format!("{} in the backup is a link, not a folder", walk.display()),
                )))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => break,
            Err(e) => return Err(io_err(&walk, e)),
        }
    }
    let existing = target.root.join(&rel);
    match fs::symlink_metadata(&existing) {
        Ok(m) if m.is_file() => {
            let src_meta = fs::symlink_metadata(src).map_err(|e| io_err(src, e))?;
            let same = src_meta.is_file()
                && src_meta.len() == m.len()
                && match (read_back(src, cancelled), read_back(&existing, cancelled)) {
                    (Ok(Some((a, _))), Ok(Some((b, _)))) => a == b,
                    (Ok(None), _) | (_, Ok(None)) => return Ok(BackedUp::Cancelled),
                    (Err(e), _) => return Err(io_err(src, e)),
                    (_, Err(e)) => return Err(io_err(&existing, e)),
                };
            return Ok(if same {
                BackedUp::Unchanged { dest: existing }
            } else {
                BackedUp::Differs { dest: existing }
            });
        }
        // A folder, link or other non-file where the file goes: do not touch it.
        Ok(_) => return Ok(BackedUp::Differs { dest: existing }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_err(&existing, e)),
    }
    match offer_file(src, &target.root, &rel, cancelled).map_err(BackupError::Offer)? {
        // A different file appeared at the name between the check and the copy, so the copy got a
        // `-N` name: report the conflict rather than a clean copy.
        Offered::Copied { dest, .. } if dest != existing => {
            Ok(BackedUp::Differs { dest: existing })
        }
        Offered::Copied {
            dest, blake3, size, ..
        } => Ok(BackedUp::Copied { dest, blake3, size }),
        // Raced into existence between the check and the copy, with the same bytes.
        Offered::AlreadyThere { dest, .. } => Ok(BackedUp::Unchanged { dest }),
        Offered::Cancelled => Ok(BackedUp::Cancelled),
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    pub copied: u64,
    pub copied_bytes: u64,
    pub unchanged: u64,
    /// Paths whose backed-up copy differs from the source (kept, not replaced).
    pub differs: Vec<PathBuf>,
    /// Files that could not be backed up, and folders that could not be listed.
    pub failed: Vec<String>,
    pub cancelled: bool,
}

/// A backup of one source folder, advanced one file per [`BackupRun::step`] so a scheduler can
/// interleave it, show progress and cancel between files (a Pounce job drives it; [`back_up_root`]
/// is the blocking loop over the same thing).
pub struct BackupRun {
    target: BackupTarget,
    root: PathBuf,
    files: Vec<crate::offering::CardFile>,
    next: usize,
    report: Report,
}

impl BackupRun {
    /// Lists everything RAW/JPEG-shaped under `source_root`. Refuses a source that overlaps the
    /// target. Listing reads the whole tree, so call this off the UI thread.
    pub fn new(target: &BackupTarget, source_root: &Path) -> Result<Self, BackupError> {
        let root = source_root
            .canonicalize()
            .map_err(|e| io_err(source_root, e))?;
        if overlaps(&root, &target.root) {
            return Err(BackupError::Overlap);
        }
        let (files, problems) = list_card_files(&root);
        Ok(BackupRun {
            target: target.clone(),
            root,
            files,
            next: 0,
            report: Report {
                failed: problems,
                ..Report::default()
            },
        })
    }

    /// `(files done, files total)`.
    pub fn progress(&self) -> (usize, usize) {
        (self.next, self.files.len())
    }

    /// Backs up the next file. Returns `true` while more remain; `false` once every file was
    /// handled or `cancelled` fired mid-file (then [`Report::cancelled`] is set).
    pub fn step(&mut self, cancelled: &dyn Fn() -> bool) -> bool {
        let Some(f) = self.files.get(self.next) else {
            return false;
        };
        match back_up_file(&self.target, &self.root, &f.path, cancelled) {
            Ok(BackedUp::Copied { size, .. }) => {
                self.report.copied += 1;
                self.report.copied_bytes += size;
            }
            Ok(BackedUp::Unchanged { .. }) => self.report.unchanged += 1,
            Ok(BackedUp::Differs { dest }) => self.report.differs.push(dest),
            Ok(BackedUp::Cancelled) => {
                self.report.cancelled = true;
                return false;
            }
            Err(e) => self
                .report
                .failed
                .push(format!("{}: {e}", f.path.display())),
        }
        self.next += 1;
        self.next < self.files.len()
    }

    /// The report so far (final once `step` returned `false`).
    pub fn into_report(self) -> Report {
        self.report
    }
}

/// Backs up every RAW/JPEG file under `source_root`. One bad file does not stop the run. Run it off
/// the UI thread; `cancelled` is polled between chunks and `progress(done, total)` after each file.
pub fn back_up_root(
    target: &BackupTarget,
    source_root: &Path,
    cancelled: &dyn Fn() -> bool,
    progress: &mut dyn FnMut(usize, usize),
) -> Result<Report, BackupError> {
    let mut run = BackupRun::new(target, source_root)?;
    while run.step(cancelled) {
        let (done, total) = run.progress();
        progress(done, total);
    }
    let (done, total) = run.progress();
    progress(done, total);
    Ok(run.into_report())
}

/// What is worth warning about before a folder becomes a backup target ([`assess_target_folder`]).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TargetAssessment {
    /// The folder already holds files or folders (other than a backup marker).
    pub non_empty: bool,
    /// The folder is inside (or is) one of the library's folders: backups would land in the library.
    pub inside_library: Option<PathBuf>,
    /// One of the library's folders is inside the candidate: the library sits in the backup.
    pub contains_library: Option<PathBuf>,
    /// The folder already carries a backup marker (use its saved id, or pick another folder).
    pub already_a_target: bool,
}

impl TargetAssessment {
    /// Whether the UI should ask for confirmation before initialising the folder.
    pub fn needs_confirmation(&self) -> bool {
        self.non_empty || self.inside_library.is_some() || self.contains_library.is_some()
    }

    /// One line per concern, for the UI.
    pub fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(p) = &self.inside_library {
            out.push(format!(
                "This folder is inside your library folder {}: the backup would sit in the very \
                 place it protects. Pick a folder on another drive.",
                p.display()
            ));
        }
        if let Some(p) = &self.contains_library {
            out.push(format!(
                "Your library folder {} is inside this folder, so a backup would copy into itself. \
                 Pick a folder on another drive.",
                p.display()
            ));
        }
        if self.non_empty {
            out.push(
                "This folder is not empty. Backups only add files and never touch what is \
                 there, but a dedicated empty folder is safer."
                    .into(),
            );
        }
        if self.already_a_target {
            out.push("This folder is already a backup target.".into());
        }
        out
    }
}

/// Looks at `candidate` before [`init_target`]: is it non-empty, inside a library folder, or does
/// it contain one? `library_roots` are the catalog's registered folder paths. Folders that do not
/// resolve (an offline library drive) are skipped rather than guessed at. Reads at most a couple of
/// directory entries.
pub fn assess_target_folder(
    candidate: &Path,
    library_roots: &[PathBuf],
) -> Result<TargetAssessment, BackupError> {
    let cand = candidate.canonicalize().map_err(|e| io_err(candidate, e))?;
    if !cand.is_dir() {
        return Err(BackupError::Io(cand, "not a directory".into()));
    }
    let mut out = TargetAssessment::default();
    for entry in fs::read_dir(&cand).map_err(|e| io_err(&cand, e))?.take(8) {
        let entry = entry.map_err(|e| io_err(&cand, e))?;
        if entry.file_name() == MARKER_NAME {
            out.already_a_target = true;
        } else {
            out.non_empty = true;
        }
    }
    for lib in library_roots {
        let Ok(lib) = lib.canonicalize() else {
            continue;
        };
        if cand.starts_with(&lib) {
            out.inside_library = Some(lib);
            break;
        }
        if lib.starts_with(&cand) && out.contains_library.is_none() {
            out.contains_library = Some(lib);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn never() -> bool {
        false
    }

    fn put(root: &Path, rel: &str, bytes: &[u8]) -> PathBuf {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, bytes).unwrap();
        p
    }

    struct Fx {
        _t: tempfile::TempDir,
        src: PathBuf,
        dst: PathBuf,
    }

    fn fx() -> Fx {
        let t = tempfile::tempdir().unwrap();
        let (src, dst) = (t.path().join("lib"), t.path().join("backup"));
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        Fx { _t: t, src, dst }
    }

    fn run(t: &BackupTarget, src: &Path) -> Report {
        back_up_root(t, src, &never, &mut |_, _| {}).unwrap()
    }

    #[test]
    fn copies_a_tree_verified_and_a_second_run_is_a_no_op() {
        let f = fx();
        put(&f.src, "2026/10/a.NEF", &vec![1u8; 3_000_000]);
        put(&f.src, "2026/11/b.jpg", b"bee");
        put(&f.src, "notes.txt", b"not a photo");
        let t = init_target(&f.dst).unwrap();
        let r = run(&t, &f.src);
        assert_eq!((r.copied, r.unchanged), (2, 0), "{r:?}");
        assert_eq!(
            fs::read(f.dst.join("2026/10/a.NEF")).unwrap(),
            vec![1u8; 3_000_000]
        );
        assert!(
            !f.dst.join("notes.txt").exists(),
            "only photos are backed up"
        );
        let again = run(&t, &f.src);
        assert_eq!((again.copied, again.unchanged), (0, 2), "{again:?}");
    }

    #[test]
    fn a_file_deleted_from_the_library_stays_in_the_backup() {
        let f = fx();
        let a = put(&f.src, "a.NEF", b"alpha");
        let t = init_target(&f.dst).unwrap();
        run(&t, &f.src);
        fs::remove_file(a).unwrap();
        let r = run(&t, &f.src);
        assert_eq!((r.copied, r.unchanged), (0, 0));
        assert_eq!(fs::read(f.dst.join("a.NEF")).unwrap(), b"alpha");
    }

    #[test]
    fn a_changed_source_never_overwrites_or_duplicates_the_backup() {
        let f = fx();
        put(&f.src, "a.NEF", b"original");
        let t = init_target(&f.dst).unwrap();
        run(&t, &f.src);
        put(&f.src, "a.NEF", b"edited!!"); // same length, different bytes
        let r = run(&t, &f.src);
        assert_eq!(r.copied, 0);
        assert_eq!(r.differs, vec![t.root().join("a.NEF")]);
        assert_eq!(fs::read(f.dst.join("a.NEF")).unwrap(), b"original");
        let names: Vec<_> = fs::read_dir(&f.dst)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 2, "marker + a.NEF only: {names:?}");
        put(&f.src, "a.NEF", b"longer than before");
        assert_eq!(
            run(&t, &f.src).differs.len(),
            1,
            "different size also differs"
        );
    }

    #[test]
    fn the_target_must_be_the_drive_the_user_saved() {
        let f = fx();
        let t = init_target(&f.dst).unwrap();
        assert_eq!(open_target(&f.dst, t.id()).unwrap(), t);
        assert!(matches!(
            open_target(&f.dst, &"0".repeat(32)),
            Err(BackupError::WrongTarget(_))
        ));
        assert!(matches!(
            init_target(&f.dst),
            Err(BackupError::AlreadyInitialised(_))
        ));
        let other = f.dst.parent().unwrap().join("other");
        fs::create_dir_all(&other).unwrap();
        assert!(matches!(
            open_target(&other, t.id()),
            Err(BackupError::NotATarget(_))
        ));
        // A drive letter now pointing at an unmarked disk never gets a backup written to it.
        put(&f.src, "a.NEF", b"x");
        assert!(open_target(&other, t.id()).is_err());
        assert!(!other.join("a.NEF").exists());
    }

    #[test]
    fn a_garbled_or_foreign_marker_is_refused() {
        let f = fx();
        let t = init_target(&f.dst).unwrap();
        let m = f.dst.join(MARKER_NAME);
        for bad in [
            "".to_string(),
            "nicti-backup-target v1\n".to_string(),
            format!("nicti-backup-target v2\n{}\n", t.id()),
            format!("nicti-backup-target v1\n{}\nextra\n", t.id()),
            format!("nicti-backup-target v1\n{}\n", t.id().to_uppercase()),
            "x".repeat(10_000),
        ] {
            fs::write(&m, &bad).unwrap();
            assert!(open_target(&f.dst, t.id()).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn overlapping_source_and_target_are_refused() {
        let f = fx();
        let inside = f.src.join("backup");
        fs::create_dir_all(&inside).unwrap();
        put(&f.src, "a.NEF", b"x");
        let t = init_target(&inside).unwrap();
        assert_eq!(
            back_up_root(&t, &f.src, &never, &mut |_, _| {}),
            Err(BackupError::Overlap)
        );
        let t2 = init_target(&f.dst).unwrap();
        assert_eq!(
            back_up_root(&t2, &f.dst.join("."), &never, &mut |_, _| {}),
            Err(BackupError::Overlap)
        );
        assert!(!inside.join("a.NEF").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_link_in_the_source_or_the_target_is_never_followed() {
        let f = fx();
        let secret = f.src.parent().unwrap().join("secret.NEF");
        fs::write(&secret, b"secret").unwrap();
        std::os::unix::fs::symlink(&secret, f.src.join("link.NEF")).unwrap();
        // A folder in the target that is a link out of it.
        let outside = f.src.parent().unwrap().join("outside");
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, f.dst.join("2026")).unwrap();
        put(&f.src, "2026/a.NEF", b"a");
        let t = init_target(&f.dst).unwrap();
        let r = run(&t, &f.src);
        assert_eq!(r.copied, 0, "{r:?}");
        assert!(!r.failed.is_empty() || !r.differs.is_empty());
        assert!(fs::read_dir(&outside).unwrap().next().is_none());
        assert!(!f.dst.join("link.NEF").exists());
    }

    #[test]
    fn cancelling_stops_the_run_and_leaves_no_partial_file() {
        let f = fx();
        put(&f.src, "a.NEF", &vec![9u8; 4_000_000]);
        let t = init_target(&f.dst).unwrap();
        let r = back_up_root(&t, &f.src, &|| true, &mut |_, _| {}).unwrap();
        assert!(r.cancelled);
        assert!(!f.dst.join("a.NEF").exists());
        assert!(!fs::read_dir(&f.dst)
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().ends_with(".nicti-part")));
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_link_in_the_backup_cannot_fake_an_existing_copy() {
        let f = fx();
        put(&f.src, "2026/a.NEF", b"precious");
        // The "backup" folder 2026 is a link to the source's own folder: same bytes, no backup.
        std::os::unix::fs::symlink(f.src.join("2026"), f.dst.join("2026")).unwrap();
        let t = init_target(&f.dst).unwrap();
        let r = run(&t, &f.src);
        assert_eq!((r.copied, r.unchanged), (0, 0), "{r:?}");
        assert_eq!(r.failed.len(), 1, "{r:?}");
    }

    #[test]
    fn a_drive_root_is_not_a_target() {
        let root = Path::new(std::path::MAIN_SEPARATOR_STR);
        assert!(init_target(root).is_err());
    }

    #[test]
    fn a_non_file_where_a_photo_goes_is_left_alone() {
        let f = fx();
        put(&f.src, "a.NEF", b"x");
        fs::create_dir_all(f.dst.join("a.NEF")).unwrap();
        let t = init_target(&f.dst).unwrap();
        let r = run(&t, &f.src);
        assert_eq!((r.copied, r.differs.len()), (0, 1));
        assert!(f.dst.join("a.NEF").is_dir());
    }

    #[test]
    fn a_stepped_run_advances_one_file_at_a_time_and_matches_the_blocking_loop() {
        let f = fx();
        put(&f.src, "a.NEF", b"aaa");
        put(&f.src, "sub/b.NEF", b"bbb");
        put(&f.src, "sub/c.jpg", b"ccc");
        let t = init_target(&f.dst).unwrap();
        let mut run = BackupRun::new(&t, &f.src).unwrap();
        assert_eq!(run.progress(), (0, 3));
        let mut steps = 0;
        while run.step(&never) {
            steps += 1;
            assert_eq!(run.progress().0, steps, "one file per step");
        }
        let (done, total) = run.progress();
        assert_eq!((done, total), (3, 3));
        let r = run.into_report();
        assert_eq!((r.copied, r.unchanged, r.cancelled), (3, 0, false), "{r:?}");
        // The same folder through the blocking loop is now entirely unchanged.
        let again = run_blocking(&t, &f.src);
        assert_eq!((again.copied, again.unchanged), (0, 3));
        // An empty folder is a finished run with nothing to do, not an error or a hang.
        let empty = f._t.path().join("empty");
        fs::create_dir_all(&empty).unwrap();
        let mut run = BackupRun::new(&t, &empty).unwrap();
        assert!(!run.step(&never));
        assert_eq!(run.into_report(), Report::default());
    }

    fn run_blocking(t: &BackupTarget, src: &Path) -> Report {
        run(t, src)
    }

    #[test]
    fn a_stepped_run_refuses_overlap_like_the_blocking_loop() {
        let f = fx();
        let t = init_target(&f.dst).unwrap();
        assert_eq!(BackupRun::new(&t, &f.dst).err(), Some(BackupError::Overlap));
        assert_eq!(
            BackupRun::new(&t, f.dst.parent().unwrap()).err(),
            Some(BackupError::Overlap),
            "a source that contains the target"
        );
    }

    #[test]
    fn assessing_a_folder_warns_about_non_empty_library_and_existing_targets() {
        let f = fx();
        let library = vec![f.src.clone()];
        // Empty and elsewhere: nothing to say.
        let a = assess_target_folder(&f.dst, &library).unwrap();
        assert_eq!(a, TargetAssessment::default());
        assert!(!a.needs_confirmation() && a.warnings().is_empty());

        // Non-empty.
        put(&f.dst, "old/photo.NEF", b"x");
        let a = assess_target_folder(&f.dst, &library).unwrap();
        assert!(a.non_empty && a.needs_confirmation());
        assert!(a.warnings()[0].contains("not empty"));

        // Inside a library folder.
        let inner = f.src.join("backup-here");
        fs::create_dir_all(&inner).unwrap();
        let a = assess_target_folder(&inner, &library).unwrap();
        assert_eq!(
            a.inside_library.as_deref(),
            Some(f.src.canonicalize().unwrap().as_path())
        );
        assert!(a
            .warnings()
            .iter()
            .any(|w| w.contains("inside your library folder")));

        // The library inside the candidate.
        let a = assess_target_folder(f.src.parent().unwrap(), &library).unwrap();
        assert!(a.contains_library.is_some() && a.needs_confirmation());
        assert!(a.warnings().iter().any(|w| w.contains("copy into itself")));

        // An offline library folder is skipped, not an error.
        let a = assess_target_folder(&f.dst, &[f._t.path().join("gone")]).unwrap();
        assert!(a.inside_library.is_none() && a.contains_library.is_none());

        // A folder that is already a target says so, and its marker alone is not "non-empty".
        let fresh = f._t.path().join("fresh");
        fs::create_dir_all(&fresh).unwrap();
        init_target(&fresh).unwrap();
        let a = assess_target_folder(&fresh, &library).unwrap();
        assert!(a.already_a_target && !a.non_empty);
        assert!(a.warnings()[0].contains("already a backup target"));

        // A path that is not a folder is an error.
        assert!(assess_target_folder(&f.dst.join("old/photo.NEF"), &library).is_err());
        assert!(assess_target_folder(&f.dst.join("missing"), &library).is_err());
    }
}
