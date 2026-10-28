//! Pounce jobs behind the library grid (#30): the ordered id snapshot, and batched thumbnails.
//! Both follow `decode_job.rs`'s rule -- `step()` never returns `Err`; a failure is recorded in
//! the job's own slot, because the scheduler drops an erroring job without touching its slot and
//! a poller must never wait on a slot that will never resolve.

use std::sync::{Arc, Mutex};

use crate::t2::SharedLarder;

use nicti_calico::source_transform::SourceTransforms;
use nicti_lair::larder::LarderTier;
use nicti_lair::pounce_jobs::ReportSlot;
use nicti_lair::{CatalogStore, Filter, PreviewTier, Sort};
use nicti_pawprint::EditDocument;
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
    /// The photo has develop edits (#145): this camera-derived thumbnail doesn't show them, so
    /// the grid can flag it. Set by [`ThumbBatchJob`], `false` from [`make_thumbnail`] alone.
    pub edited: bool,
    /// Whether the pixels are a render of the edits (#397) rather than the camera's T0.
    pub render: ThumbRender,
    /// The edited photo's rendered thumbnail couldn't be consulted (the Larder was busy), so this
    /// is the camera thumbnail for now: the session asks again shortly.
    pub retry: bool,
    /// The session's redo counter when the batch was submitted; set by the session, 0 here.
    pub seq: u64,
}

/// Where a thumbnail's pixels come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThumbRender {
    /// The camera-derived T0: it doesn't show develop edits.
    Camera,
    /// A render of an earlier edit state of the same file, or of the current edits without their
    /// AI masks / AI removals (a `:partial` render): the cell still has edits it doesn't show.
    Updating,
    /// A render of the photo's current edits.
    Current,
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
    /// Source -> monitor conversion applied to every decoded thumbnail (#319).
    color: Arc<SourceTransforms>,
    /// Where rendered thumbnails live (#397); `None` shows the camera thumbnail for everything.
    rendered: Option<SharedLarder>,
}

