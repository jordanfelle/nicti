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
//! **Undo.** The Develop view doesn't route edits through `nicti_pawprint::History` yet, so
//! nothing here is undoable; deleting a spot is the way back. (Adding history is a Develop-wide
//! job, not this tool's.)
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
use nicti_tapetum::heal::{spot_key, RemovalPatch, MAX_RADIUS};
use nicti_tapetum::stages::HEAL;

use crate::develop_panel::{image_to_screen, screen_to_image};
use crate::render::DevelopView;

/// Which on-image tool owns the Develop viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Crop,
    Heal,
}

const MIN_RADIUS: f32 = 4.0;
const HANDLE_RADIUS_PX: f32 = 7.0;
/// `[`/`]` change the brush size by this factor per key press.
const RESIZE_STEP: f32 = 1.12;

/// Mask-recipe identity for spots this build removes with: pins the model pair, so a future model
/// upgrade is an explicit re-run rather than a silent change to an old edit (ADR-0021).
const RECIPE_MODEL_ID: &str = "nicti.remove.mobilesam-lama";
const RECIPE_MODEL_VERSION: &str = "1";

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
/// the image so it is more likely to stay in frame.
pub fn fallback_offset(center: (f32, f32), radius: f32, source: (f32, f32)) -> (f32, f32) {
    let dx = 3.0 * radius;
    if center.0 < source.0 / 2.0 {
        (dx, 0.0)
    } else {
        (-dx, 0.0)
    }
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
    pub key: String,
    pub result: Result<Arc<RemovalPatch>, String>,
}

