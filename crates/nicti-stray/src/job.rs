//! The LRC import as one cancellable Pounce job (`Lane::Cpu`, `Priority::Background`,
//! `JobKind::Import`). `step()` does one bounded chunk and yields, through these phases:
//!
//! 1. **Open** -- read-only, live-catalog guard, schema check; resolve each root to a local path.
//! 2. **Ingest** -- per existing root: register it (same placeholder volume as the Import button)
//!    and run the normal Scruff pass one file per chunk, so fingerprints/previews are computed
//!    fresh (ADR-0158) and sidecars are read as usual.
//! 3. **Match** -- LRC image -> cataloged asset by `(root, folded rel_path)`. Unmatched images are
//!    counted and skipped, never invented.
//! 4. **Keywords** / 5. **Collections** -- additive only (never untag, never delete), found-or-
//!    created by name so a re-run adds nothing.
//! 6. **Items** -- markers, virtual copies, translated edit + provenance, 500 images per chunk,
//!    each chunk one transaction (`CatalogStore::apply_lrc_chunk`).
//! 7. **Dirty** -- mark every asset whose markers changed catalog-dirty so the XMP sidecar sync
//!    does not revert them on the next rescan.
//!
//! A failure never makes `step` return `Err`: Pounce would drop the job without touching the slot
//! and the UI would wait forever (the `RemoveJob` gotcha). It lands in `LrcImportReport::error`
//! and the slot always resolves -- including on cancel, via `Drop`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use nicti_lair::pounce_jobs::ReportSlot;
use nicti_lair::scent_sync::{mark_catalog_dirty_many, norm_segment};
use nicti_lair::scruff::{fold, register_root, Ingest};
use nicti_lair::{
    AssetMeta, CatalogStore, Collection, CollectionKind as NictiCollectionKind, LrcItem,
    LrcProvenance,
};
use nicti_pawprint::StageEntry;
use nicti_pounce::{ChunkedJob, JobError, JobKind, JobSpec, Lane, Priority, Progress, Step};
use nicti_tapetum::coat::CameraProfileParams;
use nicti_tapetum::stages::WORKING_SPACE;
use rusqlite::Connection;

use crate::develop::{translate, Context};
use crate::open::open_validated;
use crate::paths::{resolve_root_path, RootRemap};
use crate::read::{CollectionKind, LrcCollection, LrcImage, LrcKeyword, LrcRoot, Reader};
use crate::report::{LrcImportReport, RootReport, MAX_MISSING_EXAMPLES};
use crate::StrayError;

/// Images read per Match chunk (paths only -- cheap).
const MATCH_PAGE: usize = 5000;
/// Images translated and applied per Items chunk (develop text is the heavy part).
const ITEM_PAGE: usize = 500;
/// Assets marked dirty per chunk.
const DIRTY_PAGE: usize = 500;

#[derive(Debug, Clone)]
pub struct ImportConfig {
    /// A closed `.lrcat` backup.
    pub catalog_path: PathBuf,
    /// "This LRC path prefix now lives here" rules, applied before drive-letter mapping.
    pub remaps: Vec<RootRemap>,
    /// Import only these LRC root folder ids (`AgLibraryRootFolder.id_local`); empty = all roots.
    pub only_roots: Vec<i64>,
    /// Resolves LRC camera-profile names to installed `.dcp`/Look files (#381). `None` = profiles
    /// are not imported (each such photo is counted in `LrcImportReport::profiles_missing`).
    pub profile_resolver: Option<crate::resolver::ResolverHandle>,
}

struct RootWork {
    lrc: LrcRoot,
    local_path: PathBuf,
    exists: bool,
    local_root_id: Option<i64>,
}

enum State {
    Open,
    Ingest {
        root: usize,
        ingest: Option<Ingest>,
    },
    Match {
        root: usize,
        after: i64,
        fold_map: Option<HashMap<String, i64>>,
    },
    Keywords {
        list: Option<Vec<LrcKeyword>>,
        next: usize,
    },
    Collections {
        plan: Option<CollectionPlan>,
        next: usize,
    },
    Items {
        after: i64,
    },
    Dirty {
        ids: Option<Vec<i64>>,
        next: usize,
    },
    Done,
}

struct CollectionPlan {
    /// Creation order: parents before children.
    order: Vec<LrcCollection>,
    /// LRC collection id -> nicti collection id, filled as they are created.
    made: HashMap<i64, i64>,
    existing: Vec<Collection>,
}

