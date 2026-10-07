//! Folder bit-rot check (#304): re-reads every file under a root that has a recorded
//! `asset.content_hash` (the full-file BLAKE3 a verified copy-path move, #26, stored) and compares.
//! This is the piece that proves an archive drive still holds what was written -- the move's own
//! verify re-read can be served from the OS page cache, this one runs later, cold.
//!
//! Steppable like [`crate::scruff::Ingest`]: [`Verify::step`] reads at most [`SLICE_BYTES`] of one
//! file per call, so a Pounce job (`pounce_jobs::VerifyJob`) stays cancellable mid-file. Read-only:
//! it never touches the catalog or the files. Assets without a recorded hash (renamed on the same
//! volume, or never moved) are only counted in [`VerifyReport::unhashed`]; [`Baseline`] (#386) is
//! the explicit, user-triggered step that records a hash for them.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use unicode_normalization::UnicodeNormalization;

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
    /// On a cancelled pass: `rel_path`s with a stored hash that were never looked at (#386), so
    /// "not checked" is distinguishable from "checked and fine".
    pub unchecked: Vec<String>,
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

    /// The report of a pass cut short (the job was dropped): marks it cancelled and lists every
    /// hashed file not yet finished -- the one mid-read plus the rest of the queue -- in
    /// `unchecked` (#386).
    pub fn cancel_into_report(mut self) -> VerifyReport {
        self.report.cancelled = true;
        if let Phase::Hashing { queue, current } = std::mem::replace(&mut self.phase, Phase::Done) {
            self.report.unchecked.extend(current.map(|c| c.rel_path));
            // `queue` is in reverse order; `pop` order is the order files would have been checked.
            self.report
                .unchecked
                .extend(queue.into_iter().rev().map(|(rel, _)| rel));
        }
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
                    match open_resolving(&self.root_path, &rel_path) {
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

/// Opens `rel_path` under `root`. The catalog stores it NFC (`scruff::normalize_rel_path`) but a
/// volume may hold the name NFD (HFS+-style, or a file copied from macOS), so a plain open of the
/// NFC spelling reports a false `NotFound` -- retry through [`resolve_on_disk`] before giving up.
fn open_resolving(root: &Path, rel_path: &str) -> std::io::Result<File> {
    match File::open(root.join(rel_path)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => match resolve_on_disk(root, rel_path)
        {
            Some(p) => File::open(p),
            None => Err(e),
        },
        other => other,
    }
}

/// Finds the on-disk spelling of `rel_path` under `root`, matching each path component by its NFC
/// form when the literal name isn't there. `None` if some component has no match. Same failure
/// mode as `patrol.rs`'s documented NFC/NFD limitation, fixed here only for the verify path.
fn resolve_on_disk(root: &Path, rel_path: &str) -> Option<PathBuf> {
    let mut cur = root.to_path_buf();
    for comp in rel_path.split('/').filter(|c| !c.is_empty()) {
        let direct = cur.join(comp);
        if direct.symlink_metadata().is_ok() {
            cur = direct;
            continue;
        }
        let want: String = comp.nfc().collect();
        let found = std::fs::read_dir(&cur).ok()?.flatten().find(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.nfc().collect::<String>() == want)
        })?;
        cur = found.path();
    }
    Some(cur)
}

/// Files recorded per catalog transaction by [`Baseline`].
const BASELINE_BATCH: usize = 100;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BaselineReport {
    /// Hashes written to the catalog.
    pub recorded: u64,
    /// Files whose size/mtime on disk no longer matched the catalog (or changed while being read),
    /// or whose row gained a hash/changed meanwhile -- nothing was recorded for them.
    pub skipped_changed: u64,
    /// `rel_path`s with no file on disk.
    pub missing: Vec<String>,
    /// Files that exist but couldn't be read (path, error).
    pub unreadable: Vec<(String, String)>,
    /// `root_path` didn't resolve to a directory: nothing was done.
    pub root_unreachable: bool,
    /// A catalog read or write failed; the pass stopped there.
    pub error: Option<String>,
    /// The job was dropped (cancelled) before finishing; batches already written are kept.
    pub cancelled: bool,
}

