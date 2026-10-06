//! The masks tool's egui layer (#49): state, the per-frame poll, the Masks panel, the viewport
//! gestures, and the overlay. The model-free editing operations it drives live in `mask_edit`
//! (unit-tested without a window) and the AI bake/download machinery in `mask_tool`.
//!
//! Tool layout, mirroring the heal tool: the Develop panel's tool switch has a *Masks* entry; while
//! it is active the photo is shown uncropped (so a click maps to source coordinates by a plain
//! stretch) and `handle_viewport` owns the photo area. **What a drag does** is set by the panel's
//! "Drag edits" row (`Arm`): Brush paints strokes (Alt erases, `[`/`]` resize, Shift+`[`/`]` change
//! the feather), Linear/Radial move gradient handles (or place a fresh gradient on a drag that
//! doesn't start on one), Pick clicks a colour into a colour-range mask. Every edit writes the
//! `nicti.masks` stage each frame, and the engine turns that into one GPU pass per frame.

use std::sync::Arc;

use nicti_pounce::Pounce;
use nicti_tapetum::mask::compose::ai_bake_key;
use nicti_tapetum::mask::params::{LocalAdjust, MaskParams, MaskSource, Op, MAX_CORRECTIONS};
use nicti_tapetum::stages::MASKS;

use crate::develop_panel::{image_to_screen, screen_to_image};
use crate::mask_edit::{
    add_color_sample, add_component, add_correction, begin_stroke, delete, duplicate,
    extend_stroke, has_brush, has_color_range, linear_ends, new_correction, next_id,
    preview_extent, preview_field, preview_key, radial_shape, set_linear, set_radial, Arm,
    BrushSettings, NewMask, Thumb, MAX_BRUSH_RADIUS, MIN_BRUSH_RADIUS,
};
use crate::mask_tool::MaskBakeService;
use crate::render::DevelopView;

/// Screen pixels within which a press grabs a gradient handle.
const HANDLE_GRAB_PX: f32 = 12.0;
const HANDLE_DRAW_PX: f32 = 6.0;
/// `[`/`]` change the brush size by this factor per key press.
const BRUSH_STEP: f32 = 1.12;
/// Long edge of the overlay preview texture.
const OVERLAY_LONG_EDGE: usize = 320;
const OVERLAY_COLOR: [u8; 3] = [255, 40, 40];
const OVERLAY_MAX_ALPHA: f32 = 150.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RadialHandle {
    /// A fresh drag: places the centre, then sizes the ellipse.
    New,
    Center,
    EdgeX,
    EdgeY,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MaskDrag {
    Stroke {
        comp: usize,
        stroke: usize,
    },
    /// Which end (0 = start, 1 = end) is being moved.
    Linear(u8),
    Radial(RadialHandle),
}

struct Overlay {
    key: blake3::Hash,
    texture: egui::TextureHandle,
}

pub struct MaskUi {
    pub service: MaskBakeService,
    /// Index of the selected correction.
    pub selected: Option<usize>,
    /// What a drag/click on the photo does to the selected correction.
    pub arm: Arm,
    pub brush: BrushSettings,
    pub show_overlay: bool,
    /// The op the "Add to mask" buttons use.
    add_op: Op,
    status: Option<String>,
    drag: Option<MaskDrag>,
    thumb: Option<(u64, Arc<Thumb>)>,
    overlay: Option<Overlay>,
}

impl Default for MaskUi {
    fn default() -> Self {
        Self::new()
    }
}

impl MaskUi {
    pub fn new() -> Self {
        Self::with_service(MaskBakeService::new())
    }

    pub fn with_service(service: MaskBakeService) -> Self {
        Self {
            service,
            selected: None,
            arm: Arm::None,
            brush: BrushSettings::default(),
            show_overlay: true,
            add_op: Op::Add,
            status: None,
            drag: None,
            thumb: None,
            overlay: None,
        }
    }

    /// The photo thumbnail for colour sampling, rebuilt only when the photo changes.
    fn thumb_for(&mut self, develop: &DevelopView) -> Arc<Thumb> {
        let key = develop.frame_key();
        if let Some((k, t)) = &self.thumb {
            if *k == key {
                return Arc::clone(t);
            }
        }
        let t = Arc::new(Thumb::build(develop.frame_arc()));
        self.thumb = Some((key, Arc::clone(&t)));
        t
    }

    fn has_range_mask(params: &MaskParams, selected: Option<usize>) -> bool {
        selected
            .and_then(|i| params.corrections.get(i))
            .is_some_and(|c| {
                c.mask.components.iter().any(|m| {
                    matches!(
                        m.source,
                        MaskSource::LuminanceRange { .. } | MaskSource::ColorRange { .. }
                    )
                })
            })
    }

    /// The last status/error line (tests and the panel read it).
    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }
}

/// Runs once per frame while the Develop view is open: collects finished bakes and the model
/// download, submits bakes the current masks are waiting on, and releases alphas no mask refers to.
pub fn poll(ui: &egui::Ui, develop: &mut DevelopView, pounce: &Pounce, mask: &mut MaskUi) {
    if let Some(result) = mask.service.poll_install() {
        mask.status = Some(match result {
            Ok(()) if mask.service.installing_gpu_pack() => {
                "NVIDIA GPU pack installed. Restart Nicti to use it.".to_owned()
            }
            Ok(()) => "AI mask model installed.".to_owned(),
            Err(e) => e,
        });
    }
    for event in mask.service.poll(develop) {
        if let Err(message) = event.result {
            mask.status = Some(message);
        }
    }
    mask.service.request_missing(pounce, develop);
    develop.prune_ai_alphas();
    if mask.service.pending_count() > 0 || mask.service.is_installing() {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(100));
    }
}

fn megabytes(bytes: u64) -> u64 {
    bytes / (1024 * 1024)
}

// ---------------------------------------------------------------------------------------------
// Panel
// ---------------------------------------------------------------------------------------------

/// What state an AI component is in, for its badge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AiState {
    Ready,
    Selecting,
    NeedsModel,
    Failed(String),
    Unavailable,
}

pub(crate) fn ai_state(
    mask: &MaskUi,
    develop: &DevelopView,
    source: &MaskSource,
) -> Option<AiState> {
    let recipe = source.recipe()?;
    if !mask.service.knows_model(&recipe.model_id) {
        return Some(AiState::Unavailable);
    }
    let key = ai_bake_key(source, develop.neutral_key())?;
    if develop.has_ai_alpha(&key) {
        return Some(AiState::Ready);
    }
    if let Some(message) = mask.service.failure_for(develop.frame_key(), &key) {
        return Some(AiState::Failed(message.to_owned()));
    }
    if mask.service.is_pending(develop.frame_key(), &key) {
        return Some(AiState::Selecting);
    }
    if mask.service.model_missing(&recipe.model_id) {
        return Some(AiState::NeedsModel);
    }
    Some(AiState::Selecting)
}

fn source_label(source: &MaskSource) -> String {
    match source {
        MaskSource::Ai(r) => match nicti_stalk::SegmentTarget::from_params(&r.params) {
            Ok(t) => format!("AI {}", t.as_str()),
            Err(_) => "AI mask".to_owned(),
        },
        MaskSource::LinearGradient { .. } => "Linear gradient".to_owned(),
        MaskSource::RadialGradient { .. } => "Radial gradient".to_owned(),
        MaskSource::Brush { strokes } => format!("Brush ({} strokes)", strokes.len()),
        MaskSource::LuminanceRange { .. } => "Luminance range".to_owned(),
        MaskSource::ColorRange { samples, .. } => {
            format!("Colour range ({} samples)", samples.len())
        }
    }
}

