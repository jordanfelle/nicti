//! The Library view's "Import from Lightroom Classic" panel (#62): a path to a **closed `.lrcat`
//! backup**, a Preview that lists its root folders (with an editable "this folder now lives
//! here" remap each and whether the folder exists on this machine), and an Import that submits one
//! `nicti_stray::LrcImportJob`. The job does the work; this module is only state, config building
//! and the summary -- the pure parts are unit-tested, the egui drawing is not (no display in the
//! dev sandbox).

use std::path::PathBuf;
use std::sync::Arc;

use nicti_lair::pounce_jobs::ReportSlot;
use nicti_lair::CatalogStore;
use nicti_pounce::Pounce;
use nicti_stray::open::open_validated;
use nicti_stray::paths::{resolve_root_path, RootRemap};
use nicti_stray::read::Reader;
use nicti_stray::{ImportConfig, LrcImportJob, LrcImportReport};

/// One LRC root folder in the preview list.
#[derive(Debug, Clone, PartialEq)]
pub struct RootRow {
    pub id: i64,
    /// Drive-letter path as LRC recorded it.
    pub lrc_path: String,
    /// User-typed "now lives at" path; empty = no remap.
    pub remap: String,
    pub include: bool,
    pub images: u64,
    /// The folder exists on this machine (after the remap and drive-letter mapping).
    pub exists_here: bool,
}

#[derive(Default)]
pub struct LrcImportUi {
    pub path: String,
    /// The catalog path the current `preview` was read from: editing `path` afterwards makes the
    /// preview (its roots and remaps) stale, and Import refuses until it is previewed again.
    previewed_path: String,
    /// `Err` = why the catalog could not be previewed (live catalog, wrong version ...).
    pub preview: Option<Result<Vec<RootRow>, String>>,
    pending: Option<ReportSlot<LrcImportReport>>,
    pub last_report: Option<LrcImportReport>,
    /// A one-line status for things that never reach the job (refused to start, ...).
    pub note: Option<String>,
}

fn remaps_of(rows: &[RootRow]) -> Vec<RootRemap> {
    rows.iter()
        .filter(|r| !r.remap.trim().is_empty())
        .map(|r| RootRemap {
            from: r.lrc_path.clone(),
            to: r.remap.trim().to_string(),
        })
        .collect()
}

/// Reads the catalog's roots and image counts (read-only, live-catalog guard, schema check).
pub fn preview_roots(path: &str) -> Result<Vec<RootRow>, String> {
    let path = path.trim();
    if path.is_empty() {
        return Err("Type the path of a closed .lrcat backup first.".into());
    }
    let conn = open_validated(std::path::Path::new(path)).map_err(|e| e.to_string())?;
    let reader = Reader::new(&conn).map_err(|e| e.to_string())?;
    let roots = reader.roots().map_err(|e| e.to_string())?;
    let mut rows = Vec::with_capacity(roots.len());
    for root in roots {
        let images = reader.image_count(&[root.id]).map_err(|e| e.to_string())?;
        rows.push(RootRow {
            id: root.id,
            exists_here: resolve_root_path(&root.absolute_path, &[]).is_dir(),
            lrc_path: root.absolute_path,
            remap: String::new(),
            // A root with no photos has nothing to import.
            include: images > 0,
            images,
        });
    }
    Ok(rows)
}

/// Re-checks which folders exist after a remap edit.
fn refresh_exists(rows: &mut [RootRow]) {
    for row in rows.iter_mut() {
        let remaps = remaps_of(std::slice::from_ref(row));
        row.exists_here = resolve_root_path(&row.lrc_path, &remaps).is_dir();
    }
}

impl LrcImportUi {
    /// The job config for the current preview, or why there isn't one yet.
    pub fn build_config(&self) -> Result<ImportConfig, String> {
        let rows = match &self.preview {
            Some(Ok(rows)) => rows,
            _ => return Err("Preview the catalog first.".into()),
        };
        if self.path.trim() != self.previewed_path {
            return Err("The catalog path changed since the preview; preview it again.".into());
        }
        let only_roots: Vec<i64> = rows.iter().filter(|r| r.include).map(|r| r.id).collect();
        if only_roots.is_empty() {
            return Err("Tick at least one folder to import.".into());
        }
        Ok(ImportConfig {
            catalog_path: PathBuf::from(self.path.trim()),
            remaps: remaps_of(rows),
            only_roots,
        })
    }

