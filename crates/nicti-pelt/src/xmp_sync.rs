//! XMP sidecar write-back for culling marker changes (#60, ADR-0059), plus the settings/review
//! panel for it.
//!
//! The cull writer thread (`cull/worker.rs`) must stay fast, so it never touches sidecar files:
//! [`XmpMeta`] wraps the catalog's `MetaStore`, and after every successful `set_meta` (apply,
//! undo and redo all go through it) hands the affected ids to a separate [`XmpWriter`] thread.
//! That thread resolves each asset's RAW path and calls `nicti_lair::scent_sync::write_sidecar`
//! -- or, with auto-write off, just records that the catalog is now newer than the sidecar
//! (`mark_catalog_dirty`) so a later import resolves the conflict correctly.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nicti_lair::scent_sync::{self, SyncOutcome};
use nicti_lair::{AssetMeta, CatalogStore, Keyword};

use crate::cull::worker::{CatalogMeta, MetaStore};

type Store = Arc<dyn CatalogStore + Send + Sync>;

const REVIEW_REFRESH: Duration = Duration::from_secs(2);
/// The review list shows this many rows; a conflict storm beyond it is summarised, not listed.
const REVIEW_ROWS: usize = 50;

/// Where the auto-write preference lives: a sibling of the catalog file.
pub fn autowrite_file_for(catalog_path: &Path) -> PathBuf {
    let mut name = catalog_path.as_os_str().to_owned();
    name.push(".xmp-autowrite.json");
    PathBuf::from(name)
}

/// The saved preference; on by default (LRC's own "Automatically write changes into XMP" is the
/// model), and a missing, unreadable or corrupt file also means on.
pub fn load_autowrite(catalog_path: &Path) -> bool {
    std::fs::read_to_string(autowrite_file_for(catalog_path))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.get("autowrite")?.as_bool())
        .unwrap_or(true)
}

/// Temp file + rename, so a crash never leaves a half-written document.
pub fn save_autowrite(catalog_path: &Path, on: bool) -> std::io::Result<()> {
    let path = autowrite_file_for(catalog_path);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::json!({ "autowrite": on }).to_string())?;
    std::fs::rename(&tmp, &path)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

/// An asset's RAW path as the catalog resolves it, and whether the file is actually there. The
/// path is still meaningful when it isn't (an offline volume): dirtiness is recorded against it
/// without ever creating a sidecar in a directory that may not exist.
fn raw_path_for(store: &dyn CatalogStore, asset_id: i64) -> Option<(PathBuf, bool)> {
    let asset = store.get_asset(asset_id).ok()??;
    let root = store.get_root_path(asset.root_id).ok()??;
    let raw = Path::new(&root).join(&asset.rel_path);
    let exists = raw.exists();
    Some((raw, exists))
}

/// Ids for the writer thread. `flush` ids are the ones left dirty while auto-write was off (#375):
/// they are written only if auto-write is still on when the writer reaches them, and are never
/// re-marked dirty (they already are, or an edit has since written them).
struct Batch {
    ids: Vec<i64>,
    flush: bool,
}

#[derive(Default)]
struct Status {
    last_error: Option<String>,
}

/// Handle to the sidecar writer thread. Cheap to clone; the thread exits when every clone is
/// dropped.
#[derive(Clone)]
pub struct XmpWriter {
    tx: Sender<Batch>,
    store: Store,
    autowrite: Arc<AtomicBool>,
    status: Arc<Mutex<Status>>,
}