pub struct LrcImportJob {
    store: Arc<dyn CatalogStore + Send + Sync>,
    config: ImportConfig,
    conn: Option<Connection>,
    state: State,
    report: LrcImportReport,
    result: ReportSlot<LrcImportReport>,
    progress: Progress,
    finished: bool,
    roots: Vec<RootWork>,
    /// `Adobe_images.id_local` -> asset id, for every matched image (virtual copies map to their
    /// master's asset: they share its file).
    image_asset: HashMap<i64, i64>,
    asset_dims: HashMap<i64, (u32, u32)>,
    /// Asset id -> (make, model), for resolving a camera profile by name. Only assets whose EXIF
    /// carried both are present.
    asset_cameras: HashMap<i64, (String, String)>,
    /// One resolution per (make, model, profile name) per run: the resolver reads the disk.
    profile_cache: HashMap<(String, String, String), Option<CameraProfileParams>>,
    total_images: u64,
    done_images: u64,
    tagged: HashSet<i64>,
    meta_changed: HashSet<i64>,
}

impl LrcImportJob {
    pub fn new(
        store: Arc<dyn CatalogStore + Send + Sync>,
        config: ImportConfig,
    ) -> (Self, ReportSlot<LrcImportReport>) {
        let result = Arc::new(Mutex::new(None));
        let job = LrcImportJob {
            store,
            config,
            conn: None,
            state: State::Open,
            report: LrcImportReport::default(),
            result: result.clone(),
            progress: Progress::default(),
            finished: false,
            roots: Vec::new(),
            image_asset: HashMap::new(),
            asset_dims: HashMap::new(),
            asset_cameras: HashMap::new(),
            profile_cache: HashMap::new(),
            total_images: 0,
            done_images: 0,
            tagged: HashSet::new(),
            meta_changed: HashSet::new(),
        };
        (job, result)
    }

    fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.report.assets_tagged = self.tagged.len() as u64;
        *self.result.lock().unwrap() = Some(std::mem::take(&mut self.report));
    }

    fn set_progress(&mut self, total: Option<u64>, done: u64) {
        self.progress = Progress { done, total };
    }

    /// Index of the first existing root at or after `from`.
    fn next_existing(&self, from: usize) -> Option<usize> {
        (from..self.roots.len()).find(|&i| self.roots[i].exists)
    }

    fn run(&mut self, conn: &Connection) -> Result<Step, StrayError> {
        // Ingest runs one file per step (hundreds of thousands of steps) and Dirty never reads the
        // LRC catalog: neither pays for `Reader::new`'s schema probes.
        match std::mem::replace(&mut self.state, State::Done) {
            State::Ingest { root, ingest } => return self.ingest(root, ingest),
            State::Dirty { ids, next } => return self.dirty(ids, next),
            State::Done => return Ok(Step::Done),
            other => self.state = other,
        }
        let reader = Reader::new(conn)?;
        match std::mem::replace(&mut self.state, State::Done) {
            State::Open => self.open(&reader),
            State::Match {
                root,
                after,
                fold_map,
            } => self.match_images(&reader, root, after, fold_map),
            State::Keywords { list, next } => self.keywords(&reader, list, next),
            State::Collections { plan, next } => self.collections(&reader, plan, next),
            State::Items { after } => self.items(&reader, after),
            State::Ingest { .. } | State::Dirty { .. } | State::Done => {
                unreachable!("handled above")
            }
        }
    }

    fn open(&mut self, reader: &Reader) -> Result<Step, StrayError> {
        for root in reader.roots()? {
            if !self.config.only_roots.is_empty() && !self.config.only_roots.contains(&root.id) {
                continue;
            }
            let local_path = resolve_root_path(&root.absolute_path, &self.config.remaps);
            let exists = local_path.is_dir();
            let lrc_images = reader.image_count(&[root.id])?;
            self.total_images += lrc_images;
            self.report.roots.push(RootReport {
                lrc_root_id: root.id,
                lrc_path: root.absolute_path.clone(),
                local_path: local_path.display().to_string(),
                exists,
                lrc_images,
                missing: if exists { 0 } else { lrc_images },
                ..RootReport::default()
            });
            self.roots.push(RootWork {
                lrc: root,
                local_path,
                exists,
                local_root_id: None,
            });
        }
        self.state = match self.next_existing(0) {
            Some(root) => State::Ingest { root, ingest: None },
            None => State::Keywords {
                list: None,
                next: 0,
            },
        };
        Ok(Step::Yield)
    }

    fn ingest(&mut self, root: usize, ingest: Option<Ingest>) -> Result<Step, StrayError> {
        let mut ingest = match ingest {
            Some(i) => i,
            None => {
                let local_root_id =
                    register_root(self.store.as_ref(), &self.roots[root].local_path)?;
                self.roots[root].local_root_id = Some(local_root_id);
                Ingest::new(local_root_id, &self.roots[root].local_path)
            }
        };
        let more = ingest.step(self.store.as_ref())?;
        self.set_progress(None, ingest.processed_count());
        if more {
            self.state = State::Ingest {
                root,
                ingest: Some(ingest),
            };
            return Ok(Step::Yield);
        }
        let r = ingest.into_report();
        let rep = &mut self.report.roots[root];
        rep.ingested = r.added + r.updated + r.moved + r.skipped_unchanged;
        rep.ingest_failed = r.failed.len() as u64;
        self.state = match self.next_existing(root + 1) {
            Some(next) => State::Ingest {
                root: next,
                ingest: None,
            },
            None => State::Match {
                root: self.next_existing(0).unwrap_or(0),
                after: 0,
                fold_map: None,
            },
        };
        Ok(Step::Yield)
    }

    fn match_images(
        &mut self,
        reader: &Reader,
        root: usize,
        after: i64,
        fold_map: Option<HashMap<String, i64>>,
    ) -> Result<Step, StrayError> {
        let local_root_id = self.roots[root]
            .local_root_id
            .expect("existing roots are registered");
        let fold_map = match fold_map {
            Some(m) => m,
            None => {
                let mut map = HashMap::new();
                for asset in self.store.list_assets_by_root(local_root_id)? {
                    if let (Some(w), Some(h)) = (asset.width, asset.height) {
                        self.asset_dims.insert(asset.id, (w, h));
                    }
                    if let (Some(make), Some(model)) = (&asset.make, &asset.model) {
                        self.asset_cameras
                            .insert(asset.id, (make.clone(), model.clone()));
                    }
                    map.insert(asset.rel_path_fold, asset.id);
                }
                map
            }
        };
        let page = reader.images_page(&[self.roots[root].lrc.id], after, MATCH_PAGE, false)?;
        let Some(last) = page.last().map(|i| i.id_local) else {
            self.state = match self.next_existing(root + 1) {
                Some(next) => State::Match {
                    root: next,
                    after: 0,
                    fold_map: None,
                },
                None => State::Keywords {
                    list: None,
                    next: 0,
                },
            };
            return Ok(Step::Yield);
        };
        for img in &page {
            let rep = &mut self.report.roots[root];
            match fold_map.get(&fold(&img.rel_path)) {
                Some(&asset) => {
                    self.image_asset.insert(img.id_local, asset);
                    rep.matched += 1;
                }
                None => {
                    rep.missing += 1;
                    if rep.missing_examples.len() < MAX_MISSING_EXAMPLES {
                        rep.missing_examples.push(img.rel_path.clone());
                    }
                }
            }
        }
        self.done_images += page.len() as u64;
        self.set_progress(Some(self.total_images), self.done_images);
        self.state = State::Match {
            root,
            after: last,
            fold_map: Some(fold_map),
        };
        Ok(Step::Yield)
    }

    fn keywords(
        &mut self,
        reader: &Reader,
        list: Option<Vec<LrcKeyword>>,
        next: usize,
    ) -> Result<Step, StrayError> {
        let list = match list {
            Some(l) => l,
            None => reader.keywords()?,
        };
        let Some(kw) = list.get(next) else {
            self.state = State::Collections {
                plan: None,
                next: 0,
            };
            return Ok(Step::Yield);
        };
        let segs: Vec<String> = kw
            .path
            .iter()
            .map(|s| norm_segment(s))
            .filter(|s| !s.is_empty())
            .collect();
        let mut ids: Vec<i64> = reader
            .keyword_images(kw.id)?
            .into_iter()
            .filter_map(|image| self.image_asset.get(&image).copied())
            .collect();
        ids.sort_unstable();
        ids.dedup();
        // Importing a subset of roots must not create the whole keyword tree for nothing.
        let wanted = !segs.is_empty() && (self.config.only_roots.is_empty() || !ids.is_empty());
        if wanted {
            let mut parent: Option<i64> = None;
            for i in 0..segs.len() {
                let path: Vec<&str> = segs[..=i].iter().map(String::as_str).collect();
                match self.store.keyword_by_path(&path)? {
                    Some(k) => parent = Some(k.id),
                    None => {
                        parent = Some(self.store.create_keyword(parent, &segs[i])?);
                        self.report.keywords_created += 1;
                    }
                }
            }
            if let Some(keyword_id) = parent {
                for chunk in ids.chunks(5000) {
                    self.store.tag(chunk, keyword_id)?;
                }
                self.tagged.extend(ids);
            }
        }
        self.state = State::Keywords {
            list: Some(list),
            next: next + 1,
        };
        Ok(Step::Yield)
    }

    fn collections(
        &mut self,
        reader: &Reader,
        plan: Option<CollectionPlan>,
        next: usize,
    ) -> Result<Step, StrayError> {
        let mut plan = match plan {
            Some(p) => p,
            None => {
                let (mut all, smart) = reader.collections()?;
                self.report.smart_collections_skipped = smart;
                if !self.config.only_roots.is_empty() {
                    // A subset import keeps only collections that hold a matched photo, and the
                    // sets above them -- not every collection in the catalog, mostly empty.
                    let mut keep: HashSet<i64> = HashSet::new();
                    for c in all.iter().filter(|c| c.kind == CollectionKind::Manual) {
                        if reader
                            .collection_images(c.id)?
                            .iter()
                            .any(|image| self.image_asset.contains_key(image))
                        {
                            keep.insert(c.id);
                        }
                    }
                    let parents: HashMap<i64, Option<i64>> =
                        all.iter().map(|c| (c.id, c.parent)).collect();
                    for id in keep.clone() {
                        let mut at = parents.get(&id).copied().flatten();
                        while let Some(p) = at {
                            if !keep.insert(p) {
                                break;
                            }
                            at = parents.get(&p).copied().flatten();
                        }
                    }
                    all.retain(|c| keep.contains(&c.id));
                }
                CollectionPlan {
                    order: parents_first(all),
                    made: HashMap::new(),
                    existing: self.store.list_collections()?,
                }
            }
        };
        let Some(col) = plan.order.get(next).cloned() else {
            self.state = State::Items { after: 0 };
            return Ok(Step::Yield);
        };
        // A parent that was skipped or unknown becomes top-level.
        let parent = col.parent.and_then(|p| plan.made.get(&p).copied());
        let wanted = fold(&col.name);
        let found = plan.existing.iter().find(|c| {
            c.parent_id == parent
                && c.kind == NictiCollectionKind::Manual
                && fold(&c.name) == wanted
        });
        let id = match found {
            Some(c) => c.id,
            None => {
                let id =
                    self.store
                        .create_collection(parent, &col.name, NictiCollectionKind::Manual)?;
                plan.existing.push(Collection {
                    id,
                    parent_id: parent,
                    kind: NictiCollectionKind::Manual,
                    name: col.name.clone(),
                });
                self.report.collections_created += 1;
                id
            }
        };
        plan.made.insert(col.id, id);
        if col.kind == CollectionKind::Manual {
            let mut seen = HashSet::new();
            let ids: Vec<i64> = reader
                .collection_images(col.id)?
                .into_iter()
                .filter_map(|image| self.image_asset.get(&image).copied())
                .filter(|asset| seen.insert(*asset))
                .collect();
            for chunk in ids.chunks(5000) {
                self.store.add_to_collection(id, chunk)?;
            }
            self.report.collection_members += ids.len() as u64;
        }
        self.state = State::Collections {
            plan: Some(plan),
            next: next + 1,
        };
        Ok(Step::Yield)
    }

    fn items(&mut self, reader: &Reader, after: i64) -> Result<Step, StrayError> {
        if after == 0 {
            self.done_images = 0;
        }
        let page = reader.images_page(&self.config.only_roots, after, ITEM_PAGE, true)?;
        let Some(last) = page.last().map(|i| i.id_local) else {
            let mut ids: Vec<i64> = self
                .tagged
                .iter()
                .chain(&self.meta_changed)
                .copied()
                .collect();
            ids.sort_unstable();
            ids.dedup();
            self.state = State::Dirty {
                ids: Some(ids),
                next: 0,
            };
            return Ok(Step::Yield);
        };
        let mut items = Vec::with_capacity(page.len());
        for img in &page {
            if let Some(item) = self.item_for(img) {
                items.push(item);
            }
        }
        let outcome = self.store.apply_lrc_chunk(&items)?;
        self.report.docs_written += outcome.docs_written;
        self.report.kept_local_docs += outcome.kept_local_docs;
        self.report.meta_applied += outcome.meta_applied;
        self.report.kept_local_meta += outcome.kept_local_meta;
        self.meta_changed.extend(outcome.meta_changed_assets);
        self.done_images += page.len() as u64;
        self.set_progress(Some(self.total_images), self.done_images);
        self.state = State::Items { after: last };
        Ok(Step::Yield)
    }

    /// LRC's profile `name` for this asset's camera -> the stage params that select it, or `None`.
    fn resolve_profile(&mut self, asset_id: i64, name: &str) -> Option<CameraProfileParams> {
        let resolver = self.config.profile_resolver.as_ref()?.0.clone();
        // An unknown camera is passed through as empty strings: it is the resolver's call that
        // nothing can be matched without one (and a test resolver does not need EXIF).
        let (make, model) = self
            .asset_cameras
            .get(&asset_id)
            .cloned()
            .unwrap_or_default();
        self.profile_cache
            .entry((make.clone(), model.clone(), name.to_string()))
            .or_insert_with(|| resolver.resolve(&make, &model, name))
            .clone()
    }

    /// One image -> its chunk item, folding develop-translation counters into the report. `None`
    /// when the image has no matched asset.
    fn item_for(&mut self, img: &LrcImage) -> Option<LrcItem> {
        let asset_id = *self.image_asset.get(&img.id_local)?;
        let is_copy = img.master_image.is_some();
        if is_copy {
            self.report.virtual_copies += 1;
        }
        // LRC's own file size first: `asset.width/height` is the T0 *preview's* declared size,
        // which is smaller than the sensor frame the crop pixels refer to.
        let (width, height) = img
            .file_width
            .zip(img.file_height)
            .map(|(w, h)| (w as f32, h as f32))
            .or_else(|| {
                self.asset_dims
                    .get(&asset_id)
                    .map(|&(w, h)| (w as f32, h as f32))
            })
            .map_or((None, None), |(w, h)| (Some(w), Some(h)));

        let mut untranslated = Vec::new();
        let mut document = None;
        if let Some(dev) = &img.develop {
            let ctx = Context {
                width,
                height,
                orientation: img.orientation.clone(),
                process_version: dev.process_version.clone(),
            };
            match translate(&dev.text, &ctx) {
                Ok(mut t) => {
                    if let Some(name) = t.camera_profile.take() {
                        match self.resolve_profile(asset_id, &name) {
                            Some(params) => {
                                if let Ok(value) = serde_json::to_value(&params) {
                                    t.document.stages.insert(
                                        WORKING_SPACE.to_string(),
                                        StageEntry {
                                            schema_version: 1,
                                            params: value,
                                        },
                                    );
                                    self.report.profiles_resolved += 1;
                                }
                            }
                            None => {
                                *self.report.profiles_missing.entry(name).or_default() += 1;
                                t.untranslated.push("CameraProfile".to_string());
                                t.untranslated.sort();
                            }
                        }
                    }
                    for key in &t.untranslated {
                        *self.report.untranslated.entry(key.clone()).or_default() += 1;
                    }
                    self.report.filters.add(&t.filters);
                    self.report.stats.add(&t.stats);
                    untranslated = t.untranslated;
                    document = Some(t.document);
                }
                Err(_) => self.report.develop_parse_failures += 1,
            }
        }

        let meta = (!is_copy).then(|| AssetMeta {
            // Reject wins over stars (nicti keeps reject on `rating`); the stars stay in provenance.
            rating: if img.pick == Some(-1.0) {
                Some(-1)
            } else {
                img.rating.map(|r| (r.round() as i64).clamp(0, 5))
            },
            flag: (img.pick == Some(1.0)).then_some(1),
            label: img.color_label.clone().filter(|l| !l.trim().is_empty()),
        });
        Some(LrcItem {
            asset_id,
            meta,
            variant_name: is_copy.then(|| {
                img.copy_name
                    .clone()
                    .filter(|n| !n.trim().is_empty())
                    .unwrap_or_else(|| "Virtual Copy".to_string())
            }),
            document,
            provenance: LrcProvenance {
                image_global: img.id_global.clone(),
                image_local: img.id_local,
                import_hash: img.import_hash.clone(),
                process_version: img.develop.as_ref().and_then(|d| d.process_version.clone()),
                develop_text: img.develop.as_ref().map(|d| d.text.clone()),
                has_masks: img.develop.as_ref().and_then(|d| d.has_masks),
                has_ai_masks: img.develop.as_ref().and_then(|d| d.has_ai_masks),
                has_big_data: img.develop.as_ref().and_then(|d| d.has_big_data),
                iptc_caption: img.iptc_caption.clone().filter(|s| !s.is_empty()),
                iptc_copyright: img.iptc_copyright.clone().filter(|s| !s.is_empty()),
                lrc_rating: img.rating,
                lrc_pick: img.pick,
                untranslated,
            },
        })
    }

    fn dirty(&mut self, ids: Option<Vec<i64>>, next: usize) -> Result<Step, StrayError> {
        let ids = ids.unwrap_or_default();
        if next >= ids.len() {
            self.finish();
            return Ok(Step::Done);
        }
        let root_paths: HashMap<i64, &PathBuf> = self
            .roots
            .iter()
            .filter_map(|r| r.local_root_id.map(|id| (id, &r.local_path)))
            .collect();
        let mut batch = Vec::new();
        for &id in &ids[next..(next + DIRTY_PAGE).min(ids.len())] {
            if let Some(asset) = self.store.get_asset(id)? {
                if let Some(root) = root_paths.get(&asset.root_id) {
                    batch.push((id, root.join(&asset.rel_path)));
                }
            }
        }
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        mark_catalog_dirty_many(self.store.as_ref(), &batch, now_ms)
            .map_err(|e| StrayError::Io(e.to_string()))?;
        self.report.sidecars_marked += batch.len() as u64;
        let done = (next + DIRTY_PAGE).min(ids.len());
        self.set_progress(Some(ids.len() as u64), done as u64);
        self.state = State::Dirty {
            ids: Some(ids),
            next: next + DIRTY_PAGE,
        };
        Ok(Step::Yield)
    }
}

