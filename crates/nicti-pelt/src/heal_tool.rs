//! The Develop view's Heal / Remove tool (#51): click to place a heal, clone or AI-remove spot on
//! the photo, drag the selected spot's handles, and manage the spot list and the one-time AI model
//! download from the side panel.
//!
//! **Geometry.** While the tool is active the view shows the *uncropped* image
//! (`DevelopView::uncropped_preview`), which `app.rs` paints stretched into the viewport rect, so a
//! screen point maps to source pixels by the same plain stretch the crop tool already uses
//! (`develop_panel::screen_to_image`) and spots live directly in the `HealParams` coordinate space.
//! A spot's circle is drawn as an ellipse because that stretch can be anisotropic.
//!
//! **Undo.** Spot edits go through `DevelopDoc::set_stage_params`, so they are undoable with the
//! rest of Develop (#324). A Remove spot stores only its recipe, so after a reload or an undo its
//! fill is re-run from that recipe ([`RemovalService::rerun_missing`]); fills computed this session
//! stay cached, so undoing a deletion is instant.
//!
//! The state that isn't UI ([`RemovalService`]: model install, the lazily-loaded removal backend,
//! in-flight removal jobs) is kept apart from the egui code so it can be tested without a window.

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use nicti_groom::install::{InstallHandle, InstallModelsJob};
use nicti_groom::job::{RemoveJob, RemoveOutcome, SharedBackend, Slot};
use nicti_groom::real::LazyBackend;
use nicti_groom::sam::Prompt;
use nicti_groom::source::auto_source_pick;
use nicti_groom::FramePixels;
use nicti_pounce::Pounce;
use nicti_stalk::models::{self, HttpDownloader, ModelStore, RemovalModels};
use nicti_tapetum::coat::{HealParams, MaskRecipe, Spot, SpotKind};
use nicti_tapetum::heal::{spot_key, RemovalPatch, MAX_RADIUS, MAX_SPOTS};
use nicti_tapetum::stages::HEAL;

use crate::develop_panel::{image_to_screen, screen_to_image};
use crate::fur::{self, SliderSpec};
use crate::render::DevelopDoc;

/// Which on-image tool owns the Develop viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    /// No tool: the preview shows the cropped canvas (#272), and the viewport takes no gestures.
    Idle,
    /// The crop/straighten overlay on the whole, uncropped image.
    Crop,
    Heal,
    /// Local-adjustment masks (#49).
    Mask,
    /// The Point Color eyedropper (#432): the next click on the photo adds a sample.
    PointColor,
}

const MIN_RADIUS: f32 = 4.0;
const HANDLE_RADIUS_PX: f32 = 7.0;
/// `[`/`]` change the brush size by this factor per key press.
const RESIZE_STEP: f32 = 1.12;

/// Mask-recipe identity for spots this build removes with: pins the model pair, so a future model
/// upgrade is an explicit re-run rather than a silent change to an old edit (ADR-0021).
const RECIPE_MODEL_ID: &str = "nicti.remove.mobilesam-lama";
const RECIPE_MODEL_VERSION: &str = "1";

/// How often to look for installed models again while a notice says they're missing.
const MODELS_RECHECK: std::time::Duration = std::time::Duration::from_secs(2);

// ---------------------------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------------------------

/// Builds a spot; `radius` is clamped to the supported range and `feather_frac` (0..=1 of the
/// radius) becomes the absolute feather width.
pub fn new_spot(
    kind: SpotKind,
    center: (f32, f32),
    radius: f32,
    feather_frac: f32,
    opacity: f32,
    source_offset: Option<(f32, f32)>,
    mask_recipe: Option<MaskRecipe>,
) -> Spot {
    let radius = radius.clamp(MIN_RADIUS, MAX_RADIUS);
    Spot {
        kind,
        center,
        radius,
        source_offset,
        feather: feather_frac.clamp(0.0, 1.0) * radius,
        opacity: opacity.clamp(0.0, 1.0),
        mask_recipe,
    }
}

/// A source offset for when auto-pick finds nothing: three radii sideways, toward the middle of
/// the image, and never so far that the source centre leaves the frame (a source hanging off the
/// edge would clone mostly edge-clamped pixels).
pub fn fallback_offset(center: (f32, f32), radius: f32, source: (f32, f32)) -> (f32, f32) {
    let reach = 3.0 * radius;
    let target = if center.0 < source.0 / 2.0 {
        center.0 + reach
    } else {
        center.0 - reach
    };
    (target.clamp(0.0, source.0) - center.0, 0.0)
}

/// The topmost (last-placed) spot whose destination circle contains `p` (source pixels).
pub fn spot_at(spots: &[Spot], p: (f32, f32)) -> Option<usize> {
    spots.iter().rposition(|s| {
        let (dx, dy) = (p.0 - s.center.0, p.1 - s.center.1);
        dx * dx + dy * dy <= s.radius * s.radius
    })
}

/// True when `p` is within `tolerance` (source pixels) of a Clone/Heal spot's source center.
pub fn on_source_handle(spot: &Spot, p: (f32, f32), tolerance: f32) -> bool {
    let Some((ox, oy)) = spot.source_offset else {
        return false;
    };
    let (dx, dy) = (p.0 - (spot.center.0 + ox), p.1 - (spot.center.1 + oy));
    dx * dx + dy * dy <= tolerance * tolerance
}

fn clamp_radius(r: f32) -> f32 {
    r.clamp(MIN_RADIUS, MAX_RADIUS)
}

fn removal_recipe(prompt: Prompt) -> MaskRecipe {
    MaskRecipe {
        model_id: RECIPE_MODEL_ID.to_owned(),
        model_version: RECIPE_MODEL_VERSION.to_owned(),
        params: prompt.to_json(),
        seed: None,
    }
}

fn megabytes(bytes: u64) -> u64 {
    bytes.div_ceil(1_000_000)
}

// ---------------------------------------------------------------------------------------------
// Removal service: model install, backend, in-flight jobs (no egui)
// ---------------------------------------------------------------------------------------------

/// A finished (or failed) removal, keyed by `heal::spot_key`.
#[derive(Debug)]
pub struct RemovalEvent {
    /// The photo the removal was computed for; a result for any other photo is dropped.
    pub image_key: u64,
    pub key: String,
    /// True for a removal re-run from the stored recipe (see [`RemovalService::rerun_missing`]),
    /// not one the user just placed: its spot is already in the document and must survive a
    /// failure.
    pub rerun: bool,
    pub result: Result<Arc<RemovalPatch>, String>,
}

pub struct RemovalService {
    store: Option<ModelStore>,
    backend: Option<(RemovalModels, SharedBackend)>,
    install: Option<InstallHandle>,
    pending: Vec<Slot<RemoveOutcome>>,
    /// (photo, spot) pairs with a job in flight. Keyed by photo too: the same spot at the same
    /// coordinates on a *different* photo is a different removal and must get its own job.
    pending_keys: HashSet<(u64, String)>,
    /// The subset of `pending_keys` that is a re-run of a spot already in the document.
    reruns: HashSet<(u64, String)>,
    /// Re-runs that failed, so a failing spot is reported once instead of retried every frame.
    /// Cleared when a model install/repair succeeds; a spot the user edits gets a new key and so a
    /// fresh attempt.
    failed_reruns: HashSet<(u64, String)>,
    /// The photo the "needs the model download" notice was shown for: shown once per photo, not
    /// every frame. Reset when an install finishes.
    models_missing_for: Option<u64>,
    /// When the models were last looked for on disk while `models_missing_for` was set, so a
    /// hand-dropped or externally installed set is noticed within [`MODELS_RECHECK`] without a
    /// filesystem stat every frame.
    models_checked: Option<std::time::Instant>,
    /// Test seams: pretend the models are installed / substitute the backend, so gesture tests can
    /// exercise the whole click -> job -> patch path without ~250 MB of weights.
    #[cfg(test)]
    models_override: Option<RemovalModels>,
    #[cfg(test)]
    backend_override: Option<SharedBackend>,
}

impl Default for RemovalService {
    fn default() -> Self {
        Self::new()
    }
}

impl RemovalService {
    /// The store lives at `NICTI_MODELS_DIR` if set (also how a Linux dev drops in models by hand),
    /// else the platform default.
    pub fn new() -> Self {
        let root = std::env::var_os("NICTI_MODELS_DIR")
            .filter(|p| !p.is_empty())
            .map(std::path::PathBuf::from)
            .or_else(ModelStore::default_root);
        Self::with_store(root.map(ModelStore::new))
    }

    pub fn with_store(store: Option<ModelStore>) -> Self {
        Self {
            store,
            backend: None,
            install: None,
            pending: Vec::new(),
            pending_keys: HashSet::new(),
            reruns: HashSet::new(),
            failed_reruns: HashSet::new(),
            models_missing_for: None,
            models_checked: None,
            #[cfg(test)]
            models_override: None,
            #[cfg(test)]
            backend_override: None,
        }
    }

    /// Everything AI removal needs, if it is all installed.
    pub fn models(&self) -> Option<RemovalModels> {
        #[cfg(test)]
        if let Some(m) = &self.models_override {
            return Some(m.clone());
        }
        self.store.as_ref().and_then(RemovalModels::locate)
    }

    /// Bytes still to download for AI removal (0 once installed).
    pub fn download_bytes(&self) -> u64 {
        self.store
            .as_ref()
            .map_or(0, models::removal_download_bytes)
    }

    pub fn is_installing(&self) -> bool {
        self.install.is_some()
    }

    /// `(downloaded, total)` bytes of the running install.
    pub fn install_progress(&self) -> Option<(u64, u64)> {
        self.install.as_ref().map(|h| {
            (
                h.bytes.load(Ordering::Relaxed),
                h.total.load(Ordering::Relaxed),
            )
        })
    }

