//! #301: T2 (screen-resolution preview) generation for the loupe path, feeding `nicti-lair`'s
//! `Larder` (#27). ADR-0029: T2 is the camera's embedded `JpgFromRaw` decoded, resized to a
//! 3840px long edge and re-encoded as JPEG (a plain JPEG asset uses its own resized master
//! instead), keyed `(asset_id, T2, render_hash)`. Lives here rather than `nicti-lair::pounce_jobs`
//! for the same reason `decode_job.rs` does: it needs the `image` crate, which `nicti-lair`
//! deliberately doesn't depend on.
//!
//! Both jobs run on Pounce's CPU lane at `Background` priority. [`T2Job`] generates and stores
//! one asset's T2; [`CompactJob`] runs `Larder::compact` so a multi-GiB pack rewrite never
//! happens inline on whichever thread happened to `put` last (`open_larder` turns the Larder's own
//! auto-compaction off).

use std::io::Cursor;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};

use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ImageDecoder, ImageReader};
use nicti_cornea::embedded::{EmbeddedJpeg, FileSource, PreviewSource, Walker};
use nicti_lair::larder::{Larder, LarderKey, LarderTier};
use nicti_lair::pounce_jobs::ReportSlot;
use nicti_pounce::{ChunkedJob, JobError, JobKind, JobSpec, Lane, Priority, Progress, Step};

/// The larder shared between the loupe session (reads, on the UI thread) and the T2/compaction
/// jobs (writes, on Pounce workers). `Larder`'s methods take `&mut self`, so one mutex guards it.
pub type SharedLarder = Arc<Mutex<Larder>>;

/// Non-blocking lock. `None` means a `put`/compaction holds it right now. A poisoned lock (a job
/// panicked while holding it) is repaired via `Larder::recover_after_panic` and un-poisoned, so a
/// single panic can't disable the cache for the rest of the session.
pub fn try_lock_larder(larder: &SharedLarder) -> Option<MutexGuard<'_, Larder>> {
    match larder.try_lock() {
        Ok(guard) => Some(guard),
        Err(TryLockError::Poisoned(p)) => {
            let mut guard = p.into_inner();
            // A panic inside `put` can leave its transaction open, wedging every later `put`.
            guard.recover_after_panic();
            larder.clear_poison();
            Some(guard)
        }
        Err(TryLockError::WouldBlock) => None,
    }
}

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
    let mut larder = Larder::open(
        &larder_dir_for(catalog_path),
        crate::cache_settings::larder_config_for(catalog_path),
    )
    .ok()?;
    // Compaction runs as a Pounce `CompactJob` (see `loupe::poll_t2`), never inline in `put`.
    larder.set_auto_compact(false);
    Some(Arc::new(Mutex::new(larder)))
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
    let mut decoder = ImageReader::new(Cursor::new(jpeg))
        .with_guessed_format()
        .map_err(|e| e.to_string())?
        .into_decoder()
        .map_err(|e| e.to_string())?;
    // A portrait phone JPEG carries its rotation as an EXIF tag, not in the pixels.
    let orientation = decoder.orientation().map_err(|e| e.to_string())?;
    let mut decoded = DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    decoded.apply_orientation(orientation);
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
        let len = std::fs::metadata(path)
            .map_err(|e| format!("{}: {e}", path.display()))?
            .len();
        if len > MAX_EMBEDDED_JPEG_BYTES {
            return Err(format!("{}: JPEG too large ({len} bytes)", path.display()));
        }
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

/// A T2 job's outcome, stored in its slot rather than returned as a job error -- same reasoning as
/// `decode_job::DecodeResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum T2Outcome {
    /// Stored (or already present).
    Stored,
    /// Nothing stored, but nothing is wrong with the asset: the file is unreachable (an unmounted
    /// archive drive) or the Larder stayed busy past `LOCK_WAIT`. Not recorded as a failure, so
    /// the next prefetch of this asset -- the next cursor move, not a stationary cursor -- tries
    /// again.
    Retry(String),
    /// This asset can't produce a T2 (no embedded preview, corrupt JPEG, panic). Remembered per
    /// asset identity so it isn't re-read on every cursor move. Includes a failed `put`.
    Failed(String),
}

/// A compaction's outcome.
pub type CompactResult = Result<(), String>;

pub struct T2Job {
    larder: SharedLarder,
    path: PathBuf,
    asset_id: i64,
    render_hash: String,
    label: String,
    image_index: usize,
    done: bool,
    result: ReportSlot<T2Outcome>,
}

