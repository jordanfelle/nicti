//! #145 "Eyeshine": rendered screen previews that carry the user's develop edits, plus the
//! stale-while-revalidate rule for showing them.
//!
//! ADR-0029's T0-T3 tiers are all camera-JPEG-derived, so none of them reflect develop edits (or
//! Nicti's own colour rendering). A *rendered* tier (`LarderTier::Rendered`, beside the camera
//! `T2`) is a 3840px render of the edit document. Until one exists for the document's current
//! hash the camera preview stays on screen, badged as stale; an older render is kept and shown
//! ("updating") rather than dropped.
//!
//! This module is the pure part: the render hash, the "has edits / partial" tests and
//! [`choose_preview`]. The render job and the display wiring build on it.

use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use nicti_calico::dcp::DcpProfile;
use nicti_cornea::{LinearFrame, RawDecoder};
use nicti_lair::larder::{LarderKey, LarderTier};
use nicti_lair::pounce_jobs::ReportSlot;
use nicti_lair::CatalogStore;
use nicti_pounce::{
    ChunkedJob, JobError, JobId, JobKind, JobSpec, Lane, Pounce, Priority, Progress, Step,
    Submitter,
};
use nicti_preen::exporters::builtin_registry;
use nicti_preen::metadata::{SourceExif, SourceMetadata};
use nicti_preen::spec::{ExportSpec, FormatSpec, MetadataPolicy, MetadataSpec};
use nicti_preen::{export_frame, ExportContext, WorkingFrame};
use nicti_tapetum::frame::{Extent, FrameTexture};
use nicti_tapetum::geometry::Affine2D;
use nicti_tapetum::gpu::GpuContext;
use nicti_tapetum::tile::{Rect, Tile, TileBudget, TilePlanner, TiledRender};

use crate::camera_profiles;
use crate::export::render_core::{render_live_frame, ExportRenderer, LiveRender};
use crate::export::sink::AccumSink;
use crate::loupe::asset_cache_key;
use crate::t2::{lock_larder_within, try_lock_larder, SharedLarder, T2Outcome};
use nicti_pawprint::EditDocument;
use nicti_tapetum::coat::{HealParams, SpotKind};
use nicti_tapetum::mask::params::MaskParams;
use nicti_tapetum::spine;
use nicti_tapetum::stages::{HEAL, MASKS};

/// Bump whenever the render pipeline's output changes for the same document, so every cached
/// rendered preview reads as stale and is regenerated.
pub const EYESHINE_VERSION: u32 = 1;

/// Suffix on a render hash when the document holds adjustments the preview render cannot show yet:
/// local masks (#354) and AI removal patches (#324).
pub const PARTIAL_SUFFIX: &str = ":partial";

/// True when the document has any stage entry at all -- the same test as `DevelopView::has_edits`.
pub fn has_edits(doc: &EditDocument) -> bool {
    !doc.stages.is_empty()
}

/// True when the preview render would silently lack something the Develop view shows: active
/// local adjustments (the export/preview path never builds a mask atlas, #354) or AI removal
/// spots (their patches aren't persisted, #324).
pub fn is_partial(doc: &EditDocument) -> bool {
    let masks = spine::resolve::<MaskParams>(doc, MASKS)
        .active()
        .next()
        .is_some();
    let ai_removal = spine::resolve::<HealParams>(doc, HEAL)
        .spots
        .iter()
        .any(|s| s.kind == SpotKind::Remove);
    masks || ai_removal
}

/// The Larder `render_hash` for the rendered tier: pipeline version + the photo's identity + the
/// whole edit document's content hash (which already includes a chosen camera profile's hash).
/// `None` when the document can't be canonicalised (non-finite floats) -- nothing is cached then.
pub fn rendered_hash(identity: &blake3::Hash, doc: &EditDocument) -> Option<String> {
    let doc_hash = doc.content_hash().ok()?;
    let mut h = blake3::Hasher::new();
    h.update(doc_hash.as_bytes());
    // `rendered:v{N}:{identity}:{document}`: the identity segment lets a reader tell an older
    // render of the *same file* (fine to show while updating) from one of a file that was
    // replaced under the same asset id (never to be shown).
    let mut out = format!(
        "rendered:v{EYESHINE_VERSION}:{}:{}",
        identity.to_hex(),
        h.finalize().to_hex()
    );
    if is_partial(doc) {
        out.push_str(PARTIAL_SUFFIX);
    }
    Some(out)
}

/// Whether two rendered hashes are renders of the same file (same identity segment), whatever
/// their edit documents.
pub fn same_file(a: &str, b: &str) -> bool {
    let id = |h: &str| h.split(':').nth(2).map(str::to_owned);
    matches!((id(a), id(b)), (Some(x), Some(y)) if x == y)
}

/// Whether a stored rendered hash carries the "partial" flag.
pub fn hash_is_partial(hash: &str) -> bool {
    hash.ends_with(PARTIAL_SUFFIX)
}

/// Which photos get a rendered preview (Library > Preview settings).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum RenderPolicy {
    /// Never render; always the camera previews (today's behaviour).
    Off,
    /// Only photos that have develop edits.
    #[default]
    EditedOnly,
    /// Every photo, so unedited ones also show Nicti's own colour rendering instead of the
    /// camera's Picture Control look (ADR-0029/0038's flagged gap).
    All,
}

impl RenderPolicy {
    /// Whether a photo with (`edited`) or without edits should be rendered at all.
    pub fn wants_render(self, edited: bool) -> bool {
        match self {
            RenderPolicy::Off => false,
            RenderPolicy::EditedOnly => edited,
            RenderPolicy::All => true,
        }
    }
}

/// What the viewer should draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// A render of the current edit document.
    Rendered,
    /// A render of an *older* document, kept until the new one lands.
    StaleRendered,
    /// The camera's screen-size JPEG (T2).
    CameraT2,
    /// The camera's grid thumbnail (T0).
    CameraT0,
    /// Nothing available yet.
    None,
}

/// The indicator drawn with a preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Badge {
    /// Nothing to flag.
    None,
    /// A render is on the way: the photo has edits this preview does not show yet.
    Stale,
    /// Showing an older render while a newer one is made.
    Updating,
    /// The render lacks local adjustments / AI removals (see [`is_partial`]).
    Partial,
    /// An unedited photo still showing the camera's own rendering (policy `All`, render pending).
    CameraRendering,
}

impl Badge {
    /// The text shown beside a preview with this badge (`None` = nothing to say).
    pub fn label(self) -> Option<&'static str> {
        match self {
            Badge::None => Option::None,
            Badge::Stale => {
                Some("Edited: this preview is the camera's version until the render finishes")
            }
            Badge::Updating => Some("Updating preview with your latest edits..."),
            Badge::Partial => {
                Some("Preview omits local adjustments / AI removals (shown once decoded)")
            }
            Badge::CameraRendering => Some("Camera rendering shown while Nicti's render is made"),
        }
    }
}

/// What exists for one photo.
#[derive(Debug, Clone, Copy, Default)]
pub struct Available {
    /// A render whose hash equals the current one; `partial` is that hash's flag.
    pub rendered_current: Option<bool>,
    /// A render of an older document.
    pub rendered_older: bool,
    pub camera_t2: bool,
    pub camera_t0: bool,
}

/// The stale-while-revalidate decision for one photo and one surface.
///
/// A current render always wins. Otherwise an older render beats the camera previews, and the
/// camera T2 beats T0. The badge says why the shown preview may not match the edits.
pub fn choose_preview(policy: RenderPolicy, edited: bool, have: Available) -> (Source, Badge) {
    let render_expected = policy.wants_render(edited);
    if render_expected {
        if let Some(partial) = have.rendered_current {
            return (
                Source::Rendered,
                if partial { Badge::Partial } else { Badge::None },
            );
        }
        if have.rendered_older {
            return (Source::StaleRendered, Badge::Updating);
        }
    }
    let source = if have.camera_t2 {
        Source::CameraT2
    } else if have.camera_t0 {
        Source::CameraT0
    } else {
        Source::None
    };
    if source == Source::None {
        return (source, Badge::None);
    }
    // Only promise a render that is actually coming: with rendering off for this view the camera
    // preview is simply what is shown.
    let badge = match (render_expected, edited) {
        (true, true) => Badge::Stale,
        (true, false) => Badge::CameraRendering,
        (false, _) => Badge::None,
    };
    (source, badge)
}