impl XmpWriter {
    pub fn spawn(store: Store, autowrite: bool) -> Self {
        let (tx, rx) = channel::<Batch>();
        let autowrite = Arc::new(AtomicBool::new(autowrite));
        let status = Arc::new(Mutex::new(Status::default()));
        let (flag, st) = (autowrite.clone(), status.clone());
        let writer_store = store.clone();
        std::thread::Builder::new()
            .name("nicti-xmp-writer".into())
            .spawn(move || {
                while let Ok(first) = rx.recv() {
                    // Holding a key queues many commands; coalesce what's already waiting so each
                    // photo's sidecar is written once per burst, with its latest markers.
                    let mut ids: BTreeSet<i64> = BTreeSet::new();
                    let mut flush_ids: BTreeSet<i64> = BTreeSet::new();
                    let mut take = |b: Batch| {
                        if b.flush { &mut flush_ids } else { &mut ids }.extend(b.ids);
                    };
                    take(first);
                    while let Ok(more) = rx.try_recv() {
                        take(more);
                    }
                    flush_ids.retain(|id| !ids.contains(id));
                    let flushing = flag.load(Ordering::Relaxed);
                    let work = ids
                        .into_iter()
                        .map(|id| (id, false))
                        .chain(flush_ids.into_iter().map(|id| (id, true)));
                    for (id, is_flush) in work {
                        if is_flush && !flushing {
                            continue;
                        }
                        let Some((raw, exists)) = raw_path_for(&*writer_store, id) else {
                            continue;
                        };
                        // An offline RAW can't get a sidecar, but the edit must still be recorded
                        // as newer so a later rescan doesn't let a changed sidecar win over it.
                        let result = if flag.load(Ordering::Relaxed) && exists {
                            scent_sync::write_sidecar(&*writer_store, id, &raw, now_ms())
                                .map(|_| ())
                        } else {
                            scent_sync::mark_catalog_dirty(&*writer_store, id, &raw, now_ms())
                        };
                        if let Err(e) = result {
                            // A failed write (sidecar locked by LRC, unparseable at that moment)
                            // must not look like "nothing changed": record that the catalog is
                            // now newer, so a later rescan doesn't let the file silently win.
                            let _ =
                                scent_sync::mark_catalog_dirty(&*writer_store, id, &raw, now_ms());
                            st.lock().unwrap().last_error = Some(e.to_string());
                        }
                    }
                }
            })
            .expect("spawn the XMP writer thread");
        XmpWriter {
            tx,
            store,
            autowrite,
            status,
        }
    }

    pub fn queue(&self, ids: Vec<i64>) {
        if !ids.is_empty() {
            let _ = self.tx.send(Batch { ids, flush: false });
        }
    }

    pub fn autowrite(&self) -> bool {
        self.autowrite.load(Ordering::Relaxed)
    }

    /// Turning auto-write back on also queues every asset left dirty while it was off (#375), so
    /// those sidecars catch up now rather than at their next marker change. The catalog read runs
    /// off the calling (UI) thread.
    pub fn set_autowrite(&self, on: bool) {
        let was = self.autowrite.swap(on, Ordering::Relaxed);
        if on && !was {
            let (store, tx, status) = (self.store.clone(), self.tx.clone(), self.status.clone());
            std::thread::spawn(move || match store.sidecar_dirty_assets() {
                Ok(ids) if !ids.is_empty() => {
                    let _ = tx.send(Batch { ids, flush: true });
                }
                Ok(_) => {}
                Err(e) => {
                    status.lock().unwrap().last_error =
                        Some(format!("Couldn't list assets awaiting sidecar write: {e}"));
                }
            });
        }
    }

    fn last_error(&self) -> Option<String> {
        self.status.lock().unwrap().last_error.clone()
    }

    fn clear_error(&self) {
        self.status.lock().unwrap().last_error = None;
    }
}

/// The real `MetaStore`: the catalog, plus sidecar write-back after each successful write.
pub struct XmpMeta {
    inner: Arc<dyn MetaStore>,
    /// The catalog keyword writes (`tag_path`/`untag`) go to; the sidecar write-back is the same
    /// `writer` the marker writes use.
    store: Store,
    writer: XmpWriter,
}

impl XmpMeta {
    pub fn new(store: Store, writer: XmpWriter) -> Self {
        Self::wrapping(Arc::new(CatalogMeta(store.clone())), store, writer)
    }

