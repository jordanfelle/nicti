//! Scruff: the import/ingest pipeline (#22). Named the way a mother cat carries a kitten by the
//! scruff of its neck -- this is what actually moves a file into the catalog. Scans a folder for
//! RAW files, fingerprints and upserts each one, and extracts its T0 grid preview (ADR-0017) via
//! `nicti_decode::embedded`'s IFD walker -- no RAW decode needed for that.
//!
//! Runs serially in v1; wiring this into Pounce (the job scheduler) is a follow-up, not part of
//! this ticket.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use unicode_normalization::UnicodeNormalization;
use walkdir::WalkDir;

use nicti_decode::embedded::{EmbeddedJpeg, FileSource, PreviewSource, Walker};

use crate::{CatalogError, CatalogStore, NewAsset, Preview};

/// v1 targets Nikon NEF only (ADR-0002's language-and-architecture topic) -- NRW is Nikon's
/// compact-body variant of the same format. Widening this list is a future-camera-support
/// concern, not this ticket's.
const RAW_EXTENSIONS: &[&str] = &["nef", "nrw"];

const PARTIAL_HASH_WINDOW: u64 = 64 * 1024;

/// Upper bound on a T0 preview's byte length -- a real Nikon PreviewIFD JPEG runs well under 1MB
/// (the `preview-tiers` topic measured ~138KB), so this is generous headroom, not a tight fit.
/// Without a cap, a crafted or corrupt file's declared `JPEGInterchangeFormatLength` (fully
/// attacker/file-controlled -- `ifd.rs` trusts the IFD's own tag value) could drive an allocation
/// and a persistent SQLite BLOB write proportional to an arbitrarily large declared length,
/// unrelated to the file's real preview size -- found by CodeRabbit's review.
const MAX_T0_PREVIEW_BYTES: u64 = 8 * 1024 * 1024;

/// Outcome of one `ingest_root` run. A single bad file never aborts the run -- it lands in
/// `failed` and every other candidate still gets processed.
#[derive(Debug, Default)]
pub struct IngestReport {
    pub added: u64,
    pub updated: u64,
    pub skipped_unchanged: u64,
    pub moved: u64,
    pub failed: Vec<(PathBuf, String)>,
}

/// Normalizes an absolute path's tail (relative to `root_path`) into the canonical form stored in
/// `asset.rel_path`: forward slashes, NFC-composed, no leading/trailing slash. Same rule
/// `spikes/homing/src/path.rs` uses for ADR-0020's identity scheme.
fn normalize_rel_path(path: &Path) -> String {
    let raw = path.to_string_lossy().replace('\\', "/");
    let composed: String = raw.nfc().collect();
    composed.trim_matches('/').to_string()
}

/// Case-folded form for the `rel_path_fold` lookup column -- NTFS/exFAT are case-insensitive but
/// case-preserving, so a lookup by path must not depend on which case a file happened to be
/// written in.
fn fold(rel_path: &str) -> String {
    rel_path.to_lowercase()
}

