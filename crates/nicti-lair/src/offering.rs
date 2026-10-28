//! Copy-only import from a camera card (#548, ADR-0436 sections 3-6): "offering" -- the way a cat
//! brings its catch home. Files are copied, verified, and *never* deleted from the card; the user
//! formats the card, Nicti is never the only holder of the only copy (ADR-0026's "no unverified
//! deletes").
//!
//! This is the engine only: list what a card holds ([`list_card_files`]) and place one file under a
//! destination root ([`offer_file`]). Choosing destination names (the `naming.rs` templates) and
//! registering the copies is the caller's -- a UI job (follow-up) renders each `dest_rel`, calls
//! [`offer_file`] per file off the UI thread, then runs `scruff::ingest_root` on the destination.
//!
//! Each copy streams the card file once while hashing it (BLAKE3), writes to a temp name next to the
//! final one, `sync`s it, **re-reads it from the destination disk** and re-hashes (the `Carry`
//! pattern: a bad cable or a failing card shows up here, not later), and only then gives it its
//! final name. The final name is claimed with `create_new`, so an existing file is never
//! overwritten: identical content is reported as already there, different content gets a `-N`
//! suffix. The card is untrusted input: its symlinks are never followed, and `dest_rel` is
//! validated and checked to stay under the destination root.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

/// Temp files end with this (never a catalogued extension, so Scruff ignores a leftover).
pub const PART_SUFFIX: &str = ".nicti-part";

const CHUNK: usize = 1024 * 1024;
/// How many `-N` suffixes to try before giving up on a name.
const MAX_SUFFIX: u32 = 9999;

/// One importable file on a card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardFile {
    pub path: PathBuf,
    pub size: u64,
}

/// Everything RAW- or JPEG-shaped under `source`, in walk order. Symlinks are never listed or
/// followed. Paths that could not be read are returned separately, not dropped, so a card with an
/// unreadable folder doesn't look like a smaller card.
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
    /// Copied and verified; `dest` is the final path (possibly with a `-N` suffix).
    Copied {
        dest: PathBuf,
        blake3: String,
        size: u64,
    },
    /// A file with identical content was already at `dest`; nothing was written.
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
    #[error("{0} did not read back identically from the destination (card or disk fault?)")]
    VerifyFailed(PathBuf),
}