fn op_label(op: Op) -> &'static str {
    match op {
        Op::Add => "Add",
        Op::Subtract => "Subtract",
        Op::Intersect => "Intersect",
    }
}

/// A slider for a normalized -1..1 value, shown like LRC's -100..100. Double-click resets.
fn percent_slider(ui: &mut egui::Ui, label: &str, value: &mut f32) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(label);
        let r = ui.add(
            egui::Slider::new(value, -1.0..=1.0)
                .custom_formatter(|n, _| format!("{:.0}", n * 100.0))
                .custom_parser(|s| s.trim().parse::<f64>().ok().map(|v| v / 100.0)),
        );
        if r.double_clicked() {
            *value = 0.0;
            changed = true;
        }
        changed |= r.changed();
    });
    changed
}

/// Renders the adjustment sliders of `a`; returns whether anything changed.
fn adjust_sliders(ui: &mut egui::Ui, a: &mut LocalAdjust) -> bool {
    let mut changed = false;
    ui.label("Light");
    ui.horizontal(|ui| {
        ui.label("Exposure");
        let r = ui.add(egui::Slider::new(&mut a.exposure, -5.0..=5.0).fixed_decimals(2));
        if r.double_clicked() {
            a.exposure = 0.0;
            changed = true;
        }
        changed |= r.changed();
    });
    for (label, v) in [
        ("Contrast", &mut a.contrast),
        ("Highlights", &mut a.highlights),
        ("Shadows", &mut a.shadows),
        ("Whites", &mut a.whites),
        ("Blacks", &mut a.blacks),
    ] {
        changed |= percent_slider(ui, label, v);
    }
    ui.label("Colour");
    for (label, v) in [
        ("Temp", &mut a.temp),
        ("Tint", &mut a.tint),
        ("Saturation", &mut a.saturation),
        ("Hue", &mut a.hue),
    ] {
        changed |= percent_slider(ui, label, v);
    }
    let mut overlay = a.color.unwrap_or_default();
    let mut overlay_changed = false;
    ui.horizontal(|ui| {
        ui.label("Colour tint");
        let hue = ui.add(egui::Slider::new(&mut overlay.hue_deg, 0.0..=360.0).suffix("°"));
        let amount = ui.add(egui::Slider::new(&mut overlay.saturation, 0.0..=1.0));
        overlay_changed |= hue.changed() || amount.changed();
        if hue.double_clicked() || amount.double_clicked() {
            overlay = Default::default();
            overlay_changed = true;
        }
    });
    if overlay_changed {
        // Keep a chosen hue even while the amount is still 0 (an all-default tint is "none").
        a.color = (overlay != Default::default()).then_some(overlay);
        changed = true;
    }
    ui.label("Effects");
    for (label, v) in [
        ("Clarity", &mut a.clarity),
        ("Texture", &mut a.texture),
        ("Dehaze", &mut a.dehaze),
    ] {
        changed |= percent_slider(ui, label, v);
    }
    ui.label("Detail");
    for (label, v) in [("Sharpness", &mut a.sharpness), ("Noise", &mut a.noise)] {
        changed |= percent_slider(ui, label, v);
    }
    changed
}

fn show_download(ui: &mut egui::Ui, mask: &mut MaskUi, develop: &DevelopView, pounce: &Pounce) {
    if let Some((done, total)) = mask.service.install_progress() {
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
            mask.service.cancel_install();
        }
        return;
    }
    if mask.service.needs_repair() {
        ui.colored_label(
            egui::Color32::from_rgb(255, 130, 0),
            "The AI mask model failed its integrity check.",
        );
        if ui.button("Repair model").clicked() {
            mask.status = mask.service.start_repair(pounce).err();
        }
    }
    if mask.service.has_failures() && ui.button("Retry failed masks").clicked() {
        mask.service.retry_failed();
        mask.status = None;
    }
    if let Some(bytes) = mask.service.download_needed(develop) {
        ui.group(|ui| {
            ui.label(format!(
                "Select Subject / Background needs a one-time download of about {} MB:\n\
                 - BiRefNet (MIT), from huggingface.co/onnx-community/BiRefNet-ONNX\n\
                 - ONNX Runtime (MIT), from github.com/microsoft/onnxruntime\n\
                 BiRefNet is trained on DIS-TR; this is a third-party ONNX conversion.\n\
                 Everything runs on this computer; nothing is uploaded.",
                megabytes(bytes)
            ));
            if ui.button("Download model").clicked() {
                mask.status = mask.service.start_install(pounce).err();
            }
        });
    }
    if let Some(bytes) = mask.service.gpu_pack_offer() {
        ui.group(|ui| {
            ui.label(format!(
                "Optional: faster AI masks on this NVIDIA GPU (about 6 s down to 0.2 s on an \
                 RTX 5080). A one-time download of about {} MB:\n\
                 - ONNX Runtime CUDA build (MIT), from github.com/microsoft/onnxruntime\n\
                 - NVIDIA cuDNN and cuBLAS, from NVIDIA\n\
                 - BiRefNet fp16 (MIT), from huggingface.co/onnx-community/BiRefNet-ONNX\n\
                 Needs an RTX 20-series or newer with about 8 GB of free video memory; otherwise \
                 masks keep running on the CPU. Takes effect after restarting Nicti. Everything \
                 runs on this computer.",
                megabytes(bytes)
            ));
            if ui.button("Download NVIDIA GPU pack").clicked() {
                mask.status = mask.service.start_gpu_pack_install(pounce).err();
            }
        });
    }
}

enum ListAction {
    Duplicate(bool),
    Delete,
}

const LIMIT_MESSAGE: &str = "A photo can have at most 16 local corrections.";

