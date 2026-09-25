//! Loads `docs/ref-10k-manifest.csv` and verifies the local ref-10k copy against it.
//!
//! docs/benchmarks.md (#17, #14) requires every benchmark run to check the local files against
//! the manifest's `sha256` column before trusting the run -- this module is the one place that
//! check lives, so every caller (the hero-scenario PowerShell scripts, the `prowl` binary, and
//! any future research ticket's own harness) shares it instead of re-implementing it.

use std::fs::File;
use std::io;
use std::path::Path;

use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::SeedableRng;
use rayon::prelude::*;
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// One row of `docs/ref-10k-manifest.csv`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Entry {
    pub id: String,
    pub sha256: String,
    pub bucket: String,
    pub model: String,
    pub iso: u32,
    pub compression: String,
    pub width: u32,
    pub height: u32,
    pub size_bytes: u64,
}

/// Which subset of the manifest a [`Manifest::verify`] call checks.
#[derive(Debug, Clone)]
pub enum Scope {
    /// Every entry in the manifest.
    All,
    /// Exactly these ids, in manifest order.
    Ids(Vec<String>),
    /// A deterministic random sample, for a cheap spot-check without hashing all 9k+ files.
    Sample { n: usize, seed: u64 },
}

/// The outcome of one [`Manifest::verify`] call.
#[derive(Debug, Clone, Default)]
pub struct VerifyReport {
    /// Number of files that hashed to their manifest sha256.
    pub ok: usize,
    /// Files the manifest lists that are missing (or unreadable) on disk.
    pub missing: Vec<String>,
    /// Files present on disk whose hash doesn't match the manifest.
    pub mismatched: Vec<Mismatch>,
    /// Requested ids ([`Scope::Ids`] only) that aren't in the manifest at all.
    pub unknown: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mismatch {
    pub id: String,
    pub expected: String,
    pub actual: String,
}

impl VerifyReport {
    pub fn is_clean(&self) -> bool {
        self.missing.is_empty() && self.mismatched.is_empty() && self.unknown.is_empty()
    }

    pub fn checked(&self) -> usize {
        self.ok + self.missing.len() + self.mismatched.len()
    }

    /// Turns a dirty report into an error. Nothing that consumes a `VerifyReport` (the `perf`
    /// module's [`crate::perf::Protocol::run`] in particular) should trust a run without calling
    /// this first.
    pub fn into_result(self) -> anyhow::Result<Self> {
        if self.is_clean() {
            Ok(self)
        } else {
            anyhow::bail!(
                "ref-10k verification failed: {} missing, {} mismatched, {} unknown (of {} checked)",
                self.missing.len(),
                self.mismatched.len(),
                self.unknown.len(),
                self.checked(),
            );
        }
    }
}

/// The parsed manifest.
#[derive(Debug, Clone)]
pub struct Manifest {
    entries: Vec<Entry>,
}

impl Manifest {
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let mut reader = csv::Reader::from_path(path)
            .map_err(|e| anyhow::anyhow!("reading manifest {}: {e}", path.display()))?;
        let entries = reader
            .deserialize::<Entry>()
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| anyhow::anyhow!("parsing manifest {}: {e}", path.display()))?;
        Ok(Manifest { entries })
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn get(&self, id: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id == id)
    }

    pub fn by_bucket<'a>(&'a self, bucket: &'a str) -> impl Iterator<Item = &'a Entry> {
        self.entries.iter().filter(move |e| e.bucket == bucket)
    }

    /// Verifies `root`'s files against this manifest for the given `scope`. Hashing runs in
    /// parallel (rayon) since a full-manifest verify touches 9k+ files.
    pub fn verify(&self, root: impl AsRef<Path>, scope: Scope) -> VerifyReport {
        let root = root.as_ref();

        let (targets, unknown): (Vec<&Entry>, Vec<String>) = match &scope {
            Scope::All => (self.entries.iter().collect(), Vec::new()),
            Scope::Ids(ids) => {
                let mut targets = Vec::with_capacity(ids.len());
                let mut unknown = Vec::new();
                for id in ids {
                    match self.get(id) {
                        Some(entry) => targets.push(entry),
                        None => unknown.push(id.clone()),
                    }
                }
                (targets, unknown)
            }
            Scope::Sample { n, seed } => {
                let mut rng = StdRng::seed_from_u64(*seed);
                let mut pool: Vec<&Entry> = self.entries.iter().collect();
                pool.shuffle(&mut rng);
                pool.truncate(*n);
                (pool, Vec::new())
            }
        };

        let mut ok = 0usize;
        let mut missing = Vec::new();
        let mut mismatched = Vec::new();

        for result in targets
            .par_iter()
            .map(|entry| check_one(root, entry))
            .collect::<Vec<_>>()
        {
            match result {
                CheckResult::Ok => ok += 1,
                CheckResult::Missing(id) => missing.push(id),
                CheckResult::Mismatch(m) => mismatched.push(m),
            }
        }

        VerifyReport {
            ok,
            missing,
            mismatched,
            unknown,
        }
    }
}

enum CheckResult {
    Ok,
    Missing(String),
    Mismatch(Mismatch),
}

fn check_one(root: &Path, entry: &Entry) -> CheckResult {
    let path = root.join(&entry.id);
    let file = match File::open(&path) {
        Ok(f) => f,
        Err(_) => return CheckResult::Missing(entry.id.clone()),
    };
    match hash_file(file) {
        Ok(actual) if actual.eq_ignore_ascii_case(&entry.sha256) => CheckResult::Ok,
        Ok(actual) => CheckResult::Mismatch(Mismatch {
            id: entry.id.clone(),
            expected: entry.sha256.clone(),
            actual,
        }),
        Err(_) => CheckResult::Missing(entry.id.clone()),
    }
}