/// Collections and sets ordered so every parent precedes its children; a parent that isn't in the
/// list (a skipped kind) makes the child a root. Cycle-safe: whatever never resolves is appended
/// in id order rather than looping.
fn parents_first(all: Vec<LrcCollection>) -> Vec<LrcCollection> {
    let known: HashSet<i64> = all.iter().map(|c| c.id).collect();
    let mut placed: HashSet<i64> = HashSet::new();
    let mut out = Vec::with_capacity(all.len());
    let mut remaining = all;
    loop {
        let before = remaining.len();
        let mut later = Vec::new();
        for c in remaining {
            let ready = c
                .parent
                .is_none_or(|p| !known.contains(&p) || placed.contains(&p));
            if ready {
                placed.insert(c.id);
                out.push(c);
            } else {
                later.push(c);
            }
        }
        remaining = later;
        if remaining.is_empty() || remaining.len() == before {
            out.append(&mut remaining);
            return out;
        }
    }
}

impl ChunkedJob for LrcImportJob {
    fn spec(&self) -> JobSpec {
        JobSpec {
            priority: Priority::Background,
            kind: JobKind::Import,
            lane: Lane::Cpu,
            vram_bytes: 0,
            image_index: None,
        }
    }

    fn label(&self) -> String {
        format!("Import LRC catalog: {}", self.config.catalog_path.display())
    }

    fn progress(&self) -> Progress {
        self.progress
    }

    fn step(&mut self) -> Result<Step, JobError> {
        if self.finished {
            return Ok(Step::Done);
        }
        // Open the catalog on the first step; keep the connection across steps.
        let conn = match self.conn.take() {
            Some(c) => c,
            None => match open_validated(&self.config.catalog_path) {
                Ok(c) => c,
                Err(e) => {
                    self.report.error = Some(e.to_string());
                    self.finish();
                    return Ok(Step::Done);
                }
            },
        };
        let outcome = self.run(&conn);
        self.conn = Some(conn);
        match outcome {
            Ok(step) => Ok(step),
            Err(e) => {
                self.report.error = Some(e.to_string());
                self.finish();
                Ok(Step::Done)
            }
        }
    }
}

impl Drop for LrcImportJob {
    /// Pounce drops a cancelled job without finishing it: resolve the slot so a poller never
    /// waits on it, flagged cancelled. What was already applied stays applied (chunks are whole).
    fn drop(&mut self) {
        if !self.finished {
            self.report.cancelled = true;
            self.finish();
        }
    }
}
