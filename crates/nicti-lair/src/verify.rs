//! Folder bit-rot check (#304): re-reads every file under a root that has a recorded
//! `asset.content_hash` (the full-file BLAKE3 a verified copy-path move, #26, stored) and compares.
//! This is the piece that proves an archive drive still holds what was written -- the move's own
//! verify re-read can be served from the OS page cache, this one runs later, cold.
//!
//! Steppable like [`crate::scruff::Ingest`]: [`Verify::step`] reads at most [`SLICE_BYTES`] of one
//! file per call, so a Pounce job (`pounce_jobs::VerifyJob`) stays cancellable mid-file. Read-only:
//! it never touches the catalog or the files. Assets without a recorded hash (renamed on the same
//! volume, or never moved) are only counted in [`VerifyReport::unhashed`].

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::CatalogStore;

/// Bytes hashed per [`Verify::step`] call.
const SLICE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerifyReport {
    /// Files re-read and hashed (matched + mismatched).
    pub checked: u64,
    pub matched: u64,
    /// `rel_path`s whose bytes no longer hash to the stored value -- the bit-rot signal.
    pub mismatched: Vec<String>,
    /// `rel_path`s with a stored hash whose file is gone from disk.
    pub missing: Vec<String>,
    /// Files that exist but couldn't be read (path, error) -- unverified, not proven corrupt.
    pub unreadable: Vec<(String, String)>,
    /// Assets under the root with no stored hash, so nothing to compare against.
    pub unhashed: u64,
    /// `root_path` didn't resolve to a directory (unplugged drive): nothing was checked.
    pub root_unreachable: bool,
    /// A catalog read failed; the pass stopped there.
    pub error: Option<String>,
    /// Set by `VerifyJob` when it was dropped (cancelled) before finishing.
    pub cancelled: bool,
}

impl VerifyReport {
    /// `true` only if every hashed asset was found, readable and matching.
    pub fn is_clean(&self) -> bool {
        self.mismatched.is_empty()
            && self.missing.is_empty()
            && self.unreadable.is_empty()
            && !self.root_unreachable
            && self.error.is_none()
            && !self.cancelled
    }
}

struct Current {
    rel_path: String,
    expected: String,
    file: File,
    hasher: blake3::Hasher,
}

enum Phase {
    Start,
    /// `queue` is in reverse order so `pop` yields the lowest asset id first.
    Hashing {
        queue: Vec<(String, String)>,
        current: Option<Box<Current>>,
    },
    Done,
}

pub struct Verify {
    root_id: i64,
    root_path: PathBuf,
    phase: Phase,
    total: u64,
    report: VerifyReport,
    buf: Vec<u8>,
}

impl Verify {
    pub fn new(root_id: i64, root_path: &Path) -> Self {
        Verify {
            root_id,
            root_path: root_path.to_path_buf(),
            phase: Phase::Start,
            total: 0,
            report: VerifyReport::default(),
            buf: Vec::new(),
        }
    }

    /// `(files finished, total hashed files)`; the total is 0 until the first step has listed them.
    pub fn progress(&self) -> (u64, u64) {
        let done = self.report.checked
            + self.report.missing.len() as u64
            + self.report.unreadable.len() as u64;
        (done, self.total)
    }

    pub fn into_report(self) -> VerifyReport {
        self.report
    }

    /// Runs one slice of work. `true` if more remains.
    pub fn step(&mut self, store: &dyn CatalogStore) -> bool {
        match std::mem::replace(&mut self.phase, Phase::Done) {
            Phase::Done => false,
            Phase::Start => {
                if !matches!(self.root_path.try_exists(), Ok(true)) || !self.root_path.is_dir() {
                    self.report.root_unreachable = true;
                    return false;
                }
                let listed = store
                    .list_assets_by_root(self.root_id)
                    .and_then(|assets| Ok((assets, store.content_hashes_by_root(self.root_id)?)));
                let (assets, hashes) = match listed {
                    Ok(v) => v,
                    Err(e) => {
                        self.report.error = Some(e.to_string());
                        return false;
                    }
                };
                let mut hashes: HashMap<i64, String> = hashes.into_iter().collect();
                let mut queue = Vec::new();
                for a in assets {
                    match hashes.remove(&a.id) {
                        Some(h) => queue.push((a.rel_path, h)),
                        None => self.report.unhashed += 1,
                    }
                }
                self.total = queue.len() as u64;
                queue.reverse();
                self.phase = Phase::Hashing {
                    queue,
                    current: None,
                };
                true
            }
            Phase::Hashing {
                mut queue,
                mut current,
            } => {
                if current.is_none() {
                    let Some((rel_path, expected)) = queue.pop() else {
                        return false;
                    };
                    match File::open(self.root_path.join(&rel_path)) {
                        Ok(file) => {
                            current = Some(Box::new(Current {
                                rel_path,
                                expected,
                                file,
                                hasher: blake3::Hasher::new(),
                            }))
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            self.report.missing.push(rel_path)
                        }
                        Err(e) => self.report.unreadable.push((rel_path, e.to_string())),
                    }
                } else if let Some(cur) = current.as_mut() {
                    self.buf.resize(SLICE_BYTES, 0);
                    match read_up_to(&mut cur.file, &mut self.buf, &mut cur.hasher) {
                        Ok(true) => {} // more bytes remain
                        Ok(false) => {
                            let cur = current.take().unwrap();
                            self.report.checked += 1;
                            if cur.hasher.finalize().to_hex().as_str() == cur.expected {
                                self.report.matched += 1;
                            } else {
                                self.report.mismatched.push(cur.rel_path);
                            }
                        }
                        Err(e) => {
                            let cur = current.take().unwrap();
                            self.report.unreadable.push((cur.rel_path, e.to_string()));
                        }
                    }
                }
                self.phase = Phase::Hashing { queue, current };
                true
            }
        }
    }
}

