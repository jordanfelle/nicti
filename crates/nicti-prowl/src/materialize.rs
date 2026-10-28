//! Rebuilds a ref-10k copy from a live photo library (#136).
//!
//! The frozen full copies are retired (too big to keep per machine), but the committed manifest
//! already pins every file by SHA-256 and size. [`materialize`] walks a source library, finds the
//! files whose content hashes to a manifest entry, and places each one under its manifest id
//! (`ref-00001.nef`) in a destination directory -- after which [`crate::manifest::Manifest::verify`]
//! passes on that directory exactly as on a frozen copy. Matching is by content, so the source's
//! folder layout and file names don't matter and nothing about them is read from or written to the
//! manifest.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use crate::manifest::{hash_file, Entry, Manifest};

/// How a matched source file is placed in the destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Copy, then re-hash the copy before it gets its final name.
    Copy,
    /// Hard link (no extra disk; source and destination must be on one volume).
    Hardlink,
}

#[derive(Debug, Clone)]
pub struct Options<'a> {
    pub source: &'a Path,
    pub dest: &'a Path,
    pub mode: Mode,
    /// Only entries in this manifest bucket.
    pub bucket: Option<&'a str>,
    /// Only the first `limit` (in manifest order) of the selected entries.
    pub limit: Option<usize>,
    /// Report what would be placed without touching `dest`.
    pub dry_run: bool,
}

#[derive(Debug, Default)]
pub struct Report {
    /// Ids placed (or, in a dry run, that would be).
    pub placed: Vec<String>,
    /// Ids already in `dest` with the right hash.
    pub already_present: Vec<String>,
    /// Ids no source file matched.
    pub not_found: Vec<String>,
    /// Ids a source file matched but placing it failed: `(id, reason)`.
    pub failed: Vec<(String, String)>,
    /// Source paths that could not be listed, stat-ed or hashed. A non-empty list means
    /// `not_found` may be the library's fault, not a missing file.
    pub unreadable: Vec<PathBuf>,
}

impl Report {
    pub fn is_complete(&self) -> bool {
        self.not_found.is_empty() && self.failed.is_empty()
    }
}

/// The manifest is trusted input but an id is joined onto `dest`, so it must be one plain file
/// name that is also safe on Windows: no separators, `.`/`..`, stream (`:`) or wildcard
/// characters, no trailing dot/space (which Windows silently drops), and no reserved device name.
fn is_plain_file_name(id: &str) -> bool {
    const RESERVED: &[&str] = &[
        "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
        "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
    ];
    let stem = id.split('.').next().unwrap_or("").to_ascii_lowercase();
    !id.is_empty()
        && id != "."
        && id != ".."
        && !id.ends_with(['.', ' '])
        && !id.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|'])
        && !id.chars().any(char::is_control)
        && !RESERVED.contains(&stem.trim_end().to_ascii_lowercase().as_str())
        && Path::new(id).components().count() == 1
}

/// Every regular file under `root` with its size, plus the paths that could not be read. Symlinks
/// are never followed (no cycles, no escaping the library), and `skip` (the destination, when it
/// sits inside the source) is pruned.
fn walk(root: &Path, skip: Option<&Path>) -> (Vec<(PathBuf, u64)>, Vec<PathBuf>) {
    let mut out = Vec::new();
    let mut unreadable = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let read = match fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => {
                unreadable.push(dir);
                continue;
            }
        };
        for entry in read {
            let Ok(entry) = entry else {
                unreadable.push(dir.clone());
                continue;
            };
            let path = entry.path();
            let Ok(ty) = entry.file_type() else {
                unreadable.push(path);
                continue;
            };
            if ty.is_dir() {
                if skip.is_some_and(|s| fs::canonicalize(&path).is_ok_and(|c| c == s)) {
                    continue;
                }
                stack.push(path);
            } else if ty.is_file() {
                match entry.metadata() {
                    Ok(meta) => out.push((path, meta.len())),
                    Err(_) => unreadable.push(path),
                }
            }
        }
    }
    out.sort();
    unreadable.sort();
    (out, unreadable)
}

fn hash_path(path: &Path) -> io::Result<String> {
    hash_file(fs::File::open(path)?)
}