/// The Masks controls: creation, the correction list, the selected correction's components and
/// adjustments, and the AI-model download.
pub fn show_panel(
    ui: &mut egui::Ui,
    develop: &mut DevelopView,
    mask: &mut MaskUi,
    pounce: &Pounce,
) {
    ui.separator();
    ui.heading("Masks");
    ui.label(
        "Pick a mask kind, then drag on the photo. [ and ] resize the brush (Shift: feather), \
         Alt-drag erases, O toggles the overlay.",
    );

    let mut params: MaskParams = develop.stage_params(MASKS);
    let mut edited = false;

    // Create.
    ui.horizontal_wrapped(|ui| {
        for kind in NewMask::ALL {
            if ui.button(kind.label()).clicked() {
                let id = next_id(&params);
                match add_correction(&mut params, new_correction(kind, id)) {
                    Some(i) => {
                        mask.selected = Some(i);
                        mask.arm = kind.arm();
                        edited = true;
                    }
                    None => mask.status = Some(LIMIT_MESSAGE.to_owned()),
                }
            }
        }
    });

    show_download(ui, mask, develop, pounce);
    if let Some(status) = mask.status() {
        ui.label(status.to_owned());
    }

    // The list.
    if mask.selected.is_some_and(|i| i >= params.corrections.len()) {
        mask.selected = None;
    }
    if params.corrections.is_empty() {
        ui.label("No masks yet.");
    }
    let mut action: Option<(usize, ListAction)> = None;
    for (i, c) in params.corrections.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            edited |= ui.checkbox(&mut c.enabled, "").changed();
            if ui
                .selectable_label(mask.selected == Some(i), c.name.clone())
                .clicked()
                && mask.selected != Some(i)
            {
                // The armed tool belonged to the previous correction; a stale one would otherwise
                // make the next drag edit (or add a component to) this one.
                mask.selected = Some(i);
                mask.arm = Arm::None;
                mask.drag = None;
            }
            if ui.small_button("Dup").on_hover_text("Duplicate").clicked() {
                action = Some((i, ListAction::Duplicate(false)));
            }
            if ui
                .small_button("Inv")
                .on_hover_text("Duplicate and invert (e.g. background from subject)")
                .clicked()
            {
                action = Some((i, ListAction::Duplicate(true)));
            }
            if ui.small_button("Del").on_hover_text("Delete").clicked() {
                action = Some((i, ListAction::Delete));
            }
        });
    }
    match action {
        Some((i, ListAction::Duplicate(invert))) => match duplicate(&mut params, i, invert) {
            Some(j) => {
                mask.selected = Some(j);
                edited = true;
            }
            None => mask.status = Some(LIMIT_MESSAGE.to_owned()),
        },
        Some((i, ListAction::Delete)) if delete(&mut params, i) => {
            mask.selected = match mask.selected {
                Some(s) if s == i => None,
                Some(s) if s > i => Some(s - 1),
                other => other,
            };
            edited = true;
        }
        Some((_, ListAction::Delete)) | None => {}
    }

    // The selected correction.
    if let Some(sel) = mask.selected {
        edited |= show_selected(ui, develop, mask, &mut params, sel);
    }

    if edited {
        develop.set_stage_params(MASKS, &params);
    }
    debug_assert!(params.corrections.len() <= MAX_CORRECTIONS);
}

fn show_selected(
    ui: &mut egui::Ui,
    develop: &DevelopView,
    mask: &mut MaskUi,
    params: &mut MaskParams,
    sel: usize,
) -> bool {
    let mut edited = false;
    ui.separator();
    let (has_brush, has_linear, has_radial, has_color) = {
        let comps = &params.corrections[sel].mask.components;
        let any = |f: fn(&MaskSource) -> bool| comps.iter().any(|m| f(&m.source));
        (
            any(|s| matches!(s, MaskSource::Brush { .. })),
            any(|s| matches!(s, MaskSource::LinearGradient { .. })),
            any(|s| matches!(s, MaskSource::RadialGradient { .. })),
            any(|s| matches!(s, MaskSource::ColorRange { .. })),
        )
    };
    let c = &mut params.corrections[sel];
    ui.horizontal(|ui| {
        ui.label("Name");
        edited |= ui.text_edit_singleline(&mut c.name).changed();
    });

    ui.label("Mask");
    let mut remove = None;
    for (i, comp) in c.mask.components.iter_mut().enumerate() {
        ui.horizontal_wrapped(|ui| {
            ui.label(source_label(&comp.source));
            egui::ComboBox::from_id_salt(("mask-op", sel, i))
                .selected_text(op_label(comp.op))
                .width(80.0)
                .show_ui(ui, |ui| {
                    for op in [Op::Add, Op::Subtract, Op::Intersect] {
                        edited |= ui
                            .selectable_value(&mut comp.op, op, op_label(op))
                            .changed();
                    }
                });
            edited |= ui.checkbox(&mut comp.invert, "Invert").changed();
            edited |= ui
                .add(egui::Slider::new(&mut comp.opacity, 0.0..=1.0).text("Opacity"))
                .changed();
            if ui
                .small_button("x")
                .on_hover_text("Remove component")
                .clicked()
            {
                remove = Some(i);
            }
            match ai_state(mask, develop, &comp.source) {
                Some(AiState::Selecting) => {
                    ui.spinner();
                    ui.label("Selecting...");
                }
                Some(AiState::NeedsModel) => {
                    ui.colored_label(egui::Color32::from_rgb(255, 180, 0), "needs the model");
                }
                Some(AiState::Failed(m)) => {
                    ui.colored_label(egui::Color32::from_rgb(255, 90, 90), m);
                }
                Some(AiState::Unavailable) => {
                    ui.colored_label(
                        egui::Color32::from_rgb(255, 90, 90),
                        "model unavailable in this build",
                    );
                }
                Some(AiState::Ready) | None => {}
            }
        });
        // The range controls live with their component.
        match &mut comp.source {
            MaskSource::LuminanceRange { lo, hi, smooth } => {
                edited |= ui
                    .add(egui::Slider::new(lo, 0.0..=1.0).text("From"))
                    .changed();
                edited |= ui
                    .add(egui::Slider::new(hi, 0.0..=1.0).text("To"))
                    .changed();
                edited |= ui
                    .add(egui::Slider::new(smooth, 0.0..=1.0).text("Smooth"))
                    .changed();
            }
            MaskSource::ColorRange { samples, tolerance } => {
                edited |= ui
                    .add(egui::Slider::new(tolerance, 1.0..=100.0).text("Range"))
                    .changed();
                if !samples.is_empty() && ui.button("Clear samples").clicked() {
                    samples.clear();
                    edited = true;
                }
            }
            MaskSource::RadialGradient {
                angle_deg, feather, ..
            } => {
                edited |= ui
                    .add(egui::Slider::new(angle_deg, -180.0..=180.0).text("Angle"))
                    .changed();
                edited |= ui
                    .add(egui::Slider::new(feather, 0.0..=1.0).text("Feather"))
                    .changed();
            }
            _ => {}
        }
    }
    if let Some(i) = remove {
        c.mask.components.remove(i);
        edited = true;
    }

    // Add another component.
    ui.horizontal_wrapped(|ui| {
        ui.label("Add to mask:");
        egui::ComboBox::from_id_salt(("mask-add-op", sel))
            .selected_text(op_label(mask.add_op))
            .width(80.0)
            .show_ui(ui, |ui| {
                for op in [Op::Add, Op::Subtract, Op::Intersect] {
                    ui.selectable_value(&mut mask.add_op, op, op_label(op));
                }
            });
        for kind in [
            NewMask::Subject,
            NewMask::Sky,
            NewMask::Brush,
            NewMask::Linear,
            NewMask::Radial,
            NewMask::Luminance,
            NewMask::Color,
        ] {
            if ui.small_button(kind.label()).clicked()
                && add_component(c, kind, mask.add_op).is_some()
            {
                mask.arm = kind.arm();
                edited = true;
            }
        }
    });

    // What a drag on the photo does.
    ui.horizontal(|ui| {
        ui.label("Drag edits");
        ui.selectable_value(&mut mask.arm, Arm::None, "Nothing");
        ui.add_enabled_ui(has_brush, |ui| {
            ui.selectable_value(&mut mask.arm, Arm::Brush, "Brush");
        });
        ui.add_enabled_ui(has_linear, |ui| {
            ui.selectable_value(&mut mask.arm, Arm::Linear, "Linear");
        });
        ui.add_enabled_ui(has_radial, |ui| {
            ui.selectable_value(&mut mask.arm, Arm::Radial, "Radial");
        });
        ui.add_enabled_ui(has_color, |ui| {
            ui.selectable_value(&mut mask.arm, Arm::Pick, "Pick colour");
        });
    });
    if mask.arm == Arm::Brush {
        ui.add(
            egui::Slider::new(&mut mask.brush.radius, MIN_BRUSH_RADIUS..=MAX_BRUSH_RADIUS)
                .logarithmic(true)
                .text("Size"),
        );
        mask.brush.feather = mask.brush.feather.min(mask.brush.radius);
        ui.add(egui::Slider::new(&mut mask.brush.feather, 0.0..=mask.brush.radius).text("Feather"));
        ui.add(egui::Slider::new(&mut mask.brush.flow, 0.0..=1.0).text("Flow"));
    }
    ui.checkbox(&mut mask.show_overlay, "Show mask overlay");

    // Adjustments.
    ui.separator();
    ui.horizontal(|ui| {
        ui.label("Adjustments");
        if ui.small_button("Reset").clicked() {
            c.adjust = LocalAdjust::default();
            edited = true;
        }
    });
    ui.horizontal(|ui| {
        ui.label("Amount");
        let r = ui.add(egui::Slider::new(&mut c.amount, 0.0..=1.0));
        if r.double_clicked() {
            c.amount = 1.0;
            edited = true;
        }
        edited |= r.changed();
    });
    edited |= adjust_sliders(ui, &mut c.adjust);
    edited
}

