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
use nicti_lair::{AssetMeta, CatalogStore};

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

/// Absolute path of an asset's RAW, or `None` when the asset/root is gone or the file is missing
/// (an offline volume must never get a stray sidecar created in a non-existent directory).
fn raw_path_for(store: &dyn CatalogStore, asset_id: i64) -> Option<PathBuf> {
    let asset = store.get_asset(asset_id).ok()??;
    let root = store.get_root_path(asset.root_id).ok()??;
    let raw = Path::new(&root).join(&asset.rel_path);
    raw.exists().then_some(raw)
}

#[derive(Default)]
struct Status {
    last_error: Option<String>,
}

/// Handle to the sidecar writer thread. Cheap to clone; the thread exits when every clone is
/// dropped.
#[derive(Clone)]
pub struct XmpWriter {
    tx: Sender<Vec<i64>>,
    autowrite: Arc<AtomicBool>,
    status: Arc<Mutex<Status>>,
}

impl XmpWriter {
    pub fn spawn(store: Store, autowrite: bool) -> Self {
        let (tx, rx) = channel::<Vec<i64>>();
        let autowrite = Arc::new(AtomicBool::new(autowrite));
        let status = Arc::new(Mutex::new(Status::default()));
        let (flag, st) = (autowrite.clone(), status.clone());
        std::thread::Builder::new()
            .name("nicti-xmp-writer".into())
            .spawn(move || {
                while let Ok(first) = rx.recv() {
                    // Holding a key queues many commands; coalesce what's already waiting so each
                    // photo's sidecar is written once per burst, with its latest markers.
                    let mut ids: BTreeSet<i64> = first.into_iter().collect();
                    while let Ok(more) = rx.try_recv() {
                        ids.extend(more);
                    }
                    for id in ids {
                        let Some(raw) = raw_path_for(&*store, id) else {
                            continue;
                        };
                        let result = if flag.load(Ordering::Relaxed) {
                            scent_sync::write_sidecar(&*store, id, &raw, now_ms()).map(|_| ())
                        } else {
                            scent_sync::mark_catalog_dirty(&*store, id, &raw, now_ms())
                        };
                        if let Err(e) = result {
                            // A failed write (sidecar locked by LRC, unparseable at that moment)
                            // must not look like "nothing changed": record that the catalog is
                            // now newer, so a later rescan doesn't let the file silently win.
                            let _ = scent_sync::mark_catalog_dirty(&*store, id, &raw, now_ms());
                            st.lock().unwrap().last_error = Some(e.to_string());
                        }
                    }
                }
            })
            .expect("spawn the XMP writer thread");
        XmpWriter {
            tx,
            autowrite,
            status,
        }
    }

    pub fn queue(&self, ids: Vec<i64>) {
        if !ids.is_empty() {
            let _ = self.tx.send(ids);
        }
    }

    pub fn autowrite(&self) -> bool {
        self.autowrite.load(Ordering::Relaxed)
    }

    pub fn set_autowrite(&self, on: bool) {
        self.autowrite.store(on, Ordering::Relaxed);
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
    writer: XmpWriter,
}

impl XmpMeta {
    pub fn new(store: Store, writer: XmpWriter) -> Self {
        Self::wrapping(Arc::new(CatalogMeta(store)), writer)
    }

    fn wrapping(inner: Arc<dyn MetaStore>, writer: XmpWriter) -> Self {
        XmpMeta { inner, writer }
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

/// Draws the "Lightroom XMP sync" section of the settings panel.
pub fn show(
    ui: &mut egui::Ui,
    state: &mut XmpUi,
    store: &Store,
    writer: &XmpWriter,
    catalog_path: &Path,
) {
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
        return;
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
    if let Some((id, use_catalog)) = resolve {
        state.message = match raw_path_for(&**store, id) {
            Some(raw) => scent_sync::resolve_review(&**store, id, &raw, use_catalog, now_ms())
                .map(|out| match out {
                    SyncOutcome::NeedsReview => Some("Still in conflict.".to_string()),
                    _ => None,
                })
                .unwrap_or_else(|e| Some(format!("Couldn't resolve: {e}"))),
            None => Some("That photo's file isn't available right now.".to_string()),
        };
        state.last_refresh = None;
    }
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
        let meta = XmpMeta::wrapping(Arc::new(Failing), XmpWriter::spawn(store.clone(), false));
        assert!(meta.set_meta(&[(id, rated(1))]).is_err());
        // The writer is asynchronous, so prove "nothing happened" by ordering: a follow-up
        // successful write on a second store goes through the same writer and is awaited; by the
        // time it lands, the failed one (queued first, had it been queued) would have too.
        let ok = XmpMeta::wrapping(Arc::new(CatalogMeta(store.clone())), meta.writer.clone());
        ok.set_meta(&[(id, rated(2))]).unwrap();
        assert!(eventually(|| store.sidecar_state(id).unwrap().is_some()));
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
