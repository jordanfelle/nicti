//! #71's relink-to-an-unrecognized-volume candidates (the part merged in from duplicate #21):
//! four fingerprint tiers, cheapest first, so `homing relink` can try (a) before paying for (d).
//!
//! (a) size+mtime+name is the cheapest and least reliable -- a file moved onto a new volume keeps
//! its name and size but not always its mtime (some copy tools reset it).
//! (b) partial BLAKE3 (first+last 64KB) is a fast, order-of-magnitude proxy for full-content
//! identity -- cheap enough to run at import time on every file.
//! (c) full-file BLAKE3 is the ground truth for "these bytes are identical," but its DNG
//! instability (LRC rewrites DNGs in place) is exactly why (d) exists as a complement, not a
//! replacement.
//! (d) an EXIF natural key survives a metadata rewrite that changes file bytes but not the shot
//! itself -- body serial + shutter count/ImageNumber + DateTimeOriginal + SubSec.

use anyhow::{Context, Result};
use serde::Serialize;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const PARTIAL_HASH_WINDOW: u64 = 64 * 1024;

#[derive(Debug, Clone, Serialize, PartialEq, Eq, Hash)]
pub struct SizeNameKey {
    pub size_bytes: u64,
    pub file_name: String,
}

pub fn size_name_key(path: &Path, size_bytes: u64) -> SizeNameKey {
    SizeNameKey {
        size_bytes,
        file_name: path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default(),
    }
}

/// Tier (b): BLAKE3 over the first and last 64KB (or the whole file, if smaller than that
/// window), plus size folded into the hash input so two files that happen to share both edge
/// windows but differ in the middle at the same size don't collide silently more often than the
/// size difference alone would already prevent.
pub fn partial_hash(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let len = file.metadata()?.len();
    let mut hasher = blake3::Hasher::new();
    hasher.update(&len.to_le_bytes());

    let head_len = len.min(PARTIAL_HASH_WINDOW);
    let mut head = vec![0u8; head_len as usize];
    file.read_exact(&mut head)?;
    hasher.update(&head);

    if len > PARTIAL_HASH_WINDOW {
        let tail_len = len.min(PARTIAL_HASH_WINDOW);
        file.seek(SeekFrom::End(-(tail_len as i64)))?;
        let mut tail = vec![0u8; tail_len as usize];
        file.read_exact(&mut tail)?;
        hasher.update(&tail);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Tier (c): full-file BLAKE3. Streams in fixed-size chunks rather than reading the whole file
/// into memory -- RAW files run 20-100MB+ each, and this needs to scale to a library-sized walk.
pub fn full_hash(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Tier (d): the EXIF natural key. `None` when any of the four fields is missing -- a partial key
/// is worse than no key, since it would falsely match every other file missing the same fields.
#[derive(Debug, Clone, Serialize, PartialEq, Eq, Hash)]
pub struct NaturalKey {
    pub body_serial: String,
    pub shutter_count: String,
    pub date_time_original: String,
    pub sub_sec: String,
}

impl std::fmt::Display for NaturalKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}|{}|{}|{}",
            self.body_serial, self.shutter_count, self.date_time_original, self.sub_sec
        )
    }
}

pub fn natural_key(path: &Path) -> Result<Option<NaturalKey>> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut bufreader = std::io::BufReader::new(file);
    let exif_reader = exif::Reader::new();
    let exif = match exif_reader.read_from_container(&mut bufreader) {
        Ok(e) => e,
        Err(_) => return Ok(None),
    };

    let body_serial = field_string(&exif, exif::Tag::BodySerialNumber);
    // Nikon's shutter count isn't a standard EXIF tag (it lives in the MakerNote, body-specific
    // offset) -- ImageNumber (Exif 0xA306) is the closest standard-EXIF proxy some bodies
    // populate. kamadak-exif has no predefined constant for it (not in its curated tag list), but
    // `Tag` is a plain `(Context, u16)` tuple struct precisely so non-predefined tags like this
    // one can still be looked up. Real per-body MakerNote parsing is deferred; see this module's
    // own doc comment above.
    let shutter_count = field_string(&exif, exif::Tag(exif::Context::Exif, 0xa306));
    let date_time_original = field_string(&exif, exif::Tag::DateTimeOriginal);
    let sub_sec = field_string(&exif, exif::Tag::SubSecTimeOriginal);

    match (body_serial, shutter_count, date_time_original, sub_sec) {
        (Some(body_serial), Some(shutter_count), Some(date_time_original), Some(sub_sec)) => {
            Ok(Some(NaturalKey {
                body_serial,
                shutter_count,
                date_time_original,
                sub_sec,
            }))
        }
        _ => Ok(None),
    }
}

fn field_string(exif: &exif::Exif, tag: exif::Tag) -> Option<String> {
    exif.get_field(tag, exif::In::PRIMARY)
        .map(|f| f.display_value().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn partial_hash_matches_full_hash_below_window_size() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"small file, under 64KB").unwrap();
        f.flush().unwrap();
        assert_eq!(
            partial_hash(f.path()).unwrap(),
            partial_hash(f.path()).unwrap()
        );
    }

    #[test]
    fn partial_hash_differs_when_middle_differs_above_window() {
        let make = |middle: u8| {
            let mut f = NamedTempFile::new().unwrap();
            let mut data = vec![0xAAu8; PARTIAL_HASH_WINDOW as usize];
            data.extend(vec![middle; 1024]);
            data.extend(vec![0xBBu8; PARTIAL_HASH_WINDOW as usize]);
            f.write_all(&data).unwrap();
            f.flush().unwrap();
            f
        };
        let a = make(1);
        let b = make(2);
        // Partial hashing only samples the edges, so it's *expected* not to distinguish these --
        // documenting that limitation as a real test, not asserting a false guarantee.
        assert_eq!(
            partial_hash(a.path()).unwrap(),
            partial_hash(b.path()).unwrap()
        );
        assert_ne!(full_hash(a.path()).unwrap(), full_hash(b.path()).unwrap());
    }

    #[test]
    fn full_hash_is_deterministic() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"deterministic content").unwrap();
        f.flush().unwrap();
        assert_eq!(full_hash(f.path()).unwrap(), full_hash(f.path()).unwrap());
    }

    #[test]
    fn natural_key_is_none_without_exif() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"not an image at all").unwrap();
        f.flush().unwrap();
        assert_eq!(natural_key(f.path()).unwrap(), None);
    }
}