/// Tier (b) from ADR-0020's relink-tier list: BLAKE3 over the first and last 64KB (or the whole
/// file, if smaller), with the size folded into the hash input. Cheap enough to run on every file
/// at import time, and the tier `spikes/homing` and this pipeline both use as the default identity
/// proxy for move detection.
fn partial_hash(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
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

fn field_string(exif: &exif::Exif, tag: exif::Tag) -> Option<String> {
    exif.get_field(tag, exif::In::PRIMARY)
        .map(|f| f.display_value().to_string())
}

#[derive(Debug, Default)]
struct ExifFields {
    make: Option<String>,
    model: Option<String>,
    captured_at: Option<String>,
    natural_key: Option<String>,
}

/// Reads the EXIF fields ingest cares about. A file with no readable EXIF (or no EXIF at all)
/// yields every field `None` rather than an error -- a RAW file missing/malformed EXIF is still a
/// real asset worth cataloging, just without these facets populated yet.
fn read_exif_fields(path: &Path) -> ExifFields {
    let Ok(file) = File::open(path) else {
        return ExifFields::default();
    };
    let mut bufreader = std::io::BufReader::new(file);
    let exif_reader = exif::Reader::new();
    let Ok(exif) = exif_reader.read_from_container(&mut bufreader) else {
        return ExifFields::default();
    };

    let make = field_string(&exif, exif::Tag::Make);
    let model = field_string(&exif, exif::Tag::Model);
    let captured_at = field_string(&exif, exif::Tag::DateTimeOriginal);
    let body_serial = field_string(&exif, exif::Tag::BodySerialNumber);
    // Nikon's shutter count isn't a standard EXIF tag; ImageNumber (Exif 0xA306) is the closest
    // standard-EXIF proxy some bodies populate -- same natural-key shape `spikes/homing` uses.
    let shutter_count = field_string(&exif, exif::Tag(exif::Context::Exif, 0xa306));
    let sub_sec = field_string(&exif, exif::Tag::SubSecTimeOriginal);

    // A partial key is worse than no key -- it would falsely match every other file missing the
    // same fields. Only a complete key is stored.
    let natural_key = match (&body_serial, &shutter_count, &captured_at, &sub_sec) {
        (Some(bs), Some(sc), Some(dt), Some(ss)) => Some(format!("{bs}|{sc}|{dt}|{ss}")),
        _ => None,
    };

    ExifFields {
        make,
        model,
        captured_at,
        natural_key,
    }
}

/// Finds this file's T0 grid preview (ADR-0017: the Nikon PreviewIFD JPEG, copied verbatim), or
/// the largest embedded JPEG found if no Nikon PreviewIFD is present (a non-Nikon RAW, or a
/// PreviewIFD-less file). Returns `None` rather than an error when a file has no embedded JPEG at
/// all, or isn't a recognizable TIFF-based RAW -- ingest still catalogs the asset, just without a
/// preview to show yet.
/// Picks which discovered embedded JPEG becomes the T0 preview: the Nikon PreviewIFD JPEG
/// preferred, falling back to the largest one found -- in both cases, only among candidates whose
/// *declared* length doesn't exceed [`MAX_T0_PREVIEW_BYTES`]. A candidate is excluded before any
/// read is attempted, not truncated after allocating. Pure and directly testable (no file I/O),
/// separated from `extract_t0_preview`'s walking/reading so the cap logic doesn't need a
/// multi-megabyte fixture to exercise.
fn select_t0_candidate(jpegs: &[EmbeddedJpeg]) -> Option<&EmbeddedJpeg> {
    let within_cap = |j: &&EmbeddedJpeg| j.byte_len <= MAX_T0_PREVIEW_BYTES;
    jpegs
        .iter()
        .filter(within_cap)
        .find(|j| j.source == PreviewSource::NikonPreviewIfd)
        .or_else(|| jpegs.iter().filter(within_cap).max_by_key(|j| j.byte_len))
}

fn extract_t0_preview(path: &Path) -> Option<Preview> {
    let source = FileSource::open(path).ok()?;
    let mut walker = Walker::new(source).ok()?;
    let jpegs = walker.find_embedded_jpegs().ok()?;
    let chosen = select_t0_candidate(&jpegs)?;
    let bytes = walker
        .read_range(chosen.file_offset, chosen.byte_len as usize)
        .ok()?;
    Some(Preview {
        width: chosen.declared_width,
        height: chosen.declared_height,
        bytes,
    })
}

/// RAW-extension files under `root_path`, recursively, in walk order. A directory `WalkDir`
/// can't read (permission denied, a broken symlink) surfaces as `Err` rather than being silently
/// dropped -- found by CodeRabbit's review: `entry.ok()` used to discard that error outright, so
/// every file under an unreadable subdirectory went unvisited *and* unreported, and a caller could
/// see a clean `IngestReport` (`failed` empty) even though part of the tree was never scanned.
fn candidate_files(root_path: &Path) -> impl Iterator<Item = Result<PathBuf, walkdir::Error>> {
    WalkDir::new(root_path).into_iter().filter_map(|entry| {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => return Some(Err(e)),
        };
        if !entry.file_type().is_file() {
            return None;
        }
        let is_raw = entry
            .path()
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| RAW_EXTENSIONS.contains(&ext.to_lowercase().as_str()));
        is_raw.then(|| Ok(entry.into_path()))
    })
}

