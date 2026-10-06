//! The export run (#57): plan a batch, then push each photo through
//! decode (CPU lane) -> render (GPU lane, one tile per step) -> encode+write (CPU lane) as
//! chained Pounce jobs.
//!
//! **Why chained jobs.** One job per photo doing all three would run its ~1.7 s decode inside a
//! GPU-lane step and starve every other GPU client (`RemoveJob`); splitting by lane keeps the GPU
//! lane doing only GPU-shaped work, one tile per step (ADR-0054's ~16 ms chunks). Each stage
//! submits the next through a [`Submitter`] (a weak handle -- a job must never hold a `Pounce`
//! clone, see `Pounce::submitter`), so the batch keeps moving with no UI polling.
//!
//! **Bounded memory.** At 45 MP a decoded frame is ~270 MB, a rendered linear buffer ~540 MB, and
//! an in-flight encode ~800 MB at full size. [`advance`](Shared::advance) is the single place that
//! decides what may start, holding to: at most [`DECODE_AHEAD`] decoded-or-decoding photos ahead
//! of the renderer, one render at a time, and `rendered + encoding <= MAX_ENCODE_SLOTS`. Worst
//! case at full resolution is roughly 2.4 GB; downsized exports need far less.
//!
//! **Per-photo tickets.** Every photo gets one [`Ticket`] that follows it through the stages and
//! settles exactly once: exported, skipped, failed or cancelled. A dropped ticket (a job that
//! never ran, a cancelled queue) settles as cancelled, so the report always accounts for every
//! photo and is published exactly once. A per-photo failure never aborts the batch and never
//! surfaces as a `step()` error (Pounce would drop the job without telling anyone).
//!
//! **Locking.** `Ticket::drop` and job `Drop`s take the state lock but never submit: they can run
//! inside Pounce's scheduler lock, so submitting from there could deadlock. Only `step()` paths
//! and [`ExportRun::poll`] call `advance`, which releases the state lock before submitting.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use nicti_calico::dcp::DcpProfile;
use nicti_cornea::{LinearFrame, RawDecoder};
use nicti_lair::pounce_jobs::ReportSlot;
use nicti_lair::CatalogStore;
use nicti_pawprint::EditDocument;
use nicti_pounce::{
    ChunkedJob, JobError, JobKind, JobSpec, Lane, Priority, Progress, Step, Submitter,
};
use nicti_preen::exporters::builtin_id;
use nicti_preen::metadata::{SourceExif, SourceMetadata};
use nicti_preen::naming::{AssetFacts, DateParts};
use nicti_preen::plan::{plan_batch, FsProbe, PlanAction, PlanError, PlanItem, PlannedOutput};
use nicti_preen::spec::{ExportSpec, SpecError};
use nicti_preen::watermark::{WatermarkError, WatermarkSource};
use nicti_preen::write::{write_output, WriteOutcome};
use nicti_preen::{export_frame, ExportContext, ExporterRegistry, WorkingFrame};
use nicti_tapetum::frame::{Extent, FrameTexture};
use nicti_tapetum::geometry::Affine2D;
use nicti_tapetum::gpu::GpuContext;
use nicti_tapetum::mask::params::MaskParams;
use nicti_tapetum::spine;
use nicti_tapetum::tile::{Rect, Tile, TileBudget, TilePlanner, TiledRender};

use super::render_core::{render_live_frame, ExportRenderer, LiveRender};
use super::sink::AccumSink;
use crate::camera_profiles;
use crate::loupe::asset_cache_key;

/// Decoded (or decoding) photos allowed ahead of the renderer.
pub const DECODE_AHEAD: usize = 1;
/// Rendered-and-waiting plus encoding photos allowed at once.
pub const MAX_ENCODE_SLOTS: usize = 2;
/// Photos encoding at once.
const MAX_ENCODING: usize = 1;
/// Largest tile edge, px: keeps one tile's dispatch well under the frame-budget target.
const TILE_MAX_DIM: u32 = 4096;
/// Largest tile readback staging buffer.
const TILE_MAX_STAGING_BYTES: u64 = 64 * 1024 * 1024;

// --- public API ---------------------------------------------------------------------------------