// ---------------------------------------------------------------------------------------------
// Viewport
// ---------------------------------------------------------------------------------------------

/// The armed tool, but only if the selected correction has the component it edits. The arm is
/// reset when the selection changes, but a component can also be removed while its tool is armed;
/// without this a drag would silently *add* a fresh component to a mask that never had one.
fn effective_arm(c: &nicti_tapetum::mask::params::LocalCorrection, arm: Arm) -> Arm {
    let present = match arm {
        Arm::None => true,
        Arm::Brush => has_brush(c),
        Arm::Linear => linear_ends(c).is_some(),
        Arm::Radial => radial_shape(c).is_some(),
        Arm::Pick => has_color_range(c),
    };
    if present {
        arm
    } else {
        Arm::None
    }
}

/// Screen point -> normalized frame coordinates.
pub(crate) fn to_norm(rect: egui::Rect, source: (f32, f32), p: egui::Pos2) -> [f32; 2] {
    let (x, y) = screen_to_image(rect, source, p);
    [x / source.0.max(1.0), y / source.1.max(1.0)]
}

/// Normalized frame coordinates -> screen point.
pub(crate) fn to_screen(rect: egui::Rect, source: (f32, f32), n: [f32; 2]) -> egui::Pos2 {
    image_to_screen(rect, source, (n[0] * source.0, n[1] * source.1))
}

fn near(a: egui::Pos2, b: egui::Pos2) -> bool {
    a.distance(b) <= HANDLE_GRAB_PX
}

/// The radial handles in screen space: centre, the right-edge (x radius) and bottom-edge (y
/// radius) handles. Radii are fractions of the long edge, so `radius * long / width` is the
/// normalized x extent and `radius * long / height` the normalized y extent.
pub(crate) fn radial_handles(
    rect: egui::Rect,
    source: (f32, f32),
    center: [f32; 2],
    radii: [f32; 2],
    angle_deg: f32,
) -> (egui::Pos2, egui::Pos2, egui::Pos2) {
    let c = to_screen(rect, source, center);
    let ex = to_screen(
        rect,
        source,
        local_to_norm(center, radii[0], 0.0, angle_deg, source),
    );
    let ey = to_screen(
        rect,
        source,
        local_to_norm(center, 0.0, radii[1], angle_deg, source),
    );
    (c, ex, ey)
}

/// A point `(lx, ly)` (fractions of the long edge) in the ellipse's own rotated frame, as normalized
/// frame coordinates. The mask's x axis is `(cos a, sin a)` in pixel space, exactly as
/// `raster::radial_weight` rotates it, so handles and outline sit on the real mask edge.
pub(crate) fn local_to_norm(
    center: [f32; 2],
    lx: f32,
    ly: f32,
    angle_deg: f32,
    source: (f32, f32),
) -> [f32; 2] {
    let long = source.0.max(source.1).max(1.0);
    let (s, c) = angle_deg.to_radians().sin_cos();
    let (px, py) = (lx * long, ly * long);
    [
        center[0] + (px * c - py * s) / source.0.max(1.0),
        center[1] + (px * s + py * c) / source.1.max(1.0),
    ]
}

/// Radii (fractions of the long edge) of an axis-aligned ellipse centred at `center` passing
/// through `p` -- how a drag sizes a radial mask.
pub(crate) fn radii_to(
    center: [f32; 2],
    p: [f32; 2],
    source: (f32, f32),
    angle_deg: f32,
) -> [f32; 2] {
    let long = source.0.max(source.1).max(1.0);
    // The pointer's offset in pixels, expressed in the ellipse's own (rotated) axes.
    let (dx, dy) = ((p[0] - center[0]) * source.0, (p[1] - center[1]) * source.1);
    let (s, c) = angle_deg.to_radians().sin_cos();
    let (lx, ly) = (dx * c + dy * s, -dx * s + dy * c);
    [(lx.abs() / long).max(0.002), (ly.abs() / long).max(0.002)]
}