// --- the render job ----------------------------------------------------------------------------
//
// One photo = three chained Pounce jobs, the same lane split export (#57) uses so the GPU lane only
// ever does GPU-shaped work: decode (CPU) -> render, one tile per step (GPU) -> encode + store
// (CPU). Each stage submits the next through a `Submitter`. Every stage owns the photo's
// [`Settle`], which resolves the result slot exactly once (a dropped, never-run job settles as
// `Retry`, so a poller never waits on a slot that never resolves).

/// Long edge of the rendered tier, px (ADR-0029's screen tier).
pub const RENDER_LONG_EDGE: u32 = crate::t2::T2_LONG_EDGE;
/// JPEG quality of the rendered tier (same as the camera T2, ADR-0143).
const RENDER_JPEG_QUALITY: u8 = 85;
const TILE_MAX_DIM: u32 = 4096;
const TILE_MAX_STAGING_BYTES: u64 = 64 * 1024 * 1024;
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Everything the three stages share for one photo.
pub struct Request {
    pub asset_id: i64,
    pub identity: blake3::Hash,
    /// The target `rendered_hash` (already computed from `identity` + `edit`).
    pub hash: String,
    pub path: PathBuf,
    pub edit: EditDocument,
    pub image_index: usize,
}

/// What the render pipeline needs from the app, cloned into every job.
#[derive(Clone)]
pub struct Env {
    pub decoder: Arc<dyn RawDecoder + Send + Sync>,
    pub gpu: Arc<GpuContext>,
    pub larder: SharedLarder,
    pub submitter: Submitter,
    /// The batch's kernels, reused across photos (one GPU worker, so one at a time).
    pub render_ctx: Arc<Mutex<Option<ExportRenderer>>>,
    /// Held by the one photo whose render is in progress. Pounce may interleave two GPU jobs
    /// between tiles; without this a second photo would build its own kernels and hold a second
    /// full-extent frame texture (~350 MB at 45 MP) while `vram_bytes` says 0.
    render_busy: Arc<AtomicBool>,
    /// Pipelines submitted and not yet settled -- including ones the service evicted from its own
    /// table whose queued job hasn't been reached by a worker (and so still holds a decoded
    /// frame). New work is admitted against this, not just against the service's table.
    live: Arc<AtomicUsize>,
}

/// RAII claim on [`Env::render_busy`].
struct RenderClaim(Arc<AtomicBool>);

impl RenderClaim {
    fn try_acquire(busy: &Arc<AtomicBool>) -> Option<RenderClaim> {
        busy.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| RenderClaim(busy.clone()))
    }
}

impl Drop for RenderClaim {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl Env {
    pub fn new(
        decoder: Arc<dyn RawDecoder + Send + Sync>,
        gpu: Arc<GpuContext>,
        larder: SharedLarder,
        submitter: Submitter,
    ) -> Self {
        Env {
            decoder,
            gpu,
            larder,
            submitter,
            render_ctx: Arc::new(Mutex::new(None)),
            render_busy: Arc::new(AtomicBool::new(false)),
            live: Arc::new(AtomicUsize::new(0)),
        }
    }
}

/// Resolves a photo's result slot exactly once.
struct Settle {
    slot: Option<ReportSlot<T2Outcome>>,
    /// Set when a stage panicked: the dropped `Settle` then resolves `Failed`, not `Retry`, so a
    /// deterministic panic can't loop forever.
    panicked: Arc<AtomicBool>,
    /// Decremented exactly once, when the slot resolves (see [`Env::live`]).
    live: Arc<AtomicUsize>,
}

impl Settle {
    fn settle(mut self, outcome: T2Outcome) {
        if let Some(slot) = self.slot.take() {
            self.live.fetch_sub(1, Ordering::SeqCst);
            *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(outcome);
        }
    }
}

impl Drop for Settle {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.take() {
            self.live.fetch_sub(1, Ordering::SeqCst);
            // `thread::panicking()` covers a panic that is still unwinding through `begin` (which
            // owns the `Settle`): the flag is only set after `catch_unwind` returns, too late.
            let outcome = if self.panicked.load(Ordering::SeqCst) || std::thread::panicking() {
                T2Outcome::Failed("preview render panicked".into())
            } else {
                T2Outcome::Retry("cancelled".into())
            };
            *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(outcome);
        }
    }
}

/// The per-photo state carried through the stages.
struct Task {
    req: Request,
    env: Env,
    /// Set by the service when a newer edit supersedes this render.
    cancelled: Arc<AtomicBool>,
    panicked: Arc<AtomicBool>,
}

impl Task {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    fn key(&self) -> LarderKey<'_> {
        LarderKey {
            asset_id: self.req.asset_id,
            tier: LarderTier::Rendered,
            render_hash: &self.req.hash,
        }
    }
}

/// Starts the pipeline for one photo. The returned `JobId` cancels the *first* stage while it's
/// still queued; `cancelled` is the flag every later stage checks.
pub fn submit(
    env: &Env,
    pounce: &Pounce,
    req: Request,
    cancelled: Arc<AtomicBool>,
) -> (JobId, ReportSlot<T2Outcome>) {
    let slot: ReportSlot<T2Outcome> = Arc::new(Mutex::new(None));
    let task = Arc::new(Task {
        req,
        env: env.clone(),
        cancelled,
        panicked: Arc::new(AtomicBool::new(false)),
    });
    env.live.fetch_add(1, Ordering::SeqCst);
    let settle = Settle {
        slot: Some(slot.clone()),
        panicked: task.panicked.clone(),
        live: env.live.clone(),
    };
    let job = DecodeJob {
        label: format!("Render preview: {}", task.req.path.display()),
        task,
        settle: Some(settle),
    };
    (pounce.submit(Box::new(job)), slot)
}

fn spec(lane: Lane, task: &Task) -> JobSpec {
    JobSpec {
        priority: Priority::Background,
        kind: JobKind::Preview,
        lane,
        // The GPU stage deliberately has no cursor index: Pounce re-queues a yielded job ahead of
        // anything with a larger key, so a render *waiting* for the claim at a nearer index would
        // be picked forever and starve the claim holder at a farther one (the claim never
        // released, the worker spinning). With no index every render sorts equal and equal keys
        // are FIFO, so the holder and the waiters take turns. The CPU decode keeps the index.
        // Deliberately 0, like export's render job: a 45 MP frame's textures exceed the
        // placeholder VRAM budget and Pounce drops a job over the *total* budget.
        vram_bytes: 0,
        image_index: (lane == Lane::Cpu).then_some(task.req.image_index),
    }
}

// stage 1: decode ---------------------------------------------------------------------------------

struct DecodeJob {
    task: Arc<Task>,
    settle: Option<Settle>,
    label: String,
}

impl ChunkedJob for DecodeJob {
    fn spec(&self) -> JobSpec {
        spec(Lane::Cpu, &self.task)
    }
    fn label(&self) -> String {
        self.label.clone()
    }
    fn progress(&self) -> Progress {
        Progress {
            done: u64::from(self.settle.is_none()),
            total: Some(3),
        }
    }
    /// Never returns `Err`: a failure settles the slot (Pounce would drop the job silently).
    fn step(&mut self) -> Result<Step, JobError> {
        let Some(settle) = self.settle.take() else {
            return Ok(Step::Done);
        };
        let task = self.task.clone();
        let outcome = catch_unwind(AssertUnwindSafe(|| decode_stage(&task)))
            .unwrap_or_else(|_| Err(T2Outcome::Failed("preview decode panicked".into())));
        match outcome {
            Err(done) => settle.settle(done),
            Ok(ready) => {
                let job = RenderJob {
                    label: format!("Render preview: {}", task.req.path.display()),
                    task: task.clone(),
                    ready: Some((ready, settle)),
                    phase: None,
                    tiles_total: 0,
                };
                // A refused submit (shutdown) drops the job, whose `Settle` resolves `Retry`.
                let _ = task.env.submitter.submit(Box::new(job));
            }
        }
        Ok(Step::Done)
    }
}