/// Everything an export run needs from the app.
pub struct ExportEnv {
    pub submitter: Submitter,
    pub store: Arc<dyn CatalogStore + Send + Sync>,
    pub decoder: Arc<dyn RawDecoder + Send + Sync>,
    pub gpu: Arc<GpuContext>,
    pub registry: Arc<ExporterRegistry>,
    /// Written to EXIF `Software` / XMP `CreatorTool`, e.g. `Nicti 0.3.1`.
    pub software: String,
}

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("export settings: {0}")]
    Spec(#[from] SpecError),
    #[error("watermark: {0}")]
    Watermark(#[from] WatermarkError),
    #[error("planning: {0}")]
    Plan(#[from] PlanError),
    #[error("catalog: {0}")]
    Catalog(String),
    #[error("no exporter is registered for the chosen format")]
    NoExporter,
    #[error("there is nothing to export")]
    NothingToExport,
}

/// What happened to every photo of a finished run.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExportReport {
    pub total: usize,
    pub exported: Vec<(i64, PathBuf)>,
    pub skipped: Vec<(i64, PathBuf, String)>,
    pub failed: Vec<(i64, String)>,
    pub cancelled: usize,
    pub warnings: Vec<String>,
}

impl ExportReport {
    fn settled(&self) -> usize {
        self.exported.len() + self.skipped.len() + self.failed.len() + self.cancelled
    }
}

/// A running (or finished) export. Cheap to hold; poll it once per frame.
/// The template-facing facts of one catalog asset (also used for the dialog's filename preview).
pub fn facts_for(asset: &nicti_lair::Asset, source_path: &std::path::Path) -> AssetFacts {
    let source_dir = source_path.parent().map(PathBuf::from).unwrap_or_default();
    AssetFacts {
        asset_id: asset.id,
        stem: source_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
        folder: source_dir
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
        captured: asset.captured_at.as_deref().and_then(DateParts::parse),
        mtime_unix: asset.mtime_unix,
        rating: asset.rating.map(|r| r as i32),
        make: asset.make.clone(),
        model: asset.model.clone(),
    }
}

pub struct ExportRun {
    shared: Arc<Shared>,
}

impl ExportRun {
    /// Plans and starts exporting `asset_ids` (in that order -- it is also the `{Sequence}`
    /// order). Planning runs on the calling thread (a few catalog reads and one `stat` per photo);
    /// everything heavy runs on Pounce.
    pub fn start(
        env: ExportEnv,
        asset_ids: &[i64],
        spec: ExportSpec,
    ) -> Result<ExportRun, StartError> {
        spec.validate()?;
        let exporter = env
            .registry
            .get(builtin_id(spec.format.format()))
            .ok_or(StartError::NoExporter)?;
        let watermark = match &spec.watermark {
            Some(w) => Some(Arc::new(WatermarkSource::load(&w.path)?)),
            None => None,
        };

        let catalog_err = |e: nicti_lair::CatalogError| StartError::Catalog(e.to_string());
        let mut warnings = Vec::new();
        let mut failed_up_front: Vec<(i64, String)> = Vec::new();
        struct Prepared {
            plan_item: PlanItem,
            source_path: PathBuf,
            identity: blake3::Hash,
            edit: EditDocument,
            meta: SourceMetadata,
        }
        let mut prepared: Vec<Prepared> = Vec::new();
        for &id in asset_ids {
            let Some(asset) = env.store.get_asset(id).map_err(catalog_err)? else {
                warnings.push(format!("Photo {id} is no longer in the catalog."));
                continue;
            };
            if asset.missing_since.is_some() {
                failed_up_front.push((id, "the source file is missing from disk".into()));
                continue;
            }
            let Some(root_path) = env
                .store
                .get_root_path(asset.root_id)
                .map_err(catalog_err)?
            else {
                failed_up_front.push((id, "the photo's folder is no longer registered".into()));
                continue;
            };
            let source_path = PathBuf::from(root_path).join(&asset.rel_path);
            let source_dir = source_path.parent().map(PathBuf::from).unwrap_or_default();
            let keywords = env
                .store
                .keywords_for(id)
                .map_err(catalog_err)?
                .into_iter()
                .map(|k| k.name)
                .collect();
            // A photo whose saved edits can't be read fails on its own (naming it) instead of
            // aborting the whole batch.
            let edit = match env.store.get_master_edit(id) {
                Ok(edit) => edit.unwrap_or_default(),
                Err(e) => {
                    failed_up_front.push((id, format!("its saved edits couldn't be read: {e}")));
                    continue;
                }
            };
            let facts = facts_for(&asset, &source_path);
            prepared.push(Prepared {
                plan_item: PlanItem { facts, source_dir },
                source_path,
                identity: asset_cache_key(&asset),
                edit,
                meta: SourceMetadata {
                    exif: SourceExif::default(),
                    catalog_make: asset.make,
                    catalog_model: asset.model,
                    catalog_captured_at: asset.captured_at,
                    rating: asset.rating.map(|r| r as i32),
                    label: asset.label,
                    keywords,
                },
            });
        }
        if prepared.is_empty() && failed_up_front.is_empty() {
            return Err(StartError::NothingToExport);
        }

        let plan_items: Vec<PlanItem> = prepared.iter().map(|p| p.plan_item.clone()).collect();
        let plan = plan_batch(&plan_items, &spec, exporter.extension(), &FsProbe)?;
        warnings.extend(plan.warnings);
        // Local adjustments (#49) aren't rendered by the export path yet (#354): its live pass never
        // gets a mask atlas. Say so rather than hand back an image that silently lacks them.
        let with_masks: Vec<String> = prepared
            .iter()
            .filter(|p| {
                spine::resolve::<MaskParams>(&p.edit, nicti_tapetum::stages::MASKS)
                    .active()
                    .next()
                    .is_some()
            })
            .map(|p| {
                p.source_path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
            .collect();
        if !with_masks.is_empty() {
            warnings.push(format!(
                "{} photo(s) have local adjustments (masks), which are not applied to exports yet (#354): {}.",
                with_masks.len(),
                with_masks.join(", ")
            ));
        }

        let total = prepared.len() + failed_up_front.len();
        let items: Vec<Item> = prepared
            .into_iter()
            .zip(plan.outputs)
            .map(|(p, planned)| Item {
                asset_id: p.plan_item.facts.asset_id,
                name: p
                    .source_path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                source_path: p.source_path,
                identity: p.identity,
                edit: p.edit,
                meta: p.meta,
                planned,
            })
            .collect();

        let shared = Arc::new(Shared {
            submitter: env.submitter,
            decoder: env.decoder,
            gpu: env.gpu,
            registry: env.registry,
            software: env.software,
            spec,
            watermark,
            items,
            cancelled: AtomicBool::new(false),
            settled: AtomicUsize::new(0),
            total,
            state: Mutex::new(RunState {
                next_decode: 0,
                decoding: 0,
                ready: VecDeque::new(),
                rendering: false,
                rendered: VecDeque::new(),
                encoding: 0,
                published: false,
                report: ExportReport {
                    total,
                    warnings,
                    ..ExportReport::default()
                },
            }),
            render_ctx: Mutex::new(None),
            report_slot: Arc::new(Mutex::new(None)),
        });
        {
            let mut st = shared.state.lock().unwrap();
            for (id, why) in failed_up_front {
                shared.record_locked(&mut st, Outcome::Failed(id, why));
            }
        }
        shared.advance();
        Ok(ExportRun { shared })
    }

    /// Drives the pipeline (a backstop: jobs normally chain themselves) and returns the final
    /// report once every photo has settled. Call once per frame.
    pub fn poll(&self) -> Option<ExportReport> {
        self.shared.advance();
        self.shared.report_slot.lock().unwrap().clone()
    }

    /// `(settled, total)` photos, lock-free.
    pub fn progress(&self) -> (usize, usize) {
        (
            self.shared.settled.load(Ordering::Relaxed),
            self.shared.total,
        )
    }

    /// Stops after the photos currently mid-stage; everything not yet exported is reported as
    /// cancelled. A decode or an encode can't be interrupted, so this can take a moment.
    pub fn cancel(&self) {
        self.shared.cancelled.store(true, Ordering::SeqCst);
        self.shared.advance();
    }
}

// --- shared state -------------------------------------------------------------------------------

struct Item {
    asset_id: i64,
    /// File name, for job labels.
    name: String,
    source_path: PathBuf,
    identity: blake3::Hash,
    edit: EditDocument,
    /// Catalog-derived metadata; the file's own EXIF is read at decode time.
    meta: SourceMetadata,
    planned: PlannedOutput,
}

enum Outcome {
    Exported(i64, PathBuf),
    Skipped(i64, PathBuf, String),
    Failed(i64, String),
    Cancelled,
}

struct Ready {
    ticket: Ticket,
    frame: Arc<LinearFrame>,
    exif: SourceExif,
    profile: Option<Arc<DcpProfile>>,
    look: Option<Arc<nicti_calico::xmp_profile::LookProfile>>,
}

struct Rendered {
    ticket: Ticket,
    frame: WorkingFrame,
    exif: SourceExif,
}

struct RunState {
    next_decode: usize,
    decoding: usize,
    ready: VecDeque<Ready>,
    rendering: bool,
    rendered: VecDeque<Rendered>,
    encoding: usize,
    published: bool,
    report: ExportReport,
}

struct Shared {
    submitter: Submitter,
    decoder: Arc<dyn RawDecoder + Send + Sync>,
    gpu: Arc<GpuContext>,
    registry: Arc<ExporterRegistry>,
    software: String,
    spec: ExportSpec,
    watermark: Option<Arc<WatermarkSource>>,
    items: Vec<Item>,
    cancelled: AtomicBool,
    settled: AtomicUsize,
    total: usize,
    state: Mutex<RunState>,
    /// The batch's render kernels, built lazily on the first render job and reused. Only ever
    /// touched by the (single) GPU-lane worker.
    render_ctx: Mutex<Option<ExportRenderer>>,
    report_slot: ReportSlot<ExportReport>,
}

/// A pending unit of work `advance` decided on while holding the lock.
enum Launch {
    Decode(usize),
    Render(Ready),
    Encode(Rendered),
}

impl Shared {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    fn with_state<R>(&self, f: impl FnOnce(&mut RunState) -> R) -> R {
        f(&mut self.state.lock().unwrap())
    }

    /// Records one photo's outcome and publishes the report when the last one settles.
    fn record_locked(&self, st: &mut RunState, outcome: Outcome) {
        match outcome {
            Outcome::Exported(id, path) => st.report.exported.push((id, path)),
            Outcome::Skipped(id, path, why) => st.report.skipped.push((id, path, why)),
            Outcome::Failed(id, why) => st.report.failed.push((id, why)),
            Outcome::Cancelled => st.report.cancelled += 1,
        }
        self.settled.store(st.report.settled(), Ordering::Relaxed);
        if st.report.settled() >= self.total && !st.published {
            st.published = true;
            *self.report_slot.lock().unwrap() = Some(st.report.clone());
        }
    }

    fn record(&self, outcome: Outcome) {
        self.with_state(|st| self.record_locked(st, outcome));
    }

    /// Starts whatever the memory/concurrency limits now allow. Never call with the state lock
    /// held, and never from a `Drop` (see the module doc).
    fn advance(self: &Arc<Self>) {
        let mut launches: Vec<Launch> = Vec::new();
        // Queued stage outputs dropped after the lock is released (their tickets re-lock it).
        let mut discarded: (Vec<Ready>, Vec<Rendered>) = (Vec::new(), Vec::new());
        {
            let mut st = self.state.lock().unwrap();
            if st.published {
                return;
            }
            let n = self.items.len();
            if self.is_cancelled() {
                while st.next_decode < n {
                    st.next_decode += 1;
                    self.record_locked(&mut st, Outcome::Cancelled);
                }
                discarded.0.extend(st.ready.drain(..));
                discarded.1.extend(st.rendered.drain(..));
            } else {
                // Decode ahead.
                while st.next_decode < n && st.decoding + st.ready.len() < DECODE_AHEAD {
                    let idx = st.next_decode;
                    st.next_decode += 1;
                    match &self.items[idx].planned.action {
                        PlanAction::Skip(why) => {
                            let outcome = Outcome::Skipped(
                                self.items[idx].asset_id,
                                self.items[idx].planned.path.clone(),
                                why.clone(),
                            );
                            self.record_locked(&mut st, outcome);
                        }
                        PlanAction::Write | PlanAction::Overwrite => {
                            st.decoding += 1;
                            launches.push(Launch::Decode(idx));
                        }
                    }
                }
                // Render.
                if !st.rendering
                    && !st.ready.is_empty()
                    && st.rendered.len() + st.encoding < MAX_ENCODE_SLOTS
                {
                    let ready = st.ready.pop_front().expect("checked non-empty");
                    st.rendering = true;
                    launches.push(Launch::Render(ready));
                }
                // Encode.
                while st.encoding < MAX_ENCODING && !st.rendered.is_empty() {
                    let rendered = st.rendered.pop_front().expect("checked non-empty");
                    st.encoding += 1;
                    launches.push(Launch::Encode(rendered));
                }
            }
        }
        drop(discarded);

        let mut runtime_gone = false;
        for launch in launches {
            let job: Box<dyn ChunkedJob> = match launch {
                Launch::Decode(idx) => Box::new(DecodeJob {
                    label: format!("Export: decode {}", self.items[idx].name),
                    ticket: Some(Ticket::new(self.clone(), idx)),
                }),
                Launch::Render(ready) => Box::new(RenderJob::new(self.clone(), ready)),
                Launch::Encode(rendered) => Box::new(EncodeJob::new(self.clone(), rendered)),
            };
            if self.submitter.submit(job).is_none() {
                runtime_gone = true;
            }
        }
        if runtime_gone {
            // The scheduler is shutting down: the dropped jobs' tickets settled as cancelled;
            // wrap up the rest.
            self.cancelled.store(true, Ordering::SeqCst);
            self.advance();
        }
    }
}

// --- tickets ------------------------------------------------------------------------------------

/// One photo's claim on a place in the report. Settles exactly once; dropping it unsettled
/// records the photo as cancelled.
struct Ticket {
    shared: Arc<Shared>,
    idx: usize,
    settled: bool,
}

impl Ticket {
    fn new(shared: Arc<Shared>, idx: usize) -> Self {
        Ticket {
            shared,
            idx,
            settled: false,
        }
    }

    fn item(&self) -> &Item {
        &self.shared.items[self.idx]
    }

    fn settle(mut self, outcome: Outcome) {
        self.settled = true;
        self.shared.record(outcome);
    }

    fn exported(self, path: PathBuf) {
        let id = self.item().asset_id;
        self.settle(Outcome::Exported(id, path));
    }

    fn skipped(self, path: PathBuf, why: String) {
        let id = self.item().asset_id;
        self.settle(Outcome::Skipped(id, path, why));
    }

    fn failed(self, why: String) {
        let id = self.item().asset_id;
        self.settle(Outcome::Failed(id, why));
    }

    fn cancelled(self) {
        self.settle(Outcome::Cancelled);
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        if !self.settled {
            self.shared.record(Outcome::Cancelled);
        }
    }
}

// --- stage 1: decode ---------------------------------------------------------------------------

struct DecodeJob {
    label: String,
    ticket: Option<Ticket>,
}

impl ChunkedJob for DecodeJob {
    fn spec(&self) -> JobSpec {
        JobSpec {
            priority: Priority::Background,
            kind: JobKind::Export,
            lane: Lane::Cpu,
            vram_bytes: 0,
            image_index: None,
        }
    }

    fn label(&self) -> String {
        self.label.clone()
    }

    fn progress(&self) -> Progress {
        Progress {
            done: u64::from(self.ticket.is_none()),
            total: Some(1),
        }
    }

    fn step(&mut self) -> Result<Step, JobError> {
        let Some(ticket) = self.ticket.take() else {
            return Ok(Step::Done);
        };
        let shared = ticket.shared.clone();
        if shared.is_cancelled() {
            shared.with_state(|st| st.decoding -= 1);
            ticket.cancelled();
        } else {
            match decode_one(&shared, ticket.item()) {
                Ok((frame, exif, profile, look)) => shared.with_state(|st| {
                    st.decoding -= 1;
                    st.ready.push_back(Ready {
                        ticket,
                        frame,
                        exif,
                        profile,
                        look,
                    });
                }),
                Err(why) => {
                    shared.with_state(|st| st.decoding -= 1);
                    ticket.failed(why);
                }
            }
        }
        shared.advance();
        Ok(Step::Done)
    }
}

impl Drop for DecodeJob {
    fn drop(&mut self) {
        // Dropped without running (cancelled while queued / runtime shutting down).
        if let Some(ticket) = self.ticket.take() {
            ticket.shared.with_state(|st| st.decoding -= 1);
            drop(ticket);
        }
    }
}

#[allow(clippy::type_complexity)]
fn decode_one(
    shared: &Shared,
    item: &Item,
) -> Result<
    (
        Arc<LinearFrame>,
        SourceExif,
        Option<Arc<DcpProfile>>,
        Option<Arc<nicti_calico::xmp_profile::LookProfile>>,
    ),
    String,
> {
    let frame = shared
        .decoder
        .decode_linear(&item.source_path)
        .map_err(|e| format!("couldn't decode {}: {e}", item.name))?;
    let exif = SourceExif::from_path(&item.source_path);
    // A camera profile the edit names must reload and still match: silently rendering with a
    // different profile than the user edited with would be worse than failing this photo.
    let profile = camera_profiles::load_for_document(&item.edit, &frame.make, &frame.model)
        .map_err(|e| format!("camera profile: {e}"))?;
    // Same for the Look `.xmp` layered on it.
    let look = camera_profiles::load_look_for_document(&item.edit)
        .map_err(|e| format!("camera profile: {e}"))?;
    Ok((Arc::new(frame), exif, profile, look))
}

// --- stage 2: render ---------------------------------------------------------------------------

/// A photo mid-render: the live suffix is done, tiles are being read back.
struct RenderPhase {
    ticket: Ticket,
    exif: SourceExif,
    ctx: ExportRenderer,
    live: Arc<FrameTexture>,
    base: Affine2D,
    tiles: Vec<Tile>,
    next: usize,
    sink: AccumSink,
}

struct RenderJob {
    shared: Arc<Shared>,
    label: String,
    ready: Option<Ready>,
    phase: Option<RenderPhase>,
    finished: bool,
    tiles_total: u64,
}

impl RenderJob {
    fn new(shared: Arc<Shared>, ready: Ready) -> Self {
        RenderJob {
            label: format!("Export: render {}", ready.ticket.item().name),
            shared,
            ready: Some(ready),
            phase: None,
            finished: false,
            tiles_total: 0,
        }
    }

    /// Returns the batch's kernels for the next photo.
    fn give_back(&self, ctx: ExportRenderer) {
        *self.shared.render_ctx.lock().unwrap() = Some(ctx);
    }

    /// First step: build the live suffix for the whole frame and plan the crop-sized tile grid.
    fn begin(&mut self, ready: Ready) -> Result<(), (Ticket, String)> {
        let Ready {
            ticket,
            frame,
            exif,
            profile,
            look,
        } = ready;
        let shared = self.shared.clone();
        let item = ticket.item();
        let mut ctx = shared
            .render_ctx
            .lock()
            .unwrap()
            .take()
            .unwrap_or_else(|| ExportRenderer::new(&shared.gpu));

        let LiveRender { live, inputs } = match render_live_frame(
            &mut ctx,
            &shared.gpu,
            &item.edit,
            item.identity,
            &frame,
            profile.as_deref(),
            look.as_deref(),
        ) {
            Ok(r) => r,
            Err(why) => {
                self.give_back(ctx);
                return Err((ticket, why));
            }
        };
        // The decoded frame (~270 MB at 45 MP) isn't needed past this point.
        drop(frame);

        let rect = inputs.crop_rect;
        let out_w = (rect.width.round() as u32).max(1);
        let out_h = (rect.height.round() as u32).max(1);
        let sink = match AccumSink::try_new(out_w, out_h) {
            Ok(s) => s,
            Err(e) => {
                self.give_back(ctx);
                return Err((ticket, e));
            }
        };
        let out_extent = Extent {
            width: out_w,
            height: out_h,
        };
        let tiles = TilePlanner::plan(
            out_extent,
            Rect {
                x: 0,
                y: 0,
                width: out_w,
                height: out_h,
            },
            1, // bilinear neighbor read
            TileBudget {
                max_dim: shared.gpu.limits.max_texture_dimension_2d.min(TILE_MAX_DIM),
                max_staging_bytes: TILE_MAX_STAGING_BYTES,
                target_chunk_ms: 16.0,
            },
        );
        self.tiles_total = tiles.len() as u64;
        self.phase = Some(RenderPhase {
            ticket,
            exif,
            ctx,
            live,
            base: inputs.crop_transform,
            tiles,
            next: 0,
            sink,
        });
        Ok(())
    }
}

impl ChunkedJob for RenderJob {
    fn spec(&self) -> JobSpec {
        JobSpec {
            priority: Priority::Background,
            kind: JobKind::Export,
            lane: Lane::Gpu,
            // Deliberately 0: Pounce drops a background job that declares more than the *total*
            // VRAM budget, and a 45 MP frame's textures exceed the placeholder budget. The render
            // is serialized by the one GPU worker regardless. (`RemoveJob` does the same.)
            vram_bytes: 0,
            image_index: None,
        }
    }

    fn label(&self) -> String {
        self.label.clone()
    }

    fn progress(&self) -> Progress {
        Progress {
            done: self.phase.as_ref().map_or(0, |p| p.next as u64),
            total: Some(self.tiles_total.max(1)),
        }
    }

    fn step(&mut self) -> Result<Step, JobError> {
        let shared = self.shared.clone();

        if let Some(ready) = self.ready.take() {
            if shared.is_cancelled() {
                shared.with_state(|st| st.rendering = false);
                self.finished = true;
                ready.ticket.cancelled();
                shared.advance();
                return Ok(Step::Done);
            }
            if let Err((ticket, why)) = self.begin(ready) {
                shared.with_state(|st| st.rendering = false);
                self.finished = true;
                ticket.failed(why);
                shared.advance();
                return Ok(Step::Done);
            }
            return Ok(Step::Yield);
        }

        let Some(phase) = self.phase.as_mut() else {
            self.finished = true;
            return Ok(Step::Done);
        };
        if shared.is_cancelled() {
            let phase = self.phase.take().expect("checked Some");
            self.give_back(phase.ctx);
            shared.with_state(|st| st.rendering = false);
            self.finished = true;
            phase.ticket.cancelled();
            shared.advance();
            return Ok(Step::Done);
        }

        // One tile per step.
        let tile = phase.tiles[phase.next];
        TiledRender::new(
            shared.gpu.clone(),
            &phase.live,
            &phase.ctx.crop_kernel,
            phase.base,
            vec![tile],
        )
        .step(&mut phase.sink);
        phase.next += 1;
        if phase.next < phase.tiles.len() {
            return Ok(Step::Yield);
        }

        let RenderPhase {
            ticket,
            exif,
            ctx,
            live,
            sink,
            ..
        } = self.phase.take().expect("checked Some");
        drop(live);
        self.give_back(ctx);
        self.finished = true;
        let frame = WorkingFrame {
            width: sink.width,
            height: sink.height,
            pixels: sink.pixels,
        };
        shared.with_state(|st| {
            st.rendering = false;
            st.rendered.push_back(Rendered {
                ticket,
                frame,
                exif,
            });
        });
        shared.advance();
        Ok(Step::Done)
    }
}

impl Drop for RenderJob {
    fn drop(&mut self) {
        // Dropped without finishing (cancelled while queued / runtime shutting down): free the
        // renderer slot and return the kernels; the tickets settle as cancelled on their own.
        if let Some(phase) = self.phase.take() {
            self.give_back(phase.ctx);
        }
        drop(self.ready.take());
        if !self.finished {
            self.shared.with_state(|st| st.rendering = false);
        }
    }
}

// --- stage 3: encode + write -------------------------------------------------------------------

struct EncodeJob {
    shared: Arc<Shared>,
    label: String,
    rendered: Option<Rendered>,
}

impl EncodeJob {
    fn new(shared: Arc<Shared>, rendered: Rendered) -> Self {
        EncodeJob {
            label: format!("Export: encode {}", rendered.ticket.item().name),
            shared,
            rendered: Some(rendered),
        }
    }
}

impl ChunkedJob for EncodeJob {
    fn spec(&self) -> JobSpec {
        JobSpec {
            priority: Priority::Background,
            kind: JobKind::Export,
            lane: Lane::Cpu,
            vram_bytes: 0,
            image_index: None,
        }
    }

    fn label(&self) -> String {
        self.label.clone()
    }

    fn progress(&self) -> Progress {
        Progress {
            done: u64::from(self.rendered.is_none()),
            total: Some(1),
        }
    }

    fn step(&mut self) -> Result<Step, JobError> {
        let Some(Rendered {
            ticket,
            frame,
            exif,
        }) = self.rendered.take()
        else {
            return Ok(Step::Done);
        };
        let shared = self.shared.clone();
        if shared.is_cancelled() {
            drop(frame);
            shared.with_state(|st| st.encoding -= 1);
            ticket.cancelled();
        } else {
            let item = ticket.item();
            let source = SourceMetadata {
                exif,
                ..item.meta.clone()
            };
            let ctx = ExportContext {
                spec: &shared.spec,
                source: &source,
                watermark: shared.watermark.as_deref(),
                software: &shared.software,
            };
            let result = export_frame(frame, &ctx, &shared.registry)
                .map_err(|e| format!("encoding failed: {e}"))
                .and_then(|exported| {
                    write_output(&item.planned.path, &exported.bytes, shared.spec.collision)
                        .map_err(|e| format!("couldn't write the file: {e}"))
                });
            shared.with_state(|st| st.encoding -= 1);
            match result {
                Ok(WriteOutcome::Written(path)) => ticket.exported(path),
                Ok(WriteOutcome::Skipped(path)) => {
                    ticket.skipped(path, "a file with this name already exists".into())
                }
                Err(why) => ticket.failed(why),
            }
        }
        shared.advance();
        Ok(Step::Done)
    }
}

impl Drop for EncodeJob {
    fn drop(&mut self) {
        if let Some(rendered) = self.rendered.take() {
            self.shared.with_state(|st| st.encoding -= 1);
            drop(rendered);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::{Duration, Instant};

    use nicti_claw::Module;
    use nicti_cornea::DecodeError;
    use nicti_lair::{NewAsset, SqliteCatalog};
    use nicti_pawprint::StageEntry;
    use nicti_pounce::Pounce;
    use nicti_preen::exporters::builtin_registry;
    use nicti_preen::spec::{
        BitDepth, CollisionPolicy, DestinationBase, DestinationSpec, FormatSpec, NamingSpec,
        ResizeMode, ResizeSpec, WatermarkSpec,
    };
    use nicti_tapetum::coat::{CameraProfileParams, CropParams};
    use nicti_tapetum::stages::{CROP, WORKING_SPACE};

    /// Decodes every path to the synthetic 64x64 gradient, rolled by the file name so different
    /// photos have different pixels at the *same* size. A name containing "bad" fails to decode.
    struct FakeDecoder;

    impl Module for FakeDecoder {
        fn id(&self) -> &str {
            "test.decoder.export"
        }
        fn schema_version(&self) -> u32 {
            1
        }
        fn migrate_params(&self, _: u32, _: serde_json::Value) -> Option<serde_json::Value> {
            None
        }
    }

    impl RawDecoder for FakeDecoder {
        fn decode_linear(&self, path: &Path) -> Result<LinearFrame, DecodeError> {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if name.contains("bad") {
                return Err(DecodeError::Io {
                    path: path.to_path_buf(),
                    source: std::io::Error::other("corrupt file"),
                });
            }
            let mut frame = crate::render::synthetic_linear_frame();
            let rows = (name.as_bytes()[0] as usize % 32) * frame.width as usize * 3;
            frame.pixels.rotate_left(rows);
            Ok(frame)
        }
    }

    struct Fixture {
        gpu: Arc<GpuContext>,
        store: Arc<SqliteCatalog>,
        ids: Vec<i64>,
        _root: tempfile::TempDir,
        out: tempfile::TempDir,
        pounce: Pounce,
    }

    fn fixture(names: &[&str]) -> Option<Fixture> {
        let gpu = crate::test_gpu::shared()?;
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(SqliteCatalog::open_in_memory().unwrap());
        let volume = store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = store
            .ensure_root(volume, &root.path().to_string_lossy())
            .unwrap();
        let ids = names
            .iter()
            .map(|name| {
                store
                    .insert_asset(
                        root_id,
                        &NewAsset {
                            rel_path: (*name).into(),
                            rel_path_fold: name.to_lowercase(),
                            size_bytes: 1,
                            mtime_unix: 1_700_000_000,
                            fingerprint: Some(format!("fp-{name}")),
                            natural_key: None,
                            make: Some("NIKON".into()),
                            model: Some("Z 8".into()),
                            captured_at: Some("2026-09-27 14:03:09".into()),
                            width: Some(64),
                            height: Some(64),
                            imported_at: 0,
                        },
                        None,
                    )
                    .unwrap()
            })
            .collect();
        Some(Fixture {
            gpu,
            store,
            ids,
            _root: root,
            out: tempfile::tempdir().unwrap(),
            pounce: Pounce::new(u64::MAX, 2, 2, || {}),
        })
    }

    impl Fixture {
        fn env(&self) -> ExportEnv {
            ExportEnv {
                submitter: self.pounce.submitter(),
                store: self.store.clone(),
                decoder: Arc::new(FakeDecoder),
                gpu: self.gpu.clone(),
                registry: Arc::new(builtin_registry()),
                software: "Nicti test".into(),
            }
        }

        fn spec(&self) -> ExportSpec {
            ExportSpec {
                format: FormatSpec::Png {
                    depth: BitDepth::Eight,
                },
                naming: NamingSpec {
                    template: "{Sequence:2}_{Filename}".into(),
                    sequence_start: 1,
                },
                destination: DestinationSpec {
                    base: DestinationBase::Folder(self.out.path().to_path_buf()),
                    subfolder: None,
                },
                ..ExportSpec::default()
            }
        }

        fn out_files(&self) -> Vec<String> {
            let mut v: Vec<String> = std::fs::read_dir(self.out.path())
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            v.sort();
            v
        }
    }

    fn wait(run: &ExportRun) -> ExportReport {
        let start = Instant::now();
        loop {
            if let Some(report) = run.poll() {
                return report;
            }
            assert!(
                start.elapsed() < Duration::from_secs(120),
                "export never finished"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn png(path: &Path) -> image::RgbImage {
        image::open(path).unwrap().to_rgb8()
    }

    #[test]
    fn exports_every_photo_with_planned_names_and_distinct_pixels() {
        let Some(fx) = fixture(&["a.NEF", "b.NEF", "c.NEF"]) else {
            return;
        };
        let mut spec = fx.spec();
        spec.resize = ResizeSpec {
            mode: ResizeMode::LongEdge(32),
            dont_enlarge: true,
        };
        let run = ExportRun::start(fx.env(), &fx.ids, spec).unwrap();
        let report = wait(&run);

        assert_eq!(report.total, 3);
        assert_eq!(report.exported.len(), 3, "{report:?}");
        assert!(report.failed.is_empty() && report.skipped.is_empty() && report.cancelled == 0);
        assert_eq!(fx.out_files(), ["01_a.png", "02_b.png", "03_c.png"]);
        let imgs: Vec<_> = ["01_a.png", "02_b.png", "03_c.png"]
            .iter()
            .map(|n| png(&fx.out.path().join(n)))
            .collect();
        assert!(imgs.iter().all(|i| i.dimensions() == (32, 32)));
        // Same size, different photos: none may be served another's cached pixels.
        assert_ne!(imgs[0], imgs[1]);
        assert_ne!(imgs[1], imgs[2]);
        assert_ne!(imgs[0], imgs[2]);
        assert_eq!(run.progress(), (3, 3));
    }

    #[test]
    fn a_bad_decode_fails_only_that_photo_and_leaves_a_gap_in_the_sequence() {
        let Some(fx) = fixture(&["a.NEF", "bad.NEF", "c.NEF"]) else {
            return;
        };
        let report = wait(&ExportRun::start(fx.env(), &fx.ids, fx.spec()).unwrap());
        assert_eq!(report.exported.len(), 2, "{report:?}");
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].0, fx.ids[1]);
        assert!(report.failed[0].1.contains("decode"), "{:?}", report.failed);
        // The failed photo's number is not reused: names never depend on outcomes.
        assert_eq!(fx.out_files(), ["01_a.png", "03_c.png"]);
    }

    #[test]
    fn a_cropped_edit_exports_at_the_crop_size() {
        let Some(fx) = fixture(&["a.NEF"]) else {
            return;
        };
        let mut doc = EditDocument::default();
        doc.stages.insert(
            CROP.to_string(),
            StageEntry {
                schema_version: 1,
                params: serde_json::to_value(CropParams {
                    x: 8.0,
                    y: 16.0,
                    width: 40.0,
                    height: 24.0,
                    rotation_degrees: 0.0,
                })
                .unwrap(),
            },
        );
        fx.store.put_master_edit(fx.ids[0], &doc).unwrap();
        let report = wait(&ExportRun::start(fx.env(), &fx.ids, fx.spec()).unwrap());
        assert_eq!(report.exported.len(), 1, "{report:?}");
        assert_eq!(png(&fx.out.path().join("01_a.png")).dimensions(), (40, 24));
    }

    #[test]
    fn the_edit_changes_the_exported_pixels() {
        let Some(fx) = fixture(&["a.NEF"]) else {
            return;
        };
        wait(&ExportRun::start(fx.env(), &fx.ids, fx.spec()).unwrap());
        let plain = png(&fx.out.path().join("01_a.png"));

        let mut doc = EditDocument::default();
        doc.stages.insert(
            nicti_tapetum::stages::EXPOSURE.to_string(),
            StageEntry {
                schema_version: 1,
                params: serde_json::json!({ "stops": 1.5 }),
            },
        );
        fx.store.put_master_edit(fx.ids[0], &doc).unwrap();
        let mut spec = fx.spec();
        spec.collision = CollisionPolicy::Overwrite;
        wait(&ExportRun::start(fx.env(), &fx.ids, spec).unwrap());
        let brighter = png(&fx.out.path().join("01_a.png"));
        let sum = |i: &image::RgbImage| i.pixels().map(|p| p[0] as u64 + p[1] as u64).sum::<u64>();
        assert!(
            sum(&brighter) > sum(&plain),
            "+1.5 EV must brighten the export"
        );
    }

    #[test]
    fn collision_policies_are_applied() {
        let Some(fx) = fixture(&["a.NEF"]) else {
            return;
        };
        std::fs::write(fx.out.path().join("01_a.png"), b"original").unwrap();

        let mut spec = fx.spec();
        spec.collision = CollisionPolicy::Skip;
        let report = wait(&ExportRun::start(fx.env(), &fx.ids, spec).unwrap());
        assert_eq!(report.skipped.len(), 1, "{report:?}");
        assert_eq!(
            std::fs::read(fx.out.path().join("01_a.png")).unwrap(),
            b"original"
        );

        let report = wait(&ExportRun::start(fx.env(), &fx.ids, fx.spec()).unwrap());
        assert_eq!(report.exported.len(), 1);
        assert_eq!(fx.out_files(), ["01_a-2.png", "01_a.png"]);
        assert_eq!(
            std::fs::read(fx.out.path().join("01_a.png")).unwrap(),
            b"original"
        );
    }

    #[test]
    fn cancelling_settles_every_photo_and_leaves_no_temp_files() {
        let Some(fx) = fixture(&["a.NEF", "b.NEF", "c.NEF", "d.NEF", "e.NEF"]) else {
            return;
        };
        let run = ExportRun::start(fx.env(), &fx.ids, fx.spec()).unwrap();
        run.cancel();
        let report = wait(&run);
        assert_eq!(
            report.exported.len() + report.failed.len() + report.skipped.len() + report.cancelled,
            5,
            "{report:?}"
        );
        assert!(report.cancelled >= 1, "{report:?}");
        assert!(
            fx.out_files().iter().all(|n| !n.contains("nicti-tmp")),
            "{:?}",
            fx.out_files()
        );
        assert!(run.poll().is_some());
    }

    #[test]
    fn a_camera_profile_that_cannot_be_reloaded_fails_that_photo_instead_of_exporting_wrong_colors()
    {
        let Some(fx) = fixture(&["a.NEF", "b.NEF"]) else {
            return;
        };
        let mut doc = EditDocument::default();
        doc.stages.insert(
            WORKING_SPACE.to_string(),
            StageEntry {
                schema_version: 1,
                params: serde_json::to_value(CameraProfileParams {
                    name: Some("Gone".into()),
                    path: Some("/definitely/not/here.dcp".into()),
                    content_hash: Some("00".repeat(32)),
                    look: None,
                })
                .unwrap(),
            },
        );
        fx.store.put_master_edit(fx.ids[0], &doc).unwrap();
        let report = wait(&ExportRun::start(fx.env(), &fx.ids, fx.spec()).unwrap());
        assert_eq!(report.failed.len(), 1, "{report:?}");
        assert!(report.failed[0].1.contains("camera profile"));
        assert_eq!(report.exported.len(), 1);
    }

    #[test]
    fn bad_settings_and_an_unreadable_watermark_are_refused_before_any_work() {
        let Some(fx) = fixture(&["a.NEF"]) else {
            return;
        };
        let mut spec = fx.spec();
        spec.naming.template = "{Nope}".into();
        assert!(matches!(
            ExportRun::start(fx.env(), &fx.ids, spec),
            Err(StartError::Spec(_))
        ));

        let mut spec = fx.spec();
        spec.watermark = Some(WatermarkSpec {
            path: fx.out.path().join("missing.svg"),
            ..WatermarkSpec::default()
        });
        assert!(matches!(
            ExportRun::start(fx.env(), &fx.ids, spec),
            Err(StartError::Watermark(_))
        ));
        assert!(fx.out_files().is_empty(), "nothing was written");

        assert!(matches!(
            ExportRun::start(fx.env(), &[], fx.spec()),
            Err(StartError::NothingToExport)
        ));
    }

    #[test]
    fn a_photo_with_active_masks_exports_with_a_warning_that_they_are_not_applied() {
        use nicti_tapetum::mask::params::{
            LocalAdjust, LocalCorrection, MaskComponent, MaskGroup, MaskSource,
        };
        let Some(fx) = fixture(&["a.NEF", "b.NEF"]) else {
            return;
        };
        let mut doc = EditDocument::default();
        let masks = MaskParams {
            corrections: vec![LocalCorrection {
                mask: MaskGroup {
                    components: vec![MaskComponent {
                        source: MaskSource::LinearGradient {
                            p0: [0.0, 0.5],
                            p1: [1.0, 0.5],
                        },
                        ..MaskComponent::default()
                    }],
                },
                adjust: LocalAdjust {
                    exposure: 1.0,
                    ..LocalAdjust::default()
                },
                ..LocalCorrection::default()
            }],
        };
        doc.stages.insert(
            nicti_tapetum::stages::MASKS.to_string(),
            StageEntry {
                schema_version: 1,
                params: serde_json::to_value(&masks).unwrap(),
            },
        );
        fx.store.put_master_edit(fx.ids[0], &doc).unwrap();
        let report = wait(&ExportRun::start(fx.env(), &fx.ids, fx.spec()).unwrap());
        assert_eq!(report.exported.len(), 2, "both still export: {report:?}");
        assert_eq!(report.warnings.len(), 1, "{report:?}");
        assert!(report.warnings[0].contains("a.NEF"), "{report:?}");
        assert!(
            !report.warnings[0].contains("b.NEF"),
            "only the masked photo"
        );
    }

    #[test]
    fn photos_gone_from_the_catalog_are_warned_about_not_fatal() {
        let Some(fx) = fixture(&["a.NEF"]) else {
            return;
        };
        let ids = [fx.ids[0], 987_654];
        let report = wait(&ExportRun::start(fx.env(), &ids, fx.spec()).unwrap());
        assert_eq!(report.exported.len(), 1);
        assert_eq!(report.warnings.len(), 1, "{report:?}");
    }
}
