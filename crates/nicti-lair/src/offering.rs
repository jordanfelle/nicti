//! Copy-only import from a camera card (#548, ADR-0436 sections 3-6): "offering" -- the way a cat
//! brings its catch home. Files are copied, checked, and *never* deleted from the card; the user
//! formats the card, Nicti is never the only holder of the only copy (ADR-0026's "no unverified
//! deletes").
//!
//! This is the engine only: list what a card holds ([`list_card_files`]) and place one file under a
//! destination root ([`offer_file`]). Choosing destination names (the `naming.rs` templates) and
//! registering the copies is the caller's -- a UI job (#602) renders each `dest_rel`, calls
//! [`offer_file`] per file off the UI thread, then runs `scruff::ingest_root` on the destination.
//!
//! Each copy streams the card file once while hashing it (BLAKE3) into a uniquely named temp file
//! beside the final one, fails if the card file changed size while it was read, `sync`s the temp
//! file, re-opens it and re-hashes it, and only then publishes it under its final name.
//!
//! What the re-read does and does not prove: it compares what is on the destination (as the OS
//! serves it) with the bytes that were read from the card, so it catches short or failed writes and
//! destination-side corruption that the OS reports. It is *not* a cache-bypassing read of the
//! destination media, and the card is read only once, so corruption on the card side that returns
//! the same wrong bytes cannot be seen (a second card read, and dropping the page cache, are #602).
//!
//! Publishing never overwrites and never exposes a partial file: the temp file is hard-linked to its
//! final name (atomic, fails if the name exists) and the temp removed; on volumes without hard links
//! (FAT/exFAT cards' readers, some network shares) the name is claimed with `create_new` and the
//! temp renamed over the claim. An existing file of identical content is reported as already there;
//! a different one gets a `-N` suffix. The card is untrusted input: its symlinks are never followed
//! or listed, `dest_rel` is validated, and every existing destination folder it names is checked not
//! to be a link before anything is created under it.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Temp files end with this (never a catalogued extension, so Scruff ignores a leftover). A crash
/// can leave one behind; they are safe to delete when no import is running.
pub const PART_SUFFIX: &str = ".nicti-part";

const CHUNK: usize = 1024 * 1024;
/// How many `-N` suffixes to try before giving up on a name.
const MAX_SUFFIX: u32 = 9999;
/// A name longer than this (UTF-16 units, the NTFS/exFAT unit) is refused: the temp name and a
/// `-N` suffix are added to it and the whole must stay under the 255 limit.
const MAX_NAME_UNITS: usize = 200;

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// One importable file on a card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardFile {
    pub path: PathBuf,
    pub size: u64,
}