impl T2Job {
    pub fn new(
        larder: SharedLarder,
        path: PathBuf,
        asset_id: i64,
        render_hash: String,
        image_index: usize,
    ) -> (Self, ReportSlot<T2Outcome>) {
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

    fn run(&self) -> T2Outcome {
        // Reachability first, on this worker rather than the UI thread: `metadata` on a hung
        // network/unmounted drive can block for the OS timeout.
        if std::fs::metadata(&self.path).is_err() {
            return T2Outcome::Retry("source file unreachable".into());
        }
        // Never wait unboundedly on the Larder lock: a compaction holds it for the whole rewrite,
        // and a worker parked here is a worker a foreground decode can't use.
        {
            let Some(larder) = lock_larder_within(&self.larder, LOCK_WAIT) else {
                return T2Outcome::Retry("larder busy".into());
            };
            // Another path (a second session, an earlier job that finished after this was queued)
            // may have stored it already.
            if larder.contains(self.key()).unwrap_or(false) {
                return T2Outcome::Stored;
            }
        }
        let payload = match generate_t2(&self.path) {
            Ok(payload) => payload,
            Err(e) => return T2Outcome::Failed(e),
        };
        let Some(mut larder) = lock_larder_within(&self.larder, LOCK_WAIT) else {
            return T2Outcome::Retry("larder busy".into());
        };
        match larder.put(self.key(), &payload) {
            Ok(true) => T2Outcome::Stored,
            Ok(false) => T2Outcome::Failed("T2 larger than the whole cache cap".into()),
            // A `put` error (disk full, read-only directory, a wedged transaction) tends to be
            // persistent: `Failed` (once per identity) rather than `Retry`, so it can't become a
            // decode-resize-encode-fail loop on every cursor move.
            Err(e) => T2Outcome::Failed(e.to_string()),
        }
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
        // Pounce doesn't catch panics, and a hostile file must not be able to wedge this asset's
        // slot (and a worker thread) for the rest of the session.
        let outcome = catch_unwind(AssertUnwindSafe(|| self.run()))
            .unwrap_or_else(|_| T2Outcome::Failed("T2 generation panicked".into()));
        *self.result.lock().unwrap() = Some(outcome);
        self.done = true;
        Ok(Step::Done)
    }
}

/// How long a T2 job waits for the Larder lock before giving up with `Retry`. Bounded so a worker
/// is never parked behind a whole compaction, but long enough that two jobs finishing together
/// (or a UI-thread `get`) don't cost a finished payload.
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

pub(crate) fn lock_larder_within(
    larder: &SharedLarder,
    wait: std::time::Duration,
) -> Option<MutexGuard<'_, Larder>> {
    let deadline = std::time::Instant::now() + wait;
    loop {
        if let Some(guard) = try_lock_larder(larder) {
            return Some(guard);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Runs `Larder::compact` as a Pounce background job.
pub struct CompactJob {
    larder: SharedLarder,
    done: bool,
    result: ReportSlot<CompactResult>,
}

impl CompactJob {
    pub fn new(larder: SharedLarder) -> (Self, ReportSlot<CompactResult>) {
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
        // The one place that *should* block on the lock: compaction is the long holder.
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let mut larder = self.larder.lock().unwrap_or_else(|e| e.into_inner());
            larder.compact().map_err(|e| e.to_string())
        }))
        .unwrap_or_else(|_| Err("compaction panicked".to_string()));
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
    use nicti_lair::larder::LarderConfig;

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
        assert_eq!(slot.lock().unwrap().take(), Some(T2Outcome::Stored));

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
        // A missing source is an unreachable file (retry later), not a permanent failure.
        assert!(matches!(
            slot.lock().unwrap().take(),
            Some(T2Outcome::Retry(_))
        ));
    }

    #[test]
    fn t2_job_retries_after_a_bounded_wait_when_the_larder_stays_busy() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a.NEF");
        std::fs::write(&src, tiff_with_jpeg(&jpeg_of(800, 600))).unwrap();
        let larder = open_larder(&dir.path().join("larder"));

        let (mut job, slot) = T2Job::new(larder.clone(), src, 7, "embedded:x".into(), 0);
        // Simulates a running compaction holding the lock: the job must not park a worker on it.
        let held = larder.lock().unwrap();
        assert!(matches!(job.step(), Ok(Step::Done)));
        drop(held);
        assert!(matches!(
            slot.lock().unwrap().take(),
            Some(T2Outcome::Retry(_))
        ));
        assert_eq!(larder.lock().unwrap().stats().unwrap().entry_count, 0);
    }

    #[test]
    fn a_corrupt_embedded_jpeg_is_a_permanent_failure() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a.NEF");
        std::fs::write(&src, tiff_with_jpeg(b"not really a jpeg")).unwrap();
        let larder = open_larder(&dir.path().join("larder"));
        let (mut job, slot) = T2Job::new(larder, src, 1, "embedded:x".into(), 0);
        assert!(matches!(job.step(), Ok(Step::Done)));
        assert!(matches!(
            slot.lock().unwrap().take(),
            Some(T2Outcome::Failed(_))
        ));
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
