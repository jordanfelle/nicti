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
}

impl Report {
    pub fn is_complete(&self) -> bool {
        self.not_found.is_empty() && self.failed.is_empty()
    }
}

/// The manifest is trusted input but an id is joined onto `dest`, so it must be one plain file
/// name: no separators, no `.`/`..`, nothing that could write outside `dest`.
fn is_plain_file_name(id: &str) -> bool {
    !id.is_empty()
        && id != "."
        && id != ".."
        && !id.contains(['/', '\\', '\0', ':'])
        && Path::new(id).components().count() == 1
}

/// Every regular file under `root` with its size. Symlinks are never followed (no cycles, no
/// escaping the library), and `skip` (the destination, when it sits inside the source) is pruned.
fn walk(root: &Path, skip: Option<&Path>) -> Vec<(PathBuf, u64)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            let Ok(ty) = entry.file_type() else { continue };
            let path = entry.path();
            if ty.is_dir() {
                if skip.is_some_and(|s| fs::canonicalize(&path).is_ok_and(|c| c == s)) {
                    continue;
                }
                stack.push(path);
            } else if ty.is_file() {
                if let Ok(meta) = entry.metadata() {
                    out.push((path, meta.len()));
                }
            }
        }
    }
    out.sort();
    out
}

fn hash_path(path: &Path) -> Option<String> {
    hash_file(fs::File::open(path).ok()?).ok()
}

fn place(src: &Path, dest_dir: &Path, id: &str, sha256: &str, mode: Mode) -> Result<(), String> {
    let final_path = dest_dir.join(id);
    match mode {
        Mode::Hardlink => {
            // A stale, wrong-content file at the final name is replaced, not trusted.
            let _ = fs::remove_file(&final_path);
            fs::hard_link(src, &final_path).map_err(|e| format!("hard link: {e}"))
        }
        Mode::Copy => {
            let tmp = dest_dir.join(format!("{id}.partial"));
            let result = fs::copy(src, &tmp)
                .map_err(|e| format!("copy: {e}"))
                .and_then(|_| match hash_path(&tmp) {
                    Some(h) if h.eq_ignore_ascii_case(sha256) => Ok(()),
                    _ => Err("copy did not hash to the manifest sha256".to_string()),
                })
                .and_then(|_| fs::rename(&tmp, &final_path).map_err(|e| format!("rename: {e}")));
            if result.is_err() {
                let _ = fs::remove_file(&tmp);
            }
            result
        }
    }
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
    if !opts.dry_run {
        fs::create_dir_all(opts.dest)?;
    }

    let mut report = Report::default();

    // Idempotent: whatever is already in `dest` and verifies is left alone.
    let present: Vec<bool> = wanted
        .par_iter()
        .map(|e| {
            hash_path(&opts.dest.join(&e.id)).is_some_and(|h| h.eq_ignore_ascii_case(&e.sha256))
        })
        .collect();
    let mut keep = present.iter();
    wanted.retain(|e| {
        let is_present = *keep.next().expect("one flag per entry");
        if is_present {
            report.already_present.push(e.id.clone());
        }
        !is_present
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
    let candidates: Vec<(PathBuf, u64)> = walk(opts.source, skip.as_deref())
        .into_iter()
        .filter(|(_, size)| sizes.contains(size))
        .collect();
    let hashed: Vec<Option<String>> = candidates
        .par_iter()
        .map(|(p, _)| hash_path(p).map(|h| h.to_ascii_lowercase()))
        .collect();

    // Source order is path-sorted, so when one content hash appears twice in the library the
    // same file always wins.
    let mut claimed: HashSet<&str> = HashSet::new();
    for ((path, size), hash) in candidates.iter().zip(hashed) {
        let Some(hash) = hash else { continue };
        for e in index.get(&(*size, hash)).into_iter().flatten() {
            if !claimed.insert(&e.id) {
                continue;
            }
            if opts.dry_run {
                report.placed.push(e.id.clone());
                continue;
            }
            match place(path, opts.dest, &e.id, &e.sha256, opts.mode) {
                Ok(()) => report.placed.push(e.id.clone()),
                Err(why) => report.failed.push((e.id.clone(), why)),
            }
        }
    }
    report.not_found = wanted
        .iter()
        .filter(|e| !claimed.contains(e.id.as_str()))
        .map(|e| e.id.clone())
        .collect();
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
        let seen = walk(&src, fs::canonicalize(&dst).ok().as_deref());
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
}
