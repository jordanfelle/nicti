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

use egui::{TextureHandle, TextureOptions};
use nicti_lair::pounce_jobs::ReportSlot;
use nicti_lair::{CatalogStore, Filter, Sort, SortDirection, SortField};
use nicti_pounce::{JobId, Pounce};
use nicti_tapetum::cache::Tier;

use super::jobs::{SnapshotJob, SnapshotResult, ThumbBatchJob, ThumbImage, ThumbSlot};
use super::layout::{batch_indices, batches_for};

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

struct PendingSnapshot {
    job_id: JobId,
    slot: ReportSlot<SnapshotResult>,
    /// Which `request_snapshot` this answers. Only the latest generation is applied: a slower,
    /// older snapshot (an earlier sort or filter) finishing after a newer one must not overwrite it.
    generation: u64,
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
    /// "loaded, and empty".
    loaded: bool,
    generation: u64,
    pending_snapshot: Option<PendingSnapshot>,
    last_error: Option<String>,
    textures: Tier<TextureHandle>,
    /// Assets whose thumbnail failed (no stored preview, corrupt JPEG) -- not re-requested until
    /// [`Self::refresh`], so a cell that can never render isn't re-read on every scroll.
    failed: HashSet<i64>,
    inflight: HashMap<usize, InflightBatch>,
    pending_uploads: VecDeque<(i64, ThumbImage)>,
    cursor: Option<usize>,
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
            generation: 0,
            pending_snapshot: None,
            last_error: None,
            textures: Tier::new(texture_budget_bytes, |t: &TextureHandle| {
                let [w, h] = t.size();
                (w * h * 4) as u64
            }),
            failed: HashSet::new(),
            inflight: HashMap::new(),
            pending_uploads: VecDeque::new(),
            cursor: None,
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

    pub fn set_cursor(&mut self, cursor: Option<usize>) {
        self.cursor = cursor.map(|c| c.min(self.ids.len().saturating_sub(1)));
        if self.ids.is_empty() {
            self.cursor = None;
        }
    }

    /// Changes what the grid shows. A no-op if nothing changed; otherwise reads a fresh id
    /// snapshot in the background. The previous ids stay on screen (and keep their thumbnails)
    /// until the new snapshot lands, so a sort change never blanks the grid.
    pub fn set_query(&mut self, filter: Filter, sort: Sort, pounce: &Pounce) {
        if filter == self.filter
            && sort == self.sort
            && (self.loaded || self.pending_snapshot.is_some())
        {
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
        self.pending_uploads.clear();
        self.request_snapshot(pounce);
    }

    fn request_snapshot(&mut self, pounce: &Pounce) {
        self.generation += 1;
        if let Some(old) = self.pending_snapshot.take() {
            pounce.cancel(old.job_id);
        }
        let (job, slot) = SnapshotJob::new(self.store.clone(), self.filter.clone(), self.sort);
        let job_id = pounce.submit(Box::new(job));
        self.pending_snapshot = Some(PendingSnapshot {
            job_id,
            slot,
            generation: self.generation,
        });
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
        let generation = pending.generation;
        self.pending_snapshot = None;
        if generation != self.generation {
            return; // superseded by a newer request that is (or was) also pending
        }
        match result {
            Ok(ids) => {
                self.cancel_batches(pounce);
                self.ids = ids;
                self.loaded = true;
                self.last_error = None;
                self.cursor = match self.cursor {
                    _ if self.ids.is_empty() => None,
                    Some(c) => Some(c.min(self.ids.len() - 1)),
                    None => None,
                };
            }
            Err(msg) => {
                // Keep showing the previous ids; surface why the new query didn't apply.
                self.loaded = true;
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
                    Ok(thumb) => self.pending_uploads.push_back((id, thumb)),
                    Err(_) => {
                        self.failed.insert(id);
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
                .filter(|&id| {
                    !self.failed.contains(&id) && !self.textures.contains(&texture_key(id))
                })
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

        session.upload(&ctx);
        assert_eq!(
            session.pending_uploads.len(),
            decoded - MAX_UPLOADS_PER_FRAME
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