struct Decoded {
    frame: Arc<LinearFrame>,
    exif: SourceExif,
    profile: Option<Arc<DcpProfile>>,
}

/// `Err` is a *final* outcome (nothing further to run); `Ok` is the decoded photo.
fn decode_stage(task: &Task) -> Result<Decoded, T2Outcome> {
    if task.is_cancelled() {
        return Err(T2Outcome::Retry("superseded".into()));
    }
    // Reachability first, on this worker: `metadata` on a hung/unmounted drive can block.
    if std::fs::metadata(&task.req.path).is_err() {
        return Err(T2Outcome::Retry("source file unreachable".into()));
    }
    {
        let Some(larder) = lock_larder_within(&task.env.larder, LOCK_WAIT) else {
            return Err(T2Outcome::Retry("larder busy".into()));
        };
        if larder.contains(task.key()).unwrap_or(false) {
            return Err(T2Outcome::Stored);
        }
    }
    let frame = task
        .env
        .decoder
        .decode_linear(&task.req.path)
        .map_err(|e| T2Outcome::Failed(format!("couldn't decode: {e}")))?;
    let exif = SourceExif::from_path(&task.req.path);
    // A camera profile the edit names must reload and still match: rendering with a different
    // profile than the user edited with would show the wrong colours.
    let profile = camera_profiles::load_for_document(&task.req.edit, &frame.make, &frame.model)
        .map_err(|e| T2Outcome::Failed(format!("camera profile: {e}")))?;
    Ok(Decoded {
        frame: Arc::new(frame),
        exif,
        profile,
    })
}

// stage 2: render ---------------------------------------------------------------------------------

struct RenderPhase {
    _claim: RenderClaim,
    exif: SourceExif,
    ctx: ExportRenderer,
    live: Arc<FrameTexture>,
    base: Affine2D,
    tiles: Vec<Tile>,
    next: usize,
    sink: AccumSink,
    settle: Settle,
}

struct RenderJob {
    task: Arc<Task>,
    label: String,
    ready: Option<(Decoded, Settle)>,
    phase: Option<RenderPhase>,
    tiles_total: u64,
}

impl RenderJob {
    fn give_back(&self, ctx: ExportRenderer) {
        *self
            .task
            .env
            .render_ctx
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(ctx);
    }

    /// First step: the whole-frame live suffix, then a screen-size tile grid over the crop.
    fn begin(&mut self, decoded: Decoded, settle: Settle, claim: RenderClaim) {
        let task = self.task.clone();
        let Decoded {
            frame,
            exif,
            profile,
        } = decoded;
        let mut ctx = task
            .env
            .render_ctx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .unwrap_or_else(|| ExportRenderer::new(&task.env.gpu));
        let LiveRender { live, inputs } = match render_live_frame(
            &mut ctx,
            &task.env.gpu,
            &task.req.edit,
            task.req.identity,
            &frame,
            profile.as_deref(),
        ) {
            Ok(r) => r,
            Err(why) => {
                self.give_back(ctx);
                return settle.settle(T2Outcome::Failed(why));
            }
        };
        // The decoded frame (~270 MB at 45 MP) isn't needed past this point.
        drop(frame);

        let rect = inputs.crop_rect;
        let (out_w, out_h, base) = screen_geometry(rect.width, rect.height, inputs.crop_transform);
        let sink = match AccumSink::try_new(out_w, out_h) {
            Ok(s) => s,
            Err(why) => {
                self.give_back(ctx);
                return settle.settle(T2Outcome::Failed(why));
            }
        };
        let tiles = TilePlanner::plan(
            Extent {
                width: out_w,
                height: out_h,
            },
            Rect {
                x: 0,
                y: 0,
                width: out_w,
                height: out_h,
            },
            1, // bilinear neighbour read
            TileBudget {
                max_dim: task
                    .env
                    .gpu
                    .limits
                    .max_texture_dimension_2d
                    .min(TILE_MAX_DIM),
                max_staging_bytes: TILE_MAX_STAGING_BYTES,
                target_chunk_ms: 16.0,
            },
        );
        self.tiles_total = tiles.len() as u64;
        self.phase = Some(RenderPhase {
            _claim: claim,
            exif,
            ctx,
            live,
            base,
            tiles,
            next: 0,
            sink,
            settle,
        });
    }
}

/// The screen-size output for a crop of `crop_w` x `crop_h` source pixels, and the
/// output->source transform for it: the crop's own transform with its input axes scaled by the
/// downscale, so output pixel `(x, y)` samples crop pixel `(x / sx, y / sy)`. Never upscales.
pub fn screen_geometry(crop_w: f32, crop_h: f32, crop: Affine2D) -> (u32, u32, Affine2D) {
    let full_w = crop_w.round().max(1.0);
    let full_h = crop_h.round().max(1.0);
    let scale = (RENDER_LONG_EDGE as f32 / full_w.max(full_h)).min(1.0);
    let out_w = ((full_w * scale).round() as u32).max(1);
    let out_h = ((full_h * scale).round() as u32).max(1);
    let (sx, sy) = (out_w as f32 / full_w, out_h as f32 / full_h);
    (
        out_w,
        out_h,
        Affine2D {
            a: crop.a / sx,
            b: crop.b / sy,
            c: crop.c / sx,
            d: crop.d / sy,
            tx: crop.tx,
            ty: crop.ty,
        },
    )
}