struct BaselineCurrent {
    asset_id: i64,
    rel_path: String,
    size: u64,
    mtime: i64,
    file: File,
    hasher: blake3::Hasher,
    hashed: u64,
}

enum BaselinePhase {
    Start,
    /// `queue` is in reverse order so `pop` yields the lowest asset id first.
    Hashing {
        queue: Vec<(i64, String, u64, i64)>,
        current: Option<Box<BaselineCurrent>>,
    },
    Done,
}

/// Records a trusted baseline `content_hash` (#386) for every asset under a root that has none, so
/// [`Verify`] has something to compare against next time. It proves only that the file matches
/// *itself as it is now*: a file already corrupted is baselined as-is. Hence an explicit,
/// user-triggered step, never run implicitly. Steppable like [`Verify`]; a file is baselined only
/// if its on-disk size/mtime equal the catalog's before and after the read and the bytes read
/// equal that size, and it is committed in batches of [`BASELINE_BATCH`].
pub struct Baseline {
    root_id: i64,
    root_path: PathBuf,
    phase: BaselinePhase,
    total: u64,
    done: u64,
    pending: Vec<(i64, u64, i64, String)>,
    report: BaselineReport,
    buf: Vec<u8>,
}

impl Baseline {
    pub fn new(root_id: i64, root_path: &Path) -> Self {
        Baseline {
            root_id,
            root_path: root_path.to_path_buf(),
            phase: BaselinePhase::Start,
            total: 0,
            done: 0,
            pending: Vec::new(),
            report: BaselineReport::default(),
            buf: Vec::new(),
        }
    }

    /// `(files finished, total unhashed files)`; the total is 0 until the first step has listed them.
    pub fn progress(&self) -> (u64, u64) {
        (self.done, self.total)
    }

    pub fn into_report(self) -> BaselineReport {
        self.report
    }

    /// The report of a pass cut short: writes the batch already hashed (each file in it was proven
    /// unchanged), then marks the report cancelled.
    pub fn cancel_into_report(mut self, store: &dyn CatalogStore) -> BaselineReport {
        self.flush(store);
        self.report.cancelled = true;
        self.report
    }

    fn flush(&mut self, store: &dyn CatalogStore) {
        if self.pending.is_empty() {
            return;
        }
        let batch = std::mem::take(&mut self.pending);
        match store.record_baseline_hashes(self.root_id, &batch) {
            Ok(n) => {
                self.report.recorded += n;
                self.report.skipped_changed += batch.len() as u64 - n;
            }
            Err(e) => self.report.error = Some(e.to_string()),
        }
    }

    /// Runs one slice of work. `true` if more remains.
    pub fn step(&mut self, store: &dyn CatalogStore) -> bool {
        if self.report.error.is_some() {
            self.phase = BaselinePhase::Done;
            return false;
        }
        match std::mem::replace(&mut self.phase, BaselinePhase::Done) {
            BaselinePhase::Done => false,
            BaselinePhase::Start => {
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
                let hashed: HashSet<i64> = hashes.into_iter().map(|(id, _)| id).collect();
                let mut queue: Vec<_> = assets
                    .into_iter()
                    .filter(|a| !hashed.contains(&a.id))
                    .map(|a| (a.id, a.rel_path, a.size_bytes, a.mtime_unix))
                    .collect();
                self.total = queue.len() as u64;
                queue.reverse();
                self.phase = BaselinePhase::Hashing {
                    queue,
                    current: None,
                };
                true
            }
            BaselinePhase::Hashing {
                mut queue,
                mut current,
            } => {
                if current.is_none() {
                    let Some((asset_id, rel_path, size, mtime)) = queue.pop() else {
                        self.flush(store);
                        return false;
                    };
                    match open_resolving(&self.root_path, &rel_path) {
                        Ok(file) if stat_matches(&file, size, mtime) => {
                            current = Some(Box::new(BaselineCurrent {
                                asset_id,
                                rel_path,
                                size,
                                mtime,
                                file,
                                hasher: blake3::Hasher::new(),
                                hashed: 0,
                            }))
                        }
                        Ok(_) => {
                            self.done += 1;
                            self.report.skipped_changed += 1;
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            self.done += 1;
                            self.report.missing.push(rel_path)
                        }
                        Err(e) => {
                            self.done += 1;
                            self.report.unreadable.push((rel_path, e.to_string()))
                        }
                    }
                } else if let Some(cur) = current.as_mut() {
                    self.buf.resize(SLICE_BYTES, 0);
                    match read_up_to_counting(
                        &mut cur.file,
                        &mut self.buf,
                        &mut cur.hasher,
                        &mut cur.hashed,
                    ) {
                        Ok(true) => {}
                        Ok(false) => {
                            let cur = current.take().unwrap();
                            self.done += 1;
                            if cur.hashed == cur.size
                                && stat_matches(&cur.file, cur.size, cur.mtime)
                            {
                                self.pending.push((
                                    cur.asset_id,
                                    cur.size,
                                    cur.mtime,
                                    cur.hasher.finalize().to_hex().to_string(),
                                ));
                                if self.pending.len() >= BASELINE_BATCH {
                                    self.flush(store);
                                }
                            } else {
                                self.report.skipped_changed += 1;
                            }
                        }
                        Err(e) => {
                            let cur = current.take().unwrap();
                            self.done += 1;
                            self.report.unreadable.push((cur.rel_path, e.to_string()));
                        }
                    }
                }
                self.phase = BaselinePhase::Hashing { queue, current };
                true
            }
        }
    }
}

