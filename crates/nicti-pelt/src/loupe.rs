//! #31: loupe navigation state + directional prefetch. Owns an ordered list of asset ids for the
//! current browsing context (a folder, for now -- real grid/filter-driven selection is #30/#242's
//! job, confirmed not a hard blocker for this ticket) plus a cursor, and a small RAM cache of
//! decoded `LinearFrame`s filled by submitting `DecodeJob`s to Pounce for the cursor and its
//! immediate neighbors whenever the cursor moves. Doesn't render or display anything itself, and
//! doesn't touch Tapetum's `RenderGraph` -- wiring this into the actual egui Loupe view, the T0
//! embedded-preview instant fallback, and `RenderGraph::set_own_hash(DECODE, ...)` for a real
//! per-asset bake-cache key (see [`asset_cache_key`]'s own doc comment) are #31 phase 3's job.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use nicti_cornea::{LinearFrame, RawDecoder};
use nicti_lair::pounce_jobs::ReportSlot;
use nicti_lair::{Asset, CatalogError, CatalogStore};
use nicti_pounce::{JobId, Pounce};
use nicti_tapetum::cache::Tier;

use crate::decode_job::{DecodeJob, DecodeResult};

/// How many neighbors on each side of the cursor to keep decoded ahead of navigation -- #31's
/// own "directional prefetch" target. One each side (cursor-1, cursor, cursor+1) is the minimum
/// that actually hides decode latency on a next/prev press; a wider window is a tuning knob for
/// later, not this ticket's scope.
const PREFETCH_RADIUS: usize = 1;

/// A stable per-asset cache key: fingerprint when the catalog has one (every real ingested asset
/// does, `scruff::Ingest` always computes one), falling back to `(id, mtime_unix)` for the
/// pathological case of a row with none recorded, so this stays total rather than panicking.
/// This is also what #31 phase 3 must feed into Tapetum's `RenderGraph::set_own_hash(DECODE, ...)`
/// to disambiguate its own baked-output cache across different real photos at the same pixel
/// extent -- today's `render.rs::build_graph` uses a constant hash, correct only because it always
/// renders the one synthetic frame.
pub fn asset_cache_key(asset: &Asset) -> blake3::Hash {
    match &asset.fingerprint {
        Some(fp) => blake3::hash(fp.as_bytes()),
        None => blake3::hash(format!("{}:{}", asset.id, asset.mtime_unix).as_bytes()),
    }
}

struct Inflight {
    job_id: JobId,
    /// The asset's identity at the moment this decode was submitted -- `poll` compares this
    /// against the asset's *current* identity before caching a completed frame, and discards it
    /// on a mismatch instead of caching a decode of the old file content under the new identity's
    /// key. Without this, a re-ingest (fingerprint/mtime change) racing an in-flight decode of the
    /// same asset could otherwise store stale pixel data under the fresh key, and `current_frame`
    /// would serve it as if it were current.
    cache_key: blake3::Hash,
    slot: ReportSlot<DecodeResult>,
}

pub struct LoupeSession {
    asset_ids: Vec<i64>,
    cursor: usize,
    decoder: Arc<dyn RawDecoder + Send + Sync>,
    cache: Tier<Arc<LinearFrame>>,
    inflight: HashMap<i64, Inflight>,
    /// The last decode error per asset, so a caller can show it instead of silently retrying
    /// forever -- `request_prefetch` skips any asset recorded here, rather than resubmitting it
    /// every time it re-enters the prefetch window (a real bug an earlier version had: cleared on
    /// every submission attempt, and neither `inflight` nor `cache` account for a failed asset,
    /// so it kept getting retried on every cursor move that touched it). Cleared only by an
    /// explicit [`Self::retry`] call.
    errors: HashMap<i64, String>,
}

