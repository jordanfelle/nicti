//! `GridSession` (#30): the library grid's state -- an ordered id snapshot, a byte-budgeted cache
//! of GPU thumbnails, and the Pounce jobs that fill it. The UI (`grid::show`) asks it for the
//! visible window each frame; everything else (batching, cancelling what scrolled away,
//! discarding stale results, capping texture uploads per frame) lives here so it's testable
//! without a window.
//!
//! Memory at 1M assets: the id snapshot is 8 MB; textures are bounded by `texture_budget_bytes`
//! regardless of catalog size; nothing else scales with the catalog.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{TextureHandle, TextureOptions};
use nicti_lair::pounce_jobs::ReportSlot;
use nicti_lair::{CatalogStore, Filter, Sort, SortDirection, SortField};
use nicti_pounce::{JobId, JobKind, Pounce};
use nicti_tapetum::cache::Tier;

use super::jobs::{SnapshotJob, SnapshotResult, ThumbBatchJob, ThumbError, ThumbImage, ThumbSlot};
use super::layout::{batch_indices, batches_for};

/// How long a cell whose thumbnail hit a *transient* catalog error waits before being asked for
/// again -- long enough that a persistent error can't turn `request_visible` (called every frame)
/// into a hot resubmit loop.
const TRANSIENT_RETRY_AFTER: Duration = Duration::from_secs(5);

/// Batches requested beyond the visible ones on each side, so a normal scroll finds thumbnails
/// already decoded.
const OVERSCAN_BATCHES: usize = 2;
/// In-flight batches further than this many batches from the visible window are cancelled: the
/// user scrolled past them and they'd only delay the batches that matter.
const CANCEL_MARGIN_BATCHES: usize = 4;
/// Decoded thumbnails turned into GPU textures per frame. A fast scroll can finish dozens at
/// once; uploading them all in one frame is what would drop it below 60 fps.
const MAX_UPLOADS_PER_FRAME: usize = 32;

pub const DEFAULT_SORT: Sort = Sort {
    field: SortField::Captured,
    direction: SortDirection::Asc,
};

/// At most one snapshot is ever pending: `request_snapshot` cancels and drops the previous
/// job's slot before submitting the next, so a slower, older snapshot (an earlier sort or filter)
/// can never land after -- and overwrite -- a newer one; nothing holds its slot to apply it.
struct PendingSnapshot {
    job_id: JobId,
    slot: ReportSlot<SnapshotResult>,
}

struct InflightBatch {
    job_id: JobId,
    slot: ThumbSlot,
}

pub struct GridSession {
    store: Arc<dyn CatalogStore + Send + Sync>,
    filter: Filter,
    sort: Sort,
    ids: Vec<i64>,
    /// `true` once a snapshot has ever been applied, so the view can tell "still loading" from
    /// "loaded, and empty" -- and stays `false` if the first read failed.
    loaded: bool,
    /// Whether a snapshot for the current filter/sort has ever been requested. Gates
    /// `set_query`'s no-op, so a *failed* first snapshot isn't resubmitted every frame the
    /// toolbar calls it; the view's Retry button is the way to ask again.
    requested: bool,
    pending_snapshot: Option<PendingSnapshot>,
    last_error: Option<String>,
    textures: Tier<TextureHandle>,
    /// Assets whose thumbnail can never render (no stored preview, corrupt JPEG) -- not
    /// re-requested until [`Self::refresh`], so such a cell isn't re-read on every scroll.
    failed: HashSet<i64>,
    /// Assets whose thumbnail hit a transient catalog error, and when: skipped until
    /// [`TRANSIENT_RETRY_AFTER`] has passed, then asked for again.
    transient_failed: HashMap<i64, Instant>,
    inflight: HashMap<usize, InflightBatch>,
    pending_uploads: VecDeque<(i64, ThumbImage)>,
    /// The ids in `pending_uploads`: decoded but not yet textured (uploads are capped per frame).
    /// `needs_thumbnail` must skip them, or `request_visible` -- which runs after `poll` in the
    /// same frame, once the batch has left `inflight` -- resubmits work whose result is already
    /// sitting in the queue.
    pending_ids: HashSet<i64>,
    /// The selection, tracked by asset id (`cursor_id`) with its current index (`cursor`) as a
    /// cache: a snapshot change (an import adding assets, a new sort) moves indices, and the
    /// selection must stay on the same photo, not the same slot.
    cursor: Option<usize>,
    cursor_id: Option<i64>,
    last_window: Option<Range<usize>>,
}