/// Handles the photo area while the Masks tool is active: brush strokes, gradient handles, the
/// colour eyedropper, the brush-size keys, and the overlay.
pub fn handle_viewport(
    ui: &mut egui::Ui,
    response: &egui::Response,
    rect: egui::Rect,
    develop: &mut DevelopView,
    mask: &mut MaskUi,
) {
    let source = develop.source_extent();
    let mut params: MaskParams = develop.stage_params(MASKS);
    if mask.selected.is_some_and(|i| i >= params.corrections.len()) {
        mask.selected = None;
    }
    let mut changed = false;
    let long = source.0.max(source.1).max(1.0);

    // Keys act only while the pointer is over the photo, so typing in a panel field never resizes
    // the brush or toggles the overlay.
    if response.hovered() {
        let (bigger, smaller, shift, overlay) = ui.input(|i| {
            (
                i.key_pressed(egui::Key::CloseBracket),
                i.key_pressed(egui::Key::OpenBracket),
                i.modifiers.shift,
                i.key_pressed(egui::Key::O),
            )
        });
        let factor = match (bigger, smaller) {
            (true, false) => Some(BRUSH_STEP),
            (false, true) => Some(1.0 / BRUSH_STEP),
            _ => None,
        };
        if let Some(f) = factor {
            if shift {
                mask.brush.refeather(f);
            } else {
                mask.brush.resize(f);
            }
        }
        if overlay {
            mask.show_overlay = !mask.show_overlay;
        }
    }

    if let Some(sel) = mask.selected {
        // `drag_started` fires only after the pointer crosses egui's drag threshold, so anchor the
        // gesture at the press position, not the (already moved) current one.
        let press = ui.input(|i| i.pointer.press_origin());
        let pointer = response
            .interact_pointer_pos()
            .or_else(|| ui.input(|i| i.pointer.hover_pos()));
        match effective_arm(&params.corrections[sel], mask.arm) {
            Arm::Brush => {
                if response.drag_started() {
                    if let Some(p) = press {
                        let erase = ui.input(|i| i.modifiers.alt);
                        if let Some((comp, stroke)) = begin_stroke(
                            &mut params.corrections[sel],
                            &mask.brush,
                            erase,
                            to_norm(rect, source, p),
                        ) {
                            mask.drag = Some(MaskDrag::Stroke { comp, stroke });
                            changed = true;
                        } else {
                            mask.status = Some(
                                "This mask can't take more brush strokes (limit reached)."
                                    .to_owned(),
                            );
                        }
                    }
                } else if response.dragged() {
                    if let (Some(MaskDrag::Stroke { comp, stroke }), Some(p)) = (mask.drag, pointer)
                    {
                        // A quarter of the brush radius between stored points, in normalized x.
                        let step = mask.brush.radius * 0.25 * long / source.0.max(1.0);
                        changed |= extend_stroke(
                            &mut params.corrections[sel],
                            comp,
                            stroke,
                            to_norm(rect, source, p),
                            step,
                        );
                    }
                }
            }
            Arm::Linear => {
                if response.drag_started() {
                    if let Some(press) = press {
                        let c = &mut params.corrections[sel];
                        let grab = linear_ends(c).and_then(|(p0, p1)| {
                            if near(press, to_screen(rect, source, p0)) {
                                Some(0u8)
                            } else if near(press, to_screen(rect, source, p1)) {
                                Some(1u8)
                            } else {
                                None
                            }
                        });
                        let handle = match grab {
                            Some(h) => h,
                            None => {
                                let n = to_norm(rect, source, press);
                                changed |= set_linear(c, n, n);
                                1
                            }
                        };
                        mask.drag = Some(MaskDrag::Linear(handle));
                    }
                } else if response.dragged() {
                    if let (Some(MaskDrag::Linear(h)), Some(p)) = (mask.drag, pointer) {
                        let c = &mut params.corrections[sel];
                        if let Some((mut p0, mut p1)) = linear_ends(c) {
                            let n = to_norm(rect, source, p);
                            if h == 0 {
                                p0 = n;
                            } else {
                                p1 = n;
                            }
                            changed |= set_linear(c, p0, p1);
                        }
                    }
                }
            }
            Arm::Radial => {
                if response.drag_started() {
                    if let Some(press) = press {
                        let c = &mut params.corrections[sel];
                        let handle = match radial_shape(c) {
                            Some((center, radii, angle, _)) => {
                                let (hc, hx, hy) =
                                    radial_handles(rect, source, center, radii, angle);
                                if near(press, hc) {
                                    RadialHandle::Center
                                } else if near(press, hx) {
                                    RadialHandle::EdgeX
                                } else if near(press, hy) {
                                    RadialHandle::EdgeY
                                } else {
                                    RadialHandle::New
                                }
                            }
                            None => RadialHandle::New,
                        };
                        if handle == RadialHandle::New {
                            let n = to_norm(rect, source, press);
                            changed |= set_radial(c, n, [0.002, 0.002]);
                        }
                        mask.drag = Some(MaskDrag::Radial(handle));
                    }
                } else if response.dragged() {
                    if let (Some(MaskDrag::Radial(h)), Some(p)) = (mask.drag, pointer) {
                        let c = &mut params.corrections[sel];
                        if let Some((center, radii, angle, _)) = radial_shape(c) {
                            let n = to_norm(rect, source, p);
                            let to = radii_to(center, n, source, angle);
                            let (new_center, new_radii) = match h {
                                RadialHandle::Center => (n, radii),
                                RadialHandle::EdgeX => (center, [to[0], radii[1]]),
                                RadialHandle::EdgeY => (center, [radii[0], to[1]]),
                                RadialHandle::New => (center, to),
                            };
                            changed |= set_radial(c, new_center, new_radii);
                        }
                    }
                }
            }
            Arm::Pick => {
                if response.clicked() {
                    if let Some(p) = response.interact_pointer_pos() {
                        let n = to_norm(rect, source, p);
                        let lab = mask.thumb_for(develop).sample_lab(n[0], n[1]);
                        changed |= add_color_sample(&mut params.corrections[sel], lab);
                    }
                }
            }
            Arm::None => {}
        }
        if response.drag_stopped() {
            mask.drag = None;
        }
    }

    if changed {
        develop.set_stage_params(MASKS, &params);
    }
    draw_overlay(ui, response, rect, develop, mask, &params);
}