impl LoupeSession {
    pub fn new(
        asset_ids: Vec<i64>,
        decoder: Arc<dyn RawDecoder + Send + Sync>,
        cache_budget_bytes: u64,
    ) -> Self {
        Self {
            asset_ids,
            cursor: 0,
            decoder,
            cache: Tier::new(cache_budget_bytes, |frame: &Arc<LinearFrame>| {
                (frame.pixels.len() * std::mem::size_of::<u16>()) as u64
            }),
            inflight: HashMap::new(),
            errors: HashMap::new(),
        }
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn len(&self) -> usize {
        self.asset_ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.asset_ids.is_empty()
    }

    pub fn current_asset_id(&self) -> Option<i64> {
        self.asset_ids.get(self.cursor).copied()
    }

    /// Moves the cursor and re-requests prefetch for the new window -- the caller (the egui Loupe
    /// view, once #31 phase 3 wires it) calls this on next/prev navigation.
    pub fn set_cursor(
        &mut self,
        cursor: usize,
        store: &dyn CatalogStore,
        pounce: &Pounce,
    ) -> Result<(), CatalogError> {
        if self.asset_ids.is_empty() {
            return Ok(());
        }
        self.cursor = cursor.min(self.asset_ids.len() - 1);
        self.request_prefetch(store, pounce)
    }

    /// Submits a `DecodeJob` for every asset in the prefetch window (cursor +/- `PREFETCH_RADIUS`)
    /// not already cached or in flight, then reprioritizes every pending background job so the
    /// nearest-to-cursor one runs next -- `Pounce::reprioritize` is meant to be "called on every
    /// cursor move" per its own doc comment, using the same distance-from-cursor key
    /// `nicti_tapetum::prefetch::priority_order` sorts by.
    fn request_prefetch(
        &mut self,
        store: &dyn CatalogStore,
        pounce: &Pounce,
    ) -> Result<(), CatalogError> {
        let lo = self.cursor.saturating_sub(PREFETCH_RADIUS);
        let hi = (self.cursor + PREFETCH_RADIUS).min(self.asset_ids.len() - 1);
        for index in lo..=hi {
            let asset_id = self.asset_ids[index];
            if self.inflight.contains_key(&asset_id) || self.errors.contains_key(&asset_id) {
                continue;
            }
            let Some(asset) = store.get_asset(asset_id)? else {
                continue;
            };
            let key = asset_cache_key(&asset);
            if self.cache.contains(&key) {
                continue;
            }
            let Some(root_path) = store.get_root_path(asset.root_id)? else {
                continue;
            };
            let path = PathBuf::from(root_path).join(&asset.rel_path);
            let (job, slot) = DecodeJob::new(self.decoder.clone(), path, index);
            let job_id = pounce.submit(Box::new(job));
            self.inflight.insert(
                asset_id,
                Inflight {
                    job_id,
                    cache_key: key,
                    slot,
                },
            );
        }
        let cursor = self.cursor;
        pounce.reprioritize(move |spec| {
            spec.image_index
                .map(|i| i.abs_diff(cursor))
                .unwrap_or(usize::MAX)
        });
        Ok(())
    }

    /// Polls every in-flight decode, moving a finished one into the cache (success) or the error
    /// map (failure). Call once per frame -- the same pattern `app.rs::poll_backup` already uses
    /// for its own `ReportSlot`. A successful decode is only cached if the asset's identity still
    /// matches what was submitted -- see [`Inflight::cache_key`]'s own doc comment for why a
    /// mismatch (a re-ingest racing this decode) discards the result instead of caching it under
    /// the asset's new identity. A discarded/mismatched asset isn't explicitly requeued here; it
    /// naturally gets resubmitted by the next `set_cursor` that includes it, since it's neither
    /// cached nor (after this) in flight.
    pub fn poll(&mut self, store: &dyn CatalogStore) {
        let mut finished = Vec::new();
        for (&asset_id, inflight) in &self.inflight {
            if let Some(result) = inflight.slot.lock().unwrap().take() {
                finished.push((asset_id, inflight.cache_key, result));
            }
        }
        for (asset_id, submitted_key, result) in finished {
            self.inflight.remove(&asset_id);
            match result {
                Ok(frame) => {
                    if let Ok(Some(asset)) = store.get_asset(asset_id) {
                        if asset_cache_key(&asset) == submitted_key {
                            self.cache.put(submitted_key, frame);
                        }
                    }
                }
                Err(msg) => {
                    self.errors.insert(asset_id, msg);
                }
            }
        }
    }

    /// The current image's decoded frame, if its decode has completed and is still cached --
    /// `None` means "still decoding, or its cache entry was since evicted": a caller falls back
    /// to the T0 embedded preview in the meantime (#31 phase 3).
    pub fn current_frame(&mut self, store: &dyn CatalogStore) -> Option<Arc<LinearFrame>> {
        let asset_id = self.current_asset_id()?;
        let asset = store.get_asset(asset_id).ok()??;
        let key = asset_cache_key(&asset);
        self.cache.get(&key).cloned()
    }

    /// The current image's last decode error, if any -- cleared only by [`Self::retry`].
    pub fn current_error(&self) -> Option<&str> {
        let asset_id = self.current_asset_id()?;
        self.errors.get(&asset_id).map(String::as_str)
    }

    /// Clears a failed asset's recorded error so the next `set_cursor` call (if it's still in the
    /// prefetch window) resubmits it for decode -- what a caller wires to an explicit "Retry"
    /// action (#31 phase 3), since `request_prefetch` otherwise never retries a failed asset on
    /// its own.
    pub fn retry(&mut self, asset_id: i64) {
        self.errors.remove(&asset_id);
    }

    /// Cancels every in-flight decode and forgets about it -- for a caller tearing down the
    /// session (switching folders, closing the loupe view) before its jobs would naturally
    /// finish. Clears `inflight` immediately rather than waiting for each job's `ReportSlot` to
    /// resolve, because a job `Pounce::cancel` catches while still queued (not yet picked up by a
    /// worker) never runs its own `step()` at all -- `queue::Scheduler::take_next` drops it
    /// straight into `cancelled_while_queued` -- so its slot would never resolve, and `poll`'s own
    /// "only remove from `inflight` once the slot resolves" rule would otherwise wedge that asset
    /// id out of `inflight` (and therefore un-resubmittable) for the rest of this session's life.
    /// A job that's already running when cancelled still finishes its one chunk and resolves its
    /// slot normally -- that result is simply no longer looked at, which is fine: nothing else
    /// holds a reference to `inflight`'s entry once this drops it, so there's nothing to leak,
    /// just wasted work matching the caller's own "abandon everything" intent. (Caught by
    /// adversarial review: an earlier version left `inflight` entries in place until their slot
    /// resolved, which for a cancelled-while-queued job never happened.)
    pub fn cancel_all(&mut self, pounce: &Pounce) {
        for inflight in self.inflight.values() {
            pounce.cancel(inflight.job_id);
        }
        self.inflight.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_claw::Module;
    use nicti_cornea::DecodeError;
    use nicti_lair::{NewAsset, SqliteCatalog};
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    struct CountingDecoder {
        calls: AtomicUsize,
    }

    impl Module for CountingDecoder {
        fn id(&self) -> &str {
            "test.decoder.counting"
        }
        fn schema_version(&self) -> u32 {
            1
        }
        fn migrate_params(&self, _: u32, _: serde_json::Value) -> Option<serde_json::Value> {
            None
        }
    }

    impl RawDecoder for CountingDecoder {
        fn decode_linear(&self, path: &Path) -> Result<LinearFrame, DecodeError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if path.to_string_lossy().contains("bad") {
                return Err(DecodeError::Io {
                    path: path.to_path_buf(),
                    source: std::io::Error::other("simulated failure"),
                });
            }
            Ok(LinearFrame {
                make: "Test".to_string(),
                model: "Fake".to_string(),
                width: 2,
                height: 2,
                black: 0,
                maximum: 4095,
                cam_mul: [1.0, 1.0, 1.0, 1.0],
                pre_mul: [1.0, 1.0, 1.0, 1.0],
                cam_xyz: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0],
                cblack: [0, 0, 0, 0],
                pixels: vec![0; 2 * 2 * 3],
            })
        }
    }