fn hash_file(mut file: File) -> io::Result<String> {
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_manifest(dir: &Path, rows: &[(&str, &str, &str)]) -> std::path::PathBuf {
        let path = dir.join("manifest.csv");
        let mut f = File::create(&path).unwrap();
        writeln!(
            f,
            "id,sha256,bucket,model,iso,compression,width,height,size_bytes"
        )
        .unwrap();
        for (id, sha256, bucket) in rows {
            writeln!(
                f,
                "{id},{sha256},{bucket},NIKON Z 8,64,Lossless,100,100,1000"
            )
            .unwrap();
        }
        path
    }

    fn write_file_with_known_hash(dir: &Path, name: &str, content: &[u8]) -> String {
        let path = dir.join(name);
        File::create(&path).unwrap().write_all(content).unwrap();
        let mut hasher = Sha256::new();
        hasher.update(content);
        format!("{:x}", hasher.finalize())
    }

    #[test]
    fn verify_all_clean() {
        let dir = tempfile::tempdir().unwrap();
        let hash = write_file_with_known_hash(dir.path(), "a.nef", b"hello");
        let manifest_path = write_manifest(dir.path(), &[("a.nef", &hash, "z8")]);
        let manifest = Manifest::load(&manifest_path).unwrap();

        let report = manifest.verify(dir.path(), Scope::All);
        assert!(report.is_clean(), "{report:?}");
        assert_eq!(report.ok, 1);
    }

    #[test]
    fn verify_detects_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = write_manifest(dir.path(), &[("missing.nef", "deadbeef", "z8")]);
        let manifest = Manifest::load(&manifest_path).unwrap();

        let report = manifest.verify(dir.path(), Scope::All);
        assert!(!report.is_clean());
        assert_eq!(report.missing, vec!["missing.nef".to_string()]);
        assert!(report.into_result().is_err());
    }

    #[test]
    fn verify_detects_mismatched_hash() {
        let dir = tempfile::tempdir().unwrap();
        write_file_with_known_hash(dir.path(), "a.nef", b"hello");
        // Manifest expects a different hash than the file actually has.
        let manifest_path = write_manifest(
            dir.path(),
            &[(
                "a.nef",
                "0000000000000000000000000000000000000000000000000000000000000000",
                "z8",
            )],
        );
        let manifest = Manifest::load(&manifest_path).unwrap();

        let report = manifest.verify(dir.path(), Scope::All);
        assert!(!report.is_clean());
        assert_eq!(report.mismatched.len(), 1);
        assert_eq!(report.mismatched[0].id, "a.nef");
    }

    #[test]
    fn verify_ids_flags_unknown_id() {
        let dir = tempfile::tempdir().unwrap();
        let hash = write_file_with_known_hash(dir.path(), "a.nef", b"hello");
        let manifest_path = write_manifest(dir.path(), &[("a.nef", &hash, "z8")]);
        let manifest = Manifest::load(&manifest_path).unwrap();

        let report = manifest.verify(
            dir.path(),
            Scope::Ids(vec!["a.nef".into(), "nope.nef".into()]),
        );
        assert_eq!(report.ok, 1);
        assert_eq!(report.unknown, vec!["nope.nef".to_string()]);
        assert!(!report.is_clean());
    }

    #[test]
    fn verify_empty_ids_is_a_clean_no_op() {
        // Regression test for a review-caught bug in prowl's CLI (not this module): an
        // explicit-but-empty `--ids` must verify zero files and report clean, not silently fall
        // through to verifying the whole manifest.
        let dir = tempfile::tempdir().unwrap();
        let hash = write_file_with_known_hash(dir.path(), "a.nef", b"hello");
        let manifest_path = write_manifest(dir.path(), &[("a.nef", &hash, "z8")]);
        let manifest = Manifest::load(&manifest_path).unwrap();

        let report = manifest.verify(dir.path(), Scope::Ids(vec![]));
        assert!(report.is_clean());
        assert_eq!(report.ok, 0);
        assert_eq!(report.checked(), 0);
    }

    #[test]
    fn verify_sample_is_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let mut rows = Vec::new();
        let mut hashes = Vec::new();
        for i in 0..20 {
            let name = format!("ref-{i:05}.nef");
            let hash = write_file_with_known_hash(dir.path(), &name, name.as_bytes());
            hashes.push((name, hash));
        }
        for (name, hash) in &hashes {
            rows.push((name.as_str(), hash.as_str(), "z8"));
        }
        let manifest_path = write_manifest(dir.path(), &rows);
        let manifest = Manifest::load(&manifest_path).unwrap();

        let a = manifest.verify(dir.path(), Scope::Sample { n: 5, seed: 43 });
        let b = manifest.verify(dir.path(), Scope::Sample { n: 5, seed: 43 });
        assert_eq!(a.ok, 5);
        assert_eq!(b.ok, 5);

        let different_seed = manifest.verify(dir.path(), Scope::Sample { n: 5, seed: 44 });
        assert_eq!(different_seed.ok, 5);
    }

    #[test]
    fn by_bucket_filters() {
        let dir = tempfile::tempdir().unwrap();
        let hash_a = write_file_with_known_hash(dir.path(), "a.nef", b"a");
        let hash_b = write_file_with_known_hash(dir.path(), "b.dng", b"b");
        let manifest_path = write_manifest(
            dir.path(),
            &[("a.nef", &hash_a, "z8"), ("b.dng", &hash_b, "d3400_dng")],
        );
        let manifest = Manifest::load(&manifest_path).unwrap();

        let z8: Vec<&str> = manifest.by_bucket("z8").map(|e| e.id.as_str()).collect();
        assert_eq!(z8, vec!["a.nef"]);
    }
}