/// `true` if `file`'s current size and mtime (whole seconds, as `scruff` records them) equal the
/// catalog's.
fn stat_matches(file: &File, size: u64, mtime: i64) -> bool {
    let Ok(meta) = file.metadata() else {
        return false;
    };
    // An unreadable mtime never matches (it must not equal a stored 0).
    let Some(on_disk) = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
    else {
        return false;
    };
    meta.len() == size && on_disk == mtime
}

/// [`read_up_to`] that also adds the bytes it hashed to `total`.
fn read_up_to_counting(
    f: &mut File,
    buf: &mut [u8],
    hasher: &mut blake3::Hasher,
    total: &mut u64,
) -> std::io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = f.read(&mut buf[filled..])?;
        if n == 0 {
            hasher.update(&buf[..filled]);
            *total += filled as u64;
            return Ok(false);
        }
        filled += n;
    }
    hasher.update(buf);
    *total += filled as u64;
    Ok(true)
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

    #[test]
    fn nfd_on_disk_name_is_found_not_reported_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("archive");
        fs::create_dir_all(dir.join("shot\u{301}s")).unwrap();
        // On disk NFD (`e` + combining acute), in the catalog NFC (`é`).
        fs::write(dir.join("shot\u{301}s").join("cafe\u{301}.NEF"), b"alpha").unwrap();
        let cat = SqliteCatalog::open_in_memory().unwrap();
        let vol = cat.upsert_volume("v", None, None, 0).unwrap();
        let root = cat.ensure_root(vol, "/ssd/placeholder").unwrap();
        let rel = "sho\u{0074}\u{301}s/caf\u{e9}.NEF"
            .nfc()
            .collect::<String>();
        let id = cat.insert_asset(root, &new_asset(&rel), None).unwrap();
        let mv = cat
            .begin_root_move(root, &dir.to_string_lossy(), 1)
            .unwrap();
        cat.commit_root_move(mv, &[(id, hash_hex(b"alpha"))])
            .unwrap();
        let r = run(&cat, root, &dir);
        assert!(r.missing.is_empty(), "false missing: {:?}", r.missing);
        assert_eq!((r.checked, r.matched), (1, 1));
    }

    #[test]
    fn cancelled_pass_lists_every_unfinished_file_as_unchecked() {
        let (_t, cat, root, dir) = fixture(b"alpha", b"bravo");
        let mut v = Verify::new(root, &dir);
        assert!(v.step(&cat)); // list
        assert!(v.step(&cat)); // open a.NEF
        let r = v.cancel_into_report();
        assert!(r.cancelled && !r.is_clean());
        assert_eq!(r.unchecked, vec!["a.NEF".to_string(), "b.NEF".to_string()]);
        // A finished pass has nothing unchecked.
        assert!(run(&cat, root, &dir).unchecked.is_empty());
    }

    fn mtime_of(p: &Path) -> i64 {
        fs::metadata(p)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    /// Unhashed root of real files `x.NEF` / `y.NEF` whose catalog size/mtime match the disk.
    fn baseline_fixture() -> (tempfile::TempDir, SqliteCatalog, i64, PathBuf, [i64; 2]) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("archive");
        fs::create_dir_all(&dir).unwrap();
        let cat = SqliteCatalog::open_in_memory().unwrap();
        let vol = cat.upsert_volume("v", None, None, 0).unwrap();
        let root = cat.ensure_root(vol, "/ssd/placeholder").unwrap();
        let mut ids = [0; 2];
        for (i, (name, bytes)) in [("x.NEF", &b"xray"[..]), ("y.NEF", &b"yankee"[..])]
            .into_iter()
            .enumerate()
        {
            fs::write(dir.join(name), bytes).unwrap();
            let mut a = new_asset(name);
            a.size_bytes = bytes.len() as u64;
            a.mtime_unix = mtime_of(&dir.join(name));
            ids[i] = cat.insert_asset(root, &a, None).unwrap();
        }
        (tmp, cat, root, dir, ids)
    }

    fn run_baseline(cat: &SqliteCatalog, root: i64, dir: &Path) -> BaselineReport {
        let mut b = Baseline::new(root, dir);
        while b.step(cat) {}
        b.into_report()
    }

    #[test]
    fn baseline_records_hashes_so_the_next_verify_has_nothing_unhashed() {
        let (_t, cat, root, dir, ids) = baseline_fixture();
        assert_eq!(run(&cat, root, &dir).unhashed, 2);
        let r = run_baseline(&cat, root, &dir);
        assert_eq!((r.recorded, r.skipped_changed), (2, 0));
        assert_eq!(cat.content_hash(ids[0]).unwrap(), Some(hash_hex(b"xray")));
        let v = run(&cat, root, &dir);
        assert_eq!((v.unhashed, v.checked, v.matched), (0, 2, 2));
        assert!(v.is_clean());
    }

    #[test]
    fn baseline_never_overwrites_an_existing_hash() {
        let (_t, cat, root, dir, ids) = baseline_fixture();
        let mv = cat
            .begin_root_move(root, &dir.to_string_lossy(), 1)
            .unwrap();
        cat.commit_root_move(mv, &[(ids[0], "recorded-earlier".to_string())])
            .unwrap();
        let r = run_baseline(&cat, root, &dir);
        assert_eq!(r.recorded, 1); // only y.NEF
        assert_eq!(
            cat.content_hash(ids[0]).unwrap().as_deref(),
            Some("recorded-earlier")
        );
    }

    #[test]
    fn baseline_skips_a_file_changed_since_ingest() {
        let (_t, cat, root, dir, ids) = baseline_fixture();
        fs::write(dir.join("y.NEF"), b"yankee, edited since ingest").unwrap();
        let r = run_baseline(&cat, root, &dir);
        assert_eq!((r.recorded, r.skipped_changed), (1, 1));
        assert_eq!(cat.content_hash(ids[1]).unwrap(), None);
    }

    #[test]
    fn baseline_reports_missing_and_unreachable() {
        let (_t, cat, root, dir, _ids) = baseline_fixture();
        fs::remove_file(dir.join("x.NEF")).unwrap();
        let r = run_baseline(&cat, root, &dir);
        assert_eq!(
            (r.recorded, r.missing.clone()),
            (1, vec!["x.NEF".to_string()])
        );
        assert!(run_baseline(&cat, root, &dir.join("gone")).root_unreachable);
    }

    #[test]
    fn cancelled_baseline_keeps_what_it_already_hashed() {
        let (_t, cat, root, dir, ids) = baseline_fixture();
        let mut b = Baseline::new(root, &dir);
        for _ in 0..3 {
            assert!(b.step(&cat)); // list, open x, hash x
        }
        let r = b.cancel_into_report(&cat);
        assert!(r.cancelled);
        assert_eq!(r.recorded, 1);
        assert_eq!(cat.content_hash(ids[0]).unwrap(), Some(hash_hex(b"xray")));
        assert_eq!(cat.content_hash(ids[1]).unwrap(), None);
    }
}