    fn new_asset(rel_path: &str) -> NewAsset {
        NewAsset {
            rel_path: rel_path.to_string(),
            rel_path_fold: rel_path.to_lowercase(),
            size_bytes: 100,
            mtime_unix: 0,
            fingerprint: Some(format!("fp-{rel_path}")),
            natural_key: None,
            make: None,
            model: Some("Z8".to_string()),
            captured_at: None,
            width: None,
            height: None,
            imported_at: 0,
        }
    }

    /// Sets up a catalog with `n` assets under one root at a real temp folder (so
    /// `get_root_path`/`rel_path` join to a resolvable-looking path -- the fake decoder never
    /// touches the filesystem, so the path just needs to exist as a string, not on disk), plus a
    /// `Pounce` with a real worker thread so submitted `DecodeJob`s actually run.
    fn setup(n: usize) -> (SqliteCatalog, Vec<i64>, Pounce) {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let volume_id = store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = store.ensure_root(volume_id, "/photos").unwrap();
        let ids: Vec<i64> = (0..n)
            .map(|i| {
                store
                    .insert_asset(root_id, &new_asset(&format!("{i}.NEF")), None)
                    .unwrap()
            })
            .collect();
        let pounce = Pounce::new(0, 2, 2, || {});
        (store, ids, pounce)
    }