/// Reads up to `buf.len()` bytes into `hasher`. `Ok(true)` if the buffer filled (the file may have
/// more), `Ok(false)` at EOF.
fn read_up_to(f: &mut File, buf: &mut [u8], hasher: &mut blake3::Hasher) -> std::io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = f.read(&mut buf[filled..])?;
        if n == 0 {
            hasher.update(&buf[..filled]);
            return Ok(false);
        }
        filled += n;
    }
    hasher.update(buf);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NewAsset, SqliteCatalog};
    use std::fs;

    fn new_asset(rel: &str) -> NewAsset {
        NewAsset {
            rel_path: rel.to_string(),
            rel_path_fold: rel.to_lowercase(),
            size_bytes: 0,
            mtime_unix: 0,
            fingerprint: None,
            natural_key: None,
            make: None,
            model: None,
            captured_at: None,
            width: None,
            height: None,
            imported_at: 0,
        }
    }

    fn hash_hex(b: &[u8]) -> String {
        blake3::hash(b).to_hex().to_string()
    }

    /// A root at a real temp dir with `a.NEF` (hashed), `b.NEF` (hashed), `c.NEF` (unhashed).
    fn fixture(a: &[u8], b: &[u8]) -> (tempfile::TempDir, SqliteCatalog, i64, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("archive");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.NEF"), a).unwrap();
        fs::write(dir.join("b.NEF"), b).unwrap();
        fs::write(dir.join("c.NEF"), b"never moved").unwrap();
        let cat = SqliteCatalog::open_in_memory().unwrap();
        let vol = cat.upsert_volume("v", None, None, 0).unwrap();
        let root = cat.ensure_root(vol, "/ssd/placeholder").unwrap();
        let ia = cat.insert_asset(root, &new_asset("a.NEF"), None).unwrap();
        let ib = cat.insert_asset(root, &new_asset("b.NEF"), None).unwrap();
        cat.insert_asset(root, &new_asset("c.NEF"), None).unwrap();
        // The only trait path that records hashes is a move commit; point the root at `dir`.
        let mv = cat
            .begin_root_move(root, &dir.to_string_lossy(), 1)
            .unwrap();
        cat.commit_root_move(mv, &[(ia, hash_hex(a)), (ib, hash_hex(b))])
            .unwrap();
        (tmp, cat, root, dir)
    }

    fn run(cat: &SqliteCatalog, root: i64, dir: &Path) -> VerifyReport {
        let mut v = Verify::new(root, dir);
        while v.step(cat) {}
        v.into_report()
    }

    #[test]
    fn clean_root_matches_and_counts_unhashed() {
        let (_t, cat, root, dir) = fixture(b"alpha", b"bravo");
        let r = run(&cat, root, &dir);
        assert_eq!((r.checked, r.matched, r.unhashed), (2, 2, 1));
        assert!(r.is_clean());
    }

    #[test]
    fn flipped_byte_is_reported_as_mismatch_and_nothing_is_rewritten() {
        let (_t, cat, root, dir) = fixture(b"alpha", b"bravo");
        fs::write(dir.join("b.NEF"), b"bravX").unwrap();
        let r = run(&cat, root, &dir);
        assert_eq!(r.mismatched, vec!["b.NEF".to_string()]);
        assert_eq!(r.matched, 1);
        assert!(!r.is_clean());
        assert_eq!(fs::read(dir.join("b.NEF")).unwrap(), b"bravX");
    }

    #[test]
    fn missing_file_is_reported_not_counted_as_checked() {
        let (_t, cat, root, dir) = fixture(b"alpha", b"bravo");
        fs::remove_file(dir.join("a.NEF")).unwrap();
        let r = run(&cat, root, &dir);
        assert_eq!(r.missing, vec!["a.NEF".to_string()]);
        assert_eq!(r.checked, 1);
    }

    #[test]
    fn unreachable_root_checks_nothing() {
        let (_t, cat, root, dir) = fixture(b"alpha", b"bravo");
        let r = run(&cat, root, &dir.join("gone"));
        assert!(r.root_unreachable);
        assert_eq!(r.checked, 0);
        assert!(!r.is_clean());
    }

    #[test]
    fn files_larger_than_one_slice_hash_across_steps() {
        let big = vec![5u8; SLICE_BYTES * 2 + 123];
        let (_t, cat, root, dir) = fixture(&big, b"bravo");
        let mut v = Verify::new(root, &dir);
        let mut steps = 0;
        while v.step(&cat) {
            steps += 1;
        }
        assert!(steps >= 5, "expected a multi-step read, got {steps}");
        let r = v.into_report();
        assert_eq!(r.matched, 2);
    }
}