impl ThumbBatchJob {
    pub fn new(
        store: Arc<dyn CatalogStore + Send + Sync>,
        ids: Vec<i64>,
        first_index: usize,
        color: Arc<SourceTransforms>,
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
            color,
            rendered: None,
        };
        (job, out)
    }

    /// Prefer the rendered thumbnail stored in `larder` for edited photos (#397).
    pub fn with_rendered(mut self, larder: Option<SharedLarder>) -> Self {
        self.rendered = larder;
        self
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
                    // Not in the catalog: an archived folder's thumbnail lives in a sidecar (#72).
                    // A catalog error here is transient (a momentary lock), not "no preview".
                    let preview = match by_id.remove(&id) {
                        Some(preview) => Ok(Some(preview)),
                        None => nicti_lair::tier::load_t0_by_id(&*self.store, id)
                            .map_err(|e| ThumbError::Transient(e.to_string())),
                    };
                    let outcome = match preview {
                        Ok(Some(preview)) => make_thumbnail(&preview.bytes, &self.color)
                            .map_err(ThumbError::Permanent),
                        Ok(None) => Err(ThumbError::Permanent("no stored preview".to_string())),
                        Err(e) => Err(e),
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

        // #145: which of these photos have edits, one batch read for the chunk. A failed read just
        // leaves them unflagged -- a missing badge, never a failed thumbnail.
        // One unparseable stored document fails the whole batch call, so on error fall back to
        // per-photo reads rather than dropping the badge for every photo in the chunk.
        let edits: Vec<(i64, EditDocument)> = match self.store.get_master_edits(chunk) {
            Ok(docs) => docs
                .into_iter()
                .filter_map(|(id, doc)| doc.map(|d| (id, d)))
                .filter(|(_, d)| !d.stages.is_empty())
                .collect(),
            Err(_) => chunk
                .iter()
                .copied()
                .filter_map(|id| {
                    let doc = self.store.get_master_edit(id).ok().flatten()?;
                    (!doc.stages.is_empty()).then_some((id, doc))
                })
                .collect(),
        };
        let edited: std::collections::HashMap<i64, EditDocument> = edits.into_iter().collect();
        for (id, outcome) in &mut results {
            if let Ok(thumb) = outcome {
                thumb.edited = edited.contains_key(id);
            }
        }
        self.apply_rendered(&edited, &mut results);

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

impl ThumbBatchJob {
    /// #397: swaps in the stored rendered thumbnail for every edited photo that has one. A render
    /// of the same file but an older edit state still shows ("updating"); one of a replaced file,
    /// an unreadable entry or a busy Larder leaves the camera thumbnail.
    fn apply_rendered(
        &self,
        edited: &std::collections::HashMap<i64, EditDocument>,
        results: &mut [(i64, Result<ThumbImage, ThumbError>)],
    ) {
        let Some(larder) = &self.rendered else { return };
        if edited.is_empty() {
            return;
        }
        // Never wait on the Larder (a compaction holds it for minutes): the camera thumbnail is
        // a fine answer for now, and `retry` has the session ask again after a back-off.
        let Some(mut larder) = crate::t2::try_lock_larder(larder) else {
            for (id, outcome) in results.iter_mut() {
                if let (Ok(thumb), true) = (outcome.as_mut(), edited.contains_key(id)) {
                    thumb.retry = true;
                }
            }
            return;
        };
        for (id, outcome) in results.iter_mut() {
            let (Ok(thumb), Some(doc)) = (outcome.as_mut(), edited.get(id)) else {
                continue;
            };
            let Ok(Some((hash, bytes))) = larder.get_latest(*id, LarderTier::RenderedThumb) else {
                continue;
            };
            let Some(identity) = self
                .store
                .get_asset(*id)
                .ok()
                .flatten()
                .map(|a| crate::loupe::asset_cache_key(&a))
            else {
                continue;
            };
            let Some(want) = crate::eyeshine::rendered_hash_in(&larder, *id, &identity, doc) else {
                continue;
            };
            let render = if hash == want && !crate::eyeshine::hash_is_partial(&hash) {
                ThumbRender::Current
            } else if crate::eyeshine::same_file(&hash, &want) {
                ThumbRender::Updating
            } else {
                continue;
            };
            if let Ok(rendered) = make_thumbnail(&bytes, &self.color) {
                thumb.image = rendered.image;
                thumb.render = render;
            }
        }
    }
}

impl Drop for ThumbBatchJob {
    /// A cancelled job is dropped without a final `step()`; marking `done` here means a poller
    /// that still holds the slot sees it finish rather than wait forever.
    fn drop(&mut self) {
        self.finish();
    }
}

/// JPEG-decodes `bytes`, downsizes to [`THUMB_LONG_EDGE`] (never upscales), then converts the
/// pixels from the JPEG's own color space to the display (#319). Converting after the downsize
/// keeps it to ~64k pixels rather than the full T0.
pub fn make_thumbnail(bytes: &[u8], color: &SourceTransforms) -> Result<ThumbImage, String> {
    let decoded = crate::preview_color::decode(bytes)?;
    let small = if decoded.image.width().max(decoded.image.height()) > THUMB_LONG_EDGE {
        crate::preview_color::DecodedJpeg {
            image: decoded.image.thumbnail(THUMB_LONG_EDGE, THUMB_LONG_EDGE),
            icc: decoded.icc,
        }
    } else {
        decoded
    };
    let rgba = crate::preview_color::to_display_rgba(small, color);
    let size = [rgba.width() as usize, rgba.height() as usize];
    Ok(ThumbImage {
        image: egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()),
        edited: false,
        render: ThumbRender::Camera,
        retry: false,
        seq: 0,
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
    use nicti_calico::icc::profile_bytes;
    use nicti_calico::space::OutputSpace;
    use nicti_calico::transform::DisplayProfile;
    use nicti_lair::{NewAsset, Preview, SqliteCatalog};

    fn srgb() -> SourceTransforms {
        SourceTransforms::new(&DisplayProfile::Space(OutputSpace::Srgb))
    }

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
        let big = make_thumbnail(&jpeg(640, 424, [10, 20, 30]), &srgb()).unwrap();
        // 424 * 256/640 = 169.6, which `image` rounds to 170.
        assert_eq!(big.image.size, [256, 170]);
        let small = make_thumbnail(&jpeg(100, 60, [10, 20, 30]), &srgb()).unwrap();
        assert_eq!(small.image.size, [100, 60]);
        assert!(make_thumbnail(b"not a jpeg", &srgb()).is_err());
    }

    #[test]
    fn edited_photos_use_their_rendered_thumbnail_when_it_is_for_the_same_file() {
        use nicti_lair::larder::{Larder, LarderConfig, LarderKey};
        use nicti_pawprint::StageEntry;
        let (store, ids) = seeded(4, |_| true);
        let doc = |stops: f64| {
            let mut d = EditDocument::default();
            d.stages.insert(
                "exposure".to_string(),
                StageEntry {
                    schema_version: 1,
                    params: serde_json::json!({ "stops": stops }),
                },
            );
            d
        };
        for &id in &ids[..3] {
            store.put_master_edit(id, &doc(1.0)).unwrap();
        }
        let identity =
            |id: i64| crate::loupe::asset_cache_key(&store.get_asset(id).unwrap().unwrap());
        let dir = tempfile::tempdir().unwrap();
        let larder = Larder::open(
            dir.path(),
            LarderConfig {
                cap_bytes: 1 << 20,
                compact_min_dead_bytes: 1 << 20,
            },
        )
        .unwrap();
        let larder: SharedLarder = Arc::new(Mutex::new(larder));
        let green = jpeg(64, 40, [20, 200, 20]);
        let put = |id: i64, hash: &str| {
            larder
                .lock()
                .unwrap()
                .put(
                    LarderKey {
                        asset_id: id,
                        tier: LarderTier::RenderedThumb,
                        render_hash: hash,
                    },
                    &green,
                )
                .unwrap();
        };
        // 0: current render. 1: render of an earlier edit of the same file. 2: render of another
        // file. 3: unedited, so any stored render is ignored.
        put(
            ids[0],
            &crate::eyeshine::rendered_hash(&identity(ids[0]), &doc(1.0)).unwrap(),
        );
        put(
            ids[1],
            &crate::eyeshine::rendered_hash(&identity(ids[1]), &doc(0.5)).unwrap(),
        );
        put(
            ids[2],
            &crate::eyeshine::rendered_hash(&blake3::hash(b"other"), &doc(1.0)).unwrap(),
        );
        put(ids[3], "rendered:v1:x:y");

        let run = |rendered: Option<SharedLarder>| {
            let (job, out) = ThumbBatchJob::new(store.clone(), ids.clone(), 0, Arc::new(srgb()));
            let mut job = job.with_rendered(rendered);
            run_to_done(&mut job);
            let mut ready = std::mem::take(&mut out.lock().unwrap().ready);
            ready.sort_by_key(|(id, _)| *id);
            ready
                .into_iter()
                .map(|(_, t)| t.unwrap())
                .collect::<Vec<_>>()
        };
        let with = run(Some(larder.clone()));
        let kinds: Vec<_> = with.iter().map(|t| t.render).collect();
        assert_eq!(
            kinds,
            [
                ThumbRender::Current,
                ThumbRender::Updating,
                ThumbRender::Camera,
                ThumbRender::Camera
            ]
        );
        assert_eq!(
            with[0].image.size,
            [64, 40],
            "the rendered pixels are shown"
        );
        assert_eq!(
            with[2].image.size,
            [256, 170],
            "the camera thumbnail is kept"
        );
        assert!(with[..3].iter().all(|t| t.edited) && !with[3].edited);

        let without = run(None);
        assert!(without.iter().all(|t| t.render == ThumbRender::Camera));

        // A busy Larder keeps the camera thumbnail but asks the session to try again.
        let held = larder.lock().unwrap();
        let busy = run(Some(larder.clone()));
        drop(held);
        assert!(busy.iter().all(|t| t.render == ThumbRender::Camera));
        assert_eq!(
            busy.iter().map(|t| t.retry).collect::<Vec<_>>(),
            [true, true, true, false],
            "only edited photos wait on the Larder"
        );
    }

    #[test]
    fn a_batch_reports_every_id_including_ones_with_no_preview() {
        let (store, ids) = seeded(20, |i| i % 5 != 0);
        let (mut job, out) = ThumbBatchJob::new(store, ids.clone(), 100, Arc::new(srgb()));
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
        let (mut job, out) = ThumbBatchJob::new(store, ids, 0, Arc::new(srgb()));
        assert_eq!(job.step().unwrap(), Step::Yield);
        let out = out.lock().unwrap();
        assert_eq!(out.ready.len(), IMAGES_PER_STEP);
        assert!(!out.done);
    }

    #[test]
    fn dropping_a_job_mid_batch_marks_its_slot_done() {
        let (store, ids) = seeded(20, |_| true);
        let (mut job, out) = ThumbBatchJob::new(store, ids, 0, Arc::new(srgb()));
        job.step().unwrap();
        drop(job);
        assert!(out.lock().unwrap().done);
    }

    #[test]
    fn spec_carries_a_midpoint_index_and_the_thumbnail_kind() {
        let (store, ids) = seeded(10, |_| true);
        let (job, _out) = ThumbBatchJob::new(store, ids, 640, Arc::new(srgb()));
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

    #[test]
    fn an_untagged_thumbnail_on_an_srgb_display_keeps_its_pixels() {
        let thumb = make_thumbnail(&jpeg(100, 60, [200, 40, 40]), &srgb()).unwrap();
        let plain = image::load_from_memory(&jpeg(100, 60, [200, 40, 40]))
            .unwrap()
            .to_rgba8();
        let p = thumb.image.pixels[0];
        let q = plain.get_pixel(0, 0);
        assert_eq!([p.r(), p.g(), p.b()], [q[0], q[1], q[2]]);
    }

    #[test]
    fn a_display_p3_thumbnail_is_converted_for_the_display() {
        let p3 = profile_bytes(OutputSpace::DisplayP3).unwrap();
        let tagged =
            crate::preview_color::testutil::tagged_jpeg(640, 424, [200, 120, 60], Some(&p3));
        let thumb = make_thumbnail(&tagged, &srgb()).unwrap();
        // Downsized to the thumbnail edge, and not the same numbers the unmanaged path produced.
        assert_eq!(thumb.image.size[0], THUMB_LONG_EDGE as usize);
        let unmanaged = make_thumbnail(
            &crate::preview_color::testutil::tagged_jpeg(640, 424, [200, 120, 60], None),
            &srgb(),
        )
        .unwrap();
        assert_ne!(thumb.image.pixels[0], unmanaged.image.pixels[0]);
    }
}
