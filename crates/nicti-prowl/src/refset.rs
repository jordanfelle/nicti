//! Picks reference NEFs out of the ref-10k set by the manifest's `bucket` column, and resolves
//! where the set lives on disk.
//!
//! This is deliberately *not* a Rust port of `bench/select_hero_set.py`'s stratified-by-ISO
//! sampler: that script's output is already frozen at `docs/benchmarks/hero-set.txt`, and
//! reproducing Python's `random.Random.sample` bit-for-bit in Rust would be a lot of fragile
//! work for a set that's already committed. [`hero_set`] just reads that file. [`select`] is a
//! separate, general-purpose picker for ad hoc sampling (e.g. `prowl verify --sample`, or a
//! future research ticket that wants "N files from bucket X") -- its own determinism only needs
//! to hold within Rust, not match the Python script.

use std::env;
use std::path::{Path, PathBuf};

use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::SeedableRng;

use crate::manifest::{Entry, Manifest};

/// Env var pointing at the local ref-10k root (e.g. `<REF10K_ROOT>` per docs/benchmarks.md). No
/// default path: a benchmark machine's drive layout isn't something this crate should guess at.
pub const REF10K_ENV: &str = "NICTI_REF10K";

pub fn resolve_root(explicit: Option<&Path>) -> anyhow::Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    match env::var_os(REF10K_ENV) {
        Some(p) if !p.is_empty() => Ok(PathBuf::from(p)),
        _ => anyhow::bail!(
            "ref-10k root not given and {REF10K_ENV} is not set -- ref-10k is a private reference \
             dataset (see docs/benchmarks.md and issue #136); set {REF10K_ENV} to your own copy's root"
        ),
    }
}

/// Deterministically (within one Rust build) picks `n` entries from `bucket`, sorted by id.
/// Errors if the bucket has fewer than `n` entries.
pub fn select<'a>(
    manifest: &'a Manifest,
    bucket: &'a str,
    n: usize,
    seed: u64,
) -> anyhow::Result<Vec<&'a Entry>> {
    let mut pool: Vec<&Entry> = manifest.by_bucket(bucket).collect();
    if pool.len() < n {
        anyhow::bail!("bucket {bucket} has only {} entries, need {n}", pool.len());
    }
    pool.sort_by(|a, b| a.id.cmp(&b.id));

    let mut rng = StdRng::seed_from_u64(seed);
    pool.shuffle(&mut rng);
    pool.truncate(n);
    pool.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(pool)
}

/// Reads the frozen hero set (`docs/benchmarks/hero-set.txt`, produced by
/// `bench/select_hero_set.py`) as a list of ids.
pub fn hero_set(repo_root: impl AsRef<Path>) -> anyhow::Result<Vec<String>> {
    let path = repo_root.as_ref().join("docs/benchmarks/hero-set.txt");
    let contents = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("reading hero set {}: {e}", path.display()))?;
    Ok(contents
        .lines()
        .map(str::to_string)
        .filter(|l| !l.is_empty())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_manifest(dir: &Path, buckets: &[(&str, usize)]) -> PathBuf {
        let path = dir.join("manifest.csv");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            "id,sha256,bucket,model,iso,compression,width,height,size_bytes"
        )
        .unwrap();
        for (bucket, count) in buckets {
            for i in 0..*count {
                writeln!(
                    f,
                    "{bucket}-{i:04}.nef,{:064x},{bucket},NIKON Z 8,64,Lossless,100,100,1000",
                    i
                )
                .unwrap();
            }
        }
        path
    }

    #[test]
    fn select_is_deterministic_and_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = write_manifest(dir.path(), &[("z8", 30)]);
        let manifest = Manifest::load(&manifest_path).unwrap();

        let a = select(&manifest, "z8", 10, 43).unwrap();
        let b = select(&manifest, "z8", 10, 43).unwrap();
        assert_eq!(
            a.iter().map(|e| &e.id).collect::<Vec<_>>(),
            b.iter().map(|e| &e.id).collect::<Vec<_>>()
        );
        assert_eq!(a.len(), 10);
        let ids: Vec<&str> = a.iter().map(|e| e.id.as_str()).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted);
    }

    #[test]
    fn select_errors_when_bucket_too_small() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = write_manifest(dir.path(), &[("z8", 3)]);
        let manifest = Manifest::load(&manifest_path).unwrap();

        let err = select(&manifest, "z8", 10, 43).unwrap_err();
        assert!(err.to_string().contains("only 3 entries"));
    }

    #[test]
    fn resolve_root_prefers_explicit_path() {
        let explicit = Path::new("/some/explicit/root");
        let resolved = resolve_root(Some(explicit)).unwrap();
        assert_eq!(resolved, explicit);
    }

    #[test]
    fn hero_set_reads_frozen_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("docs/benchmarks")).unwrap();
        std::fs::write(
            dir.path().join("docs/benchmarks/hero-set.txt"),
            "ref-00001.nef\nref-00002.nef\n",
        )
        .unwrap();

        let ids = hero_set(dir.path()).unwrap();
        assert_eq!(
            ids,
            vec!["ref-00001.nef".to_string(), "ref-00002.nef".to_string()]
        );
    }
}