pub struct RemovalService {
    store: Option<ModelStore>,
    backend: Option<(RemovalModels, SharedBackend)>,
    install: Option<InstallHandle>,
    pending: Vec<Slot<RemoveOutcome>>,
    pending_keys: HashSet<String>,
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
        }
    }

    /// Everything AI removal needs, if it is all installed.
    pub fn models(&self) -> Option<RemovalModels> {
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
        self.install
            .as_ref()
            .map(|h| (h.bytes.load(Ordering::Relaxed), h.total))
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
            store,
            Arc::new(HttpDownloader),
            &models::removal_artifacts(),
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
        let backend: SharedBackend = Arc::new(Mutex::new(LazyBackend::new(models.clone())));
        self.backend = Some((models, Arc::clone(&backend)));
        backend
    }

    /// Queues a removal for `spot` on Pounce's GPU lane using the installed models.
    pub fn submit(
        &mut self,
        pounce: &Pounce,
        develop: &DevelopView,
        spot: &Spot,
        prompt: Prompt,
    ) -> Result<(), String> {
        let models = self
            .models()
            .ok_or("AI removal models aren't installed yet.")?;
        let backend = self.backend_for(models);
        self.submit_with_backend(pounce, backend, develop, spot, prompt)
    }

    /// [`Self::submit`] with an explicit backend -- the seam the tests use.
    pub fn submit_with_backend(
        &mut self,
        pounce: &Pounce,
        backend: SharedBackend,
        develop: &DevelopView,
        spot: &Spot,
        prompt: Prompt,
    ) -> Result<(), String> {
        let key = spot_key(spot);
        if !self.pending_keys.insert(key.clone()) {
            return Ok(()); // already running for exactly this spot
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

    pub fn is_pending(&self, key: &str) -> bool {
        self.pending_keys.contains(key)
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Collects every removal that has finished since the last call.
    pub fn poll_removals(&mut self) -> Vec<RemovalEvent> {
        let mut events = Vec::new();
        self.pending
            .retain(|slot| match slot.lock().unwrap().take() {
                Some(outcome) => {
                    self.pending_keys.remove(&outcome.spot_key);
                    events.push(RemovalEvent {
                        key: outcome.spot_key,
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
    pub service: RemovalService,
}

impl Default for HealUi {
    fn default() -> Self {
        Self::new()
    }
}

impl HealUi {
    pub fn new() -> Self {
        Self {
            tool: Tool::Crop,
            kind: SpotKind::Heal,
            radius: 24.0,
            feather_frac: 0.3,
            opacity: 1.0,
            selected: None,
            drag: None,
            status: None,
            service: RemovalService::new(),
        }
    }

    pub fn heal_active(&self) -> bool {
        self.tool == Tool::Heal
    }
}

/// Removes the spot at `index` and prunes patches that no longer belong to any spot.
fn delete_spot(develop: &mut DevelopView, params: &mut HealParams, index: usize) {
    if index < params.spots.len() {
        params.spots.remove(index);
        develop.set_stage_params(HEAL, params);
        develop.prune_removals();
    }
}

/// Applies finished removals to the view and handles failures. Call once per frame while the
/// Develop view is up.
pub fn poll(ui: &egui::Ui, develop: &mut DevelopView, heal: &mut HealUi) {
    if let Some(result) = heal.service.poll_install() {
        heal.status = Some(match result {
            Ok(()) => "AI removal models installed.".to_owned(),
            Err(e) => e,
        });
    }
    let events = heal.service.poll_removals();
    if !events.is_empty() {
        let mut params: HealParams = develop.stage_params(HEAL);
        for event in events {
            match event.result {
                Ok(patch) => {
                    // Ignore a result for a spot the user has since deleted or edited.
                    if params.spots.iter().any(|s| spot_key(s) == event.key) {
                        develop.set_removal(event.key, patch);
                        heal.status = Some("Object removed.".to_owned());
                    }
                }
                Err(message) => {
                    if let Some(i) = params.spots.iter().position(|s| spot_key(s) == event.key) {
                        // A failed removal leaves nothing useful behind: drop its placeholder
                        // spot instead of leaving it pending forever.
                        delete_spot(develop, &mut params, i);
                        heal.selected = None;
                    }
                    heal.status = Some(message);
                }
            }
        }
    }
    if heal.service.pending_count() > 0 || heal.service.is_installing() {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(100));
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
    develop: &mut DevelopView,
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
        if let (Some(i), Some(pointer)) = (heal.selected, response.interact_pointer_pos()) {
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
    draw_overlay(ui, rect, source, &params, heal, response);
}

/// Places a new spot of the current kind at `p`.
fn place_spot(
    develop: &mut DevelopView,
    heal: &mut HealUi,
    pounce: &Pounce,
    params: &mut HealParams,
    p: (f32, f32),
    source: (f32, f32),
    changed: &mut bool,
) {
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
        if s.kind == SpotKind::Remove && heal.service.is_pending(&spot_key(s)) {
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
    ui.horizontal(|ui| {
        ui.label("Tool");
        ui.selectable_value(&mut heal.tool, Tool::Crop, "Crop");
        ui.selectable_value(&mut heal.tool, Tool::Heal, "Heal / Remove");
    });
}

fn kind_label(kind: SpotKind) -> &'static str {
    match kind {
        SpotKind::Clone => "Clone",
        SpotKind::Heal => "Heal",
        SpotKind::Remove => "Remove",
    }
}

/// The Heal / Remove controls: spot kind and size, the spot list, and the AI-model download.
pub fn show_panel(
    ui: &mut egui::Ui,
    develop: &mut DevelopView,
    heal: &mut HealUi,
    pounce: &Pounce,
) {
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
            edited |= ui
                .add(
                    egui::Slider::new(&mut s.radius, MIN_RADIUS..=MAX_RADIUS)
                        .logarithmic(true)
                        .text("Size"),
                )
                .changed();
            edited |= ui
                .add(egui::Slider::new(&mut frac, 0.0..=1.0).text("Feather"))
                .changed();
            edited |= ui
                .add(egui::Slider::new(&mut s.opacity, 0.0..=1.0).text("Opacity"))
                .changed();
            s.radius = clamp_radius(s.radius);
            s.feather = frac * s.radius;
        }
        None => {
            ui.label("New spot");
            ui.add(
                egui::Slider::new(&mut heal.radius, MIN_RADIUS..=MAX_RADIUS)
                    .logarithmic(true)
                    .text("Size"),
            );
            ui.add(egui::Slider::new(&mut heal.feather_frac, 0.0..=1.0).text("Feather"));
            ui.add(egui::Slider::new(&mut heal.opacity, 0.0..=1.0).text("Opacity"));
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
            if s.kind == SpotKind::Remove && heal.service.is_pending(&spot_key(s)) {
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
    if heal.service.models().is_some() {
        ui.label("AI removal models are installed.");
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
        assert!(svc.is_pending(&spot_key(&spot)));
        drain(&p);
        let events = svc.poll_removals();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].key, spot_key(&spot));
        assert_eq!(events[0].result.as_ref().unwrap().side, 3);
        assert!(!svc.is_pending(&spot_key(&spot)));
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
    fn a_missing_store_cant_start_an_install() {
        let p = pounce();
        let mut svc = RemovalService::with_store(None);
        assert!(svc.start_install(&p).is_err());
        assert!(!svc.is_installing());
    }
}