    /// Folds a finished job's report in. Call once per frame.
    pub fn poll(&mut self) {
        if let Some(slot) = self.pending.clone() {
            if let Some(report) = slot.lock().unwrap().take() {
                self.note = Some(report.summary());
                self.last_report = Some(report);
                self.pending = None;
            }
        }
    }

    pub fn running(&self) -> bool {
        self.pending.is_some()
    }

    fn submit(&mut self, store: &Arc<dyn CatalogStore + Send + Sync>, pounce: &Pounce) {
        match self.build_config() {
            Ok(config) => {
                let (job, slot) = LrcImportJob::new(store.clone(), config);
                pounce.submit(Box::new(job));
                self.pending = Some(slot);
                self.last_report = None;
                self.note = Some("Importing from Lightroom Classic ...".into());
            }
            Err(e) => self.note = Some(e),
        }
    }
}

/// Draws the panel. `blocked` is why an import can't start right now (a move/delete/export is
/// running), if so.
pub fn show(
    ui: &mut egui::Ui,
    state: &mut LrcImportUi,
    store: &Arc<dyn CatalogStore + Send + Sync>,
    pounce: &Pounce,
    blocked: Option<&str>,
) {
    state.poll();
    ui.collapsing("Import from Lightroom Classic", |ui| {
        ui.label(
            "Point at a closed .lrcat backup (never the catalog Lightroom has open). Ratings, \
             flags, labels, keywords, collections, virtual copies and develop settings come \
             across; nothing in Lightroom's catalog or your photo folders is changed.",
        );
        ui.horizontal(|ui| {
            ui.label("Catalog:");
            ui.text_edit_singleline(&mut state.path);
            if ui.button("Preview").clicked() {
                state.preview = Some(preview_roots(&state.path));
                state.previewed_path = state.path.trim().to_string();
                state.note = None;
            }
        });
        match &mut state.preview {
            Some(Err(e)) => {
                ui.colored_label(egui::Color32::RED, e.as_str());
            }
            Some(Ok(rows)) => {
                let mut changed = false;
                for row in rows.iter_mut() {
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut row.include, "");
                        ui.label(format!("{} ({} photos)", row.lrc_path, row.images));
                        if !row.exists_here {
                            ui.colored_label(egui::Color32::YELLOW, "not found here");
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("   now at:");
                        changed |= ui.text_edit_singleline(&mut row.remap).changed();
                    });
                }
                if changed {
                    refresh_exists(rows);
                }
            }
            None => {}
        }
        let can_start = blocked.is_none() && !state.running();
        ui.add_enabled_ui(can_start, |ui| {
            if ui.button("Import").clicked() {
                state.submit(store, pounce);
            }
        });
        if let Some(reason) = blocked {
            ui.label(reason);
        }
    });
    if let Some(note) = &state.note {
        ui.label(note);
    }
    if let Some(report) = &state.last_report {
        ui.collapsing("Last Lightroom import: details", |ui| {
            for root in &report.roots {
                ui.label(format!(
                    "{}: {} matched, {} missing{}",
                    root.lrc_path,
                    root.matched,
                    root.missing,
                    if root.exists {
                        ""
                    } else {
                        " (folder not found)"
                    }
                ));
            }
            ui.label(format!(
                "{} edits written, {} left alone because you changed them in nicti, {} virtual \
                 copies, {} develop texts that did not parse",
                report.docs_written,
                report.kept_local_docs + report.kept_local_meta,
                report.virtual_copies,
                report.develop_parse_failures,
            ));
            let s = &report.stats;
            ui.label(format!(
                "Local adjustments: {} brought across, {} skipped (brushes, people/object \
                 masks, rotated originals); crops skipped: {}; retouch areas skipped: {}",
                s.mask_corrections,
                s.mask_corrections_skipped,
                s.crops_skipped,
                s.heal_spots_skipped
            ));
            if report.smart_collections_skipped > 0 {
                ui.label(format!(
                    "{} smart collections were not imported.",
                    report.smart_collections_skipped
                ));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn fixture(dir: &std::path::Path) -> PathBuf {
        let path = dir.join("t.lrcat");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE AgLibraryRootFolder (id_local INTEGER PRIMARY KEY, absolutePath TEXT, name TEXT);
             CREATE TABLE AgLibraryFolder (id_local INTEGER PRIMARY KEY, rootFolder INTEGER, pathFromRoot TEXT);
             CREATE TABLE AgLibraryFile (id_local INTEGER PRIMARY KEY, folder INTEGER, baseName TEXT, extension TEXT);
             CREATE TABLE Adobe_images (id_local INTEGER PRIMARY KEY, rootFile INTEGER, rating, pick, colorLabels TEXT);
             CREATE TABLE AgLibraryKeyword (id_local INTEGER PRIMARY KEY, name TEXT, genealogy TEXT);
             CREATE TABLE AgLibraryKeywordImage (image INTEGER, tag INTEGER);
             CREATE TABLE AgLibraryCollection (id_local INTEGER PRIMARY KEY, name TEXT, parent INTEGER, creationId TEXT);
             CREATE TABLE AgLibraryCollectionImage (collection INTEGER, image INTEGER);
             INSERT INTO AgLibraryRootFolder VALUES (1, '/definitely/not/here/', 'a'), (2, '/empty/', 'b');
             INSERT INTO AgLibraryFolder VALUES (10, 1, '');
             INSERT INTO AgLibraryFile VALUES (100, 10, 'x', 'NEF');
             INSERT INTO Adobe_images VALUES (1, 100, NULL, 0, '');",
        )
        .unwrap();
        path
    }

    #[test]
    fn preview_lists_roots_with_counts_and_skips_empty_ones_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let rows = preview_roots(fixture(dir.path()).to_str().unwrap()).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            (rows[0].images, rows[0].include, rows[0].exists_here),
            (1, true, false)
        );
        assert_eq!((rows[1].images, rows[1].include), (0, false));
    }

    #[test]
    fn preview_explains_a_blank_path_and_a_non_catalog() {
        assert!(preview_roots("  ").unwrap_err().contains("closed .lrcat"));
        let dir = tempfile::tempdir().unwrap();
        let bogus = dir.path().join("x.lrcat");
        Connection::open(&bogus)
            .unwrap()
            .execute_batch("CREATE TABLE t(x);")
            .unwrap();
        assert!(preview_roots(bogus.to_str().unwrap())
            .unwrap_err()
            .contains("not a Lightroom"));
    }

    #[test]
    fn build_config_needs_a_preview_and_a_ticked_root_and_carries_remaps() {
        let mut ui = LrcImportUi::default();
        assert!(ui.build_config().is_err());
        let dir = tempfile::tempdir().unwrap();
        ui.path = fixture(dir.path()).display().to_string();
        ui.preview = Some(preview_roots(&ui.path));
        ui.previewed_path = ui.path.trim().to_string();
        if let Some(Ok(rows)) = &mut ui.preview {
            rows[0].remap = " /mnt/g/photos ".into();
        }
        let cfg = ui.build_config().unwrap();
        assert_eq!(cfg.only_roots, vec![1]);
        assert_eq!(
            cfg.remaps,
            vec![RootRemap {
                from: "/definitely/not/here/".into(),
                to: "/mnt/g/photos".into()
            }]
        );
        if let Some(Ok(rows)) = &mut ui.preview {
            rows[0].include = false;
        }
        assert!(ui.build_config().unwrap_err().contains("at least one"));
        // Editing the path after the preview makes the ticked roots stale.
        if let Some(Ok(rows)) = &mut ui.preview {
            rows[0].include = true;
        }
        ui.path.push('x');
        assert!(ui.build_config().unwrap_err().contains("preview it again"));
    }

    #[test]
    fn a_remap_to_an_existing_folder_flips_exists_here() {
        let dir = tempfile::tempdir().unwrap();
        let mut rows = preview_roots(fixture(dir.path()).to_str().unwrap()).unwrap();
        rows[0].remap = dir.path().display().to_string();
        refresh_exists(&mut rows);
        assert!(rows[0].exists_here);
    }
}
