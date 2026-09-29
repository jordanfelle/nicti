//! Pounce jobs behind the library grid (#30): the ordered id snapshot, and batched thumbnails.
//! Both follow `decode_job.rs`'s rule -- `step()` never returns `Err`; a failure is recorded in
//! the job's own slot, because the scheduler drops an erroring job without touching its slot and
//! a poller must never wait on a slot that will never resolve.

use std::sync::{Arc, Mutex};

use nicti_lair::pounce_jobs::ReportSlot;
use nicti_lair::{CatalogStore, Filter, PreviewTier, Sort};
use nicti_pounce::{ChunkedJob, JobError, JobKind, JobSpec, Lane, Priority, Progress, Step};

/// The catalog's ordered id list for one `Filter`/`Sort` (`CatalogStore::hunt_ids`), or the
/// catalog error's message.
pub type SnapshotResult = Result<Vec<i64>, String>;

/// One index-ordered catalog scan on the CPU lane, so a 1M-row snapshot never blocks the UI
/// thread. Single-chunk: a SQLite statement isn't interruptible mid-call.
pub struct SnapshotJob {
    store: Arc<dyn CatalogStore + Send + Sync>,
    filter: Filter,
    sort: Sort,
    done: bool,
    result: ReportSlot<SnapshotResult>,
}

impl SnapshotJob {
    pub fn new(
        store: Arc<dyn CatalogStore + Send + Sync>,
        filter: Filter,
        sort: Sort,
    ) -> (Self, ReportSlot<SnapshotResult>) {
        let result = Arc::new(Mutex::new(None));
        let job = SnapshotJob {
            store,
            filter,
            sort,
            done: false,
            result: result.clone(),
        };
        (job, result)
    }
}

impl ChunkedJob for SnapshotJob {
    fn spec(&self) -> JobSpec {
        JobSpec {
            priority: Priority::Background,
            kind: JobKind::Snapshot,
            lane: Lane::Cpu,
            vram_bytes: 0,
            // Not tied to a grid position. `GridSession::request_visible`'s reprioritization
            // special-cases `JobKind::Snapshot` to sort first; without that a reload would queue
            // behind every thumbnail batch of a scroll backlog.
            image_index: None,
        }
    }

    fn label(&self) -> String {
        "Library: reading catalog order".to_string()
    }

    fn progress(&self) -> Progress {
        Progress {
            done: u64::from(self.done),
            total: Some(1),
        }
    }

    fn step(&mut self) -> Result<Step, JobError> {
        let outcome = self
            .store
            .hunt_ids(&self.filter, self.sort)
            .map_err(|e| e.to_string());
        *self.result.lock().unwrap() = Some(outcome);
        self.done = true;
        Ok(Step::Done)
    }
}

/// Longest edge of a stored grid thumbnail, in pixels. T0 is 640x424; the grid never draws a cell
/// anywhere near that large, and 256 keeps a decoded thumbnail to ~256 KB of RGBA.
pub const THUMB_LONG_EDGE: u32 = 256;

/// Images decoded per `step()` -- small enough that a cancel takes effect within ~20 ms
/// (ADR-0029 measured ~2-3 ms per T0 decode), large enough that the scheduler's re-sort between
/// chunks isn't per image.
const IMAGES_PER_STEP: usize = 8;

/// One decoded thumbnail's RGBA pixels, ready for `egui::Context::load_texture` on the UI thread.
pub struct ThumbImage {
    pub image: egui::ColorImage,
}

/// Why a cell has no thumbnail. The split decides whether the grid ever asks again: a missing or
/// undecodable preview can't fix itself (until a rescan, which `GridSession::refresh` handles),
/// but a catalog error -- a busy/locked database, say -- usually can.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThumbError {
    /// No stored T0, or its bytes don't decode. Not retried until a refresh.
    Permanent(String),
    /// The catalog read itself failed. Retried after a short delay.
    Transient(String),
}

/// Finished thumbnails accumulate here as the job progresses, so the grid can show the first
/// cells of a batch before its last is decoded. `done` is set by the job's final step (or its
/// failure), which is how the session knows to drop the batch from its in-flight set.
#[derive(Default)]
pub struct ThumbOutput {
    pub ready: Vec<(i64, Result<ThumbImage, ThumbError>)>,
    pub done: bool,
}

pub type ThumbSlot = Arc<Mutex<ThumbOutput>>;

