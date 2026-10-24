//! Culling (#32): marking photos (stars, pick/reject, colour labels) with the standard keys,
//! undo/redo, and the state the views draw badges from.
//!
//! The workflow is deliberately not baked in. Every marker is available at once; the user marks
//! with whichever they like, filters the Library on it, selects all, and deletes (`grid`'s filter
//! row and `app.rs`'s delete flow). See `keys.rs` for the vocabulary, `worker.rs` for why writes
//! happen off the UI thread, and `undo.rs` for the undo ring.

pub mod badges;
pub mod compare;
pub mod delete;
pub mod input;
pub mod keys;
pub mod previews;
pub mod survey;
pub mod tile;
pub mod undo;
pub mod worker;

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use nicti_lair::AssetMeta;

use keys::{apply_all, CullAction};
use worker::{MetaStore, MetaWorker, Reply};

/// Cached markers kept before the cache is trimmed. ~100 bytes each: 300k is ~30 MB, a few
/// screens of scrolling over a 1M catalog, and trimming just re-reads what is on screen.
const CACHE_CAP: usize = 300_000;
/// Ids per read request, so one request never monopolises the worker ahead of a keypress's write.
const READ_CHUNK: usize = 512;
/// After a failed marker read, how long `ensure` stays quiet. It runs every frame with the visible
/// window, so retrying at once would turn one catalog hiccup into a read-and-error storm.
const READ_RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(3);
/// Above this many targets an optimistic local update isn't worth computing on the UI thread
/// (a select-all over a big library); the worker's reply updates the cache instead.
const OPTIMISTIC_MAX: usize = 5_000;

/// Whether a mark should move on to the next photo: the auto-advance toggle, flipped by Shift for
/// that one press, and only when exactly one photo was marked (a multi-selection has no "next").
pub fn should_advance(auto_advance: bool, invert_advance: bool, targets: usize) -> bool {
    targets == 1 && (auto_advance != invert_advance)
}

/// Writes in flight per photo. Small marks are counted per id; a mark over [`OPTIMISTIC_MAX`]
/// targets (a select-all over a big library) is kept as one sorted id list instead, because
/// building and settling a million-entry map cost ~100 ms in `mark` and ~370 ms in one `poll`
/// frame (#497). The worker handles marks in order and answers each one once, so a reply of
/// bulk size settles the oldest bulk entry.
#[derive(Default)]
struct Pending {
    counts: HashMap<i64, u32>,
    bulk: VecDeque<Vec<i64>>,
}

impl Pending {
    fn add(&mut self, ids: &[i64]) {
        if ids.len() > OPTIMISTIC_MAX {
            let mut sorted = ids.to_vec();
            if !sorted.is_sorted() {
                sorted.sort_unstable();
            }
            self.bulk.push_back(sorted);
        } else {
            for id in ids {
                *self.counts.entry(*id).or_default() += 1;
            }
        }
    }

    fn contains(&self, id: i64) -> bool {
        self.counts.contains_key(&id) || self.bulk.iter().any(|b| b.binary_search(&id).is_ok())
    }

    /// Settles the write that targeted `ids` (as given to [`Self::add`]).
    fn settle(&mut self, ids: &[i64]) {
        if ids.len() > OPTIMISTIC_MAX {
            self.bulk.pop_front();
            return;
        }
        for id in ids {
            if let Some(n) = self.counts.get_mut(id) {
                *n -= 1;
                if *n == 0 {
                    self.counts.remove(id);
                }
            }
        }
    }

    /// A deleted photo has no write left to wait for. Only per-id counts are dropped: a bulk
    /// entry stays until its reply, which is harmless for an id that no longer exists.
    fn remove(&mut self, id: i64) {
        self.counts.remove(&id);
    }

    fn clear(&mut self) {
        self.counts.clear();
        self.bulk.clear();
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.counts.is_empty() && self.bulk.is_empty()
    }
}

/// What the views need from culling: the markers to draw, and the entry points that mark/undo.
pub struct CullState {
    worker: MetaWorker,
    /// Markers as the UI believes them: confirmed reads, plus optimistic edits for writes still
    /// in flight.
    cache: HashMap<i64, AssetMeta>,
    /// Ids a read is in flight for, so scrolling doesn't re-request them every frame.
    requested: HashSet<i64>,
    /// Writes in flight per photo. While non-zero the cache holds the user's latest intent, so
    /// an older read or write reply must not overwrite it.
    pending: Pending,
    /// Advance to the next photo after a single-photo mark. On by default; Shift inverts it for
    /// one keypress.
    pub auto_advance: bool,
    last_error: Option<String>,
    /// Set by a failed read: `ensure` requests nothing until then.
    read_backoff_until: Option<std::time::Instant>,
    /// The writer thread died and this was reported; marks are no longer being saved.
    writer_dead: bool,
    /// Barriers sent and not yet answered (tests only ever raise it).
    barriers: usize,
}

impl CullState {
    pub fn new(store: Arc<dyn MetaStore>, wake: impl Fn() + Send + 'static) -> Self {
        CullState {
            worker: MetaWorker::spawn(store, wake),
            cache: HashMap::new(),
            requested: HashSet::new(),
            pending: Pending::default(),
            auto_advance: true,
            last_error: None,
            read_backoff_until: None,
            writer_dead: false,
            barriers: 0,
        }
    }

