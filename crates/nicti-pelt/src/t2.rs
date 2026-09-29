//! #301: T2 (screen-resolution preview) generation for the loupe path, feeding `nicti-lair`'s
//! `Larder` (#27). ADR-0029: T2 is the camera's embedded `JpgFromRaw` decoded, resized to a
//! 3840px long edge and re-encoded as JPEG (a plain JPEG asset uses its own resized master
//! instead), keyed `(asset_id, T2, render_hash)`. Lives here rather than `nicti-lair::pounce_jobs`
//! for the same reason `decode_job.rs` does: it needs the `image` crate, which `nicti-lair`
//! deliberately doesn't depend on.
//!
//! Both jobs run on Pounce's CPU lane at `Background` priority. [`T2Job`] generates and stores
//! one asset's T2; [`CompactJob`] runs `Larder::compact` so a multi-GiB pack rewrite never
//! happens inline on whichever thread happened to `put` last (the session turns the Larder's own
//! auto-compaction off, see `LoupeSession::with_larder`).

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ImageReader};
use nicti_cornea::embedded::{EmbeddedJpeg, FileSource, PreviewSource, Walker};
use nicti_lair::larder::{Larder, LarderConfig, LarderKey, LarderTier};
use nicti_lair::pounce_jobs::ReportSlot;
use nicti_pounce::{ChunkedJob, JobError, JobKind, JobSpec, Lane, Priority, Progress, Step};

/// The larder shared between the loupe session (reads, on the UI thread) and the T2/compaction
/// jobs (writes, on Pounce workers). `Larder`'s methods take `&mut self`, so one mutex guards it.
pub type SharedLarder = Arc<Mutex<Larder>>;

/// ADR-0029: T2's long edge, in pixels. Sources already smaller are never upscaled.
pub const T2_LONG_EDGE: u32 = 3840;
/// JPEG quality ADR-0029/ADR-0143 measured T2 at (SSIM 0.9335).
const T2_JPEG_QUALITY: u8 = 85;
/// Rejects an embedded-JPEG entry whose IFD-declared length is absurd -- the offset/length come
/// straight from the (untrusted) file, and `read_range` allocates that many bytes up front.
const MAX_EMBEDDED_JPEG_BYTES: u64 = 256 * 1024 * 1024;

/// The Larder `render_hash` for a camera-JPEG-derived T2. Includes the asset's identity (its
/// `loupe::asset_cache_key`, hex) rather than a bare constant sentinel: a re-ingest that changes
/// the file's content changes the identity, so the old T2 reads as stale (a miss) instead of being
/// served for the new revision. No edit-document hash is involved -- T2 here is the *camera's*
/// preview, not a render of the user's edits.
pub fn render_hash(identity: &blake3::Hash) -> String {
    format!("embedded:{}", identity.to_hex())
}

/// Where a session's Larder lives: a sibling of the catalog file, so it moves with it and never
/// lands in the cwd on its own.
pub fn larder_dir_for(catalog_path: &Path) -> PathBuf {
    let mut name = catalog_path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| "nicti".into());
    name.push(".larder");
    catalog_path.with_file_name(name)
}

/// Opens (creating if needed) the Larder beside `catalog_path`, or `None` if it can't be opened.
pub fn open_larder(catalog_path: &Path) -> Option<SharedLarder> {
    Larder::open(&larder_dir_for(catalog_path), LarderConfig::default())
        .ok()
        .map(|l| Arc::new(Mutex::new(l)))
}

fn is_plain_jpeg(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("jpg") || e.eq_ignore_ascii_case("jpeg"))
}

/// The best embedded JPEG to derive T2 from: Nikon's PreviewIFD (`JpgFromRaw`) when present,
/// otherwise the largest one found.
fn pick_embedded(mut found: Vec<EmbeddedJpeg>) -> Option<EmbeddedJpeg> {
    found.retain(|j| j.byte_len > 0 && j.byte_len <= MAX_EMBEDDED_JPEG_BYTES);
    if let Some(i) = found
        .iter()
        .position(|j| j.source == PreviewSource::NikonPreviewIfd)
    {
        return Some(found.swap_remove(i));
    }
    found.into_iter().max_by_key(|j| {
        let area =
            u64::from(j.declared_width.unwrap_or(0)) * u64::from(j.declared_height.unwrap_or(0));
        (area, j.byte_len)
    })
}