/// Everything RAW- or JPEG-shaped under `source`, in walk order. Symlinks are never listed or
/// followed. Paths that could not be read are returned separately, not dropped, so a card with an
/// unreadable folder doesn't look like a smaller card. Pass the returned paths to [`offer_file`].
pub fn list_card_files(source: &Path) -> (Vec<CardFile>, Vec<String>) {
    let mut files = Vec::new();
    let mut problems = Vec::new();
    for entry in crate::scruff::candidate_files(source) {
        match entry {
            Ok(path) => match fs::symlink_metadata(&path) {
                Ok(m) if m.is_file() => files.push(CardFile {
                    size: m.len(),
                    path,
                }),
                Ok(_) => {}
                Err(e) => problems.push(format!("{}: {e}", path.display())),
            },
            Err(e) => problems.push(e.to_string()),
        }
    }
    (files, problems)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Offered {
    /// Copied and checked; `dest` is the final path (possibly with a `-N` suffix).
    Copied {
        dest: PathBuf,
        blake3: String,
        size: u64,
    },
    /// A regular file with identical content was already at `dest`; nothing was written.
    AlreadyThere { dest: PathBuf, blake3: String },
    /// Stopped by `cancelled`; no temp file is left behind.
    Cancelled,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum OfferError {
    #[error("refusing {0}: {1}")]
    Refused(PathBuf, String),
    #[error("copying {0}: {1}")]
    Io(PathBuf, String),
    #[error("{0} did not read back identically from the destination (disk fault?)")]
    VerifyFailed(PathBuf),
}

/// A component that is safe as one file or folder name on any platform Nicti supports.
fn is_safe_component(name: &str) -> bool {
    const RESERVED: &[&str] = &[
        "con",
        "prn",
        "aux",
        "nul",
        "conin$",
        "conout$",
        "com1",
        "com2",
        "com3",
        "com4",
        "com5",
        "com6",
        "com7",
        "com8",
        "com9",
        "com\u{b9}",
        "com\u{b2}",
        "com\u{b3}",
        "lpt1",
        "lpt2",
        "lpt3",
        "lpt4",
        "lpt5",
        "lpt6",
        "lpt7",
        "lpt8",
        "lpt9",
        "lpt\u{b9}",
        "lpt\u{b2}",
        "lpt\u{b3}",
    ];
    let stem = name.split('.').next().unwrap_or("").trim_end();
    !name.is_empty()
        && name.encode_utf16().count() <= MAX_NAME_UNITS
        && name != "."
        && name != ".."
        && !name.ends_with(['.', ' '])
        && !name.to_lowercase().ends_with(PART_SUFFIX)
        && !name.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|'])
        && !name.chars().any(char::is_control)
        && !RESERVED.contains(&stem.to_lowercase().as_str())
}

/// `dest_rel` must be a plain relative path of safe components (no `..`, no root or drive prefix).
fn validate_rel(dest_rel: &Path) -> Result<Vec<String>, String> {
    let mut parts = Vec::new();
    for c in dest_rel.components() {
        match c {
            Component::Normal(n) => {
                let s = n.to_str().ok_or("a name is not valid Unicode")?;
                if !is_safe_component(s) {
                    return Err(format!("unsafe name {s:?}"));
                }
                parts.push(s.to_owned());
            }
            _ => return Err("destination must be a plain relative path".into()),
        }
    }
    if parts.is_empty() {
        return Err("empty destination".into());
    }
    Ok(parts)
}

/// `name` with `-n` before the extension: `DSC_1.NEF` -> `DSC_1-2.NEF`.
fn with_suffix(name: &str, n: u32) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem}-{n}.{ext}"),
        _ => format!("{name}-{n}"),
    }
}

/// Hashes `r`, polling `cancelled` between chunks. `None` = cancelled.
fn hash_stream(
    mut r: impl Read,
    cancelled: &dyn Fn() -> bool,
) -> io::Result<Option<(String, u64)>> {
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; CHUNK];
    let mut total = 0u64;
    loop {
        if cancelled() {
            return Ok(None);
        }
        match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                hasher.update(&buf[..n]);
                total += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(Some((hasher.finalize().to_hex().to_string(), total)))
}

pub(crate) fn read_back(
    path: &Path,
    cancelled: &dyn Fn() -> bool,
) -> io::Result<Option<(String, u64)>> {
    hash_stream(File::open(path)?, cancelled)
}

/// Walks the folders `dest_rel` names under `dest_root`, refusing any that is (or becomes) a link or
/// not a directory *before* creating anything below it, and creating the missing ones one level at
/// a time. Returns the folder, canonicalised and checked to be under the canonical root.
fn open_folder(dest_root: &Path, folders: &[String]) -> Result<PathBuf, String> {
    let root = fs::canonicalize(dest_root).map_err(|e| e.to_string())?;
    let mut cur = dest_root.to_path_buf();
    for f in folders {
        cur.push(f);
        match fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(format!("{} is a link; not followed", cur.display()))
            }
            Ok(m) if m.is_dir() => {}
            Ok(_) => return Err(format!("{} exists and is not a folder", cur.display())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&cur).map_err(|e| format!("creating {}: {e}", cur.display()))?;
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    let dir = fs::canonicalize(&cur).map_err(|e| e.to_string())?;
    if !dir.starts_with(&root) {
        return Err("destination folder resolves outside the destination root".into());
    }
    Ok(dir)
}