    pub fn meta(&self, id: i64) -> Option<&AssetMeta> {
        self.cache.get(&id)
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    pub fn clear_error(&mut self) {
        self.last_error = None;
    }

    /// Asks for the markers of any of `ids` not cached yet. Cheap to call every frame with the
    /// visible window: nothing is sent when everything is cached or already requested.
    pub fn ensure(&mut self, ids: impl IntoIterator<Item = i64>) {
        if self
            .read_backoff_until
            .is_some_and(|until| std::time::Instant::now() < until)
        {
            return;
        }
        let missing: Vec<i64> = ids
            .into_iter()
            .filter(|id| !self.cache.contains_key(id) && !self.requested.contains(id))
            .collect();
        for chunk in missing.chunks(READ_CHUNK) {
            self.requested.extend(chunk.iter().copied());
            self.worker.read(chunk.to_vec());
        }
    }

    /// Applies `action` to `ids`. The cache updates at once (so badges and auto-advance are
    /// instant) when every target's markers are already known; the write happens on the worker.
    pub fn mark(&mut self, ids: Vec<i64>, action: CullAction) {
        if ids.is_empty() {
            return;
        }
        if ids.len() <= OPTIMISTIC_MAX && ids.iter().all(|id| self.cache.contains_key(id)) {
            let before: Vec<AssetMeta> = ids.iter().map(|id| self.cache[id].clone()).collect();
            for (id, now) in ids.iter().zip(apply_all(action, &before)) {
                self.cache.insert(*id, now);
            }
        }
        self.pending.add(&ids);
        self.worker.apply(ids, action);
    }

    pub fn undo(&self) {
        self.worker.undo();
    }

    pub fn redo(&self) {
        self.worker.redo();
    }

    /// Forgets every cached marker (not the in-flight writes): after an import or sync, whose
    /// new rows can reuse the ids of photos deleted earlier, a stale entry must not be inherited.
    pub fn invalidate(&mut self) {
        let pending = &self.pending;
        self.cache.retain(|id, _| pending.contains(*id));
        self.requested.clear();
    }

    /// Photos were deleted: drop them from the cache and from undo history.
    pub fn forget(&mut self, ids: &[i64]) {
        for id in ids {
            self.cache.remove(id);
            self.requested.remove(id);
            self.pending.remove(*id);
        }
        self.worker.forget(ids.to_vec());
    }

    /// Folds worker replies into the cache. Call once per frame.
    pub fn poll(&mut self) {
        while let Some(reply) = self.worker.try_recv() {
            match reply {
                Reply::Barrier => self.barriers = self.barriers.saturating_sub(1),
                Reply::Loaded(map) => {
                    for (id, meta) in map {
                        self.requested.remove(&id);
                        if !self.pending.contains(id) {
                            self.cache.insert(id, meta);
                        }
                    }
                }
                Reply::Done {
                    ids,
                    changed,
                    counted,
                } => {
                    if counted {
                        self.pending.settle(&ids);
                    }
                    // A reply too big to have been applied optimistically (a select-all) only
                    // refreshes photos already cached: inserting a million entries just to trim
                    // them again costs hundreds of ms in one frame, and an uncached photo is
                    // read when it scrolls into view.
                    let existing_only = changed.len() > OPTIMISTIC_MAX;
                    for (id, meta) in changed {
                        // Only the last outstanding write for a photo is authoritative; an
                        // earlier reply (including an undo's) would flicker the badge back
                        // before the newer one lands, and that newer write's own reply sets
                        // the final value.
                        if existing_only {
                            // Cache first: most of a select-all reply is uncached, and
                            // `contains` scans every bulk entry in flight.
                            if let Some(slot) = self.cache.get_mut(&id) {
                                if !self.pending.contains(id) {
                                    *slot = meta;
                                }
                            }
                        } else if !self.pending.contains(id) {
                            self.cache.insert(id, meta);
                        }
                    }
                }
                Reply::Failed {
                    ids,
                    message,
                    counted,
                } => {
                    if counted {
                        self.pending.settle(&ids);
                    }
                    // The optimistic values may now be wrong: forget them so they are re-read.
                    for id in &ids {
                        if !self.pending.contains(*id) {
                            self.cache.remove(id);
                            self.requested.remove(id);
                        }
                    }
                    if counted {
                        self.last_error = Some(format!("Couldn't save the change: {message}"));
                    } else {
                        // A failed read or undo/redo: say so, and (for reads) hold off asking
                        // again for a moment instead of re-failing every frame.
                        self.last_error =
                            Some(format!("Couldn't read or restore markers: {message}"));
                        self.read_backoff_until =
                            Some(std::time::Instant::now() + READ_RETRY_AFTER);
                    }
                }
            }
        }
        if self.worker.is_dead() && !self.writer_dead {
            self.writer_dead = true;
            self.pending.clear();
            self.last_error = Some(
                "The marking writer stopped, so marks are no longer being saved. \
                 Restart Nicti."
                    .into(),
            );
        }
        if self.cache.len() > CACHE_CAP {
            let pending = &self.pending;
            self.cache.retain(|id, _| pending.contains(*id));
            self.requested.clear();
        }
    }

    /// Queues a barrier: [`Self::settle_all`] then also waits until the writer has handled every
    /// command sent before it, including undo/redo, which are not tracked in `pending`.
    #[cfg(test)]
    pub fn barrier(&mut self) {
        self.barriers += 1;
        self.worker.barrier();
    }

    /// Blocks until every queued command has been handled and its reply folded in. Tests and
    /// shutdown only -- never call from a frame.
    #[cfg(test)]
    pub fn settle_all(&mut self) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            self.poll();
            if self.pending.is_empty() && self.requested.is_empty() && self.barriers == 0 {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "worker never settled");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
}

#[cfg(test)]
mod tests;