/// The texture cache key for an asset. Keyed by id alone: a re-ingest can change an asset's
/// preview, which is why [`GridSession::refresh`] drops the whole cache rather than trying to
/// track per-asset revisions the grid never reads.
fn texture_key(asset_id: i64) -> blake3::Hash {
    blake3::hash(&asset_id.to_le_bytes())
}

impl GridSession {
    pub fn new(store: Arc<dyn CatalogStore + Send + Sync>, texture_budget_bytes: u64) -> Self {
        Self {
            store,
            filter: Filter::default(),
            sort: DEFAULT_SORT,
            ids: Vec::new(),
            loaded: false,
            requested: false,
            pending_snapshot: None,
            last_error: None,
            textures: Tier::new(texture_budget_bytes, |t: &TextureHandle| {
                let [w, h] = t.size();
                (w * h * 4) as u64
            }),
            failed: HashSet::new(),
            transient_failed: HashMap::new(),
            inflight: HashMap::new(),
            pending_uploads: VecDeque::new(),
            pending_ids: HashSet::new(),
            cursor: None,
            cursor_id: None,
            last_window: None,
        }
    }

    pub fn ids(&self) -> &[i64] {
        &self.ids
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    pub fn is_loading(&self) -> bool {
        self.pending_snapshot.is_some()
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    pub fn cursor(&self) -> Option<usize> {
        self.cursor
    }

    /// The selected asset's id -- what actually identifies the selection across snapshot changes.
    pub fn cursor_id(&self) -> Option<i64> {
        self.cursor_id
    }

    /// Selects by grid index (clamped); `None` clears the selection.
    pub fn set_cursor(&mut self, cursor: Option<usize>) {
        let index = cursor
            .filter(|_| !self.ids.is_empty())
            .map(|c| c.min(self.ids.len() - 1));
        self.cursor = index;
        self.cursor_id = index.map(|i| self.ids[i]);
    }

    /// Selects `asset_id` wherever it is in the current snapshot -- for mirroring a selection made
    /// elsewhere (the loupe, whose own id list is frozen at open time and so can't be trusted to
    /// share indices with this one). An id the snapshot doesn't hold is still remembered: the
    /// selection resolves as soon as a snapshot that has it lands. A no-op when already selected,
    /// so calling it every frame doesn't rescan the id list.
    pub fn select_asset(&mut self, asset_id: i64) {
        if self.cursor_id == Some(asset_id) {
            return;
        }
        self.cursor_id = Some(asset_id);
        self.cursor = self.ids.iter().position(|&id| id == asset_id);
    }

    /// Changes what the grid shows. A no-op if nothing changed (and it has been requested at
    /// least once); otherwise reads a fresh id snapshot in the background. The previous ids stay
    /// on screen (and keep their thumbnails) until the new snapshot lands, so a sort change never
    /// blanks the grid.
    pub fn set_query(&mut self, filter: Filter, sort: Sort, pounce: &Pounce) {
        if self.requested && filter == self.filter && sort == self.sort {
            return;
        }
        self.filter = filter;
        self.sort = sort;
        self.request_snapshot(pounce);
    }

    /// Re-reads the id snapshot only, keeping cached thumbnails -- for picking up newly imported
    /// assets while an import is still running. A no-op while a snapshot is already being read, so
    /// a periodic caller can't stack them up.
    pub fn reload(&mut self, pounce: &Pounce) {
        if self.pending_snapshot.is_none() {
            self.request_snapshot(pounce);
        }
    }

    /// Re-reads the snapshot and forgets every cached thumbnail -- for after ingest, sync, or a
    /// folder move, which can add assets or replace previews.
    pub fn refresh(&mut self, pounce: &Pounce) {
        self.textures = Tier::new(self.textures.stats().budget_bytes, |t: &TextureHandle| {
            let [w, h] = t.size();
            (w * h * 4) as u64
        });
        self.failed.clear();
        self.transient_failed.clear();
        self.pending_uploads.clear();
        self.pending_ids.clear();
        // Batches already running were decoding the *old* previews; letting them finish would
        // push those results into the fresh cache. `request_visible` re-queues what's on screen.
        self.cancel_batches(pounce);
        self.request_snapshot(pounce);
    }

    fn request_snapshot(&mut self, pounce: &Pounce) {
        self.requested = true;
        if let Some(old) = self.pending_snapshot.take() {
            pounce.cancel(old.job_id);
        }
        let (job, slot) = SnapshotJob::new(self.store.clone(), self.filter.clone(), self.sort);
        let job_id = pounce.submit(Box::new(job));
        self.pending_snapshot = Some(PendingSnapshot { job_id, slot });
    }

    /// Cancels every in-flight batch and forgets them. Batch indices refer to positions in the
    /// *current* snapshot, so they're meaningless once it changes.
    fn cancel_batches(&mut self, pounce: &Pounce) {
        for batch in self.inflight.values() {
            pounce.cancel(batch.job_id);
        }
        self.inflight.clear();
        self.last_window = None;
    }

    /// Folds finished work into the session: applies a landed snapshot, collects decoded
    /// thumbnails, and uploads up to [`MAX_UPLOADS_PER_FRAME`] of them as textures. Call once per
    /// frame before drawing. Requests a repaint itself while uploads remain queued.
    pub fn poll(&mut self, ctx: &egui::Context, pounce: &Pounce) {
        self.poll_snapshot(pounce);
        self.poll_batches();
        self.upload(ctx);
    }

    fn poll_snapshot(&mut self, pounce: &Pounce) {
        let Some(pending) = &self.pending_snapshot else {
            return;
        };
        let Some(result) = pending.slot.lock().unwrap().take() else {
            return;
        };
        self.pending_snapshot = None;
        match result {
            Ok(ids) => {
                self.last_error = None;
                // A periodic reload usually finds nothing new. Leave everything alone then:
                // cancelling the in-flight batches would throw away the on-screen cells' work
                // every couple of seconds for an import's whole duration.
                if self.loaded && ids == self.ids {
                    return;
                }
                self.cancel_batches(pounce);
                self.ids = ids;
                self.loaded = true;
                // Keep the selection on the same photo, wherever it landed (or drop it if the
                // new query no longer contains it).
                self.cursor = self
                    .cursor_id
                    .and_then(|id| self.ids.iter().position(|&x| x == id));
            }
            Err(msg) => {
                // Keep showing the previous ids (if any) and surface why the query didn't apply.
                // `loaded` is deliberately untouched: a first read that failed is not "loaded,
                // and empty", and the view offers Retry instead.
                self.last_error = Some(msg);
            }
        }
    }

    fn poll_batches(&mut self) {
        let mut finished = Vec::new();
        for (&batch, inflight) in &self.inflight {
            let mut out = inflight.slot.lock().unwrap();
            for (id, outcome) in out.ready.drain(..) {
                match outcome {
                    Ok(thumb) => {
                        self.pending_ids.insert(id);
                        self.pending_uploads.push_back((id, thumb));
                    }
                    Err(ThumbError::Permanent(_)) => {
                        self.failed.insert(id);
                    }
                    Err(ThumbError::Transient(_)) => {
                        self.transient_failed.insert(id, Instant::now());
                    }
                }
            }
            if out.done {
                finished.push(batch);
            }
        }
        for batch in finished {
            self.inflight.remove(&batch);
        }
    }

    fn upload(&mut self, ctx: &egui::Context) {
        for _ in 0..MAX_UPLOADS_PER_FRAME {
            let Some((id, thumb)) = self.pending_uploads.pop_front() else {
                return;
            };
            self.pending_ids.remove(&id);
            let handle = ctx.load_texture(
                format!("grid-thumb-{id}"),
                thumb.image,
                TextureOptions::LINEAR,
            );
            self.textures.put(texture_key(id), handle);
        }
        if !self.pending_uploads.is_empty() {
            ctx.request_repaint();
        }
    }

    /// The cached texture for `asset_id`, marking it recently used.
    pub fn texture(&mut self, asset_id: i64) -> Option<&TextureHandle> {
        self.textures.get(&texture_key(asset_id))
    }

    pub fn has_failed(&self, asset_id: i64) -> bool {
        self.failed.contains(&asset_id)
    }

    /// Whether a thumbnail for `id` should be requested now: not already textured, not
    /// permanently failed, and not inside a transient error's retry back-off.
    fn needs_thumbnail(&self, id: i64) -> bool {
        !self.failed.contains(&id)
            && !self.pending_ids.contains(&id)
            && !self
                .transient_failed
                .get(&id)
                .is_some_and(|at| at.elapsed() < TRANSIENT_RETRY_AFTER)
            && !self.textures.contains(&texture_key(id))
    }

    /// Makes sure thumbnails are being produced for the ids in `visible` (an index range into the
    /// snapshot): queues a batch for every nearby block that still has ids without a texture,
    /// cancels batches the user scrolled well past, and -- only when the window actually moved --
    /// reorders the queue nearest-first. Call every frame; it's cheap when nothing changed.
    pub fn request_visible(&mut self, visible: Range<usize>, pounce: &Pounce) {
        let total = self.ids.len();
        let wanted = batches_for(visible.clone(), total, OVERSCAN_BATCHES);
        if wanted.is_empty() {
            return;
        }

        // Cancel what's far behind us.
        let keep = wanted.start.saturating_sub(CANCEL_MARGIN_BATCHES)
            ..wanted.end.saturating_add(CANCEL_MARGIN_BATCHES);
        let stale: Vec<usize> = self
            .inflight
            .keys()
            .copied()
            .filter(|b| !keep.contains(b))
            .collect();
        for batch in stale {
            if let Some(inflight) = self.inflight.remove(&batch) {
                pounce.cancel(inflight.job_id);
            }
        }

        // Queue what's missing, nearest the middle of the window first so the cells the user is
        // looking at fill before the overscan does.
        let centre = (visible.start + visible.end) / 2;
        let mut order: Vec<usize> = wanted.clone().collect();
        order.sort_by_key(|&b| batch_indices(b, total).start.abs_diff(centre));
        for batch in order {
            if self.inflight.contains_key(&batch) {
                continue;
            }
            let range = batch_indices(batch, total);
            let first_index = range.start;
            let missing: Vec<i64> = self.ids[range]
                .iter()
                .copied()
                .filter(|&id| self.needs_thumbnail(id))
                .collect();
            if missing.is_empty() {
                continue;
            }
            let (job, slot) = ThumbBatchJob::new(self.store.clone(), missing, first_index);
            let job_id = pounce.submit(Box::new(job));
            self.inflight.insert(batch, InflightBatch { job_id, slot });
        }

        if self.last_window.as_ref() != Some(&visible) {
            self.last_window = Some(visible);
            pounce.reprioritize(move |spec| {
                // The id snapshot has no grid position and would otherwise sort as `usize::MAX`,
                // behind every thumbnail batch -- but every batch's index is only meaningful
                // against the snapshot it's waiting on, so it goes first.
                if spec.kind == JobKind::Snapshot {
                    return 0;
                }
                spec.image_index
                    .map(|i| i.abs_diff(centre))
                    .unwrap_or(usize::MAX)
            });
        }
    }

    /// Cancels in-flight thumbnail batches without touching the snapshot -- for when the grid
    /// isn't the view on screen, so a hidden grid doesn't keep decoding. Cheap to call every
    /// frame: nothing in flight means nothing to do. Coming back, `request_visible` re-queues
    /// whatever the visible window is still missing.
    pub fn pause(&mut self, pounce: &Pounce) {
        if !self.inflight.is_empty() {
            self.cancel_batches(pounce);
        }
    }

    #[cfg(test)]
    pub fn inflight_batches(&self) -> usize {
        self.inflight.len()
    }

    #[cfg(test)]
    pub fn texture_stats(&self) -> nicti_tapetum::cache::TierStats {
        self.textures.stats()
    }
}

#[cfg(test)]
mod tests {
    use super::super::jobs::testutil::jpeg;
    use super::*;
    use nicti_lair::{NewAsset, Preview, SqliteCatalog};
    use std::time::{Duration, Instant};

    fn new_asset(name: &str, imported_at: i64) -> NewAsset {
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
            imported_at,
        }
    }

    /// `n` assets in one root; asset `i` has a preview unless `no_preview(i)`.
    fn seeded(n: usize, no_preview: impl Fn(usize) -> bool) -> (Arc<SqliteCatalog>, Vec<i64>) {
        let store = Arc::new(SqliteCatalog::open_in_memory().unwrap());
        let volume = store.upsert_volume("v", None, None, 0).unwrap();
        let root = store.ensure_root(volume, "").unwrap();
        let blob = jpeg(64, 48, [30, 120, 200]);
        let ids = (0..n)
            .map(|i| {
                let preview = (!no_preview(i)).then(|| Preview {
                    width: Some(64),
                    height: Some(48),
                    bytes: blob.clone(),
                });
                store
                    .insert_asset(
                        root,
                        &new_asset(&format!("{i:05}.NEF"), i as i64),
                        preview.as_ref(),
                    )
                    .unwrap()
            })
            .collect();
        (store, ids)
    }

    fn sort_imported() -> Sort {
        Sort {
            field: SortField::Imported,
            direction: SortDirection::Asc,
        }
    }

    fn pounce() -> Pounce {
        Pounce::new(0, 2, 2, || {})
    }

    /// Polls `cond` (driving `session.poll` each round) until it holds or a deadline passes.
    fn wait_for(
        session: &mut GridSession,
        ctx: &egui::Context,
        pounce: &Pounce,
        mut cond: impl FnMut(&mut GridSession) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            session.poll(ctx, pounce);
            if cond(session) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for grid state"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn loaded_session(
        n: usize,
        budget: u64,
        no_preview: impl Fn(usize) -> bool,
    ) -> (GridSession, Vec<i64>, egui::Context, Pounce) {
        let (store, ids) = seeded(n, no_preview);
        let ctx = egui::Context::default();
        let pounce = pounce();
        let mut session = GridSession::new(store, budget);
        session.set_query(Filter::default(), sort_imported(), &pounce);
        wait_for(&mut session, &ctx, &pounce, |s| s.is_loaded());
        (session, ids, ctx, pounce)
    }

    #[test]
    fn the_snapshot_lands_in_catalog_order_without_blocking() {
        let (session, ids, _ctx, _pounce) = loaded_session(10, 1 << 20, |_| false);
        assert_eq!(session.ids(), ids.as_slice());
        assert_eq!(session.len(), 10);
        assert!(!session.is_loading());
    }

    #[test]
    fn visible_cells_get_textures_and_unrequested_ones_do_not() {
        let (mut session, ids, ctx, pounce) = loaded_session(1000, 64 << 20, |_| false);
        session.request_visible(0..20, &pounce);
        wait_for(&mut session, &ctx, &pounce, |s| s.texture(ids[0]).is_some());
        wait_for(&mut session, &ctx, &pounce, |s| s.inflight_batches() == 0);
        assert!(session.texture(ids[19]).is_some());
        // Far outside the window plus overscan: never requested.
        assert!(session.texture(ids[900]).is_none());
        assert!(!session.has_failed(ids[900]));
    }

    #[test]
    fn a_cell_with_no_preview_is_marked_failed_and_not_retried() {
        let (mut session, ids, ctx, pounce) = loaded_session(10, 1 << 20, |i| i == 3);
        session.request_visible(0..10, &pounce);
        wait_for(&mut session, &ctx, &pounce, |s| s.inflight_batches() == 0);
        assert!(session.has_failed(ids[3]));
        assert!(session.texture(ids[3]).is_none());
        assert!(session.texture(ids[4]).is_some());

        // Nothing left to fetch: a second request queues no new job.
        session.request_visible(0..10, &pounce);
        assert_eq!(session.inflight_batches(), 0);
    }

    #[test]
    fn scrolling_far_away_cancels_the_batches_left_behind() {
        let (mut session, _ids, _ctx, pounce) = loaded_session(20_000, 64 << 20, |_| false);
        session.request_visible(0..64, &pounce);
        let near_top = session.inflight_batches();
        assert!(near_top > 0);
        // Jump ~250 batches down: everything queued near the top is now outside the keep window.
        session.request_visible(16_000..16_064, &pounce);
        for batch in 0..=(OVERSCAN_BATCHES + CANCEL_MARGIN_BATCHES + 1) {
            assert!(
                !session.inflight.contains_key(&batch),
                "batch {batch} near the old window should have been cancelled"
            );
        }
        assert!(session.inflight.keys().all(|b| *b >= 16_000 / 64 - 8));
    }

    #[test]
    fn the_texture_cache_stays_within_its_budget_however_far_the_user_scrolls() {
        // A 64x48 thumbnail is 12,288 bytes; budget for ~100 of them, scroll over 1,000 cells.
        let budget = 100 * 64 * 48 * 4;
        let (mut session, ids, ctx, pounce) = loaded_session(1000, budget, |_| false);
        for start in (0..1000).step_by(64) {
            session.request_visible(start..(start + 64).min(1000), &pounce);
            wait_for(&mut session, &ctx, &pounce, |s| s.inflight_batches() == 0);
            wait_for(&mut session, &ctx, &pounce, |s| {
                s.pending_uploads.is_empty()
            });
            let stats = session.texture_stats();
            assert!(
                stats.used_bytes <= stats.budget_bytes,
                "{} > {} after scrolling to {start}",
                stats.used_bytes,
                stats.budget_bytes
            );
        }
        // The oldest cells were evicted; the newest are still there.
        assert!(session.texture(ids[0]).is_none());
        assert!(session.texture(ids[999]).is_some());
    }

    #[test]
    fn uploads_are_capped_per_frame() {
        let (mut session, ids, ctx, pounce) = loaded_session(500, 64 << 20, |_| false);
        session.request_visible(0..64, &pounce);
        // Let every batch finish decoding without uploading (poll_batches only).
        let deadline = Instant::now() + Duration::from_secs(30);
        while session.inflight_batches() > 0 {
            session.poll_batches();
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        let decoded = session.pending_uploads.len();
        assert!(
            decoded > MAX_UPLOADS_PER_FRAME,
            "need a backlog to test the cap"
        );

        // Decoded-but-not-yet-textured cells are already handled: asking for the visible window
        // again must not resubmit their batches (CodeRabbit finding: the batch has left
        // `inflight`, and without `pending_ids` every one of these ids looked untouched).
        assert_eq!(session.pending_ids.len(), decoded);
        assert!(!session.needs_thumbnail(ids[0]));
        assert!(!session.needs_thumbnail(*session.pending_ids.iter().next().unwrap()));
        session.request_visible(0..64, &pounce);
        assert_eq!(
            session.inflight_batches(),
            0,
            "cells already decoded and awaiting upload were queued for decoding a second time"
        );

        session.upload(&ctx);
        assert_eq!(
            session.pending_uploads.len(),
            decoded - MAX_UPLOADS_PER_FRAME
        );
        assert_eq!(
            session.pending_ids.len(),
            decoded - MAX_UPLOADS_PER_FRAME,
            "uploaded ids must leave the pending set"
        );
        assert!(session.texture(ids[0]).is_some());
    }

    #[test]
    fn a_new_query_keeps_the_old_ids_until_the_new_snapshot_lands() {
        let (mut session, ids, ctx, pounce) = loaded_session(50, 1 << 20, |_| false);
        session.set_query(
            Filter::default(),
            Sort {
                field: SortField::Imported,
                direction: SortDirection::Desc,
            },
            &pounce,
        );
        // Right after the request, before polling, the grid still shows the previous order.
        assert_eq!(session.ids(), ids.as_slice());
        assert!(session.is_loading());

        wait_for(&mut session, &ctx, &pounce, |s| !s.is_loading());
        let mut reversed = ids.clone();
        reversed.reverse();
        assert_eq!(session.ids(), reversed.as_slice());
    }

    #[test]
    fn a_stale_snapshot_never_overwrites_a_newer_one() {
        let (store, ids) = seeded(30, |_| false);
        let ctx = egui::Context::default();
        let pounce = pounce();
        let mut session = GridSession::new(store, 1 << 20);
        // Two back-to-back queries: only the second (descending) may be applied.
        session.set_query(Filter::default(), sort_imported(), &pounce);
        session.set_query(
            Filter::default(),
            Sort {
                field: SortField::Imported,
                direction: SortDirection::Desc,
            },
            &pounce,
        );
        wait_for(&mut session, &ctx, &pounce, |s| !s.is_loading());
        let mut reversed = ids;
        reversed.reverse();
        assert_eq!(session.ids(), reversed.as_slice());
    }

    #[test]
    fn setting_the_same_query_again_queues_no_second_snapshot() {
        let (mut session, _ids, _ctx, pounce) = loaded_session(5, 1 << 20, |_| false);
        session.set_query(Filter::default(), sort_imported(), &pounce);
        assert!(!session.is_loading());
    }

    #[test]
    fn refresh_forgets_thumbnails_and_failures_and_rereads_the_snapshot() {
        let (mut session, ids, ctx, pounce) = loaded_session(10, 1 << 20, |i| i == 0);
        session.request_visible(0..10, &pounce);
        wait_for(&mut session, &ctx, &pounce, |s| s.inflight_batches() == 0);
        wait_for(&mut session, &ctx, &pounce, |s| {
            s.pending_uploads.is_empty()
        });
        assert!(session.texture(ids[5]).is_some());
        assert!(session.has_failed(ids[0]));

        session.refresh(&pounce);
        assert!(session.is_loading());
        assert!(session.texture(ids[5]).is_none());
        assert!(!session.has_failed(ids[0]));
        wait_for(&mut session, &ctx, &pounce, |s| !s.is_loading());
        assert_eq!(session.len(), 10);
    }

    #[test]
    fn the_cursor_is_clamped_when_a_snapshot_shrinks() {
        let (mut session, _ids, ctx, pounce) = loaded_session(20, 1 << 20, |_| false);
        session.set_cursor(Some(19));
        assert_eq!(session.cursor(), Some(19));

        // Filter down to a single root that matches nothing.
        session.set_query(
            Filter {
                root_id: Some(9999),
                ..Default::default()
            },
            sort_imported(),
            &pounce,
        );
        wait_for(&mut session, &ctx, &pounce, |s| !s.is_loading());
        assert!(session.is_empty());
        assert_eq!(session.cursor(), None);
    }

    /// The catalog behind a `loaded_session`, for tests that change it after the first snapshot.
    fn loaded_with_store(
        n: usize,
    ) -> (
        GridSession,
        Arc<SqliteCatalog>,
        Vec<i64>,
        egui::Context,
        Pounce,
    ) {
        let (store, ids) = seeded(n, |_| false);
        let ctx = egui::Context::default();
        let pounce = pounce();
        let mut session = GridSession::new(store.clone(), 64 << 20);
        session.set_query(Filter::default(), sort_imported(), &pounce);
        wait_for(&mut session, &ctx, &pounce, |s| s.is_loaded());
        (session, store, ids, ctx, pounce)
    }

    /// Adds `n` assets that sort *before* everything else under `sort_imported()`.
    fn insert_earlier_assets(store: &SqliteCatalog, n: usize) {
        let root = store.list_roots().unwrap()[0].id;
        for i in 0..n {
            store
                .insert_asset(
                    root,
                    &new_asset(&format!("early{i}.NEF"), -100 + i as i64),
                    None,
                )
                .unwrap();
        }
    }

    #[test]
    fn the_selection_stays_on_its_photo_when_new_assets_sort_in_before_it() {
        let (mut session, store, ids, ctx, pounce) = loaded_with_store(10);
        session.set_cursor(Some(5));
        assert_eq!(session.cursor_id(), Some(ids[5]));

        // An import lands three assets ahead of the selection; a live reload picks them up.
        insert_earlier_assets(&store, 3);
        session.reload(&pounce);
        wait_for(&mut session, &ctx, &pounce, |s| s.len() == 13);

        assert_eq!(session.cursor_id(), Some(ids[5]));
        assert_eq!(session.cursor(), Some(8), "same photo, shifted index");
        assert_eq!(session.ids()[8], ids[5]);
    }

    #[test]
    fn the_selection_clears_when_its_photo_leaves_the_snapshot() {
        let (mut session, _store, ids, ctx, pounce) = loaded_with_store(10);
        session.set_cursor(Some(4));
        assert_eq!(session.cursor_id(), Some(ids[4]));
        session.set_query(
            Filter {
                root_id: Some(9999),
                ..Default::default()
            },
            sort_imported(),
            &pounce,
        );
        wait_for(&mut session, &ctx, &pounce, |s| s.is_empty());
        assert_eq!(session.cursor(), None);
    }

    #[test]
    fn select_asset_finds_a_photo_by_id_not_by_index() {
        let (mut session, _store, ids, _ctx, _pounce) = loaded_with_store(10);
        // The loupe's own list may order things differently; only the id is shared.
        session.select_asset(ids[7]);
        assert_eq!(session.cursor(), Some(7));
        assert_eq!(session.cursor_id(), Some(ids[7]));

        // An id this snapshot doesn't hold is remembered, with no index -- and asking for it
        // again is a no-op rather than a fresh scan of the whole id list.
        session.select_asset(987_654);
        assert_eq!(session.cursor(), None);
        assert_eq!(session.cursor_id(), Some(987_654));
    }

    #[test]
    fn an_unchanged_reload_keeps_in_flight_work_but_a_changed_one_cancels_it() {
        let (mut session, store, _ids, ctx, pounce) = loaded_with_store(2000);
        session.request_visible(0..64, &pounce);
        assert!(session.last_window.is_some());

        // Nothing changed: the live reload must not throw the on-screen cells' work away.
        session.reload(&pounce);
        wait_for(&mut session, &ctx, &pounce, |s| !s.is_loading());
        assert!(
            session.last_window.is_some(),
            "an identical snapshot must not cancel in-flight batches"
        );

        // Something changed: batch indices now refer to the wrong assets, so they're dropped.
        insert_earlier_assets(&store, 1);
        session.reload(&pounce);
        wait_for(&mut session, &ctx, &pounce, |s| s.len() == 2001);
        assert!(session.last_window.is_none());
        assert_eq!(session.inflight_batches(), 0);
    }

    #[test]
    fn refresh_cancels_batches_that_were_decoding_the_old_previews() {
        let (mut session, _store, _ids, _ctx, pounce) = loaded_with_store(2000);
        session.request_visible(0..64, &pounce);
        assert!(session.inflight_batches() > 0);
        session.refresh(&pounce);
        assert_eq!(session.inflight_batches(), 0);
        assert!(session.pending_uploads.is_empty());
    }

    #[test]
    fn a_permanent_thumbnail_error_is_never_retried_but_a_transient_one_backs_off() {
        let (mut session, _store, ids, _ctx, pounce) = loaded_with_store(5);
        // Two hand-made batch results, delivered through a real job's id so `inflight` is valid.
        let (job, slot) = ThumbBatchJob::new(session.store.clone(), Vec::new(), 0);
        let job_id = pounce.submit(Box::new(job));
        {
            let mut out = slot.lock().unwrap();
            out.ready.push((
                ids[0],
                Err(ThumbError::Permanent("no stored preview".into())),
            ));
            out.ready.push((
                ids[1],
                Err(ThumbError::Transient("database is locked".into())),
            ));
            out.done = true;
        }
        session.inflight.insert(0, InflightBatch { job_id, slot });
        session.poll_batches();

        assert!(session.has_failed(ids[0]), "permanent -> failed set");
        assert!(!session.has_failed(ids[1]), "transient -> not written off");
        assert!(!session.needs_thumbnail(ids[0]));
        assert!(
            !session.needs_thumbnail(ids[1]),
            "inside its back-off window"
        );
        assert!(
            session.needs_thumbnail(ids[2]),
            "untouched cells still need one"
        );

        // Once the back-off has passed the cell is asked for again; the permanent one never is.
        session.transient_failed.insert(
            ids[1],
            Instant::now() - TRANSIENT_RETRY_AFTER - Duration::from_secs(1),
        );
        assert!(session.needs_thumbnail(ids[1]));
        assert!(!session.needs_thumbnail(ids[0]));

        // A refresh forgives both.
        session.refresh(&pounce);
        assert!(session.needs_thumbnail(ids[0]));
    }

    #[test]
    fn a_failed_first_read_is_not_loaded_not_empty_and_not_resubmitted_every_frame() {
        let (store, _ids) = seeded(3, |_| false);
        let ctx = egui::Context::default();
        let pounce = pounce();
        let mut session = GridSession::new(store.clone(), 1 << 20);
        session.set_query(Filter::default(), sort_imported(), &pounce);
        // Swap the pending read for one that failed.
        let (job, _real_slot) = SnapshotJob::new(store, Filter::default(), sort_imported());
        let job_id = pounce.submit(Box::new(job));
        let failed: ReportSlot<SnapshotResult> = Arc::new(std::sync::Mutex::new(Some(Err(
            "disk I/O error".to_string(),
        ))));
        session.pending_snapshot = Some(PendingSnapshot {
            job_id,
            slot: failed,
        });

        session.poll(&ctx, &pounce);
        assert!(!session.is_loaded(), "a failed first read is not 'loaded'");
        assert!(!session.is_loading());
        assert_eq!(session.last_error(), Some("disk I/O error"));

        // The toolbar calls set_query with the same query every frame: it must not resubmit.
        session.set_query(Filter::default(), sort_imported(), &pounce);
        assert!(
            !session.is_loading(),
            "no resubmit without an explicit Retry"
        );

        // Retry is an explicit reload.
        session.reload(&pounce);
        assert!(session.is_loading());
        wait_for(&mut session, &ctx, &pounce, |s| s.is_loaded());
        assert_eq!(session.last_error(), None);
        assert_eq!(session.len(), 3);
    }

    #[test]
    fn pause_drops_every_inflight_batch_but_keeps_the_snapshot() {
        let (mut session, _ids, _ctx, pounce) = loaded_session(5000, 64 << 20, |_| false);
        session.request_visible(0..64, &pounce);
        assert!(session.inflight_batches() > 0);
        session.pause(&pounce);
        assert_eq!(session.inflight_batches(), 0);
        assert_eq!(session.len(), 5000);
        // Coming back re-queues what the window still lacks.
        session.request_visible(0..64, &pounce);
        assert!(session.inflight_batches() > 0);
    }
}