/// Decodes `jpeg`, downscales to [`T2_LONG_EDGE`] (never upscaling) and re-encodes as JPEG.
fn resize_and_encode(jpeg: &[u8]) -> Result<Vec<u8>, String> {
    let decoded = ImageReader::new(Cursor::new(jpeg))
        .with_guessed_format()
        .map_err(|e| e.to_string())?
        .decode()
        .map_err(|e| e.to_string())?;
    let (w, h) = (decoded.width(), decoded.height());
    if w == 0 || h == 0 {
        return Err("decoded image has a zero dimension".into());
    }
    let resized: DynamicImage = if w.max(h) > T2_LONG_EDGE {
        // `resize` keeps the aspect ratio and fits inside the bounding box, so a square box of
        // the long edge is exactly "long edge = 3840".
        decoded.resize(T2_LONG_EDGE, T2_LONG_EDGE, FilterType::Triangle)
    } else {
        decoded
    };
    let rgb = resized.to_rgb8();
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, T2_JPEG_QUALITY)
        .encode_image(&rgb)
        .map_err(|e| e.to_string())?;
    Ok(out)
}

/// Produces one asset's T2 payload from its source file -- no RAW decode involved.
pub fn generate_t2(path: &Path) -> Result<Vec<u8>, String> {
    if is_plain_jpeg(path) {
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        return resize_and_encode(&bytes);
    }
    let source = FileSource::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut walker = Walker::new(source).map_err(|e| e.to_string())?;
    let found = walker.find_embedded_jpegs().map_err(|e| e.to_string())?;
    let jpeg = pick_embedded(found).ok_or_else(|| "no embedded JPEG preview".to_string())?;
    let len = usize::try_from(jpeg.byte_len).map_err(|e| e.to_string())?;
    let bytes = walker
        .read_range(jpeg.file_offset, len)
        .map_err(|e| e.to_string())?;
    resize_and_encode(&bytes)
}

/// A T2 job's outcome. `Err` (a file with no embedded preview, a corrupt JPEG) is a normal result
/// to store, not a job failure -- same reasoning as `decode_job::DecodeResult`.
pub type T2Result = Result<(), String>;

pub struct T2Job {
    larder: SharedLarder,
    path: PathBuf,
    asset_id: i64,
    render_hash: String,
    label: String,
    image_index: usize,
    done: bool,
    result: ReportSlot<T2Result>,
}

impl T2Job {
    pub fn new(
        larder: SharedLarder,
        path: PathBuf,
        asset_id: i64,
        render_hash: String,
        image_index: usize,
    ) -> (Self, ReportSlot<T2Result>) {
        let result = Arc::new(Mutex::new(None));
        let label = format!("Preview: {}", path.display());
        let job = T2Job {
            larder,
            path,
            asset_id,
            render_hash,
            label,
            image_index,
            done: false,
            result: result.clone(),
        };
        (job, result)
    }