/// The mask overlay (a red tint of the selected correction's selection) and the gesture handles.
fn draw_overlay(
    ui: &mut egui::Ui,
    response: &egui::Response,
    rect: egui::Rect,
    develop: &DevelopView,
    mask: &mut MaskUi,
    params: &MaskParams,
) {
    let Some(c) = mask.selected.and_then(|i| params.corrections.get(i)) else {
        return;
    };
    let source = develop.source_extent();
    let painter = ui.painter_at(rect);

    if mask.show_overlay {
        let need_thumb = MaskUi::has_range_mask(params, mask.selected);
        let thumb = need_thumb.then(|| mask.thumb_for(develop));
        let key = preview_key(c, develop, develop.frame_key(), thumb.is_some());
        if mask.overlay.as_ref().is_none_or(|o| o.key != key) {
            let (w, h) = preview_extent(source, OVERLAY_LONG_EDGE);
            let field = preview_field(c, w, h, develop, thumb.as_deref());
            let pixels = field
                .data
                .iter()
                .map(|&v| {
                    egui::Color32::from_rgba_unmultiplied(
                        OVERLAY_COLOR[0],
                        OVERLAY_COLOR[1],
                        OVERLAY_COLOR[2],
                        (v.clamp(0.0, 1.0) * OVERLAY_MAX_ALPHA) as u8,
                    )
                })
                .collect();
            let image = egui::ColorImage {
                size: [w, h],
                source_size: egui::vec2(w as f32, h as f32),
                pixels,
            };
            let texture =
                ui.ctx()
                    .load_texture("nicti-mask-overlay", image, egui::TextureOptions::LINEAR);
            mask.overlay = Some(Overlay { key, texture });
        }
        if let Some(o) = &mask.overlay {
            painter.image(
                o.texture.id(),
                rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        }
    }

    let white = egui::Stroke::new(1.5, egui::Color32::WHITE);
    let accent = egui::Color32::from_rgb(255, 210, 0);
    match effective_arm(c, mask.arm) {
        Arm::Linear => {
            if let Some((p0, p1)) = linear_ends(c) {
                let (a, b) = (to_screen(rect, source, p0), to_screen(rect, source, p1));
                painter.line_segment([a, b], white);
                painter.circle_filled(a, HANDLE_DRAW_PX, accent);
                painter.circle_filled(b, HANDLE_DRAW_PX, egui::Color32::WHITE);
            }
        }
        Arm::Radial => {
            if let Some((center, radii, angle, _)) = radial_shape(c) {
                let (hc, hx, hy) = radial_handles(rect, source, center, radii, angle);
                // A polyline, not `ellipse_stroke`: the ellipse can be rotated.
                let outline: Vec<egui::Pos2> = (0..64)
                    .map(|i| {
                        let t = i as f32 / 64.0 * std::f32::consts::TAU;
                        to_screen(
                            rect,
                            source,
                            local_to_norm(
                                center,
                                radii[0] * t.cos(),
                                radii[1] * t.sin(),
                                angle,
                                source,
                            ),
                        )
                    })
                    .collect();
                painter.add(egui::Shape::closed_line(outline, white));
                painter.circle_filled(hc, HANDLE_DRAW_PX, accent);
                painter.circle_filled(hx, HANDLE_DRAW_PX, egui::Color32::WHITE);
                painter.circle_filled(hy, HANDLE_DRAW_PX, egui::Color32::WHITE);
            }
        }
        Arm::Brush => {
            if let Some(p) = response.hover_pos() {
                let long = source.0.max(source.1).max(1.0);
                let scale = long * rect.width() / source.0.max(1.0);
                painter.circle_stroke(p, mask.brush.radius * scale, white);
                painter.circle_stroke(
                    p,
                    (mask.brush.radius - mask.brush.feather).max(0.0) * scale,
                    egui::Stroke::new(1.0, egui::Color32::from_white_alpha(120)),
                );
            }
        }
        Arm::Pick => {
            if let Some(p) = response.hover_pos() {
                painter.circle_stroke(p, 8.0, white);
                painter.circle_stroke(p, 9.0, egui::Stroke::new(1.0, egui::Color32::BLACK));
            }
        }
        Arm::None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{pos2, vec2, Event, Modifiers, PointerButton, Pos2, Rect};
    use nicti_tapetum::mask::params::Stroke;

    fn pounce() -> Pounce {
        Pounce::new(u64::MAX, 2, 1, || {})
    }

    /// A headless egui context that runs one frame at a time with scripted input.
    struct Harness {
        ctx: egui::Context,
        rect: Rect,
        time: f64,
        modifiers: Modifiers,
    }

    impl Harness {
        /// Registers the viewport widget (egui hit-tests against the previous frame's layout).
        fn new(develop: &mut DevelopView, mask: &mut MaskUi) -> Self {
            let mut h = Self {
                ctx: egui::Context::default(),
                rect: Rect::NOTHING,
                time: 0.0,
                modifiers: Modifiers::NONE,
            };
            h.frame(vec![], develop, mask);
            h
        }

        fn frame(&mut self, events: Vec<Event>, develop: &mut DevelopView, mask: &mut MaskUi) {
            self.time += 0.1;
            // Modifier state is delivered as an event (egui keeps it until the next change).
            let mut events = events;
            events.insert(0, Event::ModifiersChanged(self.modifiers));
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
                handle_viewport(ui, &response, r, develop, mask);
            });
            // Headless: there is no renderer to apply the frame's texture uploads to.
            output.drop_without_applying_deltas();
            self.rect = rect;
        }

        /// Screen position of a point given as a fraction of the viewport (== normalized coords).
        fn at(&self, fx: f32, fy: f32) -> Pos2 {
            pos2(
                self.rect.left() + fx * self.rect.width(),
                self.rect.top() + fy * self.rect.height(),
            )
        }

        fn button(&self, pos: Pos2, pressed: bool) -> Event {
            Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed,
                modifiers: self.modifiers,
            }
        }

        fn click(&mut self, pos: Pos2, develop: &mut DevelopView, mask: &mut MaskUi) {
            self.frame(vec![Event::PointerMoved(pos)], develop, mask);
            let down = self.button(pos, true);
            self.frame(vec![down], develop, mask);
            let up = self.button(pos, false);
            self.frame(vec![up], develop, mask);
        }

        fn drag(&mut self, from: Pos2, to: Pos2, develop: &mut DevelopView, mask: &mut MaskUi) {
            self.frame(vec![Event::PointerMoved(from)], develop, mask);
            let down = self.button(from, true);
            self.frame(vec![down], develop, mask);
            // Several moves so egui's drag threshold is crossed and the drag keeps tracking.
            for step in 1..=8 {
                let t = step as f32 / 8.0;
                let pos = from + (to - from) * t;
                self.frame(vec![Event::PointerMoved(pos)], develop, mask);
            }
            let up = self.button(to, false);
            self.frame(vec![up], develop, mask);
        }

        fn key(&mut self, key: egui::Key, at: Pos2, develop: &mut DevelopView, mask: &mut MaskUi) {
            self.frame(vec![Event::PointerMoved(at)], develop, mask);
            let event = Event::Key {
                key,
                physical_key: Some(key),
                pressed: true,
                repeat: false,
                modifiers: self.modifiers,
            };
            self.frame(vec![event], develop, mask);
        }
    }

    /// (develop, mask ui, pounce, harness), or `None` when there is no GPU adapter.
    fn setup(kind: NewMask, arm: Arm) -> Option<(DevelopView, MaskUi, Harness)> {
        let gpu = crate::test_gpu::shared()?;
        let mut develop = DevelopView::new(gpu);
        let mut mask = MaskUi::with_service(MaskBakeService::with_store(None));
        let params = MaskParams {
            corrections: vec![new_correction(kind, "mask-1".into())],
        };
        develop.set_stage_params(MASKS, &params);
        mask.selected = Some(0);
        mask.arm = arm;
        let h = Harness::new(&mut develop, &mut mask);
        Some((develop, mask, h))
    }

    fn correction(develop: &DevelopView) -> nicti_tapetum::mask::params::LocalCorrection {
        develop
            .stage_params::<MaskParams>(MASKS)
            .corrections
            .remove(0)
    }

    fn strokes(develop: &DevelopView) -> Vec<Stroke> {
        correction(develop)
            .mask
            .components
            .iter()
            .find_map(|c| match &c.source {
                MaskSource::Brush { strokes } => Some(strokes.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    fn close(a: [f32; 2], b: [f32; 2]) -> bool {
        (a[0] - b[0]).abs() < 0.02 && (a[1] - b[1]).abs() < 0.02
    }

    #[test]
    fn dragging_with_the_brush_paints_one_stroke_along_the_path() {
        let Some((mut develop, mut mask, mut h)) = setup(NewMask::Brush, Arm::Brush) else {
            return;
        };
        let (from, to) = (h.at(0.2, 0.3), h.at(0.8, 0.6));
        h.drag(from, to, &mut develop, &mut mask);
        let s = strokes(&develop);
        assert_eq!(s.len(), 1, "one drag is one stroke");
        assert!(!s[0].erase);
        assert!(s[0].points.len() >= 3, "{} points", s[0].points.len());
        assert!(
            close(s[0].points[0], [0.2, 0.3]),
            "starts at the press: {:?}",
            s[0].points[0]
        );
        assert!(
            close(*s[0].points.last().unwrap(), [0.8, 0.6]),
            "{:?}",
            s[0].points.last()
        );
        assert_eq!(
            s[0].radius, mask.brush.radius,
            "the stroke takes the brush's size"
        );
        // Points are stored sparsely (a fraction of the radius apart), not one per frame.
        assert!(s[0].points.len() <= 20);
    }

    #[test]
    fn alt_drag_erases() {
        let Some((mut develop, mut mask, mut h)) = setup(NewMask::Brush, Arm::Brush) else {
            return;
        };
        h.drag(h.at(0.2, 0.5), h.at(0.6, 0.5), &mut develop, &mut mask);
        h.modifiers = Modifiers::ALT;
        h.drag(h.at(0.3, 0.5), h.at(0.5, 0.5), &mut develop, &mut mask);
        let s = strokes(&develop);
        assert_eq!(s.len(), 2);
        assert!(!s[0].erase && s[1].erase);
    }

    #[test]
    fn a_drag_with_nothing_selected_or_armed_edits_nothing() {
        let Some((mut develop, mut mask, mut h)) = setup(NewMask::Brush, Arm::Brush) else {
            return;
        };
        mask.arm = Arm::None;
        h.drag(h.at(0.2, 0.2), h.at(0.7, 0.7), &mut develop, &mut mask);
        assert!(strokes(&develop).is_empty(), "nothing armed");
        mask.arm = Arm::Brush;
        mask.selected = None;
        h.drag(h.at(0.2, 0.2), h.at(0.7, 0.7), &mut develop, &mut mask);
        assert!(strokes(&develop).is_empty(), "nothing selected");
    }

    #[test]
    fn the_bracket_keys_resize_the_brush_and_shift_changes_the_feather() {
        let Some((mut develop, mut mask, mut h)) = setup(NewMask::Brush, Arm::Brush) else {
            return;
        };
        let over = h.at(0.5, 0.5);
        let r0 = mask.brush.radius;
        h.key(egui::Key::CloseBracket, over, &mut develop, &mut mask);
        assert!(mask.brush.radius > r0, "] grows the brush");
        let r1 = mask.brush.radius;
        h.key(egui::Key::OpenBracket, over, &mut develop, &mut mask);
        h.key(egui::Key::OpenBracket, over, &mut develop, &mut mask);
        assert!(mask.brush.radius < r1, "[ shrinks it");

        let (radius, f0) = (mask.brush.radius, mask.brush.feather);
        h.modifiers = Modifiers::SHIFT;
        h.key(egui::Key::CloseBracket, over, &mut develop, &mut mask);
        assert_eq!(mask.brush.radius, radius, "Shift leaves the size alone");
        assert!(mask.brush.feather > f0, "and widens the feather");
    }

    #[test]
    fn keys_do_nothing_when_the_pointer_is_not_over_the_photo() {
        let Some((mut develop, mut mask, mut h)) = setup(NewMask::Brush, Arm::Brush) else {
            return;
        };
        let elsewhere = pos2(800.0, 650.0); // outside the 600x450 viewport
        let r0 = mask.brush.radius;
        h.key(egui::Key::CloseBracket, elsewhere, &mut develop, &mut mask);
        h.key(egui::Key::O, elsewhere, &mut develop, &mut mask);
        assert_eq!(
            mask.brush.radius, r0,
            "typing in a panel field must not resize the brush"
        );
        assert!(mask.show_overlay, "nor toggle the overlay");
    }

    #[test]
    fn the_o_key_toggles_the_overlay_over_the_photo() {
        let Some((mut develop, mut mask, mut h)) = setup(NewMask::Brush, Arm::None) else {
            return;
        };
        assert!(mask.show_overlay);
        h.key(egui::Key::O, h.at(0.5, 0.5), &mut develop, &mut mask);
        assert!(!mask.show_overlay);
    }

    #[test]
    fn dragging_on_empty_space_places_a_linear_gradient_between_the_press_and_the_pointer() {
        let Some((mut develop, mut mask, mut h)) = setup(NewMask::Linear, Arm::Linear) else {
            return;
        };
        // The default gradient runs (0.5, 0.25) -> (0.5, 0.75); drag well away from both ends.
        h.drag(h.at(0.1, 0.1), h.at(0.9, 0.2), &mut develop, &mut mask);
        let (p0, p1) = linear_ends(&correction(&develop)).unwrap();
        assert!(close(p0, [0.1, 0.1]), "{p0:?}");
        assert!(close(p1, [0.9, 0.2]), "{p1:?}");
    }

    #[test]
    fn dragging_a_linear_handle_moves_only_that_end() {
        let Some((mut develop, mut mask, mut h)) = setup(NewMask::Linear, Arm::Linear) else {
            return;
        };
        let (p0, p1) = linear_ends(&correction(&develop)).unwrap();
        // Grab the far end and pull it sideways.
        h.drag(
            h.at(p1[0], p1[1]),
            h.at(0.85, 0.85),
            &mut develop,
            &mut mask,
        );
        let (q0, q1) = linear_ends(&correction(&develop)).unwrap();
        assert!(close(q0, p0), "the start stayed put: {q0:?}");
        assert!(
            close(q1, [0.85, 0.85]),
            "the end followed the pointer: {q1:?}"
        );
        // And a component was edited, not added.
        assert_eq!(correction(&develop).mask.components.len(), 1);
    }

    #[test]
    fn dragging_on_empty_space_places_a_radial_gradient_and_sizes_it() {
        let Some((mut develop, mut mask, mut h)) = setup(NewMask::Radial, Arm::Radial) else {
            return;
        };
        // The default ellipse is centred (0.5, 0.5); press in a corner, far from its handles.
        h.drag(h.at(0.15, 0.15), h.at(0.35, 0.30), &mut develop, &mut mask);
        let (center, radii, _, _) = radial_shape(&correction(&develop)).unwrap();
        assert!(close(center, [0.15, 0.15]), "{center:?}");
        // Radii are fractions of the long edge: a 0.2 x 0.15 normalized drag on a square frame.
        assert!(
            (radii[0] - 0.2).abs() < 0.03 && (radii[1] - 0.15).abs() < 0.03,
            "{radii:?}"
        );
    }

    #[test]
    fn dragging_the_radial_centre_handle_moves_it_without_resizing() {
        let Some((mut develop, mut mask, mut h)) = setup(NewMask::Radial, Arm::Radial) else {
            return;
        };
        let (center, radii, _, _) = radial_shape(&correction(&develop)).unwrap();
        h.drag(
            h.at(center[0], center[1]),
            h.at(0.3, 0.7),
            &mut develop,
            &mut mask,
        );
        let (c2, r2, _, _) = radial_shape(&correction(&develop)).unwrap();
        assert!(close(c2, [0.3, 0.7]), "{c2:?}");
        assert_eq!(r2, radii, "moving the centre keeps the size");
    }

    #[test]
    fn dragging_a_radial_edge_handle_resizes_one_axis() {
        let Some((mut develop, mut mask, mut h)) = setup(NewMask::Radial, Arm::Radial) else {
            return;
        };
        let (center, radii, _, _) = radial_shape(&correction(&develop)).unwrap();
        // The right-edge handle sits at (cx + rx * long / width, cy); on the square frame that is
        // (0.5 + 0.25, 0.5). Pull it to x = 0.9.
        h.drag(
            h.at(center[0] + radii[0], center[1]),
            h.at(0.9, 0.5),
            &mut develop,
            &mut mask,
        );
        let (c2, r2, _, _) = radial_shape(&correction(&develop)).unwrap();
        assert!(close(c2, center), "the centre stayed put: {c2:?}");
        assert!((r2[0] - 0.4).abs() < 0.03, "x radius grew to ~0.4: {r2:?}");
        assert_eq!(r2[1], radii[1], "the y radius is untouched");
    }

    #[test]
    fn clicking_with_the_eyedropper_adds_a_colour_sample() {
        let Some((mut develop, mut mask, mut h)) = setup(NewMask::Color, Arm::Pick) else {
            return;
        };
        h.click(h.at(0.3, 0.3), &mut develop, &mut mask);
        h.click(h.at(0.8, 0.7), &mut develop, &mut mask);
        let samples = match &correction(&develop).mask.components[0].source {
            MaskSource::ColorRange { samples, .. } => samples.clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(samples.len(), 2);
        assert!(samples.iter().all(|lab| lab.iter().all(|v| v.is_finite())));
        // The synthetic frame is a gradient, so two distant points are different colours.
        assert_ne!(samples[0], samples[1]);
    }

    #[test]
    fn every_kind_of_mask_survives_the_overlay_pass() {
        // The overlay composes a CPU preview of whatever is selected (geometry, AI, range) --
        // run one frame for each kind to be sure none of them panics.
        for kind in NewMask::ALL {
            let Some((mut develop, mut mask, mut h)) = setup(kind, kind.arm()) else {
                return;
            };
            h.frame(
                vec![Event::PointerMoved(pos2(300.0, 200.0))],
                &mut develop,
                &mut mask,
            );
            assert!(mask.overlay.is_some(), "{kind:?} drew no overlay texture");
        }
    }

    #[test]
    fn the_overlay_only_rebuilds_when_the_mask_changes() {
        let Some((mut develop, mut mask, mut h)) = setup(NewMask::Radial, Arm::None) else {
            return;
        };
        h.frame(vec![], &mut develop, &mut mask);
        let key0 = mask.overlay.as_ref().unwrap().key;
        h.frame(vec![], &mut develop, &mut mask);
        assert_eq!(
            mask.overlay.as_ref().unwrap().key,
            key0,
            "unchanged mask, same preview"
        );
        // Editing the geometry (through the panel's own params write) changes it.
        let mut params: MaskParams = develop.stage_params(MASKS);
        set_radial(&mut params.corrections[0], [0.2, 0.2], [0.1, 0.1]);
        develop.set_stage_params(MASKS, &params);
        h.frame(vec![], &mut develop, &mut mask);
        assert_ne!(mask.overlay.as_ref().unwrap().key, key0);
    }

    /// Runs the whole panel (every widget) for a set of masks covering every component kind, twice,
    /// to catch panics and egui id clashes in the widget tree.
    #[test]
    fn the_panel_renders_every_kind_of_mask_and_the_selection_survives() {
        let Some(gpu) = crate::test_gpu::shared() else {
            return;
        };
        let mut develop = DevelopView::new(gpu);
        let mut mask = MaskUi::with_service(MaskBakeService::with_store(None));
        let mut params = MaskParams::default();
        for kind in NewMask::ALL {
            let id = next_id(&params);
            add_correction(&mut params, new_correction(kind, id));
        }
        // One correction with several components of different ops, and non-zero adjustments.
        add_component(&mut params.corrections[3], NewMask::Radial, Op::Subtract);
        add_component(
            &mut params.corrections[3],
            NewMask::Luminance,
            Op::Intersect,
        );
        params.corrections[3].adjust = LocalAdjust {
            exposure: 0.5,
            clarity: 0.3,
            dehaze: -0.2,
            ..LocalAdjust::default()
        };
        develop.set_stage_params(MASKS, &params);
        mask.selected = Some(3);
        let p = pounce();
        let ctx = egui::Context::default();
        for _ in 0..2 {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(900.0, 1400.0))),
                ..Default::default()
            };
            let output = ctx.run_ui(input, |ui| {
                show_panel(ui, &mut develop, &mut mask, &p);
            });
            output.drop_without_applying_deltas();
        }
        assert_eq!(mask.selected, Some(3));
        assert_eq!(
            develop.stage_params::<MaskParams>(MASKS),
            params,
            "just showing changes nothing"
        );
    }

    #[test]
    fn the_panel_shows_the_download_prompt_for_a_subject_mask_without_the_model() {
        let Some(gpu) = crate::test_gpu::shared() else {
            return;
        };
        let mut develop = DevelopView::new(gpu);
        let mut mask = MaskUi::with_service(MaskBakeService::with_store(None));
        let params = MaskParams {
            corrections: vec![new_correction(NewMask::Subject, "mask-1".into())],
        };
        develop.set_stage_params(MASKS, &params);
        mask.selected = Some(0);
        assert!(
            mask.service.download_needed(&develop).is_some(),
            "a subject mask waits on the model"
        );
        let p = pounce();
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(900.0, 1400.0))),
            ..Default::default()
        };
        let output = ctx.run_ui(input, |ui| show_panel(ui, &mut develop, &mut mask, &p));
        output.drop_without_applying_deltas();
        assert!(
            !mask.service.is_installing(),
            "showing the prompt downloads nothing"
        );
        assert_eq!(
            ai_state(
                &mask,
                &develop,
                &params.corrections[0].mask.components[0].source
            ),
            Some(AiState::NeedsModel)
        );
        assert!(mask.status().is_none());
    }

    #[test]
    fn a_stale_arm_never_edits_a_correction_that_lacks_the_tool() {
        // Brush armed, then a Subject mask is selected: a drag must not add a brush component.
        let Some((mut develop, mut mask, mut h)) = setup(NewMask::Subject, Arm::Brush) else {
            return;
        };
        h.drag(h.at(0.2, 0.2), h.at(0.7, 0.7), &mut develop, &mut mask);
        assert_eq!(
            correction(&develop).mask.components.len(),
            1,
            "no component was added"
        );
        assert!(strokes(&develop).is_empty());
        // Same for the other tools.
        for arm in [Arm::Linear, Arm::Radial, Arm::Pick] {
            mask.arm = arm;
            h.drag(h.at(0.2, 0.2), h.at(0.7, 0.7), &mut develop, &mut mask);
            h.click(h.at(0.5, 0.5), &mut develop, &mut mask);
            assert_eq!(correction(&develop).mask.components.len(), 1, "{arm:?}");
        }
    }

    #[test]
    fn rotated_radial_handles_sit_on_the_real_mask_edge_and_drags_size_the_rotated_axes() {
        let source = (600.0f32, 400.0);
        let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(600.0, 400.0));
        let (center, radii, angle) = ([0.5f32, 0.5], [0.2f32, 0.1], 30.0f32);
        let (hc, hx, hy) = radial_handles(rect, source, center, radii, angle);
        // The X handle is where the rasterizer's own math puts the ellipse edge along its x axis.
        let long = 600.0f32;
        let (s, c) = angle.to_radians().sin_cos();
        let want_x = pos2(300.0 + 0.2 * long * c, 200.0 + 0.2 * long * s);
        assert!(hx.distance(want_x) < 0.5, "{hx:?} vs {want_x:?}");
        assert!(hc.distance(pos2(300.0, 200.0)) < 0.5);
        // On the ellipse boundary the weight is still full (the feather starts outside it).
        let w = |p: egui::Pos2| {
            nicti_tapetum::mask::raster::radial_weight(
                (300.0, 200.0),
                (0.2 * long, 0.1 * long),
                angle,
                20.0,
                p.x,
                p.y,
            )
        };
        assert!(w(hx) > 0.99 && w(hy) > 0.99, "handles are on the edge");
        // Dragging the X handle to a point along the rotated axis gives that radius back.
        let target = local_to_norm(center, 0.3, 0.0, angle, source);
        let r = radii_to(center, target, source, angle);
        assert!((r[0] - 0.3).abs() < 1e-4 && r[1] <= 0.0021, "{r:?}");
    }

    #[test]
    fn a_tint_hue_chosen_while_the_amount_is_zero_is_kept_and_still_inert() {
        let hue_only = nicti_tapetum::mask::params::TintColor {
            hue_deg: 200.0,
            saturation: 0.0,
        };
        assert_ne!(hue_only, Default::default());
        let a = LocalAdjust {
            color: Some(hue_only),
            ..LocalAdjust::default()
        };
        assert!(a.is_noop(), "an amount-0 tint still changes nothing");
    }
}