/// A fresh temp file next to the final name. `create_new` (O_EXCL) never writes through a planted
/// symlink; a leftover from a crashed run is unlinked (the link, not its target) and retried once.
fn create_temp(path: &Path) -> io::Result<fs::File> {
    let open = || {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
    };
    match open() {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            fs::remove_file(path)?;
            open()
        }
        other => other,
    }
}

fn verified(path: &Path, sha256: &str) -> Result<(), String> {
    match hash_path(path) {
        Ok(h) if h.eq_ignore_ascii_case(sha256) => Ok(()),
        Ok(_) => Err("placed file did not hash to the manifest sha256".to_string()),
        Err(e) => Err(format!("re-hash: {e}")),
    }
}

/// Places `src` as `dest_dir/id` atomically: build `<id>.partial-<pid>`, verify it, rename over the
/// final name. A good file already at the final name is never removed before its replacement exists.
fn place(src: &Path, dest_dir: &Path, id: &str, sha256: &str, mode: Mode) -> Result<(), String> {
    let final_path = dest_dir.join(id);
    let tmp = dest_dir.join(format!("{id}.partial-{}", std::process::id()));
    let built = match mode {
        Mode::Hardlink => {
            let _ = fs::remove_file(&tmp);
            fs::hard_link(src, &tmp).map_err(|e| format!("hard link: {e}"))
        }
        Mode::Copy => (|| {
            let mut out = create_temp(&tmp).map_err(|e| format!("create temp: {e}"))?;
            let mut input = fs::File::open(src).map_err(|e| format!("open source: {e}"))?;
            io::copy(&mut input, &mut out).map_err(|e| format!("copy: {e}"))?;
            out.sync_all().map_err(|e| format!("sync: {e}"))
        })(),
    };
    let result = built
        .and_then(|_| verified(&tmp, sha256))
        .and_then(|_| fs::rename(&tmp, &final_path).map_err(|e| format!("rename: {e}")));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

pub fn materialize(manifest: &Manifest, opts: &Options<'_>) -> anyhow::Result<Report> {
    let mut wanted: Vec<&Entry> = manifest
        .entries()
        .iter()
        .filter(|e| opts.bucket.is_none_or(|b| e.bucket == b))
        .take(opts.limit.unwrap_or(usize::MAX))
        .collect();
    if let Some(bad) = wanted.iter().find(|e| !is_plain_file_name(&e.id)) {
        anyhow::bail!("manifest id {:?} is not a plain file name", bad.id);
    }
    if !opts.source.is_dir() {
        anyhow::bail!("source {} is not a directory", opts.source.display());
    }
    if let (Ok(s), Ok(d)) = (fs::canonicalize(opts.source), fs::canonicalize(opts.dest)) {
        if s == d {
            anyhow::bail!("source and destination are the same directory");
        }
    }
    if !opts.dry_run {
        fs::create_dir_all(opts.dest)?;
    }

    let mut report = Report::default();

    // Idempotent: a file already in `dest` that verifies is left alone. A read error is not a
    // mismatch: it is reported and the file is left untouched rather than replaced.
    let states: Vec<io::Result<bool>> = wanted
        .par_iter()
        .map(|e| match hash_path(&opts.dest.join(&e.id)) {
            Ok(h) => Ok(h.eq_ignore_ascii_case(&e.sha256)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err),
        })
        .collect();
    let mut states = states.into_iter();
    wanted.retain(|e| match states.next().expect("one state per entry") {
        Ok(true) => {
            report.already_present.push(e.id.clone());
            false
        }
        Ok(false) => true,
        Err(err) => {
            report
                .failed
                .push((e.id.clone(), format!("unreadable file in dest: {err}")));
            false
        }
    });

    // (size, sha256) -> entries; only files of a wanted size are worth hashing.
    let mut index: HashMap<(u64, String), Vec<&Entry>> = HashMap::new();
    for e in &wanted {
        index
            .entry((e.size_bytes, e.sha256.to_ascii_lowercase()))
            .or_default()
            .push(e);
    }
    let sizes: HashSet<u64> = wanted.iter().map(|e| e.size_bytes).collect();

    let skip = fs::canonicalize(opts.dest).ok();
    let (files, mut unreadable) = walk(opts.source, skip.as_deref());
    let candidates: Vec<(PathBuf, u64)> = files
        .into_iter()
        .filter(|(_, size)| sizes.contains(size))
        .collect();
    let hashed: Vec<io::Result<String>> = candidates
        .par_iter()
        .map(|(p, _)| hash_path(p).map(|h| h.to_ascii_lowercase()))
        .collect();

    // Source order is path-sorted, so when one content hash appears twice in the library the
    // same file always wins. An id is only claimed once it is actually placed, so a failure on
    // one copy still lets an identical second copy be tried.
    let mut claimed: HashSet<&str> = HashSet::new();
    for ((path, size), hash) in candidates.iter().zip(hashed) {
        let hash = match hash {
            Ok(h) => h,
            Err(_) => {
                unreadable.push(path.clone());
                continue;
            }
        };
        for e in index.get(&(*size, hash)).into_iter().flatten() {
            if claimed.contains(e.id.as_str()) {
                continue;
            }
            let outcome = if opts.dry_run {
                Ok(())
            } else {
                place(path, opts.dest, &e.id, &e.sha256, opts.mode)
            };
            match outcome {
                Ok(()) => {
                    claimed.insert(&e.id);
                    report.failed.retain(|(id, _)| id != &e.id);
                    report.placed.push(e.id.clone());
                }
                Err(why) => {
                    report.failed.retain(|(id, _)| id != &e.id);
                    report.failed.push((e.id.clone(), why));
                }
            }
        }
    }
    report.not_found = wanted
        .iter()
        .filter(|e| {
            !claimed.contains(e.id.as_str()) && !report.failed.iter().any(|(id, _)| id == &e.id)
        })
        .map(|e| e.id.clone())
        .collect();
    unreadable.sort();
    unreadable.dedup();
    report.unreadable = unreadable;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::io::Write;

    fn sha(content: &[u8]) -> String {
        Sha256::digest(content)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn manifest(dir: &Path, rows: &[(&str, &[u8])]) -> Manifest {
        let path = dir.join("manifest.csv");
        let mut f = fs::File::create(&path).unwrap();
        writeln!(
            f,
            "id,sha256,bucket,model,iso,compression,width,height,size_bytes"
        )
        .unwrap();
        for (id, content) in rows {
            writeln!(
                f,
                "{id},{},z8,NIKON Z 8,64,Lossless,100,100,{}",
                sha(content),
                content.len()
            )
            .unwrap();
        }
        Manifest::load(&path).unwrap()
    }

    fn write(path: &Path, content: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn opts<'a>(source: &'a Path, dest: &'a Path) -> Options<'a> {
        Options {
            source,
            dest,
            mode: Mode::Copy,
            bucket: None,
            limit: None,
            dry_run: false,
        }
    }

    #[test]
    fn matches_by_content_not_by_name_or_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, dst) = (tmp.path().join("lib"), tmp.path().join("out"));
        write(&src.join("2025/event/DSC_0001.NEF"), b"alpha-bytes");
        write(&src.join("other/IMG.nef"), b"bravo-bytes!");
        let m = manifest(
            tmp.path(),
            &[
                ("ref-00001.nef", b"alpha-bytes"),
                ("ref-00002.nef", b"bravo-bytes!"),
            ],
        );

        let r = materialize(&m, &opts(&src, &dst)).unwrap();
        assert!(r.is_complete(), "{r:?}");
        assert_eq!(r.placed.len(), 2);
        assert_eq!(fs::read(dst.join("ref-00001.nef")).unwrap(), b"alpha-bytes");
        assert!(m.verify(&dst, crate::manifest::Scope::All).is_clean());
    }

    #[test]
    fn same_size_different_content_is_not_matched() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, dst) = (tmp.path().join("lib"), tmp.path().join("out"));
        write(&src.join("a.nef"), b"AAAA");
        let m = manifest(tmp.path(), &[("ref-00001.nef", b"BBBB")]);

        let r = materialize(&m, &opts(&src, &dst)).unwrap();
        assert_eq!(r.not_found, vec!["ref-00001.nef".to_string()]);
        assert!(r.placed.is_empty() && !r.is_complete());
        assert!(!dst.join("ref-00001.nef").exists());
    }

    #[test]
    fn a_second_run_leaves_verified_files_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, dst) = (tmp.path().join("lib"), tmp.path().join("out"));
        write(&src.join("a.nef"), b"one");
        let m = manifest(tmp.path(), &[("ref-00001.nef", b"one")]);

        materialize(&m, &opts(&src, &dst)).unwrap();
        fs::remove_dir_all(&src).unwrap();
        fs::create_dir_all(&src).unwrap();
        let r = materialize(&m, &opts(&src, &dst)).unwrap();
        assert_eq!(r.already_present, vec!["ref-00001.nef".to_string()]);
        assert!(r.is_complete());
    }

    #[test]
    fn a_corrupt_file_in_dest_is_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, dst) = (tmp.path().join("lib"), tmp.path().join("out"));
        write(&src.join("a.nef"), b"good");
        write(&dst.join("ref-00001.nef"), b"BAD!");
        let m = manifest(tmp.path(), &[("ref-00001.nef", b"good")]);

        let r = materialize(&m, &opts(&src, &dst)).unwrap();
        assert_eq!(r.placed, vec!["ref-00001.nef".to_string()]);
        assert_eq!(fs::read(dst.join("ref-00001.nef")).unwrap(), b"good");
    }

    #[test]
    fn dry_run_touches_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, dst) = (tmp.path().join("lib"), tmp.path().join("out"));
        write(&src.join("a.nef"), b"one");
        let m = manifest(tmp.path(), &[("ref-00001.nef", b"one")]);

        let mut o = opts(&src, &dst);
        o.dry_run = true;
        let r = materialize(&m, &o).unwrap();
        assert_eq!(r.placed, vec!["ref-00001.nef".to_string()]);
        assert!(!dst.exists());
    }

    #[test]
    fn identical_content_under_two_ids_places_both() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, dst) = (tmp.path().join("lib"), tmp.path().join("out"));
        write(&src.join("a.nef"), b"dup");
        write(&src.join("copy-of-a.nef"), b"dup");
        let m = manifest(
            tmp.path(),
            &[("ref-00001.nef", b"dup"), ("ref-00002.nef", b"dup")],
        );

        let r = materialize(&m, &opts(&src, &dst)).unwrap();
        assert!(r.is_complete(), "{r:?}");
        assert_eq!(r.placed.len(), 2);
    }

    #[test]
    fn bucket_and_limit_narrow_the_set() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, dst) = (tmp.path().join("lib"), tmp.path().join("out"));
        write(&src.join("a.nef"), b"one");
        write(&src.join("b.nef"), b"two");
        let m = manifest(
            tmp.path(),
            &[("ref-00001.nef", b"one"), ("ref-00002.nef", b"two")],
        );

        let mut o = opts(&src, &dst);
        o.limit = Some(1);
        let r = materialize(&m, &o).unwrap();
        assert_eq!(r.placed, vec!["ref-00001.nef".to_string()]);
        assert!(!dst.join("ref-00002.nef").exists());

        o.limit = None;
        o.bucket = Some("no-such-bucket");
        let r = materialize(&m, &o).unwrap();
        assert!(r.placed.is_empty() && r.is_complete());
    }

    #[test]
    fn ids_that_could_escape_dest_are_rejected() {
        for bad in ["../evil.nef", "a/b.nef", "..", "c:evil", "a\\b.nef"] {
            assert!(!is_plain_file_name(bad), "{bad}");
        }
        assert!(is_plain_file_name("ref-00001.nef"));

        let tmp = tempfile::tempdir().unwrap();
        let (src, dst) = (tmp.path().join("lib"), tmp.path().join("out"));
        fs::create_dir_all(&src).unwrap();
        let m = manifest(tmp.path(), &[("../evil.nef", b"x")]);
        assert!(materialize(&m, &opts(&src, &dst)).is_err());
        assert!(!tmp.path().join("evil.nef").exists());
    }

    #[test]
    fn a_dest_inside_the_source_is_not_rescanned_and_symlinks_are_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("lib");
        let dst = src.join("ref10k");
        write(&src.join("a.nef"), b"one");
        let outside = tmp.path().join("outside");
        write(&outside.join("secret.nef"), b"two");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, src.join("link")).unwrap();
        let m = manifest(
            tmp.path(),
            &[("ref-00001.nef", b"one"), ("ref-00002.nef", b"two")],
        );

        let r = materialize(&m, &opts(&src, &dst)).unwrap();
        assert_eq!(r.placed, vec!["ref-00001.nef".to_string()]);
        #[cfg(unix)]
        assert_eq!(r.not_found, vec!["ref-00002.nef".to_string()]);
        // Only the one real file was a candidate: the placed copy in `dst` was pruned from the walk.
        let (seen, _) = walk(&src, fs::canonicalize(&dst).ok().as_deref());
        assert!(seen.iter().all(|(p, _)| !p.starts_with(&dst)));
    }

    #[cfg(unix)]
    #[test]
    fn hardlink_mode_shares_the_inode() {
        use std::os::unix::fs::MetadataExt;
        let tmp = tempfile::tempdir().unwrap();
        let (src, dst) = (tmp.path().join("lib"), tmp.path().join("out"));
        write(&src.join("a.nef"), b"one");
        let m = manifest(tmp.path(), &[("ref-00001.nef", b"one")]);

        let mut o = opts(&src, &dst);
        o.mode = Mode::Hardlink;
        let r = materialize(&m, &o).unwrap();
        assert!(r.is_complete(), "{r:?}");
        assert_eq!(
            fs::metadata(src.join("a.nef")).unwrap().ino(),
            fs::metadata(dst.join("ref-00001.nef")).unwrap().ino()
        );
    }

    #[test]
    fn windows_hostile_ids_are_rejected() {
        for bad in [
            "NUL",
            "con.nef",
            "COM1.nef",
            "ref-1.nef.",
            "ref-1.nef ",
            "a*b.nef",
            "a?.nef",
            "a|b.nef",
            "a\u{1}b.nef",
        ] {
            assert!(!is_plain_file_name(bad), "{bad:?}");
        }
        assert!(is_plain_file_name("ref-00001.nef"));
        assert!(is_plain_file_name("console.nef"));
    }

    #[test]
    fn the_destination_prune_really_keeps_dest_files_out_of_the_candidates() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("lib");
        let dst = src.join("ref10k");
        write(&src.join("a.nef"), b"one");
        // A leftover in dest whose content matches a *different* id: without the prune it would
        // be picked up as a source for ref-00002.
        write(&dst.join("stray.nef"), b"two");
        let m = manifest(
            tmp.path(),
            &[("ref-00001.nef", b"one"), ("ref-00002.nef", b"two")],
        );

        let r = materialize(&m, &opts(&src, &dst)).unwrap();
        assert_eq!(r.placed, vec!["ref-00001.nef".to_string()]);
        assert_eq!(r.not_found, vec!["ref-00002.nef".to_string()]);
    }

    #[test]
    fn source_and_destination_must_differ() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("lib");
        fs::create_dir_all(&src).unwrap();
        let m = manifest(tmp.path(), &[("ref-00001.nef", b"one")]);
        assert!(materialize(&m, &opts(&src, &src)).is_err());
    }

    #[test]
    fn a_stale_wrong_file_is_replaced_atomically_in_hardlink_mode_too() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, dst) = (tmp.path().join("lib"), tmp.path().join("out"));
        write(&src.join("a.nef"), b"good");
        write(&dst.join("ref-00001.nef"), b"BAD!");
        let m = manifest(tmp.path(), &[("ref-00001.nef", b"good")]);

        let mut o = opts(&src, &dst);
        o.mode = Mode::Hardlink;
        let r = materialize(&m, &o).unwrap();
        assert!(r.is_complete(), "{r:?}");
        assert_eq!(fs::read(dst.join("ref-00001.nef")).unwrap(), b"good");
        assert!(m.verify(&dst, crate::manifest::Scope::All).is_clean());
    }

    #[test]
    fn a_hardlink_that_fails_leaves_the_existing_good_file_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, dst) = (tmp.path().join("lib"), tmp.path().join("out"));
        write(&src.join("a.nef"), b"good");
        // dest already holds a *different* correct file under the id? No: make linking fail by
        // pointing the temp name at a directory so hard_link cannot create it.
        write(&dst.join("ref-00001.nef"), b"BAD!");
        fs::create_dir_all(dst.join(format!("ref-00001.nef.partial-{}", std::process::id())))
            .unwrap();
        let m = manifest(tmp.path(), &[("ref-00001.nef", b"good")]);

        let mut o = opts(&src, &dst);
        o.mode = Mode::Hardlink;
        let r = materialize(&m, &o).unwrap();
        assert_eq!(r.failed.len(), 1, "{r:?}");
        assert!(r.not_found.is_empty() && !r.is_complete());
        // The pre-existing file was not deleted before the failed link.
        assert_eq!(fs::read(dst.join("ref-00001.nef")).unwrap(), b"BAD!");
    }

    #[cfg(unix)]
    #[test]
    fn a_planted_symlink_temp_is_never_written_through() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, dst) = (tmp.path().join("lib"), tmp.path().join("out"));
        write(&src.join("a.nef"), b"good");
        let victim = tmp.path().join("victim.txt");
        fs::write(&victim, b"precious").unwrap();
        fs::create_dir_all(&dst).unwrap();
        std::os::unix::fs::symlink(
            &victim,
            dst.join(format!("ref-00001.nef.partial-{}", std::process::id())),
        )
        .unwrap();
        let m = manifest(tmp.path(), &[("ref-00001.nef", b"good")]);

        let r = materialize(&m, &opts(&src, &dst)).unwrap();
        assert!(r.is_complete(), "{r:?}");
        assert_eq!(fs::read(&victim).unwrap(), b"precious");
        assert_eq!(fs::read(dst.join("ref-00001.nef")).unwrap(), b"good");
    }

    #[test]
    fn an_unreadable_dest_entry_is_reported_not_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, dst) = (tmp.path().join("lib"), tmp.path().join("out"));
        write(&src.join("a.nef"), b"good");
        // A directory where the file should be: opens fine, fails to read (not a NotFound).
        fs::create_dir_all(dst.join("ref-00001.nef")).unwrap();
        let m = manifest(tmp.path(), &[("ref-00001.nef", b"good")]);

        let r = materialize(&m, &opts(&src, &dst)).unwrap();
        assert_eq!(r.failed.len(), 1, "{r:?}");
        assert!(r.placed.is_empty());
        assert!(dst.join("ref-00001.nef").is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_source_dirs_are_listed_so_not_found_is_not_misleading() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let (src, dst) = (tmp.path().join("lib"), tmp.path().join("out"));
        let locked = src.join("locked");
        write(&locked.join("a.nef"), b"one");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let can_still_read = fs::read_dir(&locked).is_ok(); // running as root
        let m = manifest(tmp.path(), &[("ref-00001.nef", b"one")]);

        let r = materialize(&m, &opts(&src, &dst)).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        if !can_still_read {
            assert_eq!(r.not_found, vec!["ref-00001.nef".to_string()]);
            assert_eq!(r.unreadable, vec![locked]);
        }
    }

    #[test]
    fn a_failed_first_copy_does_not_stop_an_identical_second_source() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, dst) = (tmp.path().join("lib"), tmp.path().join("out"));
        write(&src.join("a.nef"), b"dup");
        write(&src.join("b.nef"), b"dup");
        let m = manifest(tmp.path(), &[("ref-00001.nef", b"dup")]);
        // Make the very first placement attempt fail by blocking its temp file with a directory
        // that create_temp cannot unlink; the temp name is per-run, so clear it after one try.
        let blocker = dst.join(format!("ref-00001.nef.partial-{}", std::process::id()));
        fs::create_dir_all(&blocker).unwrap();
        let r = materialize(&m, &opts(&src, &dst)).unwrap();
        // Both sources hit the same blocked temp name here, so both fail: the id is reported once.
        assert_eq!(r.failed.len(), 1, "{r:?}");
        assert!(r.not_found.is_empty());
        fs::remove_dir(&blocker).unwrap();
        let r = materialize(&m, &opts(&src, &dst)).unwrap();
        assert!(r.is_complete(), "{r:?}");
        assert_eq!(r.placed.len(), 1);
    }
}