pub struct ThumbBatchJob {
    store: Arc<dyn CatalogStore + Send + Sync>,
    ids: Vec<i64>,
    next: usize,
    /// Absolute index of `ids[0]` in the grid's snapshot, for `Pounce::reprioritize`'s
    /// nearest-to-viewport ordering.
    first_index: usize,
    label: String,
    out: ThumbSlot,
}

impl ThumbBatchJob {
    pub fn new(
        store: Arc<dyn CatalogStore + Send + Sync>,
        ids: Vec<i64>,
        first_index: usize,
    ) -> (Self, ThumbSlot) {
        let out: ThumbSlot = Arc::new(Mutex::new(ThumbOutput::default()));
        let label = format!("Thumbnails: {} from #{}", ids.len(), first_index);
        let job = ThumbBatchJob {
            store,
            ids,
            next: 0,
            first_index,
            label,
            out: out.clone(),
        };
        (job, out)
    }

    fn finish(&self) {
        self.out.lock().unwrap().done = true;
    }
}

impl ChunkedJob for ThumbBatchJob {
    fn spec(&self) -> JobSpec {
        JobSpec {
            priority: Priority::Background,
            kind: JobKind::Thumbnail,
            lane: Lane::Cpu,
            vram_bytes: 0,
            // The batch's midpoint, so distance-from-viewport-centre orders whole batches sensibly.
            image_index: Some(self.first_index + self.ids.len() / 2),
        }
    }

    fn label(&self) -> String {
        self.label.clone()
    }

    fn progress(&self) -> Progress {
        Progress {
            done: self.next as u64,
            total: Some(self.ids.len() as u64),
        }
    }

    fn step(&mut self) -> Result<Step, JobError> {
        if self.next >= self.ids.len() {
            self.finish();
            return Ok(Step::Done);
        }
        let end = (self.next + IMAGES_PER_STEP).min(self.ids.len());
        let chunk = &self.ids[self.next..end];

        // One lock acquisition for the chunk's previews, not one per cell.
        let mut results: Vec<(i64, Result<ThumbImage, ThumbError>)> =
            Vec::with_capacity(chunk.len());
        match self.store.get_previews(chunk, PreviewTier::T0) {
            Ok(previews) => {
                let mut by_id: std::collections::HashMap<i64, _> = previews.into_iter().collect();
                for &id in chunk {
                    let outcome = match by_id.remove(&id) {
                        Some(preview) => {
                            make_thumbnail(&preview.bytes).map_err(ThumbError::Permanent)
                        }
                        None => Err(ThumbError::Permanent("no stored preview".to_string())),
                    };
                    results.push((id, outcome));
                }
            }
            Err(e) => {
                // A catalog error fails this chunk's cells, not the job (the rest of the batch
                // still gets its own attempt) -- and as *transient*, so the cells aren't written
                // off until the next refresh over what may be a momentary lock.
                let msg = e.to_string();
                results.extend(
                    chunk
                        .iter()
                        .map(|&id| (id, Err(ThumbError::Transient(msg.clone())))),
                );
            }
        }

        self.next = end;
        let finished = self.next >= self.ids.len();
        {
            let mut out = self.out.lock().unwrap();
            out.ready.extend(results);
            out.done = finished;
        }
        Ok(if finished { Step::Done } else { Step::Yield })
    }
}

impl Drop for ThumbBatchJob {
    /// A cancelled job is dropped without a final `step()`; marking `done` here means a poller
    /// that still holds the slot sees it finish rather than wait forever.
    fn drop(&mut self) {
        self.finish();
    }
}

/// JPEG-decodes `bytes` and downsizes to [`THUMB_LONG_EDGE`] (never upscales).
pub fn make_thumbnail(bytes: &[u8]) -> Result<ThumbImage, String> {
    let decoded = image::load_from_memory(bytes).map_err(|e| e.to_string())?;
    let small = if decoded.width().max(decoded.height()) > THUMB_LONG_EDGE {
        decoded.thumbnail(THUMB_LONG_EDGE, THUMB_LONG_EDGE)
    } else {
        decoded
    };
    let rgba = small.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Ok(ThumbImage {
        image: egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()),
    })
}

#[cfg(test)]
pub(crate) mod testutil {
    use image::codecs::jpeg::JpegEncoder;