    fn wait_for<F: FnMut() -> bool>(mut cond: F) {
        for _ in 0..200 {
            if cond() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("condition never became true");
    }

    #[test]
    fn set_cursor_decodes_the_cursor_and_its_neighbors_only() {
        let (store, ids, pounce) = setup(5);
        let decoder = Arc::new(CountingDecoder {
            calls: AtomicUsize::new(0),
        });
        let mut session = LoupeSession::new(ids.clone(), decoder.clone(), u64::MAX);

        session.set_cursor(2, &store, &pounce).unwrap();
        wait_for(|| decoder.calls.load(Ordering::SeqCst) == 3);

        // Poll drains the results; give it a moment for all three to have landed.
        wait_for(|| {
            session.poll(&store);
            session.inflight.is_empty()
        });

        // Cursor=2 -> window is {1,2,3}, not 0 or 4.
        let asset_at = |i: usize| store.get_asset(ids[i]).unwrap().unwrap();
        assert!(session.cache.contains(&asset_cache_key(&asset_at(1))));
        assert!(session.cache.contains(&asset_cache_key(&asset_at(2))));
        assert!(session.cache.contains(&asset_cache_key(&asset_at(3))));
        assert!(!session.cache.contains(&asset_cache_key(&asset_at(0))));
        assert!(!session.cache.contains(&asset_cache_key(&asset_at(4))));
    }

    #[test]
    fn current_frame_is_none_until_decode_completes_then_some_after_poll() {
        let (store, ids, pounce) = setup(1);
        let decoder = Arc::new(CountingDecoder {
            calls: AtomicUsize::new(0),
        });
        let mut session = LoupeSession::new(ids, decoder, u64::MAX);

        session.set_cursor(0, &store, &pounce).unwrap();
        assert!(
            session.current_frame(&store).is_none(),
            "decode hasn't completed (or been polled) yet"
        );

        wait_for(|| {
            session.poll(&store);
            session.current_frame(&store).is_some()
        });
        let frame = session.current_frame(&store).unwrap();
        assert_eq!(frame.width, 2);
    }

    #[test]
    fn a_decode_failure_resolves_as_an_error_not_a_hang() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let volume_id = store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = store.ensure_root(volume_id, "/photos").unwrap();
        let bad_id = store
            .insert_asset(root_id, &new_asset("bad.NEF"), None)
            .unwrap();
        let pounce = Pounce::new(0, 2, 2, || {});
        let decoder = Arc::new(CountingDecoder {
            calls: AtomicUsize::new(0),
        });
        let mut session = LoupeSession::new(vec![bad_id], decoder, u64::MAX);

        session.set_cursor(0, &store, &pounce).unwrap();
        wait_for(|| {
            session.poll(&store);
            session.current_error().is_some()
        });
        assert!(session.current_frame(&store).is_none());
        assert!(session
            .current_error()
            .unwrap()
            .contains("simulated failure"));
    }