    fn wrapping(inner: Arc<dyn MetaStore>, store: Store, writer: XmpWriter) -> Self {
        XmpMeta {
            inner,
            store,
            writer,
        }
    }

    /// Tags `ids` with the keyword named by `text` (#373), creating it -- and any missing
    /// ancestors -- on the way. `text` is a `|`-separated path (`Events|Anthrocon`), the same
    /// separator LRC's `lr:hierarchicalSubject` uses; segments are normalised exactly as the
    /// sidecar round trip normalises them, so what's typed here compares equal to what's read
    /// back. Returns the leaf keyword, or `Ok(None)` for blank input. The sidecar write-back is
    /// queued only after the catalog write succeeded.
    pub fn tag_path(&self, ids: &[i64], text: &str) -> Result<Option<Keyword>, String> {
        let path: Vec<String> = text
            .split('|')
            .map(|seg| scent_sync::norm_segment(&seg.replace(char::is_control, "")))
            .filter(|s| !s.is_empty())
            .collect();
        if path.is_empty() || ids.is_empty() {
            return Ok(None);
        }
        let e = |e: nicti_lair::CatalogError| e.to_string();
        let mut leaf: Option<Keyword> = None;
        for i in 0..path.len() {
            let segs: Vec<&str> = path[..=i].iter().map(String::as_str).collect();
            leaf = Some(match self.store.keyword_by_path(&segs).map_err(e)? {
                Some(k) => k,
                None => {
                    let parent = leaf.as_ref().map(|k| k.id);
                    let id = self.store.create_keyword(parent, &path[i]).map_err(e)?;
                    Keyword {
                        id,
                        parent_id: parent,
                        name: path[i].clone(),
                        path: String::new(),
                    }
                }
            });
        }
        let leaf = leaf.expect("path is non-empty");
        // Chunked like the marker writes (`cull::worker::CHUNK`): one transaction holds the catalog
        // mutex, which stalls the grid's reads on a huge selection. Queue what landed even if a
        // later chunk fails, so those sidecars still follow the catalog.
        let mut result = Ok(());
        for (n, chunk) in ids.chunks(crate::cull::worker::CHUNK).enumerate() {
            if let Err(err) = self.store.tag(chunk, leaf.id) {
                self.writer
                    .queue(ids[..n * crate::cull::worker::CHUNK].to_vec());
                result = Err(err);
                break;
            }
        }
        result.map_err(e)?;
        self.writer.queue(ids.to_vec());
        Ok(Some(leaf))
    }

    /// Removes `keyword_id` from `ids` (#373) and queues the sidecar write-back.
    pub fn untag(&self, ids: &[i64], keyword_id: i64) -> Result<(), String> {
        self.store
            .untag(ids, keyword_id)
            .map_err(|e| e.to_string())?;
        self.writer.queue(ids.to_vec());
        Ok(())
    }
}

impl MetaStore for XmpMeta {
    fn get_meta(&self, ids: &[i64]) -> Result<HashMap<i64, AssetMeta>, String> {
        self.inner.get_meta(ids)
    }

    fn set_meta(&self, items: &[(i64, AssetMeta)]) -> Result<(), String> {
        self.inner.set_meta(items)?;
        self.writer.queue(items.iter().map(|(id, _)| *id).collect());
        Ok(())
    }
}

/// UI-only state for the panel.
#[derive(Default)]
pub struct XmpUi {
    review: Vec<(i64, String)>,
    review_total: usize,
    last_refresh: Option<Instant>,
    message: Option<String>,
}

