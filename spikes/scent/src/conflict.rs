//! ADR-0002's "Recovery from sidecars" conflict rule, wired to real file
//! mtimes/hashes (the ADR left the actual I/O plumbing to #59), plus the
//! layer-(c) `crs:` write gate the ADR explicitly asked this ticket to pick.
//!
//! The resolution shape (`Resolution`, the hash-then-mtime-then-ambiguity-
//! window rule) mirrors `spikes/pawprint/src/xmp.rs::resolve_conflict` --
//! reimplemented here against real files rather than depended on directly,
//! since a spike-depending-on-another-spike isn't this repo's convention
//! (only real `crates/*` are shared across spikes, e.g. `nicti-prowl`).

use std::fs;
use std::io;
use std::path::Path;
use std::time::UNIX_EPOCH;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    NoConflict,
    PreferCatalog,
    PreferSidecar,
    FlagForManualReview,
}

/// One side of a catalog-vs-sidecar comparison.
pub struct Side {
    pub content_hash: blake3::Hash,
    pub mtime_ms: u128,
}

/// ADR-0002's rule: identical content is never a conflict regardless of
/// mtime; otherwise the later mtime wins, unless both mtimes fall within
/// `ambiguity_window_ms` of each other, in which case this flags for manual
/// review rather than guessing.
pub fn resolve_conflict(catalog: &Side, sidecar: &Side, ambiguity_window_ms: u128) -> Resolution {
    if catalog.content_hash == sidecar.content_hash {
        return Resolution::NoConflict;
    }
    let diff = catalog.mtime_ms.abs_diff(sidecar.mtime_ms);
    if diff <= ambiguity_window_ms {
        return Resolution::FlagForManualReview;
    }
    if catalog.mtime_ms > sidecar.mtime_ms {
        Resolution::PreferCatalog
    } else {
        Resolution::PreferSidecar
    }
}

/// Reads a file's real modification time in milliseconds since the Unix
/// epoch, and its BLAKE3 content hash.
pub fn hash_and_mtime(path: &Path) -> io::Result<(blake3::Hash, u128)> {
    let contents = fs::read(path)?;
    let hash = blake3::hash(&contents);
    let metadata = fs::metadata(path)?;
    let mtime_ms = metadata
        .modified()?
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    Ok((hash, mtime_ms))
}

/// ADR-0002's layer-(c) `crs:` write gate, resolved by #59 as: **write
/// `crs:` only if the sidecar's current hash still matches what Nicti last
/// wrote there** -- i.e. LRC hasn't independently touched the file since.
/// Otherwise, skip the write and flag the asset, rather than clobbering
/// whatever LRC just did. `last_written_hash: None` means Nicti has never
/// written `crs:` to this sidecar before, which is always safe to write.
pub fn should_write_crs(
    last_written_hash: Option<blake3::Hash>,
    current_sidecar_hash: blake3::Hash,
) -> bool {
    match last_written_hash {
        None => true,
        Some(last) => last == current_sidecar_hash,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn side(bytes: &[u8], mtime_ms: u128) -> Side {
        Side {
            content_hash: blake3::hash(bytes),
            mtime_ms,
        }
    }

    #[test]
    fn identical_content_is_never_a_conflict_regardless_of_mtime() {
        let a = side(b"same", 1000);
        let b = side(b"same", 999_999_999);
        assert_eq!(resolve_conflict(&a, &b, 0), Resolution::NoConflict);
    }

    #[test]
    fn differing_content_prefers_the_newer_mtime() {
        let catalog = side(b"catalog version", 2000);
        let sidecar = side(b"sidecar version", 1000);
        assert_eq!(
            resolve_conflict(&catalog, &sidecar, 100),
            Resolution::PreferCatalog
        );
        let catalog = side(b"catalog version", 1000);
        let sidecar = side(b"sidecar version", 2000);
        assert_eq!(
            resolve_conflict(&catalog, &sidecar, 100),
            Resolution::PreferSidecar
        );
    }

    #[test]
    fn mtimes_within_ambiguity_window_are_flagged_not_guessed() {
        let catalog = side(b"catalog version", 1000);
        let sidecar = side(b"sidecar version", 1050);
        assert_eq!(
            resolve_conflict(&catalog, &sidecar, 100),
            Resolution::FlagForManualReview
        );
    }

    #[test]
    fn crs_write_gate_allows_first_write() {
        assert!(should_write_crs(None, blake3::hash(b"anything")));
    }

    #[test]
    fn crs_write_gate_blocks_when_lrc_touched_the_sidecar_since() {
        let last = blake3::hash(b"what nicti wrote");
        let current = blake3::hash(b"what lrc wrote after that");
        assert!(!should_write_crs(Some(last), current));
    }

    #[test]
    fn crs_write_gate_allows_when_sidecar_is_unchanged_since() {
        let hash = blake3::hash(b"what nicti wrote");
        assert!(should_write_crs(Some(hash), hash));
    }

    #[test]
    fn hash_and_mtime_reads_a_real_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.xmp");
        std::fs::write(&path, b"hello world").unwrap();
        let (hash, mtime_ms) = hash_and_mtime(&path).unwrap();
        assert_eq!(hash, blake3::hash(b"hello world"));
        assert!(mtime_ms > 0);
    }
}