/// Scans `root_path` (already registered as `root_id` via [`CatalogStore::ensure_root`]) and
/// upserts every RAW file found. One bad file is recorded in the report's `failed` list rather
/// than aborting the run.
pub fn ingest_root(
    store: &dyn CatalogStore,
    root_id: i64,
    root_path: &Path,
) -> Result<IngestReport, CatalogError> {
    let mut report = IngestReport::default();

    for entry in candidate_files(root_path) {
        let path = match entry {
            Ok(path) => path,
            Err(e) => {
                let path = e.path().unwrap_or(root_path).to_path_buf();
                report.failed.push((path, e.to_string()));
                continue;
            }
        };
        if let Err(e) = ingest_one(store, root_id, root_path, &path, &mut report) {
            report.failed.push((path, e.to_string()));
        }
    }

    Ok(report)
}

fn ingest_one(
    store: &dyn CatalogStore,
    root_id: i64,
    root_path: &Path,
    path: &Path,
    report: &mut IngestReport,
) -> Result<(), CatalogError> {
    let rel = path.strip_prefix(root_path).unwrap_or(path).to_path_buf();
    let rel_path = normalize_rel_path(&rel);
    let rel_path_fold = fold(&rel_path);

    let metadata = std::fs::metadata(path).map_err(catalog_io_error)?;
    let size_bytes = metadata.len();
    let mtime_unix = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let existing = store.find_asset_by_path(root_id, &rel_path)?;
    if let Some(existing) = &existing {
        if existing.size_bytes == size_bytes && existing.mtime_unix == mtime_unix {
            report.skipped_unchanged += 1;
            return Ok(());
        }
    }

    let fingerprint = partial_hash(path).map_err(catalog_io_error)?;

    if existing.is_none() {
        // A fingerprint match under a different path is only a genuine *move* if the matched
        // row's old path is actually gone -- otherwise it's a second, distinct file that just
        // happens to share content (a literal duplicate, or a fingerprint collision), and
        // relinking would silently steal that row out from under its still-present file,
        // permanently losing that file's own catalog entry on every future rescan (found by
        // adversarial review). More than one existing asset can share this fingerprint, so every
        // candidate is checked, not just an arbitrary single match (found by CodeRabbit's
        // review) -- only checkable when a candidate is under the root currently being scanned; a
        // cross-root/cross-volume match can't be verified without resolving that other root's
        // mount path, which this trait has no way to do yet, so it's treated conservatively as
        // "can't prove it's gone".
        let move_target = store
            .find_by_fingerprint(&fingerprint)?
            .into_iter()
            .find(|matched| {
                (matched.root_id != root_id || matched.rel_path != rel_path)
                    && matched.root_id == root_id
                    && !root_path.join(&matched.rel_path).exists()
            });
        if let Some(matched) = move_target {
            store.relink_asset(
                matched.id,
                root_id,
                &rel_path,
                &rel_path_fold,
                size_bytes,
                mtime_unix,
            )?;
            report.moved += 1;
            return Ok(());
        }
    }

    let exif = read_exif_fields(path);
    let preview = extract_t0_preview(path);

    let imported_at = existing
        .as_ref()
        .map(|a| a.imported_at)
        .unwrap_or_else(now_unix);

    let new_asset = NewAsset {
        rel_path: rel_path.clone(),
        rel_path_fold,
        size_bytes,
        mtime_unix,
        fingerprint: Some(fingerprint),
        natural_key: exif.natural_key,
        make: exif.make,
        model: exif.model,
        captured_at: exif.captured_at,
        width: preview.as_ref().and_then(|p| p.width),
        height: preview.as_ref().and_then(|p| p.height),
        imported_at,
    };

    // The asset row and its T0 preview (written, or cleared if extraction found none this time)
    // commit together in one transaction -- see `CatalogStore::insert_asset`'s own doc comment
    // for why that atomicity matters (found by CodeRabbit's review).
    store.insert_asset(root_id, &new_asset, preview.as_ref())?;

    if existing.is_some() {
        report.updated += 1;
    } else {
        report.added += 1;
    }
    Ok(())
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn catalog_io_error(e: std::io::Error) -> CatalogError {
    // No dedicated CatalogError variant for a bare filesystem I/O error (stat/read failures never
    // reach rusqlite) -- surfaced through the same rusqlite::Error::ModuleError-adjacent path
    // would misrepresent the source, so a plain string-wrapped SqliteError isn't right either.
    // ingest_one always converts this into `report.failed`'s `(path, String)` shape immediately,
    // so a formatted string loses nothing a caller depends on.
    CatalogError::Io(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_rel_path_uses_forward_slashes_and_nfc() {
        assert_eq!(
            normalize_rel_path(Path::new("2026\\IMG.NEF")),
            "2026/IMG.NEF"
        );
        // "é" as e + combining acute (NFD) must normalize to the same string as precomposed é
        // (NFC).
        assert_eq!(
            normalize_rel_path(Path::new("caf\u{0065}\u{0301}.NEF")),
            normalize_rel_path(Path::new("caf\u{00e9}.NEF"))
        );
    }

    #[test]
    fn fold_is_case_insensitive() {
        assert_eq!(fold("2026/IMG_0001.NEF"), fold("2026/img_0001.nef"));
    }

    fn fake_jpeg(source: PreviewSource, byte_len: u64) -> EmbeddedJpeg {
        EmbeddedJpeg {
            source,
            file_offset: 0,
            byte_len,
            declared_width: None,
            declared_height: None,
            new_subfile_type: None,
        }
    }

    #[test]
    fn select_t0_candidate_prefers_nikon_preview_ifd_over_a_larger_thumbnail() {
        let jpegs = vec![
            fake_jpeg(PreviewSource::ThumbnailIfd, 500_000),
            fake_jpeg(PreviewSource::NikonPreviewIfd, 150_000),
        ];
        let chosen = select_t0_candidate(&jpegs).unwrap();
        assert_eq!(chosen.source, PreviewSource::NikonPreviewIfd);
    }

    #[test]
    fn select_t0_candidate_falls_back_to_largest_when_no_nikon_preview_ifd() {
        let jpegs = vec![
            fake_jpeg(PreviewSource::Ifd0, 1_000),
            fake_jpeg(PreviewSource::ThumbnailIfd, 50_000),
        ];
        let chosen = select_t0_candidate(&jpegs).unwrap();
        assert_eq!(chosen.source, PreviewSource::ThumbnailIfd);
    }

    #[test]
    fn select_t0_candidate_excludes_a_declared_length_over_the_cap() {
        // The only Nikon PreviewIFD candidate declares an implausible length -- must be excluded
        // rather than read/allocated, and the fallback must also skip it rather than picking it
        // as "largest".
        let jpegs = vec![
            fake_jpeg(PreviewSource::NikonPreviewIfd, MAX_T0_PREVIEW_BYTES + 1),
            fake_jpeg(PreviewSource::ThumbnailIfd, 50_000),
        ];
        let chosen = select_t0_candidate(&jpegs).unwrap();
        assert_eq!(chosen.source, PreviewSource::ThumbnailIfd);
    }

    #[test]
    fn select_t0_candidate_returns_none_when_every_candidate_exceeds_the_cap() {
        let jpegs = vec![fake_jpeg(
            PreviewSource::NikonPreviewIfd,
            MAX_T0_PREVIEW_BYTES + 1,
        )];
        assert!(select_t0_candidate(&jpegs).is_none());
    }

    #[test]
    fn candidate_files_filters_by_extension_case_insensitively() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.NEF"), b"x").unwrap();
        std::fs::write(dir.path().join("b.nrw"), b"x").unwrap();
        std::fs::write(dir.path().join("c.jpg"), b"x").unwrap();
        let found: Vec<_> = candidate_files(dir.path()).collect();
        assert_eq!(found.len(), 2);
    }
}
