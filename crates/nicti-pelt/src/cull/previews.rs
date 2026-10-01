//! Preview textures for the survey and compare tiles (#32).
//!
//! Culling runs on the camera's embedded previews, never a RAW decode (the PRD's "culling is
//! almost entirely embedded-JPEG"): a tile shows the T0 grid preview at once, and -- when it is a
//! *large* tile (compare) -- upgrades to the T2 screen-resolution preview from the Larder as soon
//! as one exists, generating it in the background through the same `T2Job` the loupe uses. This
//! also means a survey/compare never touches the shared `DevelopView`, so it never trips the
//! loupe's "Develop has unsaved edits" guard.
//!
//! A T0 decode is cheap enough to do on the UI thread once per tile; a T2 decode is not, so
//! upgrades are throttled and only requested for the two compare tiles, never for a whole survey.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use egui::TextureHandle;
use nicti_lair::larder::{LarderKey, LarderTier};
use nicti_lair::pounce_jobs::ReportSlot;
use nicti_lair::CatalogStore;
use nicti_pounce::{JobId, Pounce};

use crate::loupe::asset_cache_key;
use crate::t2::{self, try_lock_larder, SharedLarder, T2Job, T2Outcome};

/// Textures kept. A survey shows up to 16 and a compare 2; the rest is slack for stepping back
/// and forth through a candidate list without re-decoding every time.
const MAX_TILES: usize = 48;
/// T2 generations in flight at once. Stepping the compare candidate quickly (a held arrow key)
/// would otherwise queue one background job per photo it passes.
const MAX_T2_JOBS: usize = 4;
/// How often a tile still on T0 re-checks whether its T2 has landed -- each check is a catalog
/// read plus a Larder lookup, not something to do every frame.
const UPGRADE_CHECK_EVERY: Duration = Duration::from_millis(250);

/// How sharp a tile needs to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    /// The T0 grid preview is enough (survey tiles).
    Small,
    /// Prefer the T2 screen-resolution preview (compare tiles).
    Large,
}

struct Tile {
    texture: TextureHandle,
    is_t2: bool,
}

pub struct TilePreviews {
    tiles: HashMap<i64, Tile>,
    /// Insertion order, for evicting the oldest tile.
    order: VecDeque<i64>,
    larder: Option<SharedLarder>,
    t2_jobs: HashMap<i64, (JobId, ReportSlot<T2Outcome>)>,
    /// Photos that can never produce a T2 (no embedded preview, corrupt JPEG) or whose stored T2
    /// wouldn't decode: stay on T0 for the session rather than retrying.
    t2_gave_up: HashSet<i64>,
    /// Photos with no T0 in the catalog *or* as a sidecar, and when we last looked: don't hit the
    /// catalog and the disk for them every frame (#72's sidecar fallback made a miss costly).
    no_t0: std::collections::HashMap<i64, std::time::Instant>,
    /// Sidecar reads in flight on a worker (an archived folder's T0).
    sidecar_fetch: HashMap<i64, crate::t0_fetch::FetchSlot>,
    next_check: HashMap<i64, Instant>,
}

/// How long a photo with no thumbnail anywhere is left alone before looking again.
const NO_T0_RETRY: std::time::Duration = std::time::Duration::from_secs(5);

impl TilePreviews {
    pub fn new(larder: Option<SharedLarder>) -> Self {
        TilePreviews {
            tiles: HashMap::new(),
            order: VecDeque::new(),
            larder,
            t2_jobs: HashMap::new(),
            t2_gave_up: HashSet::new(),
            no_t0: std::collections::HashMap::new(),
            sidecar_fetch: HashMap::new(),
            next_check: HashMap::new(),
        }
    }

    /// Folds finished T2 jobs in. Call once per frame while a survey/compare is showing.
    pub fn poll(&mut self) {
        let mut finished = Vec::new();
        for (&id, (_, slot)) in &self.t2_jobs {
            if let Some(outcome) = slot.lock().unwrap().take() {
                finished.push((id, outcome));
            }
        }
        for (id, outcome) in finished {
            self.t2_jobs.remove(&id);
            match outcome {
                // Look for it on the very next frame instead of waiting out the throttle.
                T2Outcome::Stored => {
                    self.next_check.remove(&id);
                }
                T2Outcome::Retry(_) => {}
                T2Outcome::Failed(_) => {
                    self.t2_gave_up.insert(id);
                }
            }
        }
    }