    #[test]
    fn a_failed_asset_is_not_resubmitted_until_an_explicit_retry() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let volume_id = store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = store.ensure_root(volume_id, "/photos").unwrap();
        let bad_id = store
            .insert_asset(root_id, &new_asset("bad.NEF"), None)
            .unwrap();
        let pounce = Pounce::new(0, 2, 2, || {});
        let decoder = Arc::new(CountingDecoder {
            calls: AtomicUsize::new(0),
        });
        let mut session = LoupeSession::new(vec![bad_id], decoder.clone(), u64::MAX);

        session.set_cursor(0, &store, &pounce).unwrap();
        wait_for(|| {
            session.poll(&store);
            session.current_error().is_some()
        });
        assert_eq!(decoder.calls.load(Ordering::SeqCst), 1);

        // Re-requesting the same window (as any further navigation touching this asset would)
        // must not retry it on its own -- that's the bug CodeRabbit caught: request_prefetch used
        // to check only inflight/cache, not errors, so a failed asset got retried on every cursor
        // move that put it back in the window.
        session.set_cursor(0, &store, &pounce).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        session.poll(&store);
        assert_eq!(decoder.calls.load(Ordering::SeqCst), 1);
        assert!(session.current_error().is_some());

        // An explicit retry clears the error and allows exactly one more decode attempt.
        session.retry(bad_id);
        assert!(session.current_error().is_none());
        session.set_cursor(0, &store, &pounce).unwrap();
        wait_for(|| decoder.calls.load(Ordering::SeqCst) == 2);
    }

    #[test]
    fn resubmitting_an_already_cached_asset_does_not_decode_it_again() {
        let (store, ids, pounce) = setup(1);
        let decoder = Arc::new(CountingDecoder {
            calls: AtomicUsize::new(0),
        });
        let mut session = LoupeSession::new(ids, decoder.clone(), u64::MAX);

        session.set_cursor(0, &store, &pounce).unwrap();
        wait_for(|| {
            session.poll(&store);
            session.current_frame(&store).is_some()
        });
        assert_eq!(decoder.calls.load(Ordering::SeqCst), 1);

        // Moving away and back to the same single-asset window must not re-decode: it's still
        // cached, and #31's whole point is not paying decode cost twice for the same photo.
        session.set_cursor(0, &store, &pounce).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        session.poll(&store);
        assert_eq!(decoder.calls.load(Ordering::SeqCst), 1);
    }

    /// Slow enough that, with only one CPU worker and a 3-wide prefetch window, at least one
    /// submitted job is still sitting in the queue (never picked up by `step()`) when the test
    /// calls `cancel_all` right after submitting -- the exact race `cancel_all`'s own doc comment
    /// describes.
    struct SlowDecoder;

    impl Module for SlowDecoder {
        fn id(&self) -> &str {
            "test.decoder.slow"
        }
        fn schema_version(&self) -> u32 {
            1
        }
        fn migrate_params(&self, _: u32, _: serde_json::Value) -> Option<serde_json::Value> {
            None
        }
    }

    impl RawDecoder for SlowDecoder {
        fn decode_linear(&self, _path: &Path) -> Result<LinearFrame, DecodeError> {
            std::thread::sleep(Duration::from_millis(200));
            Ok(LinearFrame {
                make: "Test".to_string(),
                model: "Slow".to_string(),
                width: 2,
                height: 2,
                black: 0,
                maximum: 4095,
                cam_mul: [1.0, 1.0, 1.0, 1.0],
                pre_mul: [1.0, 1.0, 1.0, 1.0],
                cam_xyz: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0],
                cblack: [0, 0, 0, 0],
                pixels: vec![0; 2 * 2 * 3],
            })
        }
    }

    #[test]
    fn cancel_all_never_permanently_wedges_an_asset_out_of_inflight() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let volume_id = store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = store.ensure_root(volume_id, "/photos").unwrap();
        let ids: Vec<i64> = (0..3)
            .map(|i| {
                store
                    .insert_asset(root_id, &new_asset(&format!("{i}.NEF")), None)
                    .unwrap()
            })
            .collect();
        // One CPU worker: with a 3-wide window (cursor=1 -> indices 0,1,2) and a 200ms decoder,
        // at most one job can be running at a time when cancel_all is called immediately after
        // submit -- the other two are guaranteed still queued, never having run step() at all.
        let pounce = Pounce::new(0, 1, 1, || {});
        let mut session = LoupeSession::new(ids, Arc::new(SlowDecoder), u64::MAX);

        session.set_cursor(1, &store, &pounce).unwrap();
        assert_eq!(
            session.inflight.len(),
            3,
            "all three should have been submitted"
        );
        session.cancel_all(&pounce);
        assert!(
            session.inflight.is_empty(),
            "cancel_all must not leave any asset permanently wedged in inflight"
        );

        // Wait out any job that was already running (up to 200ms) plus its cancellation, then
        // confirm the session is still usable: a fresh set_cursor can resubmit and actually
        // complete a decode for the same assets, proving nothing was left un-resubmittable.
        std::thread::sleep(Duration::from_millis(250));
        session.set_cursor(1, &store, &pounce).unwrap();
        wait_for(|| {
            session.poll(&store);
            session.current_frame(&store).is_some()
        });
    }

    #[test]
    fn a_completed_decode_is_discarded_if_the_asset_s_identity_changed_while_in_flight() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let volume_id = store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = store.ensure_root(volume_id, "/photos").unwrap();
        let asset_id = store
            .insert_asset(root_id, &new_asset("0.NEF"), None)
            .unwrap();
        let pounce = Pounce::new(0, 2, 2, || {});
        // SlowDecoder's 200ms gives this test a real window to mutate the asset's identity
        // (simulating a concurrent re-ingest) before the decode it started against the old
        // identity completes.
        let mut session = LoupeSession::new(vec![asset_id], Arc::new(SlowDecoder), u64::MAX);

        session.set_cursor(0, &store, &pounce).unwrap();
        assert_eq!(session.inflight.len(), 1);

        // Simulate a re-ingest changing this asset's fingerprint (a real Scruff/Patrol rescan
        // upserts in place on (root_id, rel_path) -- same path used here) while its decode,
        // started against the *old* identity, is still running.
        let mut re_ingested = new_asset("0.NEF");
        re_ingested.fingerprint = Some("fp-0.NEF-changed".to_string());
        store.insert_asset(root_id, &re_ingested, None).unwrap();

        wait_for(|| {
            session.poll(&store);
            session.inflight.is_empty()
        });

        // The stale decode must not have been cached under the asset's new identity -- if it
        // had, current_frame (which always resolves against the *current* row) would wrongly
        // return the old file's pixels as if they belonged to the new revision.
        assert!(
            session.current_frame(&store).is_none(),
            "a decode completed against a stale identity must be discarded, not cached under the new one"
        );
    }
}