/// Makes `temp` visible as `final_path` without overwriting and without a half-written state:
/// a hard link where the volume has them, else claim the name and rename over the claim.
/// `ErrorKind::AlreadyExists` means the name is taken.
fn publish(temp: &Path, final_path: &Path, try_link: bool) -> io::Result<()> {
    if try_link {
        match fs::hard_link(temp, final_path) {
            Ok(()) => {
                let _ = fs::remove_file(temp);
                return Ok(());
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Err(e),
            Err(_) => {} // no hard links here: claim + rename below
        }
    }
    let claim = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(final_path)?;
    drop(claim);
    fs::rename(temp, final_path).inspect_err(|_| {
        let _ = fs::remove_file(final_path);
    })
}

/// Best-effort durability of the new directory entry (a crash right after `Copied` must not lose
/// it: the user is about to format the card). Not available everywhere.
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// Copies `src` (a file on the card, from [`list_card_files`]) to `dest_root/dest_rel`, checked,
/// never overwriting and never touching `src`. See the module docs. `cancelled` is polled between
/// chunks, including during the read-back (a read blocked inside the OS on a pulled card cannot be
/// interrupted; run this on a thread you can abandon).
pub fn offer_file(
    src: &Path,
    dest_root: &Path,
    dest_rel: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Result<Offered, OfferError> {
    offer_file_with(src, dest_root, dest_rel, cancelled, &read_back, true)
}

type ReadBack<'a> = &'a dyn Fn(&Path, &dyn Fn() -> bool) -> io::Result<Option<(String, u64)>>;

fn offer_file_with(
    src: &Path,
    dest_root: &Path,
    dest_rel: &Path,
    cancelled: &dyn Fn() -> bool,
    read_back: ReadBack<'_>,
    try_link: bool,
) -> Result<Offered, OfferError> {
    let refuse = |why: &str| OfferError::Refused(src.to_path_buf(), why.to_string());
    let io_err = |e: io::Error| OfferError::Io(src.to_path_buf(), e.to_string());

    let parts = validate_rel(dest_rel).map_err(|why| refuse(&why))?;
    let meta = fs::symlink_metadata(src).map_err(io_err)?;
    if !meta.is_file() {
        // Also refuses a symlink: `symlink_metadata` does not follow it, so it is not a file.
        return Err(refuse(
            "not a regular file (links on the card are not followed)",
        ));
    }

    let (name, folders) = parts.split_last().expect("validate_rel returns >= 1 part");
    let dir = open_folder(dest_root, folders).map_err(|why| refuse(&why))?;

    // Copy + hash in one pass into a temp file we exclusively created (never pre-deleted: another
    // offer in this process may own that name, so each gets its own).
    let pid = std::process::id();
    let (temp, mut out) = (0..8)
        .find_map(|_| {
            let temp = dir.join(format!(
                "{name}.{pid}-{}{PART_SUFFIX}",
                TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            match OpenOptions::new().write(true).create_new(true).open(&temp) {
                Ok(f) => Some(Ok((temp, f))),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => None,
                Err(e) => Some(Err(e)),
            }
        })
        .ok_or_else(|| refuse("could not create a temp file"))?
        .map_err(io_err)?;
    let cleanup = |temp: &Path| {
        let _ = fs::remove_file(temp);
    };

    let result = (|| -> Result<Option<(String, u64)>, OfferError> {
        let mut input = File::open(src).map_err(io_err)?;
        let opened = input.metadata().map_err(io_err)?;
        if !opened.is_file() {
            return Err(refuse("not a regular file"));
        }
        let mut hasher = blake3::Hasher::new();
        let mut buf = vec![0u8; CHUNK];
        let mut total = 0u64;
        loop {
            if cancelled() {
                return Ok(None);
            }
            let n = match input.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(io_err(e)),
            };
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n]).map_err(io_err)?;
            total += n as u64;
        }
        // A file still being written, or truncated, would otherwise verify against its own partial
        // hash and be reported Copied -- and the user would then format the card.
        let now = input.metadata().map_err(io_err)?.len();
        if total != opened.len() || total != now {
            return Err(refuse("the file changed size while it was being copied"));
        }
        out.sync_all().map_err(io_err)?;
        Ok(Some((hasher.finalize().to_hex().to_string(), total)))
    })();
    drop(out);
    let (hash, size) = match result {
        Ok(Some(done)) => done,
        Ok(None) => {
            cleanup(&temp);
            return Ok(Offered::Cancelled);
        }
        Err(e) => {
            cleanup(&temp);
            return Err(e);
        }
    };

    match read_back(&temp, cancelled) {
        Ok(Some((h, n))) if h == hash && n == size => {}
        Ok(Some(_)) => {
            cleanup(&temp);
            return Err(OfferError::VerifyFailed(temp));
        }
        Ok(None) => {
            cleanup(&temp);
            return Ok(Offered::Cancelled);
        }
        Err(e) => {
            cleanup(&temp);
            return Err(OfferError::Io(temp, format!("re-reading: {e}")));
        }
    }

    for n in 0..=MAX_SUFFIX {
        let candidate = if n == 0 {
            name.clone()
        } else {
            with_suffix(name, n)
        };
        let final_path = dir.join(&candidate);
        match publish(&temp, &final_path, try_link) {
            Ok(()) => {
                cleanup(&temp); // already gone after a link; the rename path consumed it
                sync_dir(&dir);
                return Ok(Offered::Copied {
                    dest: final_path,
                    blake3: hash,
                    size,
                });
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                // Same content already here (a re-run, or this card imported before): done. Only a
                // regular file of the right size is read (not a FIFO or a link, and no hashing of
                // every same-named file of a different size).
                let same = fs::symlink_metadata(&final_path)
                    .is_ok_and(|m| m.is_file() && m.len() == size)
                    && matches!(read_back(&final_path, cancelled), Ok(Some((h, _))) if h == hash);
                if same {
                    cleanup(&temp);
                    return Ok(Offered::AlreadyThere {
                        dest: final_path,
                        blake3: hash,
                    });
                }
            }
            Err(e) => {
                cleanup(&temp);
                return Err(io_err(e));
            }
        }
    }
    cleanup(&temp);
    Err(refuse("could not find a free name"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn never() -> bool {
        false
    }

    fn card(dir: &Path, rel: &str, bytes: &[u8]) -> PathBuf {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, bytes).unwrap();
        p
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        walkdir::WalkDir::new(dir)
            .into_iter()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(PART_SUFFIX))
            .map(|e| e.path().display().to_string())
            .collect()
    }

    #[test]
    fn copies_verified_bytes_and_never_touches_the_card() {
        let tmp = tempfile::tempdir().unwrap();
        let (cardd, dest) = (tmp.path().join("card"), tmp.path().join("lib"));
        fs::create_dir_all(&dest).unwrap();
        let bytes = vec![7u8; 3 * CHUNK + 123];
        let src = card(&cardd, "DCIM/100NIKON/DSC_0001.NEF", &bytes);

        let r = offer_file(&src, &dest, Path::new("2026/10/DSC_0001.NEF"), &never).unwrap();
        let Offered::Copied {
            dest: d,
            blake3,
            size,
        } = r
        else {
            panic!("{r:?}")
        };
        assert_eq!(
            d,
            fs::canonicalize(&dest)
                .unwrap()
                .join("2026/10/DSC_0001.NEF")
        );
        assert_eq!(size, bytes.len() as u64);
        assert_eq!(blake3, ::blake3::hash(&bytes).to_hex().to_string());
        assert_eq!(fs::read(&d).unwrap(), bytes);
        assert_eq!(fs::read(&src).unwrap(), bytes, "the card file is untouched");
        assert!(leftovers(tmp.path()).is_empty());
    }

    #[test]
    fn an_existing_different_file_is_never_overwritten_and_identical_content_is_not_duplicated() {
        let tmp = tempfile::tempdir().unwrap();
        let (cardd, dest) = (tmp.path().join("card"), tmp.path().join("lib"));
        fs::create_dir_all(&dest).unwrap();
        let a = card(&cardd, "one/IMG_0001.NEF", b"first card");
        let b = card(&cardd, "two/IMG_0001.NEF", b"second card, same name");
        let rel = Path::new("IMG_0001.NEF");

        let first = offer_file(&a, &dest, rel, &never).unwrap();
        let second = offer_file(&b, &dest, rel, &never).unwrap();
        let (Offered::Copied { dest: d1, .. }, Offered::Copied { dest: d2, .. }) = (first, second)
        else {
            panic!("both copy")
        };
        assert_ne!(d1, d2);
        assert!(d2.ends_with("IMG_0001-1.NEF"));
        assert_eq!(fs::read(&d1).unwrap(), b"first card");
        assert_eq!(fs::read(&d2).unwrap(), b"second card, same name");

        // Offering either again (a re-run of the import) writes nothing new.
        for src in [&a, &b] {
            let again = offer_file(src, &dest, rel, &never).unwrap();
            assert!(matches!(again, Offered::AlreadyThere { .. }), "{again:?}");
        }
        assert_eq!(fs::read_dir(&dest).unwrap().count(), 2);
    }

    #[test]
    fn destinations_that_could_escape_the_root_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let (cardd, dest) = (tmp.path().join("card"), tmp.path().join("lib"));
        fs::create_dir_all(&dest).unwrap();
        let src = card(&cardd, "a.NEF", b"x");
        for bad in [
            "../evil.NEF",
            "/abs/evil.NEF",
            "a/../../evil.NEF",
            "",
            "NUL.NEF",
            "a:b.NEF",
            "trail.",
            "x.nicti-part",
        ] {
            let r = offer_file(&src, &dest, Path::new(bad), &never);
            assert!(matches!(r, Err(OfferError::Refused(..))), "{bad:?}: {r:?}");
        }
        assert!(!tmp.path().join("evil.NEF").exists());
        assert_eq!(fs::read_dir(&dest).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_folder_in_the_destination_cannot_redirect_a_copy_outside_it() {
        let tmp = tempfile::tempdir().unwrap();
        let (cardd, dest, outside) = (
            tmp.path().join("card"),
            tmp.path().join("lib"),
            tmp.path().join("outside"),
        );
        fs::create_dir_all(&dest).unwrap();
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, dest.join("2026")).unwrap();
        let src = card(&cardd, "a.NEF", b"x");

        let r = offer_file(&src, &dest, Path::new("2026/a.NEF"), &never);
        assert!(matches!(r, Err(OfferError::Refused(..))), "{r:?}");
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_on_the_card_is_refused_and_not_listed() {
        let tmp = tempfile::tempdir().unwrap();
        let (cardd, dest) = (tmp.path().join("card"), tmp.path().join("lib"));
        fs::create_dir_all(&dest).unwrap();
        let secret = tmp.path().join("secret.txt");
        fs::write(&secret, b"private").unwrap();
        fs::create_dir_all(&cardd).unwrap();
        let link = cardd.join("evil.NEF");
        std::os::unix::fs::symlink(&secret, &link).unwrap();

        let r = offer_file(&link, &dest, Path::new("evil.NEF"), &never);
        assert!(matches!(r, Err(OfferError::Refused(..))), "{r:?}");
        assert!(list_card_files(&cardd).0.is_empty());
        assert_eq!(fs::read_dir(&dest).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_already_at_the_final_name_is_never_written_through_or_reported_as_identical() {
        let tmp = tempfile::tempdir().unwrap();
        let (cardd, dest) = (tmp.path().join("card"), tmp.path().join("lib"));
        fs::create_dir_all(&dest).unwrap();
        let victim = tmp.path().join("victim.txt");
        fs::write(&victim, b"photo").unwrap(); // even identical content behind the link
        std::os::unix::fs::symlink(&victim, dest.join("a.NEF")).unwrap();
        let src = card(&cardd, "a.NEF", b"photo");

        let r = offer_file(&src, &dest, Path::new("a.NEF"), &never).unwrap();
        let Offered::Copied { dest: d, .. } = r else {
            panic!("a link is not 'already there': {r:?}")
        };
        assert!(d.ends_with("a-1.NEF"));
        assert_eq!(fs::read(&victim).unwrap(), b"photo");
        assert!(leftovers(tmp.path()).is_empty());
    }

    #[test]
    fn cancelling_leaves_no_temp_file_and_no_final_file() {
        let tmp = tempfile::tempdir().unwrap();
        let (cardd, dest) = (tmp.path().join("card"), tmp.path().join("lib"));
        fs::create_dir_all(&dest).unwrap();
        let src = card(&cardd, "a.NEF", &vec![1u8; 2 * CHUNK]);

        let r = offer_file(&src, &dest, Path::new("a.NEF"), &|| true).unwrap();
        assert_eq!(r, Offered::Cancelled);
        assert_eq!(fs::read_dir(&dest).unwrap().count(), 0);
    }

    #[test]
    fn a_read_back_that_differs_fails_the_offer_and_leaves_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let (cardd, dest) = (tmp.path().join("card"), tmp.path().join("lib"));
        fs::create_dir_all(&dest).unwrap();
        let src = card(&cardd, "a.NEF", b"what the card held");
        // The destination returns different bytes than were written.
        let corrupt = |_: &Path, _: &dyn Fn() -> bool| {
            Ok(Some((
                ::blake3::hash(b"bit flipped").to_hex().to_string(),
                18,
            )))
        };
        let r = offer_file_with(&src, &dest, Path::new("a.NEF"), &never, &corrupt, true);
        assert!(matches!(r, Err(OfferError::VerifyFailed(_))), "{r:?}");
        assert_eq!(fs::read_dir(&dest).unwrap().count(), 0, "no final, no temp");
        // ...and a read-back of the wrong length is caught too.
        let short = |_: &Path, _: &dyn Fn() -> bool| {
            Ok(Some((
                ::blake3::hash(b"what the card held").to_hex().to_string(),
                3,
            )))
        };
        let r = offer_file_with(&src, &dest, Path::new("a.NEF"), &never, &short, true);
        assert!(matches!(r, Err(OfferError::VerifyFailed(_))), "{r:?}");
        assert_eq!(fs::read_dir(&dest).unwrap().count(), 0);
    }

    #[test]
    fn publishing_without_hard_links_gives_the_same_result() {
        let tmp = tempfile::tempdir().unwrap();
        let (cardd, dest) = (tmp.path().join("card"), tmp.path().join("lib"));
        fs::create_dir_all(&dest).unwrap();
        let a = card(&cardd, "one/a.NEF", b"first");
        let b = card(&cardd, "two/a.NEF", b"second");
        for src in [&a, &b, &a] {
            offer_file_with(src, &dest, Path::new("a.NEF"), &never, &read_back, false).unwrap();
        }
        assert_eq!(fs::read(dest.join("a.NEF")).unwrap(), b"first");
        assert_eq!(fs::read(dest.join("a-1.NEF")).unwrap(), b"second");
        assert_eq!(
            fs::read_dir(&dest).unwrap().count(),
            2,
            "the repeat of `a` was already there"
        );
    }

    #[test]
    fn publishing_never_shows_an_empty_or_partial_final_file() {
        // The final name appears only when its content is complete (hard link of a finished temp).
        let tmp = tempfile::tempdir().unwrap();
        let (temp, fin) = (tmp.path().join("t.nicti-part"), tmp.path().join("f.NEF"));
        fs::write(&temp, b"complete").unwrap();
        publish(&temp, &fin, true).unwrap();
        assert_eq!(fs::read(&fin).unwrap(), b"complete");
        assert!(!temp.exists());
        fs::write(&temp, b"other").unwrap();
        let e = publish(&temp, &fin, true).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&fin).unwrap(), b"complete", "never overwritten");
    }

    #[test]
    fn a_multi_level_symlinked_folder_creates_nothing_outside_the_root() {
        #[cfg(unix)]
        {
            let tmp = tempfile::tempdir().unwrap();
            let (cardd, dest, outside) = (
                tmp.path().join("card"),
                tmp.path().join("lib"),
                tmp.path().join("outside"),
            );
            fs::create_dir_all(&dest).unwrap();
            fs::create_dir_all(&outside).unwrap();
            std::os::unix::fs::symlink(&outside, dest.join("2026")).unwrap();
            let src = card(&cardd, "a.NEF", b"x");

            let r = offer_file(&src, &dest, Path::new("2026/10/22/a.NEF"), &never);
            assert!(matches!(r, Err(OfferError::Refused(..))), "{r:?}");
            assert_eq!(
                fs::read_dir(&outside).unwrap().count(),
                0,
                "no 10/ created outside"
            );
        }
    }

    #[test]
    fn concurrent_offers_to_the_same_name_each_get_their_own_temp_and_name() {
        let tmp = tempfile::tempdir().unwrap();
        let (cardd, dest) = (tmp.path().join("card"), tmp.path().join("lib"));
        fs::create_dir_all(&dest).unwrap();
        let srcs: Vec<PathBuf> = (0..8)
            .map(|i| {
                card(
                    &cardd,
                    &format!("c{i}/DSC_0001.NEF"),
                    format!("card {i}").as_bytes(),
                )
            })
            .collect();
        let results: Vec<_> = std::thread::scope(|scope| {
            let handles: Vec<_> = srcs
                .iter()
                .map(|src| {
                    let dest = &dest;
                    scope.spawn(move || offer_file(src, dest, Path::new("DSC_0001.NEF"), &never))
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for r in &results {
            assert!(matches!(r, Ok(Offered::Copied { .. })), "{r:?}");
        }
        let mut contents: Vec<String> = fs::read_dir(&dest)
            .unwrap()
            .map(|e| fs::read_to_string(e.unwrap().path()).unwrap())
            .collect();
        contents.sort();
        let want: Vec<String> = (0..8).map(|i| format!("card {i}")).collect();
        assert_eq!(
            contents, want,
            "eight distinct files, none lost or corrupted"
        );
        assert!(leftovers(tmp.path()).is_empty());
    }

    #[test]
    fn a_file_that_changes_size_while_it_is_copied_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let (cardd, dest) = (tmp.path().join("card"), tmp.path().join("lib"));
        fs::create_dir_all(&dest).unwrap();
        let src = card(&cardd, "a.NEF", b"still being written");
        let grown = std::cell::Cell::new(false);
        let grow = || {
            if !grown.replace(true) {
                let mut f = OpenOptions::new().append(true).open(&src).unwrap();
                f.write_all(b" and then some more").unwrap();
            }
            false
        };
        let r = offer_file(&src, &dest, Path::new("a.NEF"), &grow);
        assert!(matches!(r, Err(OfferError::Refused(..))), "{r:?}");
        assert_eq!(fs::read_dir(&dest).unwrap().count(), 0);
    }

    #[test]
    fn cancelling_mid_stream_and_during_the_read_back_leaves_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let (cardd, dest) = (tmp.path().join("card"), tmp.path().join("lib"));
        fs::create_dir_all(&dest).unwrap();
        let src = card(&cardd, "a.NEF", &vec![1u8; 3 * CHUNK]);
        for stop_after in [2usize, 5, 7] {
            let polls = std::cell::Cell::new(0usize);
            let cancel = || {
                polls.set(polls.get() + 1);
                polls.get() > stop_after
            };
            let r = offer_file(&src, &dest, Path::new("a.NEF"), &cancel).unwrap();
            assert_eq!(r, Offered::Cancelled, "stop_after={stop_after}");
            assert_eq!(
                fs::read_dir(&dest).unwrap().count(),
                0,
                "stop_after={stop_after}"
            );
        }
    }

    #[test]
    fn hostile_names_are_refused_on_every_platform() {
        let long = format!("{}.NEF", "a".repeat(201));
        let bad = [
            "x ".to_string(),
            "COM1.txt".into(),
            "com\u{b9}.NEF".into(),
            "CONOUT$".into(),
            "a*b.NEF".into(),
            "a?b.NEF".into(),
            "a\\b.NEF".into(),
            "a|b.NEF".into(),
            "a\u{1}b.NEF".into(),
            "x.NICTI-PART".into(),
            "nul ".into(),
            long,
        ];
        for name in &bad {
            assert!(!is_safe_component(name), "{name:?}");
        }
        assert!(is_safe_component("DSC_0001.NEF"));
        assert!(is_safe_component("console.NEF"));
    }

    #[test]
    fn a_missing_source_fails_cleanly_with_nothing_left_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("lib");
        fs::create_dir_all(&dest).unwrap();
        let r = offer_file(
            &tmp.path().join("gone.NEF"),
            &dest,
            Path::new("a.NEF"),
            &never,
        );
        assert!(matches!(r, Err(OfferError::Io(..))), "{r:?}");
        assert!(leftovers(tmp.path()).is_empty());
    }

    #[test]
    fn list_card_files_finds_raw_and_jpeg_only_and_sorts_out_sidecars() {
        let tmp = tempfile::tempdir().unwrap();
        let c = tmp.path();
        card(c, "DCIM/100NIKON/DSC_1.NEF", b"raw");
        card(c, "DCIM/100NIKON/DSC_1.JPG", b"jpeg");
        card(c, "DCIM/100NIKON/DSC_1.NEF.thumb.jpg", b"our sidecar");
        card(c, "DCIM/100NIKON/NOTES.TXT", b"text");
        card(c, "MISC/AUTOPRN.MRK", b"mark");
        let (files, problems) = list_card_files(c);
        assert!(problems.is_empty());
        let mut names: Vec<String> = files
            .iter()
            .map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, vec!["DSC_1.JPG", "DSC_1.NEF"]);
        assert_eq!(files.iter().map(|f| f.size).sum::<u64>(), 7);
    }

    #[test]
    fn suffix_goes_before_the_extension() {
        assert_eq!(with_suffix("DSC_1.NEF", 2), "DSC_1-2.NEF");
        assert_eq!(with_suffix("noext", 3), "noext-3");
        assert_eq!(with_suffix(".hidden", 1), ".hidden-1");
    }
}