impl XmpUi {
    fn refresh(&mut self, store: &dyn CatalogStore) {
        if self
            .last_refresh
            .is_some_and(|t| t.elapsed() < REVIEW_REFRESH)
        {
            return;
        }
        self.last_refresh = Some(Instant::now());
        let ids = store.sidecar_review_assets().unwrap_or_default();
        self.review_total = ids.len();
        self.review = ids
            .into_iter()
            .take(REVIEW_ROWS)
            .map(|id| {
                let name = store
                    .get_asset(id)
                    .ok()
                    .flatten()
                    .map(|a| a.rel_path)
                    .unwrap_or_else(|| format!("asset {id}"));
                (id, name)
            })
            .collect();
    }
}

/// Draws the "Lightroom XMP sync" section of the settings panel. Returns true when the catalog's
/// markers changed under the grid ("Use file" applied a sidecar), so the caller drops its cached
/// ratings/flags/labels (#375).
pub fn show(
    ui: &mut egui::Ui,
    state: &mut XmpUi,
    store: &Store,
    writer: &XmpWriter,
    catalog_path: &Path,
) -> bool {
    ui.label("Lightroom XMP sync (ratings, flags, labels and keywords in .xmp sidecars):");
    let mut on = writer.autowrite();
    if ui
        .checkbox(&mut on, "Automatically write changes into XMP sidecars")
        .changed()
    {
        writer.set_autowrite(on);
        state.message = save_autowrite(catalog_path, on)
            .err()
            .map(|e| format!("Couldn't save this setting: {e}"));
    }
    if let Some(err) = writer.last_error() {
        ui.horizontal(|ui| {
            ui.colored_label(egui::Color32::RED, format!("Last sidecar error: {err}"));
            if ui.small_button("Dismiss").clicked() {
                writer.clear_error();
            }
        });
    }
    if let Some(msg) = &state.message {
        ui.label(msg);
    }

    state.refresh(&**store);
    ui.ctx().request_repaint_after(REVIEW_REFRESH);
    if state.review_total == 0 {
        return false;
    }
    ui.colored_label(
        egui::Color32::from_rgb(0xe9, 0x5d, 0x00),
        format!(
            "{} photo(s) changed both here and in their .xmp; neither side was overwritten:",
            state.review_total
        ),
    );
    let mut resolve: Option<(i64, bool)> = None;
    for (id, name) in &state.review {
        ui.horizontal(|ui| {
            ui.label(name);
            if ui.small_button("Use catalog").clicked() {
                resolve = Some((*id, true));
            }
            if ui.small_button("Use file").clicked() {
                resolve = Some((*id, false));
            }
        });
    }
    if state.review_total > state.review.len() {
        ui.label(format!(
            "...and {} more.",
            state.review_total - state.review.len()
        ));
    }
    let mut catalog_changed = false;
    if let Some((id, use_catalog)) = resolve {
        state.message = match raw_path_for(&**store, id).filter(|(_, exists)| *exists) {
            Some((raw, _)) => scent_sync::resolve_review(&**store, id, &raw, use_catalog, now_ms())
                .map(|out| {
                    catalog_changed = out == SyncOutcome::AppliedSidecar;
                    match out {
                        SyncOutcome::NeedsReview => Some("Still in conflict.".to_string()),
                        _ => None,
                    }
                })
                .unwrap_or_else(|e| Some(format!("Couldn't resolve: {e}"))),
            None => Some("That photo's file isn't available right now.".to_string()),
        };
        state.last_refresh = None;
    }
    catalog_changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_lair::{NewAsset, SqliteCatalog};

    fn fixture() -> (tempfile::TempDir, Store, i64, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let store: Store = Arc::new(SqliteCatalog::open(&dir.path().join("c.db")).unwrap());
        let vol = store.upsert_volume("v", None, None, 1).unwrap();
        let shots = dir.path().join("shots");
        std::fs::create_dir(&shots).unwrap();
        let root = store.ensure_root(vol, shots.to_str().unwrap()).unwrap();
        let raw = shots.join("A.NEF");
        std::fs::write(&raw, b"raw").unwrap();
        let id = store
            .insert_asset(
                root,
                &NewAsset {
                    rel_path: "A.NEF".into(),
                    rel_path_fold: "a.nef".into(),
                    size_bytes: 3,
                    mtime_unix: 1,
                    fingerprint: None,
                    natural_key: None,
                    make: None,
                    model: None,
                    captured_at: None,
                    width: None,
                    height: None,
                    imported_at: 1,
                },
                None,
            )
            .unwrap();
        (dir, store, id, raw)
    }

    fn rated(n: i64) -> AssetMeta {
        AssetMeta {
            rating: Some(n),
            flag: None,
            label: None,
        }
    }

    /// Polls until `cond` holds or a generous deadline passes -- the writer is a real thread.
    fn eventually(cond: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        cond()
    }

    #[test]
    fn tagging_a_keyword_path_reaches_the_sidecar_and_untagging_removes_it() {
        let (_dir, store, id, raw) = fixture();
        let meta = XmpMeta::new(store.clone(), XmpWriter::spawn(store.clone(), true));
        let leaf = meta
            .tag_path(&[id], " Events | Anthrocon ")
            .unwrap()
            .expect("a keyword");
        assert_eq!(leaf.name, "Anthrocon");
        assert_eq!(
            store
                .keywords_for(id)
                .unwrap()
                .iter()
                .map(|k| k.id)
                .collect::<Vec<_>>(),
            vec![leaf.id]
        );

        let xmp = raw.with_extension("xmp");
        let has = |needle: &'static str| {
            let xmp = xmp.clone();
            move || {
                std::fs::read_to_string(&xmp)
                    .map(|t| t.contains(needle))
                    .unwrap_or(false)
            }
        };
        assert!(
            eventually(has("Events|Anthrocon")),
            "sidecar never got the hierarchical keyword"
        );

        meta.untag(&[id], leaf.id).unwrap();
        assert!(store.keywords_for(id).unwrap().is_empty());
        assert!(
            eventually(|| !has("Events|Anthrocon")()),
            "sidecar kept the removed keyword"
        );
        // Tagging again reuses the existing keyword rather than creating a duplicate.
        let again = meta.tag_path(&[id], "Events|Anthrocon").unwrap().unwrap();
        assert_eq!(again.id, leaf.id);
    }

    #[test]
    fn control_characters_are_stripped_from_keyword_names() {
        let (_dir, store, id, _raw) = fixture();
        let meta = XmpMeta::new(store.clone(), XmpWriter::spawn(store.clone(), false));
        let kw = meta.tag_path(&[id], "Ev\u{1}ents\n").unwrap().unwrap();
        assert_eq!(kw.name, "Events");
    }

    #[test]
    fn blank_keyword_text_tags_nothing() {
        let (_dir, store, id, _raw) = fixture();
        let meta = XmpMeta::new(store.clone(), XmpWriter::spawn(store.clone(), false));
        assert!(meta.tag_path(&[id], " | ").unwrap().is_none());
        assert!(store.list_keywords().unwrap().is_empty());
    }

    #[test]
    fn a_marker_change_is_written_to_the_sidecar() {
        let (_dir, store, id, raw) = fixture();
        let meta = XmpMeta::new(store.clone(), XmpWriter::spawn(store.clone(), true));
        meta.set_meta(&[(id, rated(4))]).unwrap();

        let xmp = raw.with_extension("xmp");
        assert!(
            eventually(|| std::fs::read_to_string(&xmp)
                .map(|t| t.contains("xmp:Rating=\"4\""))
                .unwrap_or(false)),
            "sidecar never got the rating"
        );
    }

    #[test]
    fn with_autowrite_off_nothing_is_written_but_the_catalog_is_marked_newer() {
        let (_dir, store, id, raw) = fixture();
        let meta = XmpMeta::new(store.clone(), XmpWriter::spawn(store.clone(), false));
        meta.set_meta(&[(id, rated(2))]).unwrap();

        assert!(
            eventually(|| store
                .sidecar_state(id)
                .unwrap()
                .is_some_and(|s| s.catalog_dirty_since_ms.is_some())),
            "catalog was never marked dirty"
        );
        assert!(!raw.with_extension("xmp").exists());
    }

    #[test]
    fn turning_autowrite_back_on_flushes_the_assets_left_dirty() {
        let (_dir, store, id, raw) = fixture();
        let writer = XmpWriter::spawn(store.clone(), false);
        let meta = XmpMeta::new(store.clone(), writer.clone());
        meta.set_meta(&[(id, rated(3))]).unwrap();
        assert!(
            eventually(|| store
                .sidecar_state(id)
                .unwrap()
                .is_some_and(|s| s.catalog_dirty_since_ms.is_some())),
            "catalog was never marked dirty"
        );
        let xmp = raw.with_extension("xmp");
        assert!(!xmp.exists());

        writer.set_autowrite(true);
        assert!(
            eventually(|| std::fs::read_to_string(&xmp)
                .map(|t| t.contains("xmp:Rating=\"3\""))
                .unwrap_or(false)),
            "dirty asset was never flushed"
        );
        assert!(eventually(|| store
            .sidecar_state(id)
            .unwrap()
            .is_some_and(|s| s.catalog_dirty_since_ms.is_none())));
    }

    #[test]
    fn a_failed_catalog_write_queues_nothing() {
        struct Failing;
        impl MetaStore for Failing {
            fn get_meta(&self, _: &[i64]) -> Result<HashMap<i64, AssetMeta>, String> {
                Ok(HashMap::new())
            }
            fn set_meta(&self, _: &[(i64, AssetMeta)]) -> Result<(), String> {
                Err("disk full".into())
            }
        }
        let (_dir, store, id, raw) = fixture();
        // Auto-write off so a (wrongly) queued id would leave a visible dirty record.
        let meta = XmpMeta::wrapping(
            Arc::new(Failing),
            store.clone(),
            XmpWriter::spawn(store.clone(), false),
        );
        assert!(meta.set_meta(&[(id, rated(1))]).is_err());
        // The writer is asynchronous, so prove "nothing happened" by ordering: a follow-up
        // successful write on a second store goes through the same writer and is awaited; by the
        // time it lands, the failed one (queued first, had it been queued) would have too.
        let ok = XmpMeta::wrapping(
            Arc::new(CatalogMeta(store.clone())),
            store.clone(),
            meta.writer.clone(),
        );
        ok.set_meta(&[(id, rated(2))]).unwrap();
        assert!(eventually(|| store.sidecar_state(id).unwrap().is_some()));
        assert!(!raw.with_extension("xmp").exists());
    }

    #[test]
    fn an_offline_raw_still_records_the_catalog_as_newer_without_creating_a_sidecar() {
        let (_dir, store, id, raw) = fixture();
        std::fs::remove_file(&raw).unwrap();
        let meta = XmpMeta::new(store.clone(), XmpWriter::spawn(store.clone(), true));
        meta.set_meta(&[(id, rated(3))]).unwrap();

        assert!(
            eventually(|| store
                .sidecar_state(id)
                .unwrap()
                .is_some_and(|s| s.catalog_dirty_since_ms.is_some())),
            "an offline photo's edit was never recorded as dirty"
        );
        assert!(!raw.with_extension("xmp").exists());
    }

    #[test]
    fn the_autowrite_preference_defaults_on_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = dir.path().join("catalog.db");
        assert!(load_autowrite(&catalog));
        save_autowrite(&catalog, false).unwrap();
        assert!(!load_autowrite(&catalog));
        save_autowrite(&catalog, true).unwrap();
        assert!(load_autowrite(&catalog));
        std::fs::write(autowrite_file_for(&catalog), "not json").unwrap();
        assert!(load_autowrite(&catalog), "a corrupt file falls back to on");
    }
}