    pub fn cancel_install(&self) {
        if let Some(h) = &self.install {
            h.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Submits the explicit, user-initiated download of every AI-removal model still missing.
    pub fn start_install(&mut self, pounce: &Pounce) -> Result<(), String> {
        if self.install.is_some() {
            return Ok(());
        }
        let store = self
            .store
            .clone()
            .ok_or("No place to keep models: couldn't determine a data folder.")?;
        let (job, handle) = InstallModelsJob::new(
            store.clone(),
            Arc::new(HttpDownloader),
            &models::removal_artifacts_for(&store),
        );
        self.install = Some(handle);
        pounce.submit(Box::new(job));
        Ok(())
    }

    /// Re-verifies the installed models and re-downloads any that are corrupt (see
    /// [`InstallModelsJob::new_repair`]). The way out when a removal reports a failed integrity
    /// check on files whose sizes look right.
    pub fn start_repair(&mut self, pounce: &Pounce) -> Result<(), String> {
        if self.install.is_some() {
            return Ok(());
        }
        let store = self
            .store
            .clone()
            .ok_or("No place to keep models: couldn't determine a data folder.")?;
        let (job, handle) = InstallModelsJob::new_repair(
            store.clone(),
            Arc::new(HttpDownloader),
            &models::removal_repair_artifacts(&store),
        );
        self.install = Some(handle);
        pounce.submit(Box::new(job));
        Ok(())
    }

    /// Polls the install; `Some` exactly once, when it finishes.
    pub fn poll_install(&mut self) -> Option<Result<(), String>> {
        let done = self
            .install
            .as_ref()
            .and_then(|h| h.result.lock().unwrap().take())?;
        self.install = None;
        if done.is_ok() {
            // The files on disk may have just been replaced (a repair), so any backend built over
            // the old ones must not survive to run removals on stale sessions. Spots whose re-run
            // failed (an integrity check, say) get another go with the fixed models.
            self.backend = None;
            self.failed_reruns.clear();
            self.models_missing_for = None;
            self.models_checked = None;
        }
        Some(done)
    }

    /// The shared backend for the currently installed models, created (unloaded) on first use and
    /// replaced if the model paths change.
    fn backend_for(&mut self, models: RemovalModels) -> SharedBackend {
        if let Some((m, b)) = &self.backend {
            if *m == models {
                return Arc::clone(b);
            }
        }
        let mut lazy = LazyBackend::new(models.clone());
        if let Some(store) = self.store.clone() {
            // The runtime is the store's own (the CPU build, or the GPU pack's CUDA build when that
            // is installed) unless NICTI_ORT_DYLIB points elsewhere.
            let ort_from_store =
                store.ort_runtime_path().as_deref() == Some(models.ort_dylib.as_path());
            let artifacts = models.artifacts();
            lazy = lazy
                .verified_by(move || models::verify_artifacts(&store, &artifacts, ort_from_store));
        }
        let backend: SharedBackend = Arc::new(Mutex::new(lazy));
        self.backend = Some((models, Arc::clone(&backend)));
        backend
    }

    /// Queues a removal for `spot` on Pounce's GPU lane using the installed models.
    pub fn submit(
        &mut self,
        pounce: &Pounce,
        develop: &DevelopDoc,
        spot: &Spot,
        prompt: Prompt,
    ) -> Result<(), String> {
        let models = self
            .models()
            .ok_or("AI removal models aren't installed yet.")?;
        #[cfg(test)]
        let backend = self
            .backend_override
            .clone()
            .unwrap_or_else(|| self.backend_for(models.clone()));
        #[cfg(not(test))]
        let backend = self.backend_for(models);
        self.submit_with_backend(pounce, backend, develop, spot, prompt)
    }

    /// [`Self::submit`] with an explicit backend -- the seam the tests use.
    pub fn submit_with_backend(
        &mut self,
        pounce: &Pounce,
        backend: SharedBackend,
        develop: &DevelopDoc,
        spot: &Spot,
        prompt: Prompt,
    ) -> Result<(), String> {
        let key = spot_key(spot);
        if !self.pending_keys.insert((develop.frame_key(), key.clone())) {
            return Ok(()); // already running for exactly this spot on this photo
        }
        let frame = develop.frame_arc();
        let cam_mul = frame.cam_mul;
        let (job, slot) = RemoveJob::new(
            backend,
            Arc::new(FramePixels(frame)),
            develop.frame_key(),
            cam_mul,
            prompt,
            spot.center,
            spot.radius,
            key,
        );
        self.pending.push(slot);
        pounce.submit(Box::new(job));
        Ok(())
    }

    pub fn is_pending(&self, image_key: u64, key: &str) -> bool {
        self.pending_keys.contains(&(image_key, key.to_owned()))
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Queues a removal for every AI Remove spot in the document that has no finished patch, no
    /// job in flight and no failed earlier attempt -- what a photo opened with saved removals, or
    /// an undo that restored one, needs, since only a spot's recipe (the model prompt) is stored,
    /// never its pixels (#324). A recipe from another model, or one that can't be parsed, is
    /// reported and left alone; the spot stays in the document either way, so a failed re-run
    /// loses nothing. `Err` is a message for the status line.
    pub fn rerun_missing(&mut self, pounce: &Pounce, develop: &DevelopDoc) -> Result<(), String> {
        let image_key = develop.frame_key();
        let params: HealParams = develop.stage_params(HEAL);
        let mut missing = Vec::new();
        let mut unsupported = 0;
        for spot in params.spots.iter().filter(|s| s.kind == SpotKind::Remove) {
            let id = (image_key, spot_key(spot));
            if develop.has_removal(&id.1)
                || self.pending_keys.contains(&id)
                || self.failed_reruns.contains(&id)
            {
                continue;
            }
            let prompt = spot
                .mask_recipe
                .as_ref()
                .filter(|r| {
                    r.model_id == RECIPE_MODEL_ID && r.model_version == RECIPE_MODEL_VERSION
                })
                .and_then(|r| Prompt::from_json(&r.params));
            match prompt {
                Some(prompt) => missing.push((spot, prompt)),
                None => {
                    self.failed_reruns.insert(id);
                    unsupported += 1;
                }
            }
        }
        let mut problems = Vec::new();
        if unsupported > 0 {
            problems.push(format!(
                "{unsupported} AI removal spot(s) were made with a model this version can't re-run."
            ));
        }
        if !missing.is_empty() {
            let known_missing = self.models_missing_for == Some(image_key)
                && self
                    .models_checked
                    .is_some_and(|t| t.elapsed() < MODELS_RECHECK);
            if known_missing {
                // Already told the user, and looked recently: stay quiet and don't hit the disk.
            } else if self.models().is_none() {
                self.models_checked = Some(std::time::Instant::now());
                if self.models_missing_for != Some(image_key) {
                    problems.push(format!(
                        "{} AI removal spot(s) need the model download to be re-applied.",
                        missing.len()
                    ));
                }
                self.models_missing_for = Some(image_key);
            } else {
                self.models_missing_for = None;
                for (spot, prompt) in missing {
                    let id = (image_key, spot_key(spot));
                    // Only a job that actually queued is a re-run.
                    match self.submit(pounce, develop, spot, prompt) {
                        Ok(()) => {
                            self.reruns.insert(id);
                        }
                        Err(e) => problems.push(e),
                    }
                }
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join(" "))
        }
    }

    /// Collects every removal that has finished since the last call.
    pub fn poll_removals(&mut self) -> Vec<RemovalEvent> {
        let mut events = Vec::new();
        self.pending
            .retain(|slot| match slot.lock().unwrap().take() {
                Some(outcome) => {
                    let id = (outcome.image_key, outcome.spot_key.clone());
                    self.pending_keys.remove(&id);
                    let rerun = self.reruns.remove(&id);
                    if rerun && outcome.result.is_err() {
                        self.failed_reruns.insert(id);
                    }
                    events.push(RemovalEvent {
                        image_key: outcome.image_key,
                        key: outcome.spot_key,
                        rerun,
                        result: outcome.result,
                    });
                    false
                }
                None => true,
            });
        events
    }
}

// ---------------------------------------------------------------------------------------------
// UI state
// ---------------------------------------------------------------------------------------------

enum Drag {
    Dest {
        start_center: (f32, f32),
        start_pointer: (f32, f32),
    },
    Source {
        start_offset: (f32, f32),
        start_pointer: (f32, f32),
    },
}

pub struct HealUi {
    pub tool: Tool,
    /// Defaults for the next placed spot (and what the sliders edit when nothing is selected).
    pub kind: SpotKind,
    pub radius: f32,
    pub feather_frac: f32,
    pub opacity: f32,
    selected: Option<usize>,
    drag: Option<Drag>,
    status: Option<String>,
    /// Set when a removal reported a failed model integrity check; shows the Repair button.
    needs_repair: bool,
    pub service: RemovalService,
}

impl Default for HealUi {
    fn default() -> Self {
        Self::new()
    }
}

impl HealUi {
    /// Forgets the selected spot. Call after the document is replaced under the tool (a batch
    /// paste or undo): the index could now name a different spot.
    pub fn clear_selection(&mut self) {
        self.selected = None;
    }

    pub fn new() -> Self {
        Self {
            tool: Tool::Idle,
            kind: SpotKind::Heal,
            radius: 24.0,
            feather_frac: 0.3,
            opacity: 1.0,
            selected: None,
            drag: None,
            status: None,
            needs_repair: false,
            service: RemovalService::new(),
        }
    }

    /// True while the Crop tool owns the viewport (the whole image + overlay, #272).
    pub fn crop_active(&self) -> bool {
        self.tool == Tool::Crop
    }

    /// Whether the active tool works on the whole, uncropped image (#272).
    pub fn shows_uncropped(&self) -> bool {
        self.heal_active() || self.mask_active() || self.crop_active()
    }

    pub fn heal_active(&self) -> bool {
        self.tool == Tool::Heal
    }

    /// True while the Masks tool owns the viewport (#49).
    pub fn mask_active(&self) -> bool {
        self.tool == Tool::Mask
    }

    /// True while the Point Color eyedropper owns the viewport (#432).
    pub fn point_color_active(&self) -> bool {
        self.tool == Tool::PointColor
    }
}

/// Removes the spot at `index` and prunes patches that no longer belong to any spot.
fn delete_spot(develop: &mut DevelopDoc, params: &mut HealParams, index: usize) {
    if index < params.spots.len() {
        params.spots.remove(index);
        develop.set_stage_params(HEAL, params);
        develop.prune_removals();
    }
}

/// Applies finished removals to the view and handles failures. Call once per frame while the
/// Develop view is up.
pub fn poll(ui: &egui::Ui, develop: &mut DevelopDoc, heal: &mut HealUi) {
    if let Some(result) = heal.service.poll_install() {
        heal.status = Some(match result {
            Ok(()) => {
                heal.needs_repair = false;
                "AI removal models installed.".to_owned()
            }
            Err(e) => e,
        });
    }
    let events = heal.service.poll_removals();
    if !events.is_empty() {
        let mut params: HealParams = develop.stage_params(HEAL);
        for event in events {
            // Computed for a photo that is no longer open: it means nothing here, and must not be
            // applied even if a spot on this photo happens to share its key.
            if event.image_key != develop.frame_key() {
                continue;
            }
            match event.result {
                Ok(patch) => {
                    // Ignore a result for a spot the user has since deleted or edited.
                    if params.spots.iter().any(|s| spot_key(s) == event.key) {
                        develop.set_removal(event.key, patch);
                        heal.status = Some("Object removed.".to_owned());
                    }
                }
                Err(message) if event.rerun => {
                    // The spot was already part of the user's edit: keep it (its recipe is in the
                    // document) and say why its fill is missing, rather than dropping it.
                    if message.contains("integrity check") {
                        heal.needs_repair = true;
                    }
                    heal.status = Some(format!("Couldn't re-apply an AI removal: {message}"));
                }
                Err(message) => {
                    if let Some(i) = params.spots.iter().position(|s| spot_key(s) == event.key) {
                        // The drop below is an undoable step; undoing it must not quietly retry a
                        // removal that just failed.
                        heal.service
                            .failed_reruns
                            .insert((event.image_key, event.key.clone()));
                        // A failed removal leaves nothing useful behind: drop its placeholder
                        // spot instead of leaving it pending forever.
                        delete_spot(develop, &mut params, i);
                        heal.selected = None;
                    }
                    if message.contains("integrity check") {
                        heal.needs_repair = true;
                    }
                    heal.status = Some(message);
                }
            }
        }
    }
    let spots = develop.stage_params::<HealParams>(HEAL).spots.len();
    if heal.selected.is_some_and(|i| i >= spots) {
        // An undo can shrink the spot list under the selection.
        heal.selected = None;
    }
    if heal.service.pending_count() > 0 || heal.service.is_installing() {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(100));
    }
}

/// Re-runs the AI removals the document references but has no patch for (see
/// [`RemovalService::rerun_missing`]). Call once per frame while the Develop view is up, after
/// [`poll`]. A problem is reported once per spot (or once for missing models), so it can replace the status line without being re-set every frame.
pub fn rerun_missing_removals(develop: &DevelopDoc, heal: &mut HealUi, pounce: &Pounce) {
    if let Err(message) = heal.service.rerun_missing(pounce, develop) {
        heal.status = Some(message);
    }
}

// ---------------------------------------------------------------------------------------------
// Viewport gestures and overlay
// ---------------------------------------------------------------------------------------------

/// Handles clicks, drags and keys on the viewport, then draws the spot overlay. `rect` is where
/// the (uncropped) render is painted; see this module's doc comment for the mapping.
pub fn handle_viewport(
    ui: &mut egui::Ui,
    response: &egui::Response,
    rect: egui::Rect,
    develop: &mut DevelopDoc,
    heal: &mut HealUi,
    pounce: &Pounce,
) {
    let source = develop.source_extent();
    let mut params: HealParams = develop.stage_params(HEAL);
    if heal.selected.is_some_and(|i| i >= params.spots.len()) {
        heal.selected = None;
    }
    let mut changed = false;
    let img = |p: egui::Pos2| screen_to_image(rect, source, p);
    // One screen handle radius, in source pixels (x-axis; the stretch may be anisotropic).
    let handle_tol = HANDLE_RADIUS_PX * source.0 / rect.width().max(1.0);

    // Keys act only while the pointer is over the photo, so typing in a panel text field never
    // deletes a spot.
    if response.hovered() {
        let (delete, bigger, smaller) = ui.input(|i| {
            (
                i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace),
                i.key_pressed(egui::Key::CloseBracket),
                i.key_pressed(egui::Key::OpenBracket),
            )
        });
        if delete {
            if let Some(i) = heal.selected.take() {
                // Stores the document itself, so this isn't a `changed` edit.
                delete_spot(develop, &mut params, i);
            }
        }
        let factor = match (bigger, smaller) {
            (true, false) => Some(RESIZE_STEP),
            (false, true) => Some(1.0 / RESIZE_STEP),
            _ => None,
        };
        if let Some(f) = factor {
            match heal.selected {
                Some(i) if params.spots[i].kind != SpotKind::Remove => {
                    let s = &mut params.spots[i];
                    let feather_frac = if s.radius > 0.0 {
                        s.feather / s.radius
                    } else {
                        0.0
                    };
                    s.radius = clamp_radius(s.radius * f);
                    s.feather = feather_frac * s.radius;
                    changed = true;
                }
                _ => heal.radius = clamp_radius(heal.radius * f),
            }
        }
    }

    // Dragging a handle of the selected spot.
    if response.drag_started() {
        heal.drag = None;
        // Where the button went *down*: by the time egui reports `drag_started` the pointer has
        // already moved past its drag threshold, so `interact_pointer_pos` would put the hit test
        // (and the drag's anchor) a few pixels off -- enough to miss a small handle.
        let pressed_at = ui
            .input(|i| i.pointer.press_origin())
            .or_else(|| response.interact_pointer_pos());
        if let (Some(i), Some(pointer)) = (heal.selected, pressed_at) {
            let p = img(pointer);
            let s = &params.spots[i];
            if s.kind != SpotKind::Remove {
                if on_source_handle(s, p, handle_tol) {
                    heal.drag = Some(Drag::Source {
                        start_offset: s.source_offset.unwrap_or((0.0, 0.0)),
                        start_pointer: p,
                    });
                } else if spot_at(std::slice::from_ref(s), p).is_some() {
                    heal.drag = Some(Drag::Dest {
                        start_center: s.center,
                        start_pointer: p,
                    });
                }
            }
        }
    }
    if response.dragged() {
        if let (Some(drag), Some(i), Some(pointer)) = (
            heal.drag.as_ref(),
            heal.selected,
            response.interact_pointer_pos(),
        ) {
            let p = img(pointer);
            let s = &mut params.spots[i];
            match drag {
                Drag::Dest {
                    start_center,
                    start_pointer,
                } => {
                    s.center = (
                        (start_center.0 + p.0 - start_pointer.0).clamp(0.0, source.0),
                        (start_center.1 + p.1 - start_pointer.1).clamp(0.0, source.1),
                    );
                }
                Drag::Source {
                    start_offset,
                    start_pointer,
                } => {
                    s.source_offset = Some((
                        start_offset.0 + p.0 - start_pointer.0,
                        start_offset.1 + p.1 - start_pointer.1,
                    ));
                }
            }
            changed = true;
        }
    }
    if response.drag_stopped() {
        heal.drag = None;
    }

    // A plain click: select the spot under it, else place a new one.
    if response.clicked() {
        if let Some(pointer) = response.interact_pointer_pos() {
            let p = img(pointer);
            if let Some(i) = spot_at(&params.spots, p) {
                heal.selected = Some(i);
            } else {
                heal.selected = None;
                place_spot(develop, heal, pounce, &mut params, p, source, &mut changed);
            }
        }
    }

    if changed {
        develop.set_stage_params(HEAL, &params);
        develop.prune_removals();
    }
    draw_overlay(
        ui,
        rect,
        source,
        &params,
        heal,
        develop.frame_key(),
        response,
    );
}

/// Places a new spot of the current kind at `p`.
fn place_spot(
    develop: &mut DevelopDoc,
    heal: &mut HealUi,
    pounce: &Pounce,
    params: &mut HealParams,
    p: (f32, f32),
    source: (f32, f32),
    changed: &mut bool,
) {
    if params.spots.len() >= MAX_SPOTS {
        heal.status = Some(format!(
            "That's the most spots one photo can hold ({MAX_SPOTS}). Delete some to add more."
        ));
        return;
    }
    let radius = clamp_radius(heal.radius);
    let spot = match heal.kind {
        SpotKind::Clone | SpotKind::Heal => {
            let offset = auto_source_pick(
                &FramePixels(develop.frame_arc()),
                (p.0.round() as i32, p.1.round() as i32),
                radius,
            )
            .map(|(dx, dy)| (dx as f32, dy as f32))
            .unwrap_or_else(|| fallback_offset(p, radius, source));
            new_spot(
                heal.kind,
                p,
                radius,
                heal.feather_frac,
                heal.opacity,
                Some(offset),
                None,
            )
        }
        SpotKind::Remove => {
            if heal.service.models().is_none() {
                heal.status = Some(
                    "AI removal needs a one-time model download -- use the Download button \
                     in the panel."
                        .to_owned(),
                );
                return;
            }
            let prompt = Prompt::Click { x: p.0, y: p.1 };
            new_spot(
                SpotKind::Remove,
                p,
                radius,
                heal.feather_frac,
                heal.opacity,
                None,
                Some(removal_recipe(prompt)),
            )
        }
    };
    if spot.kind == SpotKind::Remove {
        let prompt = Prompt::Click { x: p.0, y: p.1 };
        if let Err(e) = heal.service.submit(pounce, develop, &spot, prompt) {
            heal.status = Some(e);
            return;
        }
        heal.status = Some("Removing object...".to_owned());
    }
    params.spots.push(spot);
    heal.selected = Some(params.spots.len() - 1);
    *changed = true;
}

fn ellipse(
    rect: egui::Rect,
    source: (f32, f32),
    center: (f32, f32),
    radius: f32,
) -> (egui::Pos2, egui::Vec2) {
    (
        image_to_screen(rect, source, center),
        egui::vec2(
            radius * rect.width() / source.0.max(1.0),
            radius * rect.height() / source.1.max(1.0),
        ),
    )
}

fn draw_overlay(
    ui: &egui::Ui,
    rect: egui::Rect,
    source: (f32, f32),
    params: &HealParams,
    heal: &HealUi,
    frame_key: u64,
    response: &egui::Response,
) {
    let painter = ui.painter_at(rect);
    let white = egui::Stroke::new(1.5, egui::Color32::WHITE);
    let selected = egui::Stroke::new(2.0, egui::Color32::from_rgb(255, 210, 0));
    let source_stroke = egui::Stroke::new(1.5, egui::Color32::from_rgb(0, 200, 255));
    let ai = egui::Stroke::new(1.5, egui::Color32::from_rgb(255, 130, 0));

    for (i, s) in params.spots.iter().enumerate() {
        let is_selected = heal.selected == Some(i);
        let stroke = if is_selected {
            selected
        } else if s.kind == SpotKind::Remove {
            ai
        } else {
            white
        };
        let (c, r) = ellipse(rect, source, s.center, s.radius);
        painter.add(egui::Shape::ellipse_stroke(c, r, stroke));
        if let Some((ox, oy)) = s.source_offset {
            let (sc, sr) = ellipse(rect, source, (s.center.0 + ox, s.center.1 + oy), s.radius);
            painter.add(egui::Shape::ellipse_stroke(sc, sr, source_stroke));
            painter.line_segment([c, sc], egui::Stroke::new(1.0, source_stroke.color));
            if is_selected {
                painter.circle_filled(sc, HANDLE_RADIUS_PX, source_stroke.color);
            }
        }
        if s.kind == SpotKind::Remove && heal.service.is_pending(frame_key, &spot_key(s)) {
            painter.text(
                c,
                egui::Align2::CENTER_CENTER,
                "removing...",
                egui::FontId::proportional(13.0),
                ai.color,
            );
        }
    }

    // Brush preview under the pointer, sized to whatever a click would place.
    if response.hovered() && heal.drag.is_none() {
        if let Some(hover) = response.hover_pos() {
            let (_, r) = ellipse(rect, source, (0.0, 0.0), clamp_radius(heal.radius));
            painter.add(egui::Shape::ellipse_stroke(
                hover,
                r,
                egui::Stroke::new(1.0, egui::Color32::from_white_alpha(140)),
            ));
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Side panel
// ---------------------------------------------------------------------------------------------

/// The Crop | Heal tool switch, shown at the top of the Develop panel.
pub fn tool_switch(ui: &mut egui::Ui, heal: &mut HealUi) {
    ui.horizontal_wrapped(|ui| {
        ui.label("Tool");
        if ui
            .selectable_label(heal.crop_active(), "Crop")
            .on_hover_text("R: crop on the whole image; Esc or Enter: commit")
            .clicked()
        {
            heal.tool = if heal.crop_active() {
                Tool::Idle
            } else {
                Tool::Crop
            };
        }
        // Clicking the active tool again returns to the idle (cropped) canvas.
        for (tool, label) in [
            (Tool::Heal, "Heal / Remove"),
            (Tool::Mask, "Masks"),
            (Tool::PointColor, "Pick Color"),
        ] {
            if ui.selectable_label(heal.tool == tool, label).clicked() {
                heal.tool = if heal.tool == tool { Tool::Idle } else { tool };
            }
        }
    });
}

/// Develop-view keys for the crop tool (#272): **R** toggles it, **Esc**/**Enter** commit it.
/// Ignored while a text field wants the keyboard.
pub fn handle_tool_keys(ui: &egui::Ui, heal: &mut HealUi) {
    if ui.ctx().egui_wants_keyboard_input() {
        return;
    }
    let (r, done) = ui.input(|i| {
        (
            i.key_pressed(egui::Key::R) && !i.modifiers.any(),
            i.key_pressed(egui::Key::Escape) || i.key_pressed(egui::Key::Enter),
        )
    });
    if r {
        heal.tool = if heal.crop_active() {
            Tool::Idle
        } else {
            Tool::Crop
        };
    } else if done && heal.crop_active() {
        heal.tool = Tool::Idle;
    }
}

fn kind_label(kind: SpotKind) -> &'static str {
    match kind {
        SpotKind::Clone => "Clone",
        SpotKind::Heal => "Heal",
        SpotKind::Remove => "Remove",
    }
}

/// The Heal / Remove controls: spot kind and size, the spot list, and the AI-model download.
const SIZE: SliderSpec = SliderSpec::new("heal-size", "Size", MIN_RADIUS, MAX_RADIUS, 24.0)
    .scaled(1.0, 0)
    .log();
const FEATHER: SliderSpec = SliderSpec::new("heal-feather", "Feather", 0.0, 1.0, 0.3).percent();
const OPACITY: SliderSpec = SliderSpec::new("heal-opacity", "Opacity", 0.0, 1.0, 1.0).percent();

fn heal_slider(ui: &mut egui::Ui, spec: &SliderSpec, value: &mut f32) -> bool {
    fur::slider(ui, spec, value, true).changed
}

pub fn show_panel(ui: &mut egui::Ui, develop: &mut DevelopDoc, heal: &mut HealUi, pounce: &Pounce) {
    ui.separator();
    ui.heading("Heal / Remove");
    ui.label("Click the photo to place a spot. [ and ] resize; Delete removes the selected spot.");

    ui.horizontal(|ui| {
        for kind in [SpotKind::Heal, SpotKind::Clone, SpotKind::Remove] {
            let label = if kind == SpotKind::Remove {
                "Remove (AI)"
            } else {
                kind_label(kind)
            };
            ui.selectable_value(&mut heal.kind, kind, label);
        }
    });

    let mut params: HealParams = develop.stage_params(HEAL);
    let editing = heal
        .selected
        .filter(|&i| i < params.spots.len() && params.spots[i].kind != SpotKind::Remove);
    let mut edited = false;
    match editing {
        Some(i) => {
            ui.label(format!("Editing spot #{}", i + 1));
            let s = &mut params.spots[i];
            let mut frac = if s.radius > 0.0 {
                s.feather / s.radius
            } else {
                0.0
            };
            edited |= heal_slider(ui, &SIZE, &mut s.radius);
            edited |= heal_slider(ui, &FEATHER, &mut frac);
            edited |= heal_slider(ui, &OPACITY, &mut s.opacity);
            s.radius = clamp_radius(s.radius);
            s.feather = frac * s.radius;
        }
        None => {
            ui.label("New spot");
            heal_slider(ui, &SIZE, &mut heal.radius);
            heal_slider(ui, &FEATHER, &mut heal.feather_frac);
            heal_slider(ui, &OPACITY, &mut heal.opacity);
        }
    }
    if edited {
        develop.set_stage_params(HEAL, &params);
    }

    if heal.kind == SpotKind::Remove {
        show_models(ui, heal, pounce);
    }

    ui.separator();
    if params.spots.is_empty() {
        ui.label("No spots yet.");
    }
    let mut delete = None;
    for (i, s) in params.spots.iter().enumerate() {
        ui.horizontal(|ui| {
            let mut text = format!("#{} {}", i + 1, kind_label(s.kind));
            if s.kind == SpotKind::Remove
                && heal.service.is_pending(develop.frame_key(), &spot_key(s))
            {
                text.push_str(" (working...)");
            }
            if ui
                .selectable_label(heal.selected == Some(i), text)
                .clicked()
            {
                heal.selected = Some(i);
            }
            if ui
                .small_button("x")
                .on_hover_text("Delete this spot")
                .clicked()
            {
                delete = Some(i);
            }
        });
    }
    if let Some(i) = delete {
        delete_spot(develop, &mut params, i);
        heal.selected = None;
    }
    if !params.spots.is_empty() && ui.button("Clear all spots").clicked() {
        params.spots.clear();
        develop.set_stage_params(HEAL, &params);
        develop.prune_removals();
        heal.selected = None;
    }

    if let Some(status) = &heal.status {
        ui.label(status.clone());
    }
}

fn show_models(ui: &mut egui::Ui, heal: &mut HealUi, pounce: &Pounce) {
    if heal.service.models().is_some() && heal.service.install_progress().is_none() {
        ui.label("AI removal models are installed.");
        if heal.needs_repair {
            ui.colored_label(
                egui::Color32::from_rgb(255, 130, 0),
                "A model file failed its integrity check.",
            );
            if ui.button("Repair models").clicked() {
                heal.status = heal.service.start_repair(pounce).err();
            }
        }
        return;
    }
    if let Some((done, total)) = heal.service.install_progress() {
        let fraction = if total == 0 {
            0.0
        } else {
            done as f32 / total as f32
        };
        ui.add(egui::ProgressBar::new(fraction).text(format!(
            "{} / {} MB",
            megabytes(done),
            megabytes(total)
        )));
        if ui.button("Cancel download").clicked() {
            heal.service.cancel_install();
        }
        return;
    }
    let mb = megabytes(heal.service.download_bytes());
    ui.group(|ui| {
        ui.label(format!(
            "AI removal needs a one-time download of about {mb} MB:\n\
             - MobileSAM (Apache-2.0), from huggingface.co/Acly/MobileSAM\n\
             - LaMa (Apache-2.0 code; the weights were trained on Places2, whose terms limit it \
             to non-commercial research), from huggingface.co/Carve/LaMa-ONNX\n\
             - ONNX Runtime (MIT), from github.com/microsoft/onnxruntime\n\
             Everything runs on this computer; nothing is uploaded."
        ));
        if ui.button("Download models").clicked() {
            heal.status = heal.service.start_install(pounce).err();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::DevelopView;
    use nicti_groom::remove::{RemovalBackend, RemovalRequest};
    use nicti_groom::RemovalError;
    use nicti_pounce::JobState;
    use nicti_stalk::models::DownloadError;

    fn heal_spot(center: (f32, f32), radius: f32) -> Spot {
        Spot::heal_spot(center, radius, (50.0, 0.0), 2.0)
    }

    #[test]
    fn new_spot_clamps_size_and_derives_feather_from_the_fraction() {
        let s = new_spot(
            SpotKind::Heal,
            (10.0, 10.0),
            1.0,
            0.5,
            2.0,
            Some((1.0, 1.0)),
            None,
        );
        assert_eq!(s.radius, MIN_RADIUS);
        assert_eq!(s.feather, 0.5 * MIN_RADIUS);
        assert_eq!(s.opacity, 1.0);
        let big = new_spot(
            SpotKind::Clone,
            (0.0, 0.0),
            1e9,
            5.0,
            -1.0,
            Some((1.0, 1.0)),
            None,
        );
        assert_eq!(big.radius, MAX_RADIUS);
        assert_eq!(big.feather, MAX_RADIUS, "feather fraction is clamped to 1");
        assert_eq!(big.opacity, 0.0);
    }

    #[test]
    fn spot_at_picks_the_topmost_of_overlapping_spots() {
        let spots = vec![
            heal_spot((100.0, 100.0), 30.0),
            heal_spot((110.0, 100.0), 30.0),
        ];
        assert_eq!(
            spot_at(&spots, (105.0, 100.0)),
            Some(1),
            "later spot is on top"
        );
        assert_eq!(
            spot_at(&spots, (75.0, 100.0)),
            Some(0),
            "only the first reaches here"
        );
        assert_eq!(spot_at(&spots, (300.0, 300.0)), None);
        assert_eq!(spot_at(&[], (0.0, 0.0)), None);
    }

    #[test]
    fn source_handle_hit_test_uses_the_offset_and_ignores_removals() {
        let s = heal_spot((100.0, 100.0), 20.0); // source center (150, 100)
        assert!(on_source_handle(&s, (152.0, 101.0), 5.0));
        assert!(
            !on_source_handle(&s, (100.0, 100.0), 5.0),
            "the destination isn't the source"
        );
        let remove = Spot::remove_spot(
            (0.0, 0.0),
            10.0,
            1.0,
            removal_recipe(Prompt::Click { x: 0.0, y: 0.0 }),
        );
        assert!(!on_source_handle(&remove, (0.0, 0.0), 100.0));
    }

    #[test]
    fn fallback_offset_points_toward_the_middle_of_the_image() {
        let source = (1000.0, 800.0);
        assert_eq!(fallback_offset((100.0, 400.0), 20.0, source), (60.0, 0.0));
        assert_eq!(fallback_offset((900.0, 400.0), 20.0, source), (-60.0, 0.0));
    }

    #[test]
    fn fallback_offset_never_puts_the_source_outside_the_frame() {
        // A 64 px frame with a 24 px brush: 3 radii sideways would land at x = 91.
        let (dx, dy) = fallback_offset((19.0, 32.0), 24.0, (64.0, 64.0));
        assert_eq!(dy, 0.0);
        assert!(
            (0.0..=64.0).contains(&(19.0 + dx)),
            "source centre at {}",
            19.0 + dx
        );
        let (dx, _) = fallback_offset((45.0, 32.0), 24.0, (64.0, 64.0));
        assert!((0.0..=64.0).contains(&(45.0 + dx)));
    }

    #[test]
    fn removal_recipe_carries_the_prompt_and_pins_the_models() {
        let r = removal_recipe(Prompt::Click { x: 12.0, y: 34.0 });
        assert_eq!(r.model_id, RECIPE_MODEL_ID);
        assert_eq!(r.model_version, RECIPE_MODEL_VERSION);
        assert_eq!(
            Prompt::from_json(&r.params),
            Some(Prompt::Click { x: 12.0, y: 34.0 })
        );
    }

    #[test]
    fn megabytes_rounds_up() {
        assert_eq!(megabytes(0), 0);
        assert_eq!(megabytes(1), 1);
        assert_eq!(megabytes(1_000_000), 1);
        assert_eq!(megabytes(1_000_001), 2);
    }

    // -- RemovalService ---------------------------------------------------------------------

    /// A removal backend that returns a fixed patch, or fails.
    struct Fake {
        fail: bool,
    }

    impl RemovalBackend for Fake {
        fn remove(&mut self, req: &RemovalRequest<'_>) -> Result<RemovalPatch, RemovalError> {
            if self.fail {
                return Err(RemovalError::NoObject);
            }
            RemovalPatch::new(
                (req.center.0 as i32, req.center.1 as i32),
                3,
                vec![[0.4, 0.4, 0.4, 1.0]; 9],
            )
            .map_err(|e| RemovalError::BadInput(e.to_string()))
        }
    }

    fn pounce() -> Pounce {
        Pounce::new(u64::MAX, 2, 1, || {})
    }

    /// Waits until Pounce reports no queued or running jobs.
    fn drain(p: &Pounce) {
        for _ in 0..500 {
            if p.snapshot()
                .iter()
                .all(|s| !matches!(s.state, JobState::Queued | JobState::Running))
            {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("jobs did not finish");
    }

    fn gpu_develop() -> Option<DevelopView> {
        crate::test_gpu::shared().map(DevelopView::new)
    }

    #[test]
    fn a_submitted_removal_comes_back_as_an_event_keyed_by_the_spot() {
        let Some(develop) = gpu_develop() else { return };
        let p = pounce();
        let mut svc = RemovalService::with_store(None);
        let spot = Spot::remove_spot(
            (20.0, 20.0),
            10.0,
            2.0,
            removal_recipe(Prompt::Click { x: 20.0, y: 20.0 }),
        );
        let backend: SharedBackend = Arc::new(Mutex::new(Fake { fail: false }));
        svc.submit_with_backend(
            &p,
            backend,
            &develop,
            &spot,
            Prompt::Click { x: 20.0, y: 20.0 },
        )
        .unwrap();
        assert!(svc.is_pending(develop.frame_key(), &spot_key(&spot)));
        drain(&p);
        let events = svc.poll_removals();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].key, spot_key(&spot));
        assert_eq!(events[0].result.as_ref().unwrap().side, 3);
        assert!(!svc.is_pending(develop.frame_key(), &spot_key(&spot)));
        assert!(
            svc.poll_removals().is_empty(),
            "each result is delivered once"
        );
    }

    #[test]
    fn a_failed_removal_is_an_error_event_not_a_lost_one() {
        let Some(develop) = gpu_develop() else { return };
        let p = pounce();
        let mut svc = RemovalService::with_store(None);
        let spot = Spot::remove_spot(
            (20.0, 20.0),
            10.0,
            2.0,
            removal_recipe(Prompt::Click { x: 20.0, y: 20.0 }),
        );
        let backend: SharedBackend = Arc::new(Mutex::new(Fake { fail: true }));
        svc.submit_with_backend(
            &p,
            backend,
            &develop,
            &spot,
            Prompt::Click { x: 20.0, y: 20.0 },
        )
        .unwrap();
        drain(&p);
        let events = svc.poll_removals();
        assert!(events[0].result.as_ref().unwrap_err().contains("no object"));
    }

    #[test]
    fn resubmitting_the_same_spot_while_it_runs_does_not_queue_a_second_job() {
        let Some(develop) = gpu_develop() else { return };
        let p = pounce();
        let mut svc = RemovalService::with_store(None);
        let spot = Spot::remove_spot(
            (20.0, 20.0),
            10.0,
            2.0,
            removal_recipe(Prompt::Click { x: 20.0, y: 20.0 }),
        );
        let backend: SharedBackend = Arc::new(Mutex::new(Fake { fail: false }));
        for _ in 0..3 {
            svc.submit_with_backend(
                &p,
                Arc::clone(&backend),
                &develop,
                &spot,
                Prompt::Click { x: 20.0, y: 20.0 },
            )
            .unwrap();
        }
        assert_eq!(svc.pending_count(), 1);
    }

    #[test]
    fn submitting_without_installed_models_is_a_clear_error() {
        let Some(develop) = gpu_develop() else { return };
        let p = pounce();
        let dir =
            std::env::temp_dir().join(format!("nicti-heal-tool-empty-{}", std::process::id()));
        let mut svc = RemovalService::with_store(Some(ModelStore::new(dir)));
        let spot = Spot::remove_spot(
            (1.0, 1.0),
            10.0,
            2.0,
            removal_recipe(Prompt::Click { x: 1.0, y: 1.0 }),
        );
        let err = svc
            .submit(&p, &develop, &spot, Prompt::Click { x: 1.0, y: 1.0 })
            .unwrap_err();
        assert!(err.contains("aren't installed"), "{err}");
        assert_eq!(svc.pending_count(), 0);
    }

    #[test]
    fn install_reports_a_download_failure_through_poll_install() {
        // HttpDownloader is unsupported off Windows and would hit the network on it; this test
        // only asserts the non-Windows failure path, where nothing can leave the machine.
        if cfg!(windows) {
            return;
        }
        let p = pounce();
        let dir =
            std::env::temp_dir().join(format!("nicti-heal-tool-install-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut svc = RemovalService::with_store(Some(ModelStore::new(dir)));
        assert!(svc.download_bytes() > 0);
        svc.start_install(&p).unwrap();
        assert!(svc.is_installing());
        drain(&p);
        let result = svc.poll_install().expect("install finished");
        let msg = result.unwrap_err();
        assert!(msg.contains("Couldn't download"), "{msg}");
        assert!(!svc.is_installing());
        assert!(svc.poll_install().is_none(), "delivered once");
        let _ = DownloadError::Unsupported; // the underlying cause on this platform
    }

    #[test]
    fn a_finished_install_drops_the_cached_backend_but_a_failed_one_keeps_it() {
        use std::sync::atomic::{AtomicBool, AtomicU64};
        let handle = |result: Result<(), String>| InstallHandle {
            bytes: Arc::new(AtomicU64::new(0)),
            total: Arc::new(AtomicU64::new(0)),
            cancel: Arc::new(AtomicBool::new(false)),
            result: Arc::new(Mutex::new(Some(result))),
        };
        let backend: SharedBackend = Arc::new(Mutex::new(Fake { fail: false }));
        let mut svc = RemovalService::with_store(None);

        svc.backend = Some((fake_models(), Arc::clone(&backend)));
        svc.failed_reruns.insert((1, "k".into()));
        svc.models_missing_for = Some(1);
        svc.models_checked = Some(std::time::Instant::now());
        svc.install = Some(handle(Ok(())));
        assert!(svc.poll_install().unwrap().is_ok());
        assert!(
            svc.failed_reruns.is_empty()
                && svc.models_missing_for.is_none()
                && svc.models_checked.is_none(),
            "a fresh install gives failed re-runs and the missing-models notice a clean slate"
        );
        assert!(
            svc.backend.is_none(),
            "a repair may have replaced the files it was built over"
        );

        svc.backend = Some((fake_models(), backend));
        svc.failed_reruns.insert((1, "k".into()));
        svc.install = Some(handle(Err("network down".into())));
        assert!(svc.poll_install().unwrap().is_err());
        assert!(
            !svc.failed_reruns.is_empty(),
            "a failed install fixes nothing"
        );
        assert!(svc.backend.is_some(), "nothing changed on disk, so keep it");
    }

    #[test]
    fn a_missing_store_cant_start_an_install() {
        let p = pounce();
        let mut svc = RemovalService::with_store(None);
        assert!(svc.start_install(&p).is_err());
        assert!(!svc.is_installing());
    }

    // -- Gesture tests: real egui input driven through `handle_viewport` -----------------------

    use egui::{pos2, vec2, Event, Modifiers, PointerButton, Pos2, Rect};

    /// A headless egui context that runs one frame at a time with scripted input.
    struct Harness {
        ctx: egui::Context,
        rect: Rect,
        time: f64,
    }

    impl Harness {
        /// Registers the viewport widget (egui hit-tests against the previous frame's layout).
        fn new(develop: &mut DevelopView, heal: &mut HealUi, pounce: &Pounce) -> Self {
            let mut h = Self {
                ctx: egui::Context::default(),
                rect: Rect::NOTHING,
                time: 0.0,
            };
            h.frame(vec![], develop, heal, pounce);
            h
        }

        fn frame(
            &mut self,
            events: Vec<Event>,
            develop: &mut DevelopView,
            heal: &mut HealUi,
            pounce: &Pounce,
        ) {
            self.time += 0.1;
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(900.0, 700.0))),
                time: Some(self.time),
                events,
                ..Default::default()
            };
            let mut rect = self.rect;
            let output = self.ctx.run_ui(input, |ui| {
                let (r, response) =
                    ui.allocate_exact_size(vec2(600.0, 450.0), egui::Sense::click_and_drag());
                rect = r;
                handle_viewport(ui, &response, r, develop, heal, pounce);
            });
            // Headless: there is no renderer to apply the frame's texture uploads to.
            output.drop_without_applying_deltas();
            self.rect = rect;
        }

        /// Screen position of a point given as a fraction of the viewport.
        fn at(&self, fx: f32, fy: f32) -> Pos2 {
            pos2(
                self.rect.left() + fx * self.rect.width(),
                self.rect.top() + fy * self.rect.height(),
            )
        }

        fn button(pos: Pos2, pressed: bool) -> Event {
            Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed,
                modifiers: Modifiers::NONE,
            }
        }

        fn click(&mut self, pos: Pos2, develop: &mut DevelopView, heal: &mut HealUi, p: &Pounce) {
            self.frame(vec![Event::PointerMoved(pos)], develop, heal, p);
            self.frame(vec![Self::button(pos, true)], develop, heal, p);
            self.frame(vec![Self::button(pos, false)], develop, heal, p);
        }

        fn drag(
            &mut self,
            from: Pos2,
            to: Pos2,
            develop: &mut DevelopView,
            heal: &mut HealUi,
            p: &Pounce,
        ) {
            self.frame(vec![Event::PointerMoved(from)], develop, heal, p);
            self.frame(vec![Self::button(from, true)], develop, heal, p);
            // Several moves so egui's drag threshold is crossed and the drag keeps tracking.
            for step in 1..=4 {
                let t = step as f32 / 4.0;
                let pos = from + (to - from) * t;
                self.frame(vec![Event::PointerMoved(pos)], develop, heal, p);
            }
            self.frame(vec![Self::button(to, false)], develop, heal, p);
        }

        fn key(
            &mut self,
            key: egui::Key,
            at: Pos2,
            develop: &mut DevelopView,
            heal: &mut HealUi,
            p: &Pounce,
        ) {
            self.frame(vec![Event::PointerMoved(at)], develop, heal, p);
            self.frame(
                vec![Event::Key {
                    key,
                    physical_key: Some(key),
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                }],
                develop,
                heal,
                p,
            );
        }
    }

    fn spots(develop: &DevelopView) -> Vec<Spot> {
        develop.stage_params::<HealParams>(HEAL).spots
    }

    /// (develop, heal ui, pounce, harness), or `None` when there is no GPU adapter.
    fn rig() -> Option<(DevelopView, HealUi, Pounce, Harness)> {
        let mut develop = gpu_develop()?;
        let mut heal = HealUi::new();
        heal.service = RemovalService::with_store(None);
        heal.tool = Tool::Heal;
        let pounce = pounce();
        let harness = Harness::new(&mut develop, &mut heal, &pounce);
        Some((develop, heal, pounce, harness))
    }

    #[test]
    fn clicking_the_photo_places_a_heal_spot_there() {
        let Some((mut develop, mut heal, p, mut h)) = rig() else {
            return;
        };
        let source = develop.source_extent();
        h.click(h.at(0.5, 0.5), &mut develop, &mut heal, &p);
        let s = spots(&develop);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].kind, SpotKind::Heal);
        assert!(
            (s[0].center.0 - source.0 / 2.0).abs() < 2.0,
            "{:?}",
            s[0].center
        );
        assert!(
            (s[0].center.1 - source.1 / 2.0).abs() < 2.0,
            "{:?}",
            s[0].center
        );
        assert!(
            s[0].source_offset.is_some(),
            "a heal spot always has a source"
        );
        assert_eq!(s[0].radius, heal.radius);
    }

    #[test]
    fn a_click_maps_through_the_stretch_to_the_right_source_pixel() {
        let Some((mut develop, mut heal, p, mut h)) = rig() else {
            return;
        };
        let source = develop.source_extent();
        h.click(h.at(0.25, 0.75), &mut develop, &mut heal, &p);
        let c = spots(&develop)[0].center;
        assert!(
            (c.0 - 0.25 * source.0).abs() < 2.0 && (c.1 - 0.75 * source.1).abs() < 2.0,
            "{c:?}"
        );
    }

    #[test]
    fn clicking_an_existing_spot_selects_it_instead_of_adding_another() {
        let Some((mut develop, mut heal, p, mut h)) = rig() else {
            return;
        };
        h.click(h.at(0.3, 0.3), &mut develop, &mut heal, &p);
        h.click(h.at(0.7, 0.7), &mut develop, &mut heal, &p);
        assert_eq!(spots(&develop).len(), 2);
        assert_eq!(heal.selected, Some(1));
        h.click(h.at(0.3, 0.3), &mut develop, &mut heal, &p);
        assert_eq!(spots(&develop).len(), 2, "no third spot");
        assert_eq!(heal.selected, Some(0));
    }

    #[test]
    fn dragging_the_selected_spot_moves_its_destination_and_keeps_its_source_offset() {
        let Some((mut develop, mut heal, p, mut h)) = rig() else {
            return;
        };
        let source = develop.source_extent();
        h.click(h.at(0.4, 0.4), &mut develop, &mut heal, &p);
        let before = spots(&develop)[0].clone();
        h.drag(h.at(0.4, 0.4), h.at(0.6, 0.5), &mut develop, &mut heal, &p);
        let after = spots(&develop)[0].clone();
        assert!(
            (after.center.0 - 0.6 * source.0).abs() < 2.0,
            "{:?}",
            after.center
        );
        assert!(
            (after.center.1 - 0.5 * source.1).abs() < 2.0,
            "{:?}",
            after.center
        );
        assert_eq!(after.source_offset, before.source_offset);
        assert_eq!(spots(&develop).len(), 1);
    }

    #[test]
    fn dragging_the_source_handle_changes_only_the_offset() {
        let Some((mut develop, mut heal, p, mut h)) = rig() else {
            return;
        };
        // The synthetic frame is only 64 px wide: a small brush keeps the source inside it.
        heal.radius = 6.0;
        let source = develop.source_extent();
        h.click(h.at(0.3, 0.5), &mut develop, &mut heal, &p);
        let before = spots(&develop)[0].clone();
        let off = before.source_offset.unwrap();
        // Screen position of the source handle.
        let src_frac = (
            (before.center.0 + off.0) / source.0,
            (before.center.1 + off.1) / source.1,
        );
        // Grab the handle 3 screen px off its exact centre (a real click never lands dead-centre;
        // this is well inside the 7 px hit radius, and would miss with a zero-size one). The drag
        // is anchored where the button went down, so the offset changes by exactly the pointer's
        // travel regardless of where on the handle it was grabbed.
        let grab = h.at(src_frac.0, src_frac.1) + vec2(3.0, 2.0);
        let travel = vec2(0.1 * h.rect.width(), 0.05 * h.rect.height());
        h.drag(grab, grab + travel, &mut develop, &mut heal, &p);
        let after = spots(&develop)[0].clone();
        assert_eq!(after.center, before.center, "destination must not move");
        let new_off = after.source_offset.unwrap();
        assert!(
            (new_off.0 - off.0 - 0.1 * source.0).abs() < 2.0,
            "{new_off:?} vs {off:?}"
        );
        assert!(
            (new_off.1 - off.1 - 0.05 * source.1).abs() < 2.0,
            "{new_off:?} vs {off:?}"
        );
    }

    #[test]
    fn dragging_empty_space_moves_nothing() {
        let Some((mut develop, mut heal, p, mut h)) = rig() else {
            return;
        };
        h.click(h.at(0.2, 0.2), &mut develop, &mut heal, &p);
        let before = spots(&develop);
        h.drag(h.at(0.8, 0.8), h.at(0.9, 0.9), &mut develop, &mut heal, &p);
        assert_eq!(spots(&develop), before);
    }

    #[test]
    fn delete_removes_the_selected_spot() {
        let Some((mut develop, mut heal, p, mut h)) = rig() else {
            return;
        };
        h.click(h.at(0.3, 0.3), &mut develop, &mut heal, &p);
        h.click(h.at(0.7, 0.7), &mut develop, &mut heal, &p);
        assert_eq!(heal.selected, Some(1));
        h.key(
            egui::Key::Delete,
            h.at(0.5, 0.5),
            &mut develop,
            &mut heal,
            &p,
        );
        let left = spots(&develop);
        assert_eq!(left.len(), 1);
        assert!((left[0].center.0 - 0.3 * develop.source_extent().0).abs() < 2.0);
        assert_eq!(heal.selected, None);
    }

    #[test]
    fn delete_with_nothing_selected_removes_nothing() {
        let Some((mut develop, mut heal, p, mut h)) = rig() else {
            return;
        };
        h.click(h.at(0.3, 0.3), &mut develop, &mut heal, &p);
        h.click(h.at(0.9, 0.1), &mut develop, &mut heal, &p); // places a 2nd, selects it
        heal.selected = None;
        h.key(
            egui::Key::Delete,
            h.at(0.5, 0.5),
            &mut develop,
            &mut heal,
            &p,
        );
        assert_eq!(spots(&develop).len(), 2);
    }

    #[test]
    fn bracket_keys_resize_the_default_brush_when_no_spot_is_selected() {
        let Some((mut develop, mut heal, p, mut h)) = rig() else {
            return;
        };
        let r0 = heal.radius;
        h.key(
            egui::Key::CloseBracket,
            h.at(0.5, 0.5),
            &mut develop,
            &mut heal,
            &p,
        );
        assert!(heal.radius > r0);
        let r1 = heal.radius;
        h.key(
            egui::Key::OpenBracket,
            h.at(0.5, 0.5),
            &mut develop,
            &mut heal,
            &p,
        );
        h.key(
            egui::Key::OpenBracket,
            h.at(0.5, 0.5),
            &mut develop,
            &mut heal,
            &p,
        );
        assert!(heal.radius < r1);
        for _ in 0..200 {
            h.key(
                egui::Key::OpenBracket,
                h.at(0.5, 0.5),
                &mut develop,
                &mut heal,
                &p,
            );
        }
        assert_eq!(heal.radius, MIN_RADIUS, "size is clamped");
    }

    #[test]
    fn bracket_keys_resize_the_selected_spot_and_scale_its_feather_with_it() {
        let Some((mut develop, mut heal, p, mut h)) = rig() else {
            return;
        };
        h.click(h.at(0.5, 0.5), &mut develop, &mut heal, &p);
        let before = spots(&develop)[0].clone();
        h.key(
            egui::Key::CloseBracket,
            h.at(0.5, 0.5),
            &mut develop,
            &mut heal,
            &p,
        );
        let after = spots(&develop)[0].clone();
        assert!(after.radius > before.radius);
        assert!((after.feather / after.radius - before.feather / before.radius).abs() < 1e-4);
        assert_eq!(after.center, before.center);
    }

    #[test]
    fn the_clone_kind_places_a_clone_spot() {
        let Some((mut develop, mut heal, p, mut h)) = rig() else {
            return;
        };
        heal.kind = SpotKind::Clone;
        h.click(h.at(0.5, 0.5), &mut develop, &mut heal, &p);
        assert_eq!(spots(&develop)[0].kind, SpotKind::Clone);
    }

    #[test]
    fn remove_without_models_places_no_spot_and_says_why() {
        let Some((mut develop, mut heal, p, mut h)) = rig() else {
            return;
        };
        heal.kind = SpotKind::Remove;
        h.click(h.at(0.5, 0.5), &mut develop, &mut heal, &p);
        assert!(spots(&develop).is_empty());
        assert!(heal
            .status
            .as_deref()
            .unwrap_or("")
            .contains("model download"));
        assert_eq!(heal.service.pending_count(), 0);
    }

    fn fake_models() -> RemovalModels {
        let f = std::path::PathBuf::from("/nonexistent/fake");
        RemovalModels {
            ort_dylib: f.clone(),
            gpu_runtime: false,
            sam_encoder: f.clone(),
            sam_decoder: f.clone(),
            lama: f,
        }
    }

    /// Reads back the current render, for "did the removal change the picture" assertions.
    fn pixels(develop: &mut DevelopView) -> Vec<[f32; 4]> {
        let gpu = crate::test_gpu::shared().unwrap();
        let frame = develop.render();
        nicti_tapetum::frame::read_frame(&gpu, &frame)
    }

    #[test]
    fn a_remove_click_runs_a_job_and_the_patch_changes_the_render() {
        let Some((mut develop, mut heal, p, mut h)) = rig() else {
            return;
        };
        heal.kind = SpotKind::Remove;
        heal.service.models_override = Some(fake_models());
        heal.service.backend_override = Some(Arc::new(Mutex::new(Fake { fail: false })));
        let before = pixels(&mut develop);

        h.click(h.at(0.5, 0.5), &mut develop, &mut heal, &p);
        let placed = spots(&develop);
        assert_eq!(placed.len(), 1);
        assert_eq!(placed[0].kind, SpotKind::Remove);
        let prompt = Prompt::from_json(&placed[0].mask_recipe.as_ref().unwrap().params);
        assert!(
            matches!(prompt, Some(Prompt::Click { .. })),
            "the recipe stores the click"
        );
        assert!(heal
            .service
            .is_pending(develop.frame_key(), &spot_key(&placed[0])));

        // Until the job lands the render is unchanged (pending spots pass through).
        assert_eq!(pixels(&mut develop), before);

        drain(&p);
        let egui_ctx = egui::Context::default();
        egui_ctx
            .run_ui(egui::RawInput::default(), |ui| {
                poll(ui, &mut develop, &mut heal)
            })
            .drop_without_applying_deltas();
        assert_eq!(heal.status.as_deref(), Some("Object removed."));
        assert_eq!(spots(&develop).len(), 1, "the spot stays");
        assert_ne!(
            pixels(&mut develop),
            before,
            "the fake patch (0.4 gray) must now show"
        );
    }

    #[test]
    fn a_failed_removal_drops_its_placeholder_spot_and_reports() {
        let Some((mut develop, mut heal, p, mut h)) = rig() else {
            return;
        };
        heal.kind = SpotKind::Remove;
        heal.service.models_override = Some(fake_models());
        heal.service.backend_override = Some(Arc::new(Mutex::new(Fake { fail: true })));
        h.click(h.at(0.5, 0.5), &mut develop, &mut heal, &p);
        assert_eq!(spots(&develop).len(), 1);
        drain(&p);
        let ctx = egui::Context::default();
        ctx.run_ui(egui::RawInput::default(), |ui| {
            poll(ui, &mut develop, &mut heal)
        })
        .drop_without_applying_deltas();
        assert!(
            spots(&develop).is_empty(),
            "a failed removal must not leave a dead spot behind"
        );
        assert!(heal.status.as_deref().unwrap_or("").contains("no object"));
    }

    /// A photo opened with a saved AI Remove spot has the recipe but no fill: the fill is re-run
    /// from the recipe (#324), and undoing the spot's deletion afterwards needs no second run.
    #[test]
    fn a_saved_remove_spot_is_re_run_from_its_recipe_and_survives_delete_and_undo() {
        let Some((mut develop, mut heal, p, _)) = rig() else {
            return;
        };
        heal.service.models_override = Some(fake_models());
        heal.service.backend_override = Some(Arc::new(Mutex::new(Fake { fail: false })));
        let spot = Spot::remove_spot(
            (20.0, 20.0),
            10.0,
            2.0,
            removal_recipe(Prompt::Click { x: 20.0, y: 20.0 }),
        );
        develop.set_stage_params(
            HEAL,
            &HealParams {
                spots: vec![spot.clone()],
            },
        );
        let key = spot_key(&spot);
        assert!(!develop.has_removal(&key), "only the recipe is stored");

        rerun_missing_removals(&develop, &mut heal, &p);
        assert!(heal.service.is_pending(develop.frame_key(), &key));
        rerun_missing_removals(&develop, &mut heal, &p);
        assert_eq!(heal.service.pending_count(), 1, "one job per spot");
        drain(&p);
        let ctx = egui::Context::default();
        ctx.run_ui(egui::RawInput::default(), |ui| {
            poll(ui, &mut develop, &mut heal)
        })
        .drop_without_applying_deltas();
        assert!(develop.has_removal(&key), "the fill is back");

        // Delete the spot and undo: the cached fill is still there, so nothing re-runs.
        develop.disable_gesture_merging();
        crate::render::wait_for_next_ms();
        develop.set_stage_params(HEAL, &HealParams::default());
        assert!(spots(&develop).is_empty());
        assert!(develop.undo());
        assert_eq!(spots(&develop).len(), 1);
        rerun_missing_removals(&develop, &mut heal, &p);
        assert_eq!(
            heal.service.pending_count(),
            0,
            "the fill survived the delete"
        );
    }

    /// A re-run that fails must leave the user's spot in place, say why, and not retry every frame.
    #[test]
    fn a_failed_re_run_keeps_the_spot_and_reports_once() {
        let Some((mut develop, mut heal, p, _)) = rig() else {
            return;
        };
        heal.service.models_override = Some(fake_models());
        heal.service.backend_override = Some(Arc::new(Mutex::new(Fake { fail: true })));
        let spot = Spot::remove_spot(
            (20.0, 20.0),
            10.0,
            2.0,
            removal_recipe(Prompt::Click { x: 20.0, y: 20.0 }),
        );
        develop.set_stage_params(HEAL, &HealParams { spots: vec![spot] });
        rerun_missing_removals(&develop, &mut heal, &p);
        drain(&p);
        let ctx = egui::Context::default();
        ctx.run_ui(egui::RawInput::default(), |ui| {
            poll(ui, &mut develop, &mut heal)
        })
        .drop_without_applying_deltas();
        assert_eq!(spots(&develop).len(), 1, "the spot is not dropped");
        assert!(heal
            .status
            .as_deref()
            .unwrap_or("")
            .contains("Couldn't re-apply"));
        rerun_missing_removals(&develop, &mut heal, &p);
        assert_eq!(
            heal.service.pending_count(),
            0,
            "a failed spot isn't retried"
        );
    }

    /// Without the models the spot can't be re-run: say so, keep the spot.
    #[test]
    fn a_re_run_without_models_reports_and_keeps_the_spot() {
        let Some((mut develop, mut heal, p, _)) = rig() else {
            return;
        };
        let spot = Spot::remove_spot(
            (20.0, 20.0),
            10.0,
            2.0,
            removal_recipe(Prompt::Click { x: 20.0, y: 20.0 }),
        );
        develop.set_stage_params(HEAL, &HealParams { spots: vec![spot] });
        rerun_missing_removals(&develop, &mut heal, &p);
        assert_eq!(spots(&develop).len(), 1);
        assert_eq!(heal.service.pending_count(), 0);
        assert!(heal
            .status
            .as_deref()
            .unwrap_or("")
            .contains("model download"));
    }

    /// The auto-drop of a failed placement is an undoable step; undoing it must bring the spot
    /// back without quietly retrying the removal that just failed.
    #[test]
    fn undoing_a_failed_placements_drop_does_not_retry_it() {
        let Some((mut develop, mut heal, p, mut h)) = rig() else {
            return;
        };
        develop.disable_gesture_merging();
        heal.kind = SpotKind::Remove;
        heal.service.models_override = Some(fake_models());
        heal.service.backend_override = Some(Arc::new(Mutex::new(Fake { fail: true })));
        h.click(h.at(0.5, 0.5), &mut develop, &mut heal, &p);
        drain(&p);
        crate::render::wait_for_next_ms();
        let ctx = egui::Context::default();
        ctx.run_ui(egui::RawInput::default(), |ui| {
            poll(ui, &mut develop, &mut heal)
        })
        .drop_without_applying_deltas();
        assert!(spots(&develop).is_empty());
        assert!(develop.undo());
        assert_eq!(spots(&develop).len(), 1, "undo restores the dropped spot");
        rerun_missing_removals(&develop, &mut heal, &p);
        assert_eq!(heal.service.pending_count(), 0, "but doesn't re-run it");
    }

    /// The missing-models notice is shown once, not re-set (or re-checked on disk) every frame.
    #[test]
    fn the_missing_models_notice_is_reported_once() {
        let Some((mut develop, mut heal, p, _)) = rig() else {
            return;
        };
        let spot = Spot::remove_spot(
            (20.0, 20.0),
            10.0,
            2.0,
            removal_recipe(Prompt::Click { x: 20.0, y: 20.0 }),
        );
        develop.set_stage_params(HEAL, &HealParams { spots: vec![spot] });
        rerun_missing_removals(&develop, &mut heal, &p);
        assert!(heal.status.is_some());
        heal.status = None; // the user dismissed it
        rerun_missing_removals(&develop, &mut heal, &p);
        assert!(heal.status.is_none(), "not shown again every frame");

        // A different photo with missing removals is told too.
        heal.service.models_missing_for = Some(develop.frame_key().wrapping_add(1));
        rerun_missing_removals(&develop, &mut heal, &p);
        assert!(heal.status.is_some(), "the notice is per photo");
    }

    /// An unsupported recipe and absent models are both reported, not one hiding the other.
    #[test]
    fn an_unsupported_recipe_is_reported_alongside_missing_models() {
        let Some((mut develop, mut heal, p, _)) = rig() else {
            return;
        };
        let mut foreign = Spot::remove_spot(
            (60.0, 60.0),
            10.0,
            2.0,
            removal_recipe(Prompt::Click { x: 60.0, y: 60.0 }),
        );
        foreign.mask_recipe.as_mut().unwrap().model_id = "someone.else".to_owned();
        let valid = Spot::remove_spot(
            (20.0, 20.0),
            10.0,
            2.0,
            removal_recipe(Prompt::Click { x: 20.0, y: 20.0 }),
        );
        develop.set_stage_params(
            HEAL,
            &HealParams {
                spots: vec![foreign, valid],
            },
        );
        rerun_missing_removals(&develop, &mut heal, &p);
        let status = heal.status.unwrap_or_default();
        assert!(status.contains("can't re-run"), "{status}");
        assert!(status.contains("model download"), "{status}");
    }

    #[test]
    fn deleting_a_pending_removal_discards_its_late_result() {
        let Some((mut develop, mut heal, p, mut h)) = rig() else {
            return;
        };
        heal.kind = SpotKind::Remove;
        heal.service.models_override = Some(fake_models());
        heal.service.backend_override = Some(Arc::new(Mutex::new(Fake { fail: false })));
        let before = pixels(&mut develop);
        h.click(h.at(0.5, 0.5), &mut develop, &mut heal, &p);
        h.key(
            egui::Key::Delete,
            h.at(0.5, 0.5),
            &mut develop,
            &mut heal,
            &p,
        );
        assert!(spots(&develop).is_empty());
        drain(&p);
        let ctx = egui::Context::default();
        ctx.run_ui(egui::RawInput::default(), |ui| {
            poll(ui, &mut develop, &mut heal)
        })
        .drop_without_applying_deltas();
        assert_eq!(
            pixels(&mut develop),
            before,
            "a deleted spot's patch must never be applied"
        );
    }

    /// A backend that fails the integrity check, as `LazyBackend` does for a tampered install.
    struct Tampered;

    impl RemovalBackend for Tampered {
        fn remove(&mut self, _: &RemovalRequest<'_>) -> Result<RemovalPatch, RemovalError> {
            Err(RemovalError::Integrity(
                "LaMa inpainting model failed".into(),
            ))
        }
    }

    #[test]
    fn an_integrity_failure_offers_repair_and_a_successful_reinstall_clears_it() {
        let Some((mut develop, mut heal, p, mut h)) = rig() else {
            return;
        };
        heal.kind = SpotKind::Remove;
        heal.service.models_override = Some(fake_models());
        heal.service.backend_override = Some(Arc::new(Mutex::new(Tampered)));
        assert!(!heal.needs_repair);
        h.click(h.at(0.5, 0.5), &mut develop, &mut heal, &p);
        drain(&p);
        let ctx = egui::Context::default();
        ctx.run_ui(egui::RawInput::default(), |ui| {
            poll(ui, &mut develop, &mut heal)
        })
        .drop_without_applying_deltas();
        assert!(
            heal.needs_repair,
            "an integrity failure must surface the Repair button"
        );
        assert!(heal
            .status
            .as_deref()
            .unwrap_or("")
            .contains("integrity check"));
        assert!(spots(&develop).is_empty(), "the failed spot is dropped");
    }

    /// A removal that finishes after the user has switched photos must not land on the new one, and
    /// the same spot placed on the new photo must get its own job (not be swallowed by the old).
    #[test]
    fn a_result_for_another_photo_is_dropped_and_the_new_photo_gets_its_own_job() {
        struct Counting(Arc<std::sync::atomic::AtomicUsize>);
        impl RemovalBackend for Counting {
            fn remove(&mut self, req: &RemovalRequest<'_>) -> Result<RemovalPatch, RemovalError> {
                // The first (old photo's) removal fails; the second (new photo's) succeeds. With
                // the wrong-photo guard missing, the old failure would delete the new photo's
                // same-keyed spot and the success would then be discarded.
                if self.0.fetch_add(1, Ordering::Relaxed) == 0 {
                    return Err(RemovalError::NoObject);
                }
                RemovalPatch::new(
                    (req.center.0 as i32, req.center.1 as i32),
                    3,
                    vec![[0.4, 0.4, 0.4, 1.0]; 9],
                )
                .map_err(|e| RemovalError::BadInput(e.to_string()))
            }
        }

        let Some((mut develop, mut heal, p, mut h)) = rig() else {
            return;
        };
        heal.kind = SpotKind::Remove;
        heal.service.models_override = Some(fake_models());
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let backend = Arc::new(Mutex::new(Counting(calls.clone())));
        heal.service.backend_override = Some(backend.clone());

        // Hold the backend so the first job is stuck mid-flight while we switch photos.
        let gate = backend.lock().unwrap();
        h.click(h.at(0.5, 0.5), &mut develop, &mut heal, &p);
        let first_key = develop.frame_key();
        develop.load_real_frame(
            develop.frame_arc(),
            blake3::hash(b"a different photo"),
            nicti_pawprint::EditDocument::default(),
        );
        assert_ne!(develop.frame_key(), first_key);
        assert!(
            spots(&develop).is_empty(),
            "opening a photo resets the document"
        );

        // The same click on the new photo: same coordinates, same spot key, different photo.
        h.click(h.at(0.5, 0.5), &mut develop, &mut heal, &p);
        assert_eq!(spots(&develop).len(), 1);
        assert_eq!(
            heal.service.pending_count(),
            2,
            "one job per (photo, spot), not deduped"
        );
        drop(gate);
        drain(&p);

        let before = pixels(&mut develop);
        let ctx = egui::Context::default();
        ctx.run_ui(egui::RawInput::default(), |ui| {
            poll(ui, &mut develop, &mut heal)
        })
        .drop_without_applying_deltas();
        assert_eq!(
            calls.load(Ordering::Relaxed),
            2,
            "both photos' removals ran"
        );
        assert_eq!(heal.service.pending_count(), 0);
        assert_eq!(
            spots(&develop).len(),
            1,
            "the old photo's failure must not delete the new photo's spot"
        );
        assert_ne!(
            pixels(&mut develop),
            before,
            "the new photo's own result was applied"
        );
    }

    /// #272: Develop opens with no tool (the cropped canvas); R toggles the crop tool and
    /// Esc/Enter commit it.
    #[test]
    fn the_crop_tool_is_opt_in_with_r_and_committed_with_esc_or_enter() {
        fn press(ctx: &egui::Context, heal: &mut HealUi, key: egui::Key) {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(100.0, 100.0))),
                events: vec![Event::Key {
                    key,
                    physical_key: Some(key),
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                }],
                ..Default::default()
            };
            ctx.run_ui(input, |ui| handle_tool_keys(ui, heal))
                .drop_without_applying_deltas();
        }
        let ctx = egui::Context::default();
        let mut heal = HealUi::new();
        assert_eq!(heal.tool, Tool::Idle);
        assert!(!heal.crop_active());

        press(&ctx, &mut heal, egui::Key::R);
        assert!(heal.crop_active());
        press(&ctx, &mut heal, egui::Key::Escape);
        assert_eq!(heal.tool, Tool::Idle);

        press(&ctx, &mut heal, egui::Key::R);
        press(&ctx, &mut heal, egui::Key::Enter);
        assert_eq!(heal.tool, Tool::Idle);

        press(&ctx, &mut heal, egui::Key::R);
        press(&ctx, &mut heal, egui::Key::R);
        assert_eq!(heal.tool, Tool::Idle, "R toggles");

        // Esc outside the crop tool leaves the active tool alone.
        heal.tool = Tool::Heal;
        press(&ctx, &mut heal, egui::Key::Escape);
        assert_eq!(heal.tool, Tool::Heal);
    }
}