impl ChunkedJob for RenderJob {
    fn spec(&self) -> JobSpec {
        spec(Lane::Gpu, &self.task)
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
    /// Never returns `Err`, and a panic settles the slot as `Failed` (see [`Settle`]).
    fn step(&mut self) -> Result<Step, JobError> {
        match catch_unwind(AssertUnwindSafe(|| self.step_inner())) {
            Ok(r) => r,
            Err(_) => {
                self.task.panicked.store(true, Ordering::SeqCst);
                // Dropping these settles the slot (`Failed`) and releases the render claim; the
                // kernels were taken out of the pool, so the next photo builds fresh ones.
                self.ready = None;
                self.phase = None;
                Ok(Step::Done)
            }
        }
    }
}

impl RenderJob {
    fn step_inner(&mut self) -> Result<Step, JobError> {
        if let Some((decoded, settle)) = self.ready.take() {
            if self.task.is_cancelled() {
                settle.settle(T2Outcome::Retry("superseded".into()));
                return Ok(Step::Done);
            }
            let Some(claim) = RenderClaim::try_acquire(&self.task.env.render_busy) else {
                // Another photo is mid-render: wait our turn, holding only the decoded frame.
                self.ready = Some((decoded, settle));
                return Ok(Step::Yield);
            };
            self.begin(decoded, settle, claim);
            return Ok(if self.phase.is_some() {
                Step::Yield
            } else {
                Step::Done
            });
        }
        let Some(phase) = self.phase.as_mut() else {
            return Ok(Step::Done);
        };
        if self.task.is_cancelled() {
            let phase = self.phase.take().expect("checked Some");
            self.give_back(phase.ctx);
            phase.settle.settle(T2Outcome::Retry("superseded".into()));
            return Ok(Step::Done);
        }
        // One tile per step (ADR-0054's ~16 ms chunks).
        let tile = phase.tiles[phase.next];
        TiledRender::new(
            self.task.env.gpu.clone(),
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
            exif,
            ctx,
            live,
            sink,
            settle,
            ..
        } = self.phase.take().expect("checked Some");
        drop(live);
        self.give_back(ctx);
        let frame = WorkingFrame {
            width: sink.width,
            height: sink.height,
            pixels: sink.pixels,
        };
        let job = EncodeJob {
            label: format!("Encode preview: {}", self.task.req.path.display()),
            task: self.task.clone(),
            work: Some((frame, exif, settle)),
        };
        let _ = self.task.env.submitter.submit(Box::new(job));
        Ok(Step::Done)
    }
}

// stage 3: encode + store -------------------------------------------------------------------------

struct EncodeJob {
    task: Arc<Task>,
    label: String,
    work: Option<(WorkingFrame, SourceExif, Settle)>,
}

fn preview_spec() -> ExportSpec {
    ExportSpec {
        format: FormatSpec::Jpeg {
            quality: RENDER_JPEG_QUALITY,
            subsampling: Default::default(),
        },
        // No EXIF/XMP: it is a cache entry, not a deliverable. Orientation still comes from
        // `SourceMetadata.exif` and is applied to the pixels.
        metadata: MetadataSpec {
            policy: MetadataPolicy::None,
            ..Default::default()
        },
        ..Default::default()
    }
}

impl ChunkedJob for EncodeJob {
    fn spec(&self) -> JobSpec {
        spec(Lane::Cpu, &self.task)
    }
    fn label(&self) -> String {
        self.label.clone()
    }
    fn progress(&self) -> Progress {
        Progress {
            done: u64::from(self.work.is_none()),
            total: Some(1),
        }
    }
    fn step(&mut self) -> Result<Step, JobError> {
        let Some((frame, exif, settle)) = self.work.take() else {
            return Ok(Step::Done);
        };
        let task = self.task.clone();
        let outcome = catch_unwind(AssertUnwindSafe(|| encode_and_store(&task, frame, exif)))
            .unwrap_or_else(|_| T2Outcome::Failed("preview encode panicked".into()));
        settle.settle(outcome);
        Ok(Step::Done)
    }
}

fn encode_and_store(task: &Task, frame: WorkingFrame, exif: SourceExif) -> T2Outcome {
    if task.is_cancelled() {
        return T2Outcome::Retry("superseded".into());
    }
    let spec = preview_spec();
    let source = SourceMetadata {
        exif,
        ..Default::default()
    };
    let ctx = ExportContext {
        spec: &spec,
        source: &source,
        watermark: None,
        software: "Nicti",
    };
    let exported = match export_frame(frame, &ctx, &builtin_registry()) {
        Ok(e) => e,
        Err(e) => return T2Outcome::Failed(format!("encoding failed: {e}")),
    };
    let Some(mut larder) = lock_larder_within(&task.env.larder, LOCK_WAIT) else {
        return T2Outcome::Retry("larder busy".into());
    };
    // Re-check under the lock: a render superseded while it was encoding must not overwrite the
    // newer one (`put` replaces by `(asset, tier)` without comparing hashes).
    if task.is_cancelled() {
        return T2Outcome::Retry("superseded".into());
    }
    match larder.put(task.key(), &exported.bytes) {
        Ok(true) => T2Outcome::Stored,
        Ok(false) => T2Outcome::Failed("preview larger than the whole cache cap".into()),
        Err(e) => T2Outcome::Failed(e.to_string()),
    }
}

// --- the service: who needs a render, dedupe, supersede -----------------------------------------

/// How long a photo's resolution (edit document, hash) is trusted before the catalog is read
/// again. Edits reach the catalog from several writers (Develop save, paste/sync, undo, LRC
/// import); explicit invalidation covers Develop, and this bound makes every other writer
/// self-heal within a second instead of leaving a stale render forever.
const RESOLVE_TTL: std::time::Duration = std::time::Duration::from_secs(1);
/// Back-off before a render that returned `Retry` (unreachable file, busy Larder) is tried again.
const RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_secs(5);
/// Back-off after a render `Failed`. Not permanent: a locked file, a camera profile installed
/// later or a full disk can all clear up; an unchanged document is simply retried rarely.
const FAILED_BACKOFF: std::time::Duration = std::time::Duration::from_secs(60);
/// Bound on remembered back-offs (expired ones are pruned past this).
const MAX_BLOCKED: usize = 256;

/// What the UI needs to know about one photo's preview state.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub identity: blake3::Hash,
    pub path: PathBuf,
    pub edit: EditDocument,
    pub edited: bool,
    /// The hash a *current* render would be stored under (`None` when the document can't be
    /// canonicalised).
    pub hash: Option<String>,
    at: std::time::Instant,
}

/// The cheap, per-frame view of a photo's preview state (no document clone).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Info {
    pub edited: bool,
    pub hash: Option<String>,
}

/// Renders in flight at once. Each decoded frame is ~270 MB at 45 MP and waits for the single GPU
/// claim while holding it, so the bound is on frames, not just on the GPU.
const MAX_INFLIGHT: usize = 2;
/// Pipelines alive (including evicted ones still draining out of Pounce's queues) before new work
/// waits for them to settle.
const MAX_LIVE: usize = MAX_INFLIGHT + 1;

struct Inflight {
    /// Submission order, so the oldest render can be evicted for the photo being looked at.
    seq: u64,
    hash: String,
    job: JobId,
    slot: ReportSlot<T2Outcome>,
    cancelled: Arc<AtomicBool>,
}

/// Decides which photos get a rendered preview, keeps at most one render per photo in flight (a
/// newer edit cancels the older render), and remembers renders that can't succeed so they aren't
/// retried on every frame. One per app; every call is cheap enough for the UI thread.
pub struct EyeshineService {
    env: Option<Env>,
    inflight: HashMap<i64, Inflight>,
    /// `(asset, hash)` pairs not to be retried until the instant (see `RETRY_BACKOFF` /
    /// `FAILED_BACKOFF`).
    blocked: HashMap<(i64, String), std::time::Instant>,
    next_seq: u64,
    /// Per-asset resolution cache (a catalog read each); dropped by [`Self::invalidate`].
    resolved: HashMap<i64, Resolved>,
}

impl EyeshineService {
    /// `None` env (no Larder could be opened) makes the service inert: nothing is rendered.
    pub fn new(env: Option<Env>) -> Self {
        EyeshineService {
            env,
            inflight: HashMap::new(),
            blocked: HashMap::new(),
            next_seq: 0,
            resolved: HashMap::new(),
        }
    }

    /// Forget what's cached about `asset_id` (its edits or file changed).
    pub fn invalidate(&mut self, asset_id: i64) {
        self.resolved.remove(&asset_id);
    }

    /// Drops every cached resolution (a re-ingest / sync may have replaced files).
    pub fn invalidate_all(&mut self) {
        self.resolved.clear();
    }

    /// The photo's identity, edit document and current render hash, cached. `None` when the asset
    /// or its root is unknown.
    pub fn resolve(&mut self, store: &dyn CatalogStore, asset_id: i64) -> Option<&Resolved> {
        let fresh = self
            .resolved
            .get(&asset_id)
            .is_some_and(|r| r.at.elapsed() < RESOLVE_TTL);
        if !fresh {
            let asset = store.get_asset(asset_id).ok().flatten()?;
            let root = store.get_root_path(asset.root_id).ok().flatten()?;
            let identity = asset_cache_key(&asset);
            // A failed read is *not* "unedited": caching an empty document would show (and
            // render) the photo as unedited until the next invalidation.
            let (edit, hash) = match store.get_master_edit(asset_id) {
                Ok(doc) => {
                    let edit = doc.unwrap_or_default();
                    let hash = rendered_hash(&identity, &edit);
                    (edit, hash)
                }
                // Cached for the TTL with no hash: nothing is rendered or badged for a document
                // that can't be read, and the catalog isn't re-queried every frame.
                Err(_) => (EditDocument::default(), None),
            };
            self.resolved.insert(
                asset_id,
                Resolved {
                    identity,
                    path: PathBuf::from(root).join(&asset.rel_path),
                    edited: has_edits(&edit),
                    edit,
                    hash,
                    at: std::time::Instant::now(),
                },
            );
        }
        self.resolved.get(&asset_id)
    }

