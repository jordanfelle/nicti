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
    slot: ReportSlot<DecodeResult>,
}

pub struct LoupeSession {
    asset_ids: Vec<i64>,
    cursor: usize,
    decoder: Arc<dyn RawDecoder + Send + Sync>,
    cache: Tier<Arc<LinearFrame>>,
    inflight: HashMap<i64, Inflight>,
    /// The last decode error per asset, so a caller can show it instead of silently retrying
    /// forever. Cleared the next time that asset is (re-)submitted for decode.
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
            if self.inflight.contains_key(&asset_id) {
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
            self.errors.remove(&asset_id);
            let (job, slot) = DecodeJob::new(self.decoder.clone(), path, index);
            let job_id = pounce.submit(Box::new(job));
            self.inflight.insert(asset_id, Inflight { job_id, slot });
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
    /// for its own `ReportSlot`.
    pub fn poll(&mut self, store: &dyn CatalogStore) {
        let mut finished = Vec::new();
        for (&asset_id, inflight) in &self.inflight {
            if let Some(result) = inflight.slot.lock().unwrap().take() {
                finished.push((asset_id, result));
            }
        }
        for (asset_id, result) in finished {
            self.inflight.remove(&asset_id);
            match result {
                Ok(frame) => {
                    if let Ok(Some(asset)) = store.get_asset(asset_id) {
                        let key = asset_cache_key(&asset);
                        self.cache.put(key, frame);
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

    /// The current image's last decode error, if any -- cleared automatically the next time that
    /// asset's decode is (re-)submitted.
    pub fn current_error(&self) -> Option<&str> {
        let asset_id = self.current_asset_id()?;
        self.errors.get(&asset_id).map(String::as_str)
    }

    /// Cancels every in-flight decode -- for a caller tearing down the session (switching folders,
    /// closing the loupe view) before its jobs would naturally finish.
    pub fn cancel_all(&self, pounce: &Pounce) {
        for inflight in self.inflight.values() {
            pounce.cancel(inflight.job_id);
        }
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
}