/// A component that is safe as one file or folder name on any platform Nicti supports.
fn is_safe_component(name: &str) -> bool {
    const RESERVED: &[&str] = &[
        "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
        "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
    ];
    let stem = name.split('.').next().unwrap_or("").trim_end();
    !name.is_empty()
        && name.len() <= 255
        && name != "."
        && name != ".."
        && !name.ends_with(['.', ' '])
        && !name.ends_with(PART_SUFFIX)
        && !name.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|'])
        && !name.chars().any(char::is_control)
        && !RESERVED.contains(&stem.to_ascii_lowercase().as_str())
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

fn hash_stream(mut r: impl Read) -> io::Result<(String, u64)> {
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; CHUNK];
    let mut total = 0u64;
    loop {
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
    Ok((hasher.finalize().to_hex().to_string(), total))
}

/// Re-reads `path` from disk and checks it hashes to `expected`.
fn verify_on_disk(path: &Path, expected: &str) -> Result<(), OfferError> {
    let read_back = File::open(path)
        .and_then(hash_stream)
        .map_err(|e| OfferError::Io(path.to_path_buf(), format!("re-reading: {e}")))?;
    if read_back.0 == expected {
        Ok(())
    } else {
        Err(OfferError::VerifyFailed(path.to_path_buf()))
    }
}

/// Copies `src` (a file on the card) to `dest_root/dest_rel`, verified, never overwriting and never
/// touching `src`. See the module docs. `cancelled` is polled between chunks.
pub fn offer_file(
    src: &Path,
    dest_root: &Path,
    dest_rel: &Path,
    cancelled: &dyn Fn() -> bool,
) -> Result<Offered, OfferError> {
    let refuse = |why: &str| OfferError::Refused(src.to_path_buf(), why.to_string());
    let io_err = |e: io::Error| OfferError::Io(src.to_path_buf(), e.to_string());

    let parts = validate_rel(dest_rel).map_err(|why| refuse(&why))?;
    let meta = fs::symlink_metadata(src).map_err(io_err)?;
    if meta.file_type().is_symlink() {
        return Err(refuse("a symbolic link on the card is not followed"));
    }
    if !meta.is_file() {
        return Err(refuse("not a regular file"));
    }

    // Build the destination folder, then check *where it really is*: a symlinked folder already in
    // the destination tree must not let a name escape the root.
    let (name, folders) = parts.split_last().expect("validate_rel returns >= 1 part");
    let root = fs::canonicalize(dest_root).map_err(io_err)?;
    let dir = folders
        .iter()
        .fold(dest_root.to_path_buf(), |p, f| p.join(f));
    fs::create_dir_all(&dir).map_err(io_err)?;
    let dir = fs::canonicalize(&dir).map_err(io_err)?;
    if !dir.starts_with(&root) {
        return Err(refuse(
            "destination folder resolves outside the destination root",
        ));
    }

    // Copy + hash in one pass into a temp file we exclusively created.
    let temp = dir.join(format!("{name}.{}{PART_SUFFIX}", std::process::id()));
    let _ = fs::remove_file(&temp); // a leftover of ours from a crash; unlinks a link, never follows it
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(io_err)?;
    let cleanup = |temp: &Path| {
        let _ = fs::remove_file(temp);
    };

    let result = (|| -> Result<Option<(String, u64)>, OfferError> {
        let mut input = File::open(src).map_err(io_err)?;
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

    // The read-back from the destination disk: what we hashed while copying must be what is there.
    if let Err(e) = verify_on_disk(&temp, &hash) {
        cleanup(&temp);
        return Err(e);
    }

    // Claim a final name without ever overwriting: `create_new` is the atomic "this name is mine".
    for n in 0..=MAX_SUFFIX {
        let candidate = if n == 0 {
            name.clone()
        } else {
            with_suffix(name, n)
        };
        let final_path = dir.join(&candidate);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&final_path)
        {
            Ok(claim) => {
                drop(claim);
                return match fs::rename(&temp, &final_path) {
                    Ok(()) => Ok(Offered::Copied {
                        dest: final_path,
                        blake3: hash,
                        size,
                    }),
                    Err(e) => {
                        let _ = fs::remove_file(&final_path);
                        cleanup(&temp);
                        Err(io_err(e))
                    }
                };
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                // Same content already here (a re-run, or the card imported before): done.
                let existing = File::open(&final_path).and_then(hash_stream);
                if existing.is_ok_and(|(h, _)| h == hash) {
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

    #[test]
    fn a_planted_temp_symlink_is_never_written_through() {
        #[cfg(unix)]
        {
            let tmp = tempfile::tempdir().unwrap();
            let (cardd, dest) = (tmp.path().join("card"), tmp.path().join("lib"));
            fs::create_dir_all(&dest).unwrap();
            let victim = tmp.path().join("victim.txt");
            fs::write(&victim, b"precious").unwrap();
            std::os::unix::fs::symlink(
                &victim,
                dest.join(format!("a.NEF.{}{PART_SUFFIX}", std::process::id())),
            )
            .unwrap();
            let src = card(&cardd, "a.NEF", b"photo");
            offer_file(&src, &dest, Path::new("a.NEF"), &never).unwrap();
            assert_eq!(fs::read(&victim).unwrap(), b"precious");
        }
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
    fn the_read_back_check_catches_a_copy_that_differs_on_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("x.NEF");
        fs::write(&p, b"what the card held").unwrap();
        let expected = ::blake3::hash(b"what the card held").to_hex().to_string();
        assert_eq!(verify_on_disk(&p, &expected), Ok(()));
        fs::write(&p, b"what the card held, but a bit flipped").unwrap();
        assert_eq!(
            verify_on_disk(&p, &expected),
            Err(OfferError::VerifyFailed(p.clone()))
        );
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