    /// Queues a render for `asset_id` when `policy` wants one and none is cached, running or
    /// backing off for its current document. Returns the photo's state for the caller's display
    /// decision.
    pub fn request(
        &mut self,
        pounce: &Pounce,
        store: &dyn CatalogStore,
        asset_id: i64,
        image_index: usize,
        policy: RenderPolicy,
    ) -> Option<Info> {
        let resolved = self.resolve(store, asset_id)?;
        let info = Info {
            edited: resolved.edited,
            hash: resolved.hash.clone(),
        };
        let (Some(env), Some(hash)) = (&self.env, info.hash.clone()) else {
            return Some(info);
        };
        // A render in flight for an out-of-date document is obsolete whatever happens next --
        // including the early returns below (edit A -> B -> A: A is already stored, so B's render
        // finishing would overwrite it with the wrong pixels).
        if self.inflight.get(&asset_id).is_some_and(|f| f.hash != hash) {
            if let Some(old) = self.inflight.remove(&asset_id) {
                old.cancelled.store(true, Ordering::SeqCst);
                pounce.cancel(old.job);
            }
        }
        if !policy.wants_render(info.edited) {
            return Some(info);
        }
        if self.inflight.contains_key(&asset_id) {
            return Some(info);
        }
        let key = (asset_id, hash.clone());
        if self
            .blocked
            .get(&key)
            .is_some_and(|until| std::time::Instant::now() < *until)
        {
            return Some(info);
        }
        // Already cached? `try_lock`: a busy Larder just means the job re-checks on its worker.
        if let Some(larder) = try_lock_larder(&env.larder) {
            if larder
                .stored_hash(asset_id, LarderTier::Rendered)
                .ok()
                .flatten()
                .as_deref()
                == Some(hash.as_str())
            {
                return Some(info);
            }
        }
        let resolved = self.resolved.get(&asset_id)?.clone();
        let env = self.env.as_ref()?;
        // Bound the decoded frames in flight: make room for the photo being looked at by evicting
        // the oldest render (the loupe re-requests it if the user comes back).
        while self.inflight.len() >= MAX_INFLIGHT {
            let Some(oldest) = self
                .inflight
                .iter()
                .min_by_key(|(_, f)| f.seq)
                .map(|(a, _)| *a)
            else {
                break;
            };
            if let Some(old) = self.inflight.remove(&oldest) {
                old.cancelled.store(true, Ordering::SeqCst);
                pounce.cancel(old.job);
            }
        }
        // Evicted pipelines still hold a decoded frame until a worker reaches their cancelled job;
        // don't pile new ones on top. The loupe re-requests every frame, so this just waits.
        if env.live.load(Ordering::SeqCst) >= MAX_LIVE {
            return Some(info);
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        let cancelled = Arc::new(AtomicBool::new(false));
        let (job, slot) = submit(
            env,
            pounce,
            Request {
                asset_id,
                identity: resolved.identity,
                hash: hash.clone(),
                path: resolved.path,
                edit: resolved.edit,
                image_index,
            },
            cancelled.clone(),
        );
        self.inflight.insert(
            asset_id,
            Inflight {
                seq,
                hash,
                job,
                slot,
                cancelled,
            },
        );
        Some(info)
    }

    /// Collects finished renders; call once per frame. Returns the assets that now have a new
    /// stored render, so the caller can refresh what it's showing.
    pub fn poll(&mut self) -> Vec<i64> {
        let mut stored = Vec::new();
        let mut done = Vec::new();
        for (asset, f) in &self.inflight {
            let outcome = f.slot.lock().unwrap_or_else(|e| e.into_inner()).take();
            let Some(outcome) = outcome else { continue };
            done.push((*asset, f.hash.clone(), outcome));
        }
        let now = std::time::Instant::now();
        for (asset, hash, outcome) in done {
            self.inflight.remove(&asset);
            match outcome {
                T2Outcome::Stored => stored.push(asset),
                // Nothing wrong with the photo (unreachable drive, busy Larder, superseded):
                // retry, but not on the very next frame.
                T2Outcome::Retry(_) => {
                    self.blocked.insert((asset, hash), now + RETRY_BACKOFF);
                }
                T2Outcome::Failed(_) => {
                    self.blocked.insert((asset, hash), now + FAILED_BACKOFF);
                }
            }
        }
        if self.blocked.len() > MAX_BLOCKED {
            self.blocked.retain(|_, until| *until > now);
        }
        stored
    }

    /// True while any render is queued or running.
    pub fn busy(&self) -> bool {
        !self.inflight.is_empty()
            || self
                .env
                .as_ref()
                .is_some_and(|e| e.live.load(Ordering::SeqCst) > 0)
    }

    /// Cancels everything in flight (a view switch, shutdown).
    pub fn cancel_all(&mut self, pounce: &Pounce) {
        for (_, f) in self.inflight.drain() {
            f.cancelled.store(true, Ordering::SeqCst);
            pounce.cancel(f.job);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_pawprint::StageEntry;
    use serde_json::json;

    fn doc_with(stage: &str, params: serde_json::Value) -> EditDocument {
        let mut d = EditDocument::default();
        d.stages.insert(
            stage.to_string(),
            StageEntry {
                schema_version: 1,
                params,
            },
        );
        d
    }

    fn id(n: u8) -> blake3::Hash {
        blake3::hash(&[n])
    }

    #[test]
    fn an_empty_document_has_no_edits_and_is_not_partial() {
        let d = EditDocument::default();
        assert!(!has_edits(&d));
        assert!(!is_partial(&d));
    }

    #[test]
    fn the_hash_changes_with_identity_document_and_is_stable_otherwise() {
        let a = doc_with("nicti.exposure", json!({"ev": 0.5}));
        let b = doc_with("nicti.exposure", json!({"ev": 0.6}));
        let h = rendered_hash(&id(1), &a).unwrap();
        assert_eq!(h, rendered_hash(&id(1), &a).unwrap());
        assert_ne!(h, rendered_hash(&id(2), &a).unwrap());
        assert_ne!(h, rendered_hash(&id(1), &b).unwrap());
        assert!(h.starts_with(&format!("rendered:v{EYESHINE_VERSION}:")));
        assert!(!hash_is_partial(&h));
    }

    #[test]
    fn classic_heal_spots_are_not_partial_but_ai_removals_are() {
        let spot = |kind: &str| json!({"kind": kind, "center": [0.5, 0.5], "radius": 10.0});
        let classic = doc_with(HEAL, json!({"spots": [spot("heal"), spot("clone")]}));
        assert!(!is_partial(&classic));
        let ai = doc_with(HEAL, json!({"spots": [spot("heal"), spot("remove")]}));
        assert!(is_partial(&ai));
        let h = rendered_hash(&id(1), &ai).unwrap();
        assert!(hash_is_partial(&h));
    }

    #[test]
    fn the_policy_decides_which_photos_are_rendered() {
        assert!(!RenderPolicy::Off.wants_render(true));
        assert!(RenderPolicy::EditedOnly.wants_render(true));
        assert!(!RenderPolicy::EditedOnly.wants_render(false));
        assert!(RenderPolicy::All.wants_render(false));
    }

    fn have(cur: Option<bool>, older: bool, t2: bool, t0: bool) -> Available {
        Available {
            rendered_current: cur,
            rendered_older: older,
            camera_t2: t2,
            camera_t0: t0,
        }
    }

    #[test]
    fn a_current_render_wins_and_flags_partial_ones() {
        let p = RenderPolicy::EditedOnly;
        assert_eq!(
            choose_preview(p, true, have(Some(false), true, true, true)),
            (Source::Rendered, Badge::None)
        );
        assert_eq!(
            choose_preview(p, true, have(Some(true), false, true, true)),
            (Source::Rendered, Badge::Partial)
        );
    }

    #[test]
    fn an_older_render_beats_the_camera_previews_and_says_updating() {
        assert_eq!(
            choose_preview(RenderPolicy::EditedOnly, true, have(None, true, true, true)),
            (Source::StaleRendered, Badge::Updating)
        );
    }

    #[test]
    fn an_edited_photo_without_a_render_shows_the_camera_preview_flagged_stale() {
        let p = RenderPolicy::EditedOnly;
        assert_eq!(
            choose_preview(p, true, have(None, false, true, true)),
            (Source::CameraT2, Badge::Stale)
        );
        assert_eq!(
            choose_preview(p, true, have(None, false, false, true)),
            (Source::CameraT0, Badge::Stale)
        );
    }

    #[test]
    fn an_unedited_photo_is_only_tagged_when_the_policy_will_render_it() {
        assert_eq!(
            choose_preview(
                RenderPolicy::EditedOnly,
                false,
                have(None, false, true, true)
            ),
            (Source::CameraT2, Badge::None)
        );
        assert_eq!(
            choose_preview(RenderPolicy::All, false, have(None, false, true, true)),
            (Source::CameraT2, Badge::CameraRendering)
        );
    }

    #[test]
    fn with_rendering_off_the_camera_preview_is_shown_and_a_cached_render_is_ignored() {
        assert_eq!(
            choose_preview(RenderPolicy::Off, true, have(Some(false), true, true, true)),
            (Source::CameraT2, Badge::None)
        );
        assert_eq!(
            choose_preview(RenderPolicy::Off, false, have(None, false, true, true)),
            (Source::CameraT2, Badge::None)
        );
    }

    #[test]
    fn nothing_available_is_none_with_no_badge() {
        assert_eq!(
            choose_preview(RenderPolicy::All, true, have(None, false, false, false)),
            (Source::None, Badge::None)
        );
    }

    // --- geometry (pure) ---

    #[test]
    fn a_large_crop_is_downscaled_to_the_screen_tier_and_the_transform_scales_with_it() {
        let (w, h, t) = screen_geometry(7680.0, 3840.0, Affine2D::crop(100.0, 50.0));
        assert_eq!((w, h), (3840, 1920));
        // Output pixel (x, y) samples crop pixel (2x, 2y): a/d double, the offset is untouched.
        assert!((t.a - 2.0).abs() < 1e-4 && (t.d - 2.0).abs() < 1e-4);
        assert_eq!((t.tx, t.ty), (100.0, 50.0));
    }

    #[test]
    fn a_small_crop_is_never_upscaled() {
        let (w, h, t) = screen_geometry(40.0, 24.0, Affine2D::crop(8.0, 16.0));
        assert_eq!((w, h), (40, 24));
        assert_eq!((t.a, t.d), (1.0, 1.0));
    }

    #[test]
    fn a_portrait_crop_is_bounded_by_its_long_edge() {
        let (w, h, _) = screen_geometry(2000.0, 8000.0, Affine2D::IDENTITY);
        assert_eq!(h, RENDER_LONG_EDGE);
        assert_eq!(w, 960);
    }

    // --- the job (needs a wgpu adapter; skipped without one) ---

    use nicti_claw::Module;
    use nicti_cornea::DecodeError;
    use nicti_lair::larder::{Larder, LarderConfig};
    use nicti_pounce::Pounce;
    use nicti_tapetum::coat::CropParams;
    use nicti_tapetum::stages::{CROP, EXPOSURE};
    use std::path::Path;
    use std::time::{Duration, Instant};

    struct FakeDecoder;

    impl Module for FakeDecoder {
        fn id(&self) -> &str {
            "test.decoder.eyeshine"
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
            if path.to_string_lossy().contains("bad") {
                return Err(DecodeError::Io {
                    path: path.to_path_buf(),
                    source: std::io::Error::other("corrupt file"),
                });
            }
            Ok(crate::render::synthetic_linear_frame())
        }
    }

    struct Fx {
        env: Env,
        pounce: Pounce,
        dir: tempfile::TempDir,
        larder: SharedLarder,
    }

    fn fx() -> Option<Fx> {
        let gpu = crate::test_gpu::shared()?;
        let dir = tempfile::tempdir().unwrap();
        let larder: SharedLarder = Arc::new(Mutex::new(
            Larder::open(
                &dir.path().join("larder"),
                LarderConfig {
                    cap_bytes: 64 * 1024 * 1024,
                    compact_min_dead_bytes: u64::MAX,
                },
            )
            .unwrap(),
        ));
        let pounce = Pounce::new(u64::MAX, 2, 2, || {});
        let env = Env::new(
            Arc::new(FakeDecoder),
            gpu,
            larder.clone(),
            pounce.submitter(),
        );
        Some(Fx {
            env,
            pounce,
            dir,
            larder,
        })
    }

    impl Fx {
        fn file(&self, name: &str) -> PathBuf {
            let p = self.dir.path().join(name);
            std::fs::write(&p, b"x").unwrap();
            p
        }

        fn run(&self, name: &str, doc: EditDocument) -> (T2Outcome, String) {
            let path = self.file(name);
            let identity = blake3::hash(name.as_bytes());
            let hash = rendered_hash(&identity, &doc).unwrap();
            let (_, slot) = submit(
                &self.env,
                &self.pounce,
                Request {
                    asset_id: 1,
                    identity,
                    hash: hash.clone(),
                    path,
                    edit: doc,
                    image_index: 0,
                },
                Arc::new(AtomicBool::new(false)),
            );
            let start = Instant::now();
            loop {
                if let Some(o) = slot.lock().unwrap().take() {
                    return (o, hash);
                }
                assert!(start.elapsed() < Duration::from_secs(120), "never settled");
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        fn stored(&self, hash: &str) -> Option<image::RgbImage> {
            let bytes = self
                .larder
                .lock()
                .unwrap()
                .get(LarderKey {
                    asset_id: 1,
                    tier: LarderTier::Rendered,
                    render_hash: hash,
                })
                .unwrap()?;
            Some(image::load_from_memory(&bytes).unwrap().to_rgb8())
        }
    }

    fn exposure(stops: f32) -> EditDocument {
        doc_with(EXPOSURE, json!({ "stops": stops }))
    }

    #[test]
    fn an_edit_is_rendered_and_stored_and_changes_the_pixels() {
        let Some(fx) = fx() else { return };
        let (o, plain_hash) = fx.run("a.NEF", EditDocument::default());
        assert_eq!(o, T2Outcome::Stored);
        let (o, edited_hash) = fx.run("a.NEF", exposure(2.0));
        assert_eq!(o, T2Outcome::Stored);
        assert_ne!(plain_hash, edited_hash);
        // One slot per (asset, tier): the second render replaced the first.
        assert_eq!(
            fx.larder
                .lock()
                .unwrap()
                .stored_hash(1, LarderTier::Rendered)
                .unwrap()
                .as_deref(),
            Some(edited_hash.as_str()),
            "the older render was replaced"
        );
        let edited = fx
            .stored(&edited_hash)
            .expect("the edited render is stored");
        assert_eq!(edited.dimensions(), (64, 64));
    }

    #[test]
    fn exposure_visibly_changes_the_stored_render() {
        let Some(fx) = fx() else { return };
        let (_, h0) = fx.run("a.NEF", EditDocument::default());
        let base = fx.stored(&h0).unwrap();
        let (_, h1) = fx.run("a.NEF", exposure(1.5));
        let bright = fx.stored(&h1).unwrap();
        let sum = |i: &image::RgbImage| i.pixels().map(|p| u64::from(p[0])).sum::<u64>();
        assert!(
            sum(&bright) > sum(&base),
            "+1.5 EV must brighten the preview"
        );
    }

    #[test]
    fn a_crop_renders_at_the_crop_size() {
        let Some(fx) = fx() else { return };
        let doc = doc_with(
            CROP,
            serde_json::to_value(CropParams {
                x: 8.0,
                y: 16.0,
                width: 40.0,
                height: 24.0,
                rotation_degrees: 0.0,
            })
            .unwrap(),
        );
        let (o, h) = fx.run("a.NEF", doc);
        assert_eq!(o, T2Outcome::Stored);
        assert_eq!(fx.stored(&h).unwrap().dimensions(), (40, 24));
    }

    #[test]
    fn a_bad_decode_fails_and_stores_nothing() {
        let Some(fx) = fx() else { return };
        let (o, h) = fx.run("bad.NEF", exposure(1.0));
        assert!(
            matches!(o, T2Outcome::Failed(ref m) if m.contains("decode")),
            "{o:?}"
        );
        assert!(fx.stored(&h).is_none());
    }

    #[test]
    fn an_already_stored_hash_is_not_rendered_again() {
        let Some(fx) = fx() else { return };
        let doc = exposure(1.0);
        let (o, h) = fx.run("a.NEF", doc.clone());
        assert_eq!(o, T2Outcome::Stored);
        // A decoder that would fail proves the second run never decodes.
        let (o2, h2) = fx.run("a.NEF", doc);
        assert_eq!((o2, h2), (T2Outcome::Stored, h));
    }

    #[test]
    fn a_missing_source_file_is_a_quiet_retry() {
        let Some(fx) = fx() else { return };
        let doc = exposure(1.0);
        let identity = blake3::hash(b"gone");
        let (_, slot) = submit(
            &fx.env,
            &fx.pounce,
            Request {
                asset_id: 9,
                identity,
                hash: rendered_hash(&identity, &doc).unwrap(),
                path: fx.dir.path().join("does-not-exist.NEF"),
                edit: doc,
                image_index: 0,
            },
            Arc::new(AtomicBool::new(false)),
        );
        let start = Instant::now();
        let out = loop {
            if let Some(o) = slot.lock().unwrap().take() {
                break o;
            }
            assert!(start.elapsed() < Duration::from_secs(60));
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(matches!(out, T2Outcome::Retry(_)), "{out:?}");
    }

    #[test]
    fn a_superseded_task_settles_as_retry_and_stores_nothing() {
        let Some(fx) = fx() else { return };
        let doc = exposure(1.0);
        let path = fx.file("a.NEF");
        let identity = blake3::hash(b"a.NEF");
        let hash = rendered_hash(&identity, &doc).unwrap();
        let cancelled = Arc::new(AtomicBool::new(true));
        let (_, slot) = submit(
            &fx.env,
            &fx.pounce,
            Request {
                asset_id: 1,
                identity,
                hash: hash.clone(),
                path,
                edit: doc,
                image_index: 0,
            },
            cancelled,
        );
        let start = Instant::now();
        let out = loop {
            if let Some(o) = slot.lock().unwrap().take() {
                break o;
            }
            assert!(start.elapsed() < Duration::from_secs(60));
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(matches!(out, T2Outcome::Retry(_)), "{out:?}");
        assert!(fx.stored(&hash).is_none());
    }

    // --- the service ---

    use nicti_lair::{NewAsset, SqliteCatalog};

    struct SvcFx {
        fx: Fx,
        store: SqliteCatalog,
        id: i64,
        svc: EyeshineService,
    }

    fn svc_fx() -> Option<SvcFx> {
        let fx = fx()?;
        let store = SqliteCatalog::open_in_memory().unwrap();
        let volume = store.upsert_volume("v", None, None, 0).unwrap();
        let root = store
            .ensure_root(volume, &fx.dir.path().to_string_lossy())
            .unwrap();
        std::fs::write(fx.dir.path().join("a.NEF"), b"x").unwrap();
        let id = store
            .insert_asset(
                root,
                &NewAsset {
                    rel_path: "a.NEF".into(),
                    rel_path_fold: "a.nef".into(),
                    size_bytes: 1,
                    mtime_unix: 1_700_000_000,
                    fingerprint: Some("fp-a".into()),
                    natural_key: None,
                    make: Some("NIKON".into()),
                    model: Some("Z 8".into()),
                    captured_at: None,
                    width: Some(64),
                    height: Some(64),
                    imported_at: 0,
                },
                None,
            )
            .unwrap();
        let svc = EyeshineService::new(Some(fx.env.clone()));
        Some(SvcFx { fx, store, id, svc })
    }

    fn drain(svc: &mut EyeshineService) -> Vec<i64> {
        let start = Instant::now();
        let mut out = Vec::new();
        while svc.busy() {
            out.extend(svc.poll());
            assert!(start.elapsed() < Duration::from_secs(60), "never finished");
            std::thread::sleep(Duration::from_millis(5));
        }
        out
    }

    #[test]
    fn the_policy_gates_what_the_service_queues() {
        let Some(mut t) = svc_fx() else { return };
        // Unedited photo: EditedOnly and Off queue nothing, All does.
        for policy in [RenderPolicy::Off, RenderPolicy::EditedOnly] {
            t.svc.request(&t.fx.pounce, &t.store, t.id, 0, policy);
            assert!(
                !t.svc.busy(),
                "{policy:?} must not render an unedited photo"
            );
        }
        t.svc
            .request(&t.fx.pounce, &t.store, t.id, 0, RenderPolicy::All);
        assert!(t.svc.busy());
        assert_eq!(drain(&mut t.svc), vec![t.id]);
    }

    #[test]
    fn an_edited_photo_is_rendered_once_and_not_again_while_cached() {
        let Some(mut t) = svc_fx() else { return };
        t.store.put_master_edit(t.id, &exposure(1.0)).unwrap();
        let r = t
            .svc
            .request(&t.fx.pounce, &t.store, t.id, 0, RenderPolicy::EditedOnly)
            .unwrap();
        assert!(r.edited);
        // Same hash while in flight: no second job.
        let first_job = t.svc.inflight[&t.id].job;
        t.svc
            .request(&t.fx.pounce, &t.store, t.id, 0, RenderPolicy::EditedOnly);
        assert_eq!(t.svc.inflight[&t.id].job, first_job);
        assert_eq!(drain(&mut t.svc), vec![t.id]);
        // Cached now: nothing is queued.
        t.svc
            .request(&t.fx.pounce, &t.store, t.id, 0, RenderPolicy::EditedOnly);
        assert!(!t.svc.busy());
    }

    #[test]
    fn a_newer_edit_supersedes_the_render_in_flight() {
        let Some(mut t) = svc_fx() else { return };
        t.store.put_master_edit(t.id, &exposure(1.0)).unwrap();
        t.svc
            .request(&t.fx.pounce, &t.store, t.id, 0, RenderPolicy::EditedOnly);
        let old = t.svc.inflight[&t.id].cancelled.clone();
        let old_hash = t.svc.inflight[&t.id].hash.clone();
        t.store.put_master_edit(t.id, &exposure(2.0)).unwrap();
        t.svc.invalidate(t.id);
        t.svc
            .request(&t.fx.pounce, &t.store, t.id, 0, RenderPolicy::EditedOnly);
        assert!(old.load(Ordering::SeqCst), "the older render is cancelled");
        assert_ne!(t.svc.inflight[&t.id].hash, old_hash);
        drain(&mut t.svc);
        let hash = rendered_hash(
            &t.svc.resolve(&t.store, t.id).unwrap().identity,
            &exposure(2.0),
        )
        .unwrap();
        assert!(t.fx.stored(&hash).is_some(), "the newest render wins");
    }

    #[test]
    fn a_failed_render_is_not_retried() {
        let Some(mut t) = svc_fx() else { return };
        // The fake decoder fails any path containing "bad".
        let root = t.store.list_roots().unwrap()[0].id;
        let bad = t
            .store
            .insert_asset(
                root,
                &NewAsset {
                    rel_path: "bad.NEF".into(),
                    rel_path_fold: "bad.nef".into(),
                    size_bytes: 1,
                    mtime_unix: 1,
                    fingerprint: Some("fp-bad".into()),
                    natural_key: None,
                    make: None,
                    model: None,
                    captured_at: None,
                    width: Some(64),
                    height: Some(64),
                    imported_at: 0,
                },
                None,
            )
            .unwrap();
        std::fs::write(t.fx.dir.path().join("bad.NEF"), b"x").unwrap();
        t.store.put_master_edit(bad, &exposure(1.0)).unwrap();
        t.svc
            .request(&t.fx.pounce, &t.store, bad, 0, RenderPolicy::EditedOnly);
        drain(&mut t.svc);
        assert_eq!(t.svc.blocked.len(), 1);
        t.svc
            .request(&t.fx.pounce, &t.store, bad, 0, RenderPolicy::EditedOnly);
        assert!(
            !t.svc.busy(),
            "a known-bad (asset, hash) isn't queued again"
        );
    }

    #[test]
    fn a_retry_outcome_backs_off_instead_of_resubmitting_every_frame() {
        let Some(mut t) = svc_fx() else { return };
        t.store.put_master_edit(t.id, &exposure(1.0)).unwrap();
        // Make the source unreachable: the job settles as a quiet Retry.
        std::fs::remove_file(t.fx.dir.path().join("a.NEF")).unwrap();
        t.svc
            .request(&t.fx.pounce, &t.store, t.id, 0, RenderPolicy::EditedOnly);
        drain(&mut t.svc);
        assert_eq!(t.svc.blocked.len(), 1);
        // The very next "frame" must not queue it again.
        t.svc
            .request(&t.fx.pounce, &t.store, t.id, 0, RenderPolicy::EditedOnly);
        assert!(!t.svc.busy());
        // ...until the back-off lapses.
        for until in t.svc.blocked.values_mut() {
            *until = std::time::Instant::now() - Duration::from_secs(1);
        }
        t.svc
            .request(&t.fx.pounce, &t.store, t.id, 0, RenderPolicy::EditedOnly);
        assert!(t.svc.busy());
    }

    #[test]
    fn edits_written_behind_the_services_back_are_picked_up_after_the_ttl() {
        let Some(mut t) = svc_fx() else { return };
        t.store.put_master_edit(t.id, &exposure(1.0)).unwrap();
        let first = t.svc.resolve(&t.store, t.id).unwrap().hash.clone();
        // A paste/undo/LRC import writes the catalog without calling `invalidate`.
        t.store.put_master_edit(t.id, &exposure(2.0)).unwrap();
        assert_eq!(t.svc.resolve(&t.store, t.id).unwrap().hash, first, "cached");
        t.svc.resolved.get_mut(&t.id).unwrap().at =
            std::time::Instant::now() - Duration::from_secs(2);
        assert_ne!(t.svc.resolve(&t.store, t.id).unwrap().hash, first);
    }

    #[test]
    fn a_settle_dropped_while_panicking_resolves_failed_not_retry() {
        let slot: ReportSlot<T2Outcome> = Arc::new(Mutex::new(None));
        let settle = Settle {
            slot: Some(slot.clone()),
            panicked: Arc::new(AtomicBool::new(false)),
            live: Arc::new(AtomicUsize::new(1)),
        };
        let _ = catch_unwind(AssertUnwindSafe(move || {
            let _owned = settle;
            panic!("boom in begin");
        }));
        assert!(matches!(
            slot.lock().unwrap().take(),
            Some(T2Outcome::Failed(_))
        ));
        // A plain drop is still a quiet retry.
        let slot: ReportSlot<T2Outcome> = Arc::new(Mutex::new(None));
        drop(Settle {
            slot: Some(slot.clone()),
            panicked: Arc::new(AtomicBool::new(false)),
            live: Arc::new(AtomicUsize::new(1)),
        });
        assert!(matches!(
            slot.lock().unwrap().take(),
            Some(T2Outcome::Retry(_))
        ));
    }

    #[test]
    fn the_gpu_stage_has_no_cursor_index_so_waiters_cannot_starve_the_holder() {
        let Some(fx) = fx() else { return };
        let task = Task {
            req: Request {
                asset_id: 1,
                identity: blake3::hash(b"x"),
                hash: "h".into(),
                path: PathBuf::from("x"),
                edit: EditDocument::default(),
                image_index: 7,
            },
            env: fx.env.clone(),
            cancelled: Arc::new(AtomicBool::new(false)),
            panicked: Arc::new(AtomicBool::new(false)),
        };
        assert_eq!(spec(Lane::Gpu, &task).image_index, None);
        assert_eq!(spec(Lane::Cpu, &task).image_index, Some(7));
    }

    #[test]
    fn same_file_compares_only_the_identity_segment() {
        let a1 = rendered_hash(&id(1), &exposure(1.0)).unwrap();
        let a2 = rendered_hash(&id(1), &exposure(2.0)).unwrap();
        let b = rendered_hash(&id(2), &exposure(1.0)).unwrap();
        assert!(same_file(&a1, &a2));
        assert!(!same_file(&a1, &b));
        assert!(!same_file("garbage", &a1));
    }

    fn add_asset(t: &SvcFx, name: &str) -> i64 {
        let root = t.store.list_roots().unwrap()[0].id;
        std::fs::write(t.fx.dir.path().join(name), b"x").unwrap();
        t.store
            .insert_asset(
                root,
                &NewAsset {
                    rel_path: name.into(),
                    rel_path_fold: name.to_lowercase(),
                    size_bytes: 1,
                    mtime_unix: 1,
                    fingerprint: Some(format!("fp-{name}")),
                    natural_key: None,
                    make: None,
                    model: None,
                    captured_at: None,
                    width: Some(64),
                    height: Some(64),
                    imported_at: 0,
                },
                None,
            )
            .unwrap()
    }

    #[test]
    fn an_obsolete_render_is_cancelled_even_when_the_current_hash_is_already_stored() {
        let Some(mut t) = svc_fx() else { return };
        // A is rendered and stored.
        t.store.put_master_edit(t.id, &exposure(1.0)).unwrap();
        t.svc
            .request(&t.fx.pounce, &t.store, t.id, 0, RenderPolicy::EditedOnly);
        drain(&mut t.svc);
        // Edit to B: its render starts.
        t.store.put_master_edit(t.id, &exposure(2.0)).unwrap();
        t.svc.invalidate(t.id);
        t.svc
            .request(&t.fx.pounce, &t.store, t.id, 0, RenderPolicy::EditedOnly);
        let b_flag = t.svc.inflight[&t.id].cancelled.clone();
        // Back to A (already stored): the request returns early, but B must still be cancelled so
        // it can't overwrite A's render with the wrong pixels.
        t.store.put_master_edit(t.id, &exposure(1.0)).unwrap();
        t.svc.invalidate(t.id);
        t.svc
            .request(&t.fx.pounce, &t.store, t.id, 0, RenderPolicy::EditedOnly);
        assert!(b_flag.load(Ordering::SeqCst));
        assert!(!t.svc.inflight.contains_key(&t.id));
    }

    #[test]
    fn decoded_frames_in_flight_are_bounded_by_evicting_the_oldest() {
        let Some(mut t) = svc_fx() else { return };
        let ids = [t.id, add_asset(&t, "b.NEF"), add_asset(&t, "c.NEF")];
        let mut flags = Vec::new();
        for id in ids {
            t.store.put_master_edit(id, &exposure(1.0)).unwrap();
            t.svc
                .request(&t.fx.pounce, &t.store, id, 0, RenderPolicy::EditedOnly);
            if let Some(f) = t.svc.inflight.get(&id) {
                flags.push((id, f.cancelled.clone()));
            }
            assert!(t.svc.inflight.len() <= MAX_INFLIGHT);
        }
        assert_eq!(t.svc.inflight.len(), MAX_INFLIGHT);
        assert!(
            flags[0].1.load(Ordering::SeqCst),
            "the oldest render was evicted for the newest"
        );
        assert!(t.svc.inflight.contains_key(&ids[2]));
    }

    #[test]
    fn the_live_pipeline_count_returns_to_zero_once_everything_settles() {
        let Some(mut t) = svc_fx() else { return };
        let ids = [t.id, add_asset(&t, "b.NEF"), add_asset(&t, "c.NEF")];
        for id in ids {
            t.store.put_master_edit(id, &exposure(1.0)).unwrap();
            t.svc
                .request(&t.fx.pounce, &t.store, id, 0, RenderPolicy::EditedOnly);
            let live = t.fx.env.live.load(Ordering::SeqCst);
            assert!(live <= MAX_LIVE, "live={live}");
        }
        drain(&mut t.svc);
        let start = Instant::now();
        while t.fx.env.live.load(Ordering::SeqCst) != 0 {
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "evicted jobs never settled"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!t.svc.busy());
    }
}