    /// A solid-colour JPEG of the given size.
    pub fn jpeg(width: u32, height: u32, rgb: [u8; 3]) -> Vec<u8> {
        let mut buf = Vec::new();
        let pixels: Vec<u8> = (0..width * height).flat_map(|_| rgb).collect();
        JpegEncoder::new_with_quality(&mut buf, 90)
            .encode(&pixels, width, height, image::ExtendedColorType::Rgb8)
            .unwrap();
        buf
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::jpeg;
    use super::*;
    use nicti_lair::{NewAsset, Preview, SqliteCatalog};

    fn new_asset(name: &str) -> NewAsset {
        NewAsset {
            rel_path: name.to_string(),
            rel_path_fold: name.to_lowercase(),
            size_bytes: 1,
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

    fn seeded(n: usize, with_preview: impl Fn(usize) -> bool) -> (Arc<SqliteCatalog>, Vec<i64>) {
        let store = Arc::new(SqliteCatalog::open_in_memory().unwrap());
        let volume = store.upsert_volume("v", None, None, 0).unwrap();
        let root = store.ensure_root(volume, "").unwrap();
        let ids = (0..n)
            .map(|i| {
                let preview = with_preview(i).then(|| Preview {
                    width: Some(640),
                    height: Some(424),
                    bytes: jpeg(640, 424, [200, 40, 40]),
                });
                store
                    .insert_asset(root, &new_asset(&format!("{i}.NEF")), preview.as_ref())
                    .unwrap()
            })
            .collect();
        (store, ids)
    }

    fn run_to_done(job: &mut ThumbBatchJob) -> usize {
        let mut steps = 0;
        loop {
            steps += 1;
            if job.step().unwrap() == Step::Done {
                return steps;
            }
        }
    }

    #[test]
    fn thumbnails_are_downsized_to_the_long_edge_and_never_upscaled() {
        let big = make_thumbnail(&jpeg(640, 424, [10, 20, 30])).unwrap();
        // 424 * 256/640 = 169.6, which `image` rounds to 170.
        assert_eq!(big.image.size, [256, 170]);
        let small = make_thumbnail(&jpeg(100, 60, [10, 20, 30])).unwrap();
        assert_eq!(small.image.size, [100, 60]);
        assert!(make_thumbnail(b"not a jpeg").is_err());
    }

    #[test]
    fn a_batch_reports_every_id_including_ones_with_no_preview() {
        let (store, ids) = seeded(20, |i| i % 5 != 0);
        let (mut job, out) = ThumbBatchJob::new(store, ids.clone(), 100);
        let steps = run_to_done(&mut job);
        // 20 images at 8 per step: chunked so a cancel can land between them.
        assert_eq!(steps, 3);

        let out = out.lock().unwrap();
        assert!(out.done);
        assert_eq!(out.ready.len(), 20);
        for (i, (id, outcome)) in out.ready.iter().enumerate() {
            assert_eq!(*id, ids[i]);
            assert_eq!(outcome.is_ok(), i % 5 != 0, "index {i}");
            if let Err(e) = outcome {
                // A missing preview can't fix itself: it must never be retried as if transient.
                assert!(matches!(e, ThumbError::Permanent(_)), "index {i}: {e:?}");
            }
        }
    }

    #[test]
    fn partial_results_are_visible_before_the_job_finishes() {
        let (store, ids) = seeded(20, |_| true);
        let (mut job, out) = ThumbBatchJob::new(store, ids, 0);
        assert_eq!(job.step().unwrap(), Step::Yield);
        let out = out.lock().unwrap();
        assert_eq!(out.ready.len(), IMAGES_PER_STEP);
        assert!(!out.done);
    }

    #[test]
    fn dropping_a_job_mid_batch_marks_its_slot_done() {
        let (store, ids) = seeded(20, |_| true);
        let (mut job, out) = ThumbBatchJob::new(store, ids, 0);
        job.step().unwrap();
        drop(job);
        assert!(out.lock().unwrap().done);
    }

    #[test]
    fn spec_carries_a_midpoint_index_and_the_thumbnail_kind() {
        let (store, ids) = seeded(10, |_| true);
        let (job, _out) = ThumbBatchJob::new(store, ids, 640);
        let spec = job.spec();
        assert_eq!(spec.kind, JobKind::Thumbnail);
        assert_eq!(spec.lane, Lane::Cpu);
        assert_eq!(spec.image_index, Some(645));
    }

    #[test]
    fn snapshot_job_resolves_its_slot_with_the_ordered_ids() {
        let (store, ids) = seeded(5, |_| true);
        let sort = Sort {
            field: nicti_lair::SortField::Imported,
            direction: nicti_lair::SortDirection::Asc,
        };
        let (mut job, slot) = SnapshotJob::new(store, Filter::default(), sort);
        assert_eq!(job.step().unwrap(), Step::Done);
        let resolved = slot.lock().unwrap().take().expect("slot must resolve");
        assert_eq!(resolved.unwrap(), ids);
    }
}