    /// Cancels queued T2 work and forgets every texture -- when leaving the survey/compare.
    pub fn clear(&mut self, pounce: &Pounce) {
        for (job_id, _) in self.t2_jobs.values() {
            pounce.cancel(*job_id);
        }
        self.t2_jobs.clear();
        self.tiles.clear();
        self.order.clear();
        self.next_check.clear();
        self.sidecar_fetch.clear();
    }

    /// Drops one photo's texture (it was deleted).
    pub fn forget(&mut self, id: i64) {
        self.tiles.remove(&id);
        self.order.retain(|i| *i != id);
        self.t2_jobs.remove(&id);
        self.t2_gave_up.remove(&id);
        self.no_t0.remove(&id);
        self.sidecar_fetch.remove(&id);
        self.next_check.remove(&id);
    }

    /// The texture to draw for `id`, or `None` if it has no decodable preview at all.
    pub fn get(
        &mut self,
        ctx: &egui::Context,
        store: &dyn CatalogStore,
        pounce: &Pounce,
        id: i64,
        want: Want,
    ) -> Option<&TextureHandle> {
        let needs_t2 = want == Want::Large
            && self.larder.is_some()
            && !self.t2_gave_up.contains(&id)
            && !self.tiles.get(&id).is_some_and(|t| t.is_t2);
        if needs_t2 {
            self.try_upgrade(ctx, store, pounce, id);
        }
        // A sidecar read finished on a worker.
        if self.sidecar_fetch.contains_key(&id) {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        if let Some(slot) = self.sidecar_fetch.get(&id) {
            let outcome = slot.lock().unwrap().take();
            if let Some(bytes) = outcome {
                self.sidecar_fetch.remove(&id);
                // Never downgrade: T2 may have been installed while the sidecar read ran.
                let has_t2 = self.tiles.get(&id).is_some_and(|t| t.is_t2);
                let texture = bytes
                    .filter(|_| !has_t2 && !self.tiles.contains_key(&id))
                    .and_then(|b| preview_texture(ctx, format!("tile-t0-{id}"), &b));
                match texture {
                    _ if has_t2 || self.tiles.contains_key(&id) => {}
                    Some(texture) => self.insert(
                        id,
                        Tile {
                            texture,
                            is_t2: false,
                        },
                    ),
                    None => {
                        self.no_t0.insert(id, std::time::Instant::now());
                    }
                }
            }
        }
        let recently_missed = self
            .no_t0
            .get(&id)
            .is_some_and(|t| t.elapsed() < NO_T0_RETRY);
        if !self.tiles.contains_key(&id)
            && !recently_missed
            && !self.sidecar_fetch.contains_key(&id)
        {
            // Catalog only here (a local query). The sidecar half touches the archive drive, so
            // it runs on a worker and is picked up above on a later frame.
            match store.get_preview(id, nicti_lair::PreviewTier::T0) {
                Ok(Some(preview)) => {
                    if let Some(texture) =
                        preview_texture(ctx, format!("tile-t0-{id}"), &preview.bytes)
                    {
                        self.insert(
                            id,
                            Tile {
                                texture,
                                is_t2: false,
                            },
                        );
                    }
                }
                _ if self.sidecar_fetch.len() >= crate::t0_fetch::MAX_IN_FLIGHT => {
                    // At the cap (a read can block on a dead drive): try again shortly.
                    ctx.request_repaint_after(Duration::from_millis(100));
                }
                _ => match crate::t0_fetch::request(pounce, store, id) {
                    Some(slot) => {
                        self.sidecar_fetch.insert(id, slot);
                        ctx.request_repaint_after(Duration::from_millis(100));
                    }
                    None => {
                        self.no_t0.insert(id, std::time::Instant::now());
                    }
                },
            }
        }
        self.tiles.get(&id).map(|t| &t.texture)
    }

    /// Looks for a stored T2 (throttled) and swaps it in; queues its generation if there is none.
    fn try_upgrade(
        &mut self,
        ctx: &egui::Context,
        store: &dyn CatalogStore,
        pounce: &Pounce,
        id: i64,
    ) {
        let now = Instant::now();
        if self.next_check.get(&id).is_some_and(|at| now < *at) {
            return;
        }
        self.next_check.insert(id, now + UPGRADE_CHECK_EVERY);

        let Some(larder) = self.larder.clone() else {
            return;
        };
        let Ok(Some(asset)) = store.get_asset(id) else {
            return;
        };
        let render_hash = t2::render_hash(&asset_cache_key(&asset));
        let key = LarderKey {
            asset_id: id,
            tier: LarderTier::T2,
            render_hash: &render_hash,
        };
        // Busy Larder (a job is writing): try again at the next throttle tick.
        let Some(mut guard) = try_lock_larder(&larder) else {
            return;
        };
        let stored = guard.get(key).ok().flatten();
        drop(guard);

        match stored {
            Some(bytes) => match preview_texture(ctx, format!("tile-t2-{id}"), &bytes) {
                Some(texture) => self.insert(
                    id,
                    Tile {
                        texture,
                        is_t2: true,
                    },
                ),
                None => {
                    self.t2_gave_up.insert(id);
                }
            },
            None => {
                if self.t2_jobs.contains_key(&id) || self.t2_jobs.len() >= MAX_T2_JOBS {
                    // Over the cap: stay on T0 and look again at the next throttle tick.
                    return;
                }
                let Ok(Some(root)) = store.get_root_path(asset.root_id) else {
                    return;
                };
                let path = std::path::Path::new(&root).join(&asset.rel_path);
                let (job, slot) = T2Job::new(larder, path, id, render_hash, 0);
                let job_id = pounce.submit(Box::new(job));
                self.t2_jobs.insert(id, (job_id, slot));
            }
        }
    }

    fn insert(&mut self, id: i64, tile: Tile) {
        if self.tiles.insert(id, tile).is_none() {
            self.order.push_back(id);
        }
        while self.tiles.len() > MAX_TILES {
            match self.order.pop_front() {
                Some(old) if old != id => {
                    self.tiles.remove(&old);
                }
                Some(_) => self.order.push_back(id),
                None => break,
            }
        }
    }

    #[cfg(test)]
    fn has_t2(&self, id: i64) -> bool {
        self.tiles.get(&id).is_some_and(|t| t.is_t2)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.tiles.len()
    }
}

/// JPEG-decodes `bytes` into an egui texture, `None` if they aren't a decodable image.
pub fn preview_texture(
    ctx: &egui::Context,
    name: String,
    bytes: &[u8],
) -> Option<egui::TextureHandle> {
    let rgba = image::load_from_memory(bytes).ok()?.to_rgba8();
    let (w, h) = rgba.dimensions();
    let color_image =
        egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], rgba.as_raw());
    Some(ctx.load_texture(name, color_image, egui::TextureOptions::default()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::jobs::testutil::jpeg;
    use nicti_lair::larder::{Larder, LarderConfig};
    use nicti_lair::{NewAsset, Preview, SqliteCatalog};
    use std::sync::{Arc, Mutex};

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

    fn seeded(n: usize, with_preview: bool) -> (Arc<SqliteCatalog>, Vec<i64>) {
        let store = Arc::new(SqliteCatalog::open_in_memory().unwrap());
        let volume = store.upsert_volume("v", None, None, 0).unwrap();
        let root = store.ensure_root(volume, "/nowhere").unwrap();
        let blob = jpeg(64, 48, [30, 120, 200]);
        let ids = (0..n)
            .map(|i| {
                let preview = with_preview.then(|| Preview {
                    width: Some(64),
                    height: Some(48),
                    bytes: blob.clone(),
                });
                store
                    .insert_asset(root, &new_asset(&format!("{i}.NEF")), preview.as_ref())
                    .unwrap()
            })
            .collect();
        (store, ids)
    }

    fn pounce() -> Pounce {
        Pounce::new(0, 2, 2, || {})
    }

    #[test]
    fn a_tile_shows_the_t0_preview_and_keeps_the_same_texture() {
        let (store, ids) = seeded(1, true);
        let ctx = egui::Context::default();
        let pounce = pounce();
        let mut previews = TilePreviews::new(None);
        let first = previews
            .get(&ctx, &*store, &pounce, ids[0], Want::Small)
            .map(|t| (t.id(), t.size()));
        assert_eq!(first.map(|f| f.1), Some([64, 48]));
        let second = previews
            .get(&ctx, &*store, &pounce, ids[0], Want::Small)
            .map(|t| t.id());
        assert_eq!(second, first.map(|f| f.0), "decoded once, then cached");
    }

    #[test]
    fn a_photo_without_a_preview_yields_no_texture_instead_of_erroring() {
        let (store, ids) = seeded(1, false);
        let ctx = egui::Context::default();
        let mut previews = TilePreviews::new(None);
        assert!(previews
            .get(&ctx, &*store, &pounce(), ids[0], Want::Large)
            .is_none());
    }

    #[test]
    fn a_large_tile_upgrades_to_a_stored_t2_and_a_small_one_never_asks() {
        let (store, ids) = seeded(2, true);
        let dir = tempfile::tempdir().unwrap();
        let larder: SharedLarder = Arc::new(Mutex::new(
            Larder::open(&dir.path().join("larder"), LarderConfig::default()).unwrap(),
        ));
        // Store a bigger T2 for ids[0] only, keyed exactly as the loupe/T2Job key it.
        let asset = store.get_asset(ids[0]).unwrap().unwrap();
        let hash = t2::render_hash(&asset_cache_key(&asset));
        larder
            .lock()
            .unwrap()
            .put(
                LarderKey {
                    asset_id: ids[0],
                    tier: LarderTier::T2,
                    render_hash: &hash,
                },
                &jpeg(256, 192, [200, 40, 40]),
            )
            .unwrap();

        let ctx = egui::Context::default();
        let pounce = pounce();
        let mut previews = TilePreviews::new(Some(larder));

        let small = previews
            .get(&ctx, &*store, &pounce, ids[0], Want::Small)
            .map(|t| t.size());
        assert_eq!(small, Some([64, 48]), "a small tile stays on T0");
        assert!(!previews.has_t2(ids[0]));

        let large = previews
            .get(&ctx, &*store, &pounce, ids[0], Want::Large)
            .map(|t| t.size());
        assert_eq!(
            large,
            Some([256, 192]),
            "a large tile swaps in the stored T2"
        );
        assert!(previews.has_t2(ids[0]));
    }

    #[test]
    fn a_large_tile_without_a_stored_t2_shows_t0_and_queues_generation_once() {
        let (store, ids) = seeded(1, true);
        let dir = tempfile::tempdir().unwrap();
        let larder: SharedLarder = Arc::new(Mutex::new(
            Larder::open(&dir.path().join("larder"), LarderConfig::default()).unwrap(),
        ));
        let ctx = egui::Context::default();
        let pounce = pounce();
        let mut previews = TilePreviews::new(Some(larder));
        let size = previews
            .get(&ctx, &*store, &pounce, ids[0], Want::Large)
            .map(|t| t.size());
        assert_eq!(size, Some([64, 48]), "T0 shows while T2 is being made");
        assert_eq!(previews.t2_jobs.len(), 1);
        // Within the throttle window a second call must not queue another job.
        previews.get(&ctx, &*store, &pounce, ids[0], Want::Large);
        assert_eq!(previews.t2_jobs.len(), 1);
    }

    #[test]
    fn the_texture_cache_evicts_its_oldest_tile_past_the_cap() {
        let (store, ids) = seeded(MAX_TILES + 6, true);
        let ctx = egui::Context::default();
        let pounce = pounce();
        let mut previews = TilePreviews::new(None);
        for id in &ids {
            previews.get(&ctx, &*store, &pounce, *id, Want::Small);
        }
        assert_eq!(previews.len(), MAX_TILES);
        assert!(!previews.tiles.contains_key(&ids[0]), "the oldest went");
        assert!(previews.tiles.contains_key(ids.last().unwrap()));
    }

    #[test]
    fn forget_drops_a_deleted_photos_tile() {
        let (store, ids) = seeded(1, true);
        let ctx = egui::Context::default();
        let mut previews = TilePreviews::new(None);
        previews.get(&ctx, &*store, &pounce(), ids[0], Want::Small);
        previews.forget(ids[0]);
        assert_eq!(previews.len(), 0);
    }

    #[test]
    fn stepping_through_many_large_tiles_queues_only_a_few_t2_jobs() {
        let (store, ids) = seeded(12, true);
        let dir = tempfile::tempdir().unwrap();
        let larder: SharedLarder = Arc::new(Mutex::new(
            Larder::open(&dir.path().join("larder"), LarderConfig::default()).unwrap(),
        ));
        let ctx = egui::Context::default();
        let pounce = pounce();
        let mut previews = TilePreviews::new(Some(larder));
        for id in &ids {
            previews.get(&ctx, &*store, &pounce, *id, Want::Large);
        }
        assert_eq!(
            previews.t2_jobs.len(),
            MAX_T2_JOBS,
            "a held arrow key must not queue a job per photo"
        );
        // Every tile still shows something (T0) while it waits.
        assert_eq!(previews.len(), ids.len());
    }
}
