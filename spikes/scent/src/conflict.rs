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
///
/// A pre-1970 or otherwise invalid mtime is a real `io::Error`, not silently
/// treated as epoch 0 -- silently defaulting would bias `resolve_conflict`'s
/// newer-wins comparison against a file with a corrupted timestamp instead
/// of surfacing the problem.
pub fn hash_and_mtime(path: &Path) -> io::Result<(blake3::Hash, u128)> {
    let contents = fs::read(path)?;
    let hash = blake3::hash(&contents);
    let metadata = fs::metadata(path)?;
    let mtime_ms = metadata
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
        .as_millis();
    Ok((hash, mtime_ms))
}

/// ADR-0002's layer-(c) `crs:` write gate, resolved by #59 as: **write
/// `crs:` only if the sidecar's current hash still matches what Nicti last
/// wrote there** -- i.e. LRC hasn't independently touched the file since.
/// Otherwise, skip the write and flag the asset, rather than clobbering
/// whatever LRC just did. `last_written_hash: None` means Nicti has never
/// written `crs:` to this sidecar before, which is always safe to write.
///
/// **This function is a pure comparison, not an atomicity guarantee.** The
/// "never clobbers a concurrent LRC edit" property this gate exists for
/// only holds if the *caller* follows this sequence without a gap a
/// concurrent LRC save could land inside:
/// 1. Read the sidecar's current hash (`hash_and_mtime`) immediately before
///    the write this call is gating.
/// 2. Call `should_write_crs` with that hash and only proceed if it returns
///    `true`.
/// 3. On a successful write, record the hash of the **bytes just written**
///    (not a fresh re-read of the file) as the new `last_written_hash` --
///    re-reading introduces its own TOCTOU window between the write and the
///    re-read.
///
/// A real production caller (not this research spike) additionally needs a
/// filesystem lock or single-writer discipline across steps 1-3 to fully
/// close the race; this spike proves the comparison logic and the intended
/// call sequence (see the `crs_write_gate_sequential_usage_pattern` test),
/// not cross-process atomicity.
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
    fn crs_write_gate_sequential_usage_pattern() {
        // Demonstrates the calling contract `should_write_crs`'s docs
        // require: record the hash of the bytes just *written*, not a
        // fresh re-read, so a third write's gate check isn't racing its
        // own second write's re-read.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("photo.xmp");

        // First write: always allowed (no prior write).
        let first_bytes = b"sidecar v1 with crs projection A";
        assert!(should_write_crs(None, blake3::hash(first_bytes)));
        std::fs::write(&path, first_bytes).unwrap();
        let mut last_written_hash = Some(blake3::hash(first_bytes));

        // Second write: sidecar is unchanged since -> allowed.
        let (current, _) = hash_and_mtime(&path).unwrap();
        assert!(should_write_crs(last_written_hash, current));
        let second_bytes = b"sidecar v2 with crs projection B";
        std::fs::write(&path, second_bytes).unwrap();
        last_written_hash = Some(blake3::hash(second_bytes));

        // LRC edits the sidecar independently in between.
        let lrc_bytes = b"sidecar v3, LRC's own edit, not nicti's";
        std::fs::write(&path, lrc_bytes).unwrap();

        // Third write attempt: gate must block, since the sidecar no
        // longer matches what Nicti itself last wrote.
        let (current, _) = hash_and_mtime(&path).unwrap();
        assert!(!should_write_crs(last_written_hash, current));
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