    fn key(&self) -> LarderKey<'_> {
        LarderKey {
            asset_id: self.asset_id,
            tier: LarderTier::T2,
            render_hash: &self.render_hash,
        }
    }

    fn run(&self) -> T2Result {
        // Another path (a second session, an earlier job that finished after this was queued) may
        // have stored it already.
        if self
            .larder
            .lock()
            .map_err(|_| "larder lock poisoned".to_string())?
            .contains(self.key())
            .unwrap_or(false)
        {
            return Ok(());
        }
        let payload = generate_t2(&self.path)?;
        self.larder
            .lock()
            .map_err(|_| "larder lock poisoned".to_string())?
            .put(self.key(), &payload)
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

impl ChunkedJob for T2Job {
    fn spec(&self) -> JobSpec {
        JobSpec {
            priority: Priority::Background,
            kind: JobKind::Preview,
            lane: Lane::Cpu,
            vram_bytes: 0,
            image_index: Some(self.image_index),
        }
    }

    fn label(&self) -> String {
        self.label.clone()
    }

    fn progress(&self) -> Progress {
        Progress {
            done: u64::from(self.done),
            total: Some(1),
        }
    }

    /// Never returns `Err`: a failure is stored inside the slot so the poller always sees it.
    fn step(&mut self) -> Result<Step, JobError> {
        let outcome = self.run();
        *self.result.lock().unwrap() = Some(outcome);
        self.done = true;
        Ok(Step::Done)
    }
}

/// Runs `Larder::compact` as a Pounce background job.
pub struct CompactJob {
    larder: SharedLarder,
    done: bool,
    result: ReportSlot<T2Result>,
}

impl CompactJob {
    pub fn new(larder: SharedLarder) -> (Self, ReportSlot<T2Result>) {
        let result = Arc::new(Mutex::new(None));
        (
            CompactJob {
                larder,
                done: false,
                result: result.clone(),
            },
            result,
        )
    }
}

impl ChunkedJob for CompactJob {
    fn spec(&self) -> JobSpec {
        JobSpec {
            priority: Priority::Background,
            kind: JobKind::Preview,
            lane: Lane::Cpu,
            vram_bytes: 0,
            // Not tied to an image: sorts behind every nearest-to-cursor decode/T2 job.
            image_index: None,
        }
    }

    fn label(&self) -> String {
        "Compact preview cache".to_string()
    }

    fn progress(&self) -> Progress {
        Progress {
            done: u64::from(self.done),
            total: Some(1),
        }
    }

    fn step(&mut self) -> Result<Step, JobError> {
        let outcome = match self.larder.lock() {
            Ok(mut larder) => larder.compact().map_err(|e| e.to_string()),
            Err(_) => Err("larder lock poisoned".to_string()),
        };
        *self.result.lock().unwrap() = Some(outcome);
        self.done = true;
        Ok(Step::Done)
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    use super::*;

    pub(crate) fn jpeg_of(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([(x % 251) as u8, (y % 241) as u8, 90])
        });
        let mut out = Vec::new();
        JpegEncoder::new_with_quality(&mut out, 90)
            .encode_image(&img)
            .unwrap();
        out
    }

    pub(crate) fn dims(jpeg: &[u8]) -> (u32, u32) {
        let img = image::load_from_memory(jpeg).unwrap();
        (img.width(), img.height())
    }

    /// A minimal little-endian TIFF whose IFD0 carries only a JPEGInterchangeFormat pair pointing
    /// at `jpeg` -- enough for `Walker::find_embedded_jpegs` to report one `PreviewSource::Ifd0`.
    pub(crate) fn tiff_with_jpeg(jpeg: &[u8]) -> Vec<u8> {
        let jpeg_offset: u32 = 8 + 2 + 2 * 12 + 4;
        let mut t = vec![b'I', b'I', 42, 0, 8, 0, 0, 0];
        t.extend_from_slice(&2u16.to_le_bytes());
        for (tag, value) in [(0x0201u16, jpeg_offset), (0x0202u16, jpeg.len() as u32)] {
            t.extend_from_slice(&tag.to_le_bytes());
            t.extend_from_slice(&4u16.to_le_bytes()); // LONG
            t.extend_from_slice(&1u32.to_le_bytes());
            t.extend_from_slice(&value.to_le_bytes());
        }
        t.extend_from_slice(&0u32.to_le_bytes());
        t.extend_from_slice(jpeg);
        t
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;

    #[test]
    fn a_large_source_is_downscaled_to_the_t2_long_edge() {
        let out = resize_and_encode(&jpeg_of(5000, 3000)).unwrap();
        assert_eq!(dims(&out), (T2_LONG_EDGE, 2304));
    }

    #[test]
    fn a_portrait_source_is_bounded_by_its_height() {
        let out = resize_and_encode(&jpeg_of(3000, 5000)).unwrap();
        assert_eq!(dims(&out), (2304, T2_LONG_EDGE));
    }

    #[test]
    fn a_small_source_is_never_upscaled() {
        let out = resize_and_encode(&jpeg_of(640, 480)).unwrap();
        assert_eq!(dims(&out), (640, 480));
    }

    #[test]
    fn garbage_bytes_are_an_error_not_a_panic() {
        assert!(resize_and_encode(b"definitely not a jpeg").is_err());
    }

    #[test]
    fn a_plain_jpeg_asset_uses_its_own_resized_master() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.JPG");
        std::fs::write(&path, jpeg_of(4200, 2800)).unwrap();
        assert_eq!(dims(&generate_t2(&path).unwrap()), (T2_LONG_EDGE, 2560));
    }

    #[test]
    fn a_raw_container_yields_its_embedded_jpeg() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.NEF");
        std::fs::write(&path, tiff_with_jpeg(&jpeg_of(4200, 2800))).unwrap();
        assert_eq!(dims(&generate_t2(&path).unwrap()), (T2_LONG_EDGE, 2560));
    }

    #[test]
    fn a_raw_with_no_embedded_jpeg_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.NEF");
        // Valid TIFF header + an empty IFD0.
        let mut t = vec![b'I', b'I', 42, 0, 8, 0, 0, 0, 0, 0];
        t.extend_from_slice(&0u32.to_le_bytes());
        std::fs::write(&path, t).unwrap();
        assert!(generate_t2(&path).is_err());
    }

    #[test]
    fn a_missing_file_is_an_error() {
        assert!(generate_t2(Path::new("/definitely/not/here.NEF")).is_err());
    }

    fn open_larder(dir: &Path) -> SharedLarder {
        Arc::new(Mutex::new(
            Larder::open(dir, LarderConfig::default()).unwrap(),
        ))
    }

    #[test]
    fn t2_job_stores_the_payload_under_its_key_and_resolves_its_slot() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a.NEF");
        std::fs::write(&src, tiff_with_jpeg(&jpeg_of(800, 600))).unwrap();
        let larder = open_larder(&dir.path().join("larder"));

        let (mut job, slot) = T2Job::new(larder.clone(), src, 7, "embedded:x".into(), 0);
        assert!(matches!(job.step(), Ok(Step::Done)));
        assert_eq!(slot.lock().unwrap().take(), Some(Ok(())));

        let key = LarderKey {
            asset_id: 7,
            tier: LarderTier::T2,
            render_hash: "embedded:x",
        };
        let stored = larder.lock().unwrap().get(key).unwrap().unwrap();
        assert_eq!(dims(&stored), (800, 600));
        // A different identity is a stale entry -> a miss.
        let stale = LarderKey {
            render_hash: "embedded:y",
            ..key
        };
        assert!(larder.lock().unwrap().get(stale).unwrap().is_none());
    }

    #[test]
    fn t2_job_reports_a_failure_through_its_slot_instead_of_hanging() {
        let dir = tempfile::tempdir().unwrap();
        let larder = open_larder(&dir.path().join("larder"));
        let (mut job, slot) = T2Job::new(
            larder,
            dir.path().join("missing.NEF"),
            1,
            "embedded:x".into(),
            0,
        );
        assert!(matches!(job.step(), Ok(Step::Done)));
        assert!(slot.lock().unwrap().take().unwrap().is_err());
    }

    #[test]
    fn compact_job_compacts_and_resolves_its_slot() {
        let dir = tempfile::tempdir().unwrap();
        let larder = open_larder(&dir.path().join("larder"));
        let (mut job, slot) = CompactJob::new(larder);
        assert!(matches!(job.step(), Ok(Step::Done)));
        assert_eq!(slot.lock().unwrap().take(), Some(Ok(())));
    }

    #[test]
    fn larder_dir_is_a_sibling_of_the_catalog() {
        assert_eq!(
            larder_dir_for(Path::new("/x/nicti.catalog.sqlite")),
            PathBuf::from("/x/nicti.catalog.sqlite.larder")
        );
        assert_eq!(
            larder_dir_for(Path::new("nicti.catalog.sqlite")),
            PathBuf::from("nicti.catalog.sqlite.larder")
        );
    }

    #[test]
    fn render_hash_changes_with_identity() {
        assert_ne!(
            render_hash(&blake3::hash(b"a")),
            render_hash(&blake3::hash(b"b"))
        );
    }
}
