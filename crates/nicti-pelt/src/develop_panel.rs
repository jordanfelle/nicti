//! The Develop view's right-side edit panel (#46): Basic tone, Tone Curve, HSL, Detail
//! (Sharpen/Noise Reduction), a live histogram, an Auto button, and the before/after toggle. Pure
//! UI glue over `render::DevelopView`'s `stage_params`/`set_stage_params`/`reset_stage` -- every
//! slider here reads/writes one `coat.rs` params struct, the same shape a catalog-backed edit
//! (once #31 lands persistence) will read too.

use nicti_tapetum::coat::{
    CropParams, ExposureParams, HslBand, HslParams, NoiseReductionParams, SharpenParams,
    ToneCurveParams, ToneParams, VibranceParams, WbParams,
};
use nicti_tapetum::frame::FrameTexture;
use nicti_tapetum::geometry::MAX_STRAIGHTEN_DEGREES;
use nicti_tapetum::stages::{
    CROP, EXPOSURE, HEAL, HSL, MASKS, NOISE_REDUCTION, SHARPEN, TONE, TONE_CURVE, VIBRANCE, WB,
};

use crate::heal_tool::{self, HealUi};
use crate::render::{AutoApplied, DevelopView};
use nicti_tapetum::auto::AutoReason;
use std::time::{Duration, Instant};

const HSL_BAND_NAMES: [&str; 8] = ["R", "O", "Y", "G", "A", "B", "P", "M"];

/// How long a transient auto-op hint stays under its button.
const HINT_LIFETIME: Duration = Duration::from_secs(3);

/// Which automatic operation a hint belongs to (it is shown under that operation's button).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoOp {
    Tone,
    Straighten,
}

/// The non-modal feedback for automatic develop operations (ADR-0101): a transient hint under the
/// button that was clicked, and a marker beside Auto while a low-confidence tone is still in place.
/// Never a modal dialog.
#[derive(Default)]
pub struct AutoHintUi {
    /// (op, text, when, photo): the photo is `DevelopView::frame_key`, so a hint never follows the
    /// user onto a different photo.
    hint: Option<(AutoOp, &'static str, Instant, u64)>,
    /// The exposure/tone a low-confidence Auto wrote, and the photo it was written for. The marker
    /// shows while that photo's document still holds exactly these values, so any slider move,
    /// reset, undo or photo switch clears it.
    tone_marker: Option<(ExposureParams, ToneParams, u64)>,
}

/// The wording of the hint an outcome earns (`None` = silent success). Wording follows ADR-0101's
/// examples; the exact copy is deferred to this ticket by the ADR.
fn hint_text(op: AutoOp, result: AutoApplied) -> Option<&'static str> {
    match (op, result) {
        (_, AutoApplied::Applied | AutoApplied::AppliedLowConfidence(_)) => None,
        (
            _,
            AutoApplied::Skipped {
                reason: AutoReason::DecodeIncomplete,
                ..
            },
        ) => Some("Photo still loading"),
        (
            AutoOp::Straighten,
            AutoApplied::Skipped {
                low_confidence: true,
                ..
            },
        ) => Some("Uncertain angle, not applied"),
        (AutoOp::Straighten, AutoApplied::Skipped { .. }) => Some("No straight lines found"),
        (AutoOp::Straighten, AutoApplied::Unchanged) => Some("Already level"),
        (AutoOp::Tone, AutoApplied::Skipped { .. }) => Some("Auto could not analyse this photo"),
        (AutoOp::Tone, AutoApplied::Unchanged) => Some("Already at Auto settings"),
    }
}

impl AutoHintUi {
    /// Records what an auto op just did on `photo`: sets (or clears) the hint, and -- only when
    /// Auto actually wrote something -- arms or clears the low-confidence tone marker. An
    /// `Unchanged`/`Skipped` result leaves the document, and so the marker, as it was: a repeat
    /// click on a still-low-confidence result must not make it look confident.
    fn record(
        &mut self,
        op: AutoOp,
        result: AutoApplied,
        photo: u64,
        tone_now: (ExposureParams, ToneParams),
    ) {
        self.hint = hint_text(op, result).map(|text| (op, text, Instant::now(), photo));
        if op == AutoOp::Tone {
            match result {
                AutoApplied::AppliedLowConfidence(_) => {
                    self.tone_marker = Some((tone_now.0, tone_now.1, photo));
                }
                AutoApplied::Applied => self.tone_marker = None,
                AutoApplied::Unchanged | AutoApplied::Skipped { .. } => {}
            }
        }
    }

    /// The `⚠` beside Auto, while a low-confidence result is still what's in the document.
    fn show_tone_marker(&mut self, ui: &mut egui::Ui, develop: &DevelopView) {
        let Some(marker) = self.tone_marker else {
            return;
        };
        let still_current = marker.2 == develop.frame_key()
            && marker.0 == develop.stage_params::<ExposureParams>(EXPOSURE)
            && marker.1 == develop.stage_params::<ToneParams>(TONE);
        if !still_current {
            self.tone_marker = None;
            return;
        }
        ui.label(egui::RichText::new("\u{26A0}").color(egui::Color32::YELLOW))
            .on_hover_text("Low confidence: unusual histogram. Undo or adjust to change it.");
    }

    /// The transient hint for `op`, if one is live; schedules the repaint that clears it.
    fn show_hint(&mut self, ui: &mut egui::Ui, op: AutoOp, photo: u64) {
        let Some((hint_op, text, since, hint_photo)) = self.hint else {
            return;
        };
        let age = since.elapsed();
        if age >= HINT_LIFETIME || hint_photo != photo {
            self.hint = None;
            return;
        }
        if hint_op == op {
            ui.weak(text);
        }
        ui.ctx().request_repaint_after(HINT_LIFETIME - age);
    }
}

/// One slider bound to a single `f32` field, with a double-click-to-reset gesture -- the repeated
/// shape every panel section below uses.
fn slider(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    default: f32,
) {
    ui.horizontal(|ui| {
        ui.label(label);
        let response = ui.add(egui::Slider::new(value, range));
        if response.double_clicked() {
            *value = default;
        }
    });
}

/// Renders the Develop panel's controls and returns `true` if any edit changed this render's
/// params (so the caller knows to re-render) -- `develop.render()` is cheap to call unconditionally
/// though (a live-only change costs 0 bake dispatches), so callers may simply always re-render
/// after calling this rather than checking the return value.
#[allow(clippy::too_many_arguments)]
pub fn show(
    ui: &mut egui::Ui,
    develop: &mut DevelopView,
    current_frame: &FrameTexture,
    hsl_band_selected: &mut usize,
    auto_hint: &mut AutoHintUi,
    heal: &mut HealUi,
    mask: &mut crate::mask_panel::MaskUi,
    pounce: &nicti_pounce::Pounce,
) {
    ui.heading("Develop");
    heal_tool::tool_switch(ui, heal);

    ui.horizontal(|ui| {
        let before_label = if develop.show_before {
            "Showing: Before"
        } else {
            "Showing: After"
        };
        if ui.button(before_label).clicked() || ui.input(|i| i.key_pressed(egui::Key::Backslash)) {
            develop.show_before = !develop.show_before;
        }
        if ui.button("Auto").clicked() {
            let result = develop.apply_auto_tone();
            let tone_now = (develop.stage_params(EXPOSURE), develop.stage_params(TONE));
            auto_hint.record(AutoOp::Tone, result, develop.frame_key(), tone_now);
        }
        auto_hint.show_tone_marker(ui, develop);
    });
    auto_hint.show_hint(ui, AutoOp::Tone, develop.frame_key());

    show_histogram(ui, develop, current_frame);

    ui.separator();
    ui.collapsing("Basic", |ui| {
        show_camera_profile_picker(ui, develop);

        let mut wb: WbParams = develop.stage_params(WB);
        let mut temp = wb.temp_k.unwrap_or(5500.0);
        ui.horizontal(|ui| {
            ui.label("Temp");
            let use_as_shot = wb.temp_k.is_none();
            let mut manual = !use_as_shot;
            ui.checkbox(&mut manual, "Manual");
            if manual != !use_as_shot {
                wb.temp_k = if manual { Some(temp) } else { None };
            }
        });
        if wb.temp_k.is_some()
            && ui
                .add(egui::Slider::new(&mut temp, 2000.0..=50000.0))
                .changed()
        {
            wb.temp_k = Some(temp);
        }
        slider(ui, "Tint", &mut wb.tint, -150.0..=150.0, 0.0);
        develop.set_stage_params(WB, &wb);

        let mut exposure: ExposureParams = develop.stage_params(EXPOSURE);
        slider(ui, "Exposure", &mut exposure.stops, -5.0..=5.0, 0.0);
        develop.set_stage_params(EXPOSURE, &exposure);

        let mut tone: ToneParams = develop.stage_params(TONE);
        slider(ui, "Contrast", &mut tone.contrast, -1.0..=1.0, 0.0);
        slider(ui, "Highlights", &mut tone.highlights, -1.0..=1.0, 0.0);
        slider(ui, "Shadows", &mut tone.shadows, -1.0..=1.0, 0.0);
        slider(ui, "Whites", &mut tone.whites, -1.0..=1.0, 0.0);
        slider(ui, "Blacks", &mut tone.blacks, -1.0..=1.0, 0.0);
        develop.set_stage_params(TONE, &tone);

        let mut vibrance: VibranceParams = develop.stage_params(VIBRANCE);
        slider(ui, "Vibrance", &mut vibrance.amount, -1.0..=1.0, 0.0);
        develop.set_stage_params(VIBRANCE, &vibrance);
    });

    ui.collapsing("Tone Curve", |ui| {
        let mut curve: ToneCurveParams = develop.stage_params(TONE_CURVE);
        draw_curve_preview(ui, &curve);
        slider(ui, "Shadows", &mut curve.shadows, -1.0..=1.0, 0.0);
        slider(ui, "Darks", &mut curve.darks, -1.0..=1.0, 0.0);
        slider(ui, "Lights", &mut curve.lights, -1.0..=1.0, 0.0);
        slider(ui, "Highlights", &mut curve.highlights, -1.0..=1.0, 0.0);
        develop.set_stage_params(TONE_CURVE, &curve);
    });

    ui.collapsing("HSL", |ui| {
        ui.horizontal(|ui| {
            for (i, name) in HSL_BAND_NAMES.iter().enumerate() {
                if ui
                    .selectable_label(*hsl_band_selected == i, *name)
                    .clicked()
                {
                    *hsl_band_selected = i;
                }
            }
        });
        let mut hsl: HslParams = develop.stage_params(HSL);
        let band: &mut HslBand = &mut hsl.bands[*hsl_band_selected];
        slider(ui, "Hue", &mut band.hue, -1.0..=1.0, 0.0);
        slider(ui, "Saturation", &mut band.saturation, -1.0..=1.0, 0.0);
        slider(ui, "Luminance", &mut band.luminance, -1.0..=1.0, 0.0);
        develop.set_stage_params(HSL, &hsl);
    });

    ui.collapsing("Crop & Straighten", |ui| {
        let mut crop: CropParams = develop.stage_params(CROP);
        ui.label(
            "Hold Ctrl and drag along something in the photo that should be level, or use the \
             rotate handle / freeform drag on the overlay.",
        );
        ui.horizontal(|ui| {
            ui.label("Rotation");
            let mut rotation = crop.rotation_degrees;
            let response = ui.add(egui::Slider::new(
                &mut rotation,
                -MAX_STRAIGHTEN_DEGREES..=MAX_STRAIGHTEN_DEGREES,
            ));
            if response.double_clicked() {
                rotation = 0.0;
            }
            if response.changed() || response.double_clicked() {
                crop.set_rotation(rotation);
            }
        });
        if ui.button("Auto-level").clicked() {
            let result = develop.apply_auto_straighten();
            let tone_now = (develop.stage_params(EXPOSURE), develop.stage_params(TONE));
            auto_hint.record(AutoOp::Straighten, result, develop.frame_key(), tone_now);
            crop = develop.stage_params(CROP);
        }
        auto_hint.show_hint(ui, AutoOp::Straighten, develop.frame_key());
        develop.set_stage_params(CROP, &crop);

        ui.separator();
        ui.label("Crop rectangle (source pixels; 0 width/height = full frame)");
        let (src_w, src_h) = develop.source_extent();
        ui.horizontal(|ui| {
            ui.label("X");
            ui.add(egui::DragValue::new(&mut crop.x).range(0.0..=src_w));
            ui.label("Y");
            ui.add(egui::DragValue::new(&mut crop.y).range(0.0..=src_h));
        });
        ui.horizontal(|ui| {
            ui.label("W");
            ui.add(egui::DragValue::new(&mut crop.width).range(0.0..=src_w));
            ui.label("H");
            ui.add(egui::DragValue::new(&mut crop.height).range(0.0..=src_h));
        });
        develop.set_stage_params(CROP, &crop);

        if ui.button("Reset crop").clicked() {
            develop.reset_stage(CROP);
        }
    });

    ui.collapsing("Detail", |ui| {
        let mut sharpen: SharpenParams = develop.stage_params(SHARPEN);
        slider(ui, "Sharpen Amount", &mut sharpen.amount, 0.0..=1.5, 0.0);
        slider(ui, "Sharpen Radius", &mut sharpen.radius_px, 0.5..=3.0, 1.0);
        slider(ui, "Sharpen Detail", &mut sharpen.detail, 0.0..=1.0, 0.5);
        develop.set_stage_params(SHARPEN, &sharpen);

        let mut nr: NoiseReductionParams = develop.stage_params(NOISE_REDUCTION);
        slider(ui, "NR Luminance", &mut nr.luminance, 0.0..=1.0, 0.0);
        slider(ui, "NR Color", &mut nr.color, 0.0..=1.0, 0.0);
        slider(ui, "NR Detail", &mut nr.detail, 0.0..=1.0, 0.0);
        develop.set_stage_params(NOISE_REDUCTION, &nr);
    });

    if heal.heal_active() {
        heal_tool::show_panel(ui, develop, heal, pounce);
    }
    if heal.mask_active() {
        crate::mask_panel::show_panel(ui, develop, mask, pounce);
    }

    ui.separator();
    if ui.button("Reset all").clicked() {
        for id in [
            HEAL,
            WB,
            EXPOSURE,
            TONE,
            TONE_CURVE,
            VIBRANCE,
            HSL,
            SHARPEN,
            NOISE_REDUCTION,
            CROP,
            MASKS,
        ] {
            develop.reset_stage(id);
        }
        // The heal spots are gone, so their finished removals (up to tens of MB each) are too --
        // and so are the local corrections, whose finished AI alphas go with them.
        develop.prune_removals();
        develop.prune_ai_alphas();
        mask.selected = None;
    }
}

/// Which part of the crop overlay a drag started on -- decides what the drag does. Checked in
/// this order (rotate handle first, since it sits just outside the rect and would otherwise be
/// shadowed by a corner) each time a drag begins; the choice is then remembered in egui's own
/// per-widget temp storage for the duration of that one drag, so a fast pointer move mid-drag
/// can't suddenly switch what's being manipulated.
#[derive(Debug, Clone, Copy, PartialEq)]
enum DragTarget {
    Straighten,
    Rotate,
    Corner(usize),
    Pan,
}

/// Drag-start state stashed in egui memory (keyed by a fixed `Id`) so [`handle_viewport_gesture`]
/// -- a plain per-frame function, not a struct with its own state -- can compute a delta relative
/// to where the gesture began, not just this frame's incremental pointer motion.
#[derive(Debug, Clone, Copy)]
struct DragState {
    target: DragTarget,
    start_pointer: egui::Pos2,
    start_crop: CropParams,
}

const DRAG_STATE_ID: &str = "nicti_pelt::develop_panel::crop_drag_state";
const HANDLE_RADIUS_PX: f32 = 6.0;
const ROTATE_HANDLE_OFFSET_PX: f32 = 28.0;

/// Maps a screen-space point (inside `rect`) to source-image pixel space -- the whole,
/// uncropped-canvas render is painted stretched to fill `rect` (see `app.rs`'s
/// `ui.allocate_exact_size`), so this can be anisotropic when `rect`'s aspect ratio doesn't match
/// the source's.
pub(crate) fn screen_to_image(rect: egui::Rect, source: (f32, f32), p: egui::Pos2) -> (f32, f32) {
    (
        (p.x - rect.left()) / rect.width().max(1.0) * source.0,
        (p.y - rect.top()) / rect.height().max(1.0) * source.1,
    )
}

pub(crate) fn image_to_screen(rect: egui::Rect, source: (f32, f32), p: (f32, f32)) -> egui::Pos2 {
    egui::pos2(
        rect.left() + p.0 / source.0.max(1.0) * rect.width(),
        rect.top() + p.1 / source.1.max(1.0) * rect.height(),
    )
}

/// The forward (clockwise-positive, y-down) content-rotation matrix's action on a vector relative
/// to `center` -- matches `geometry::Affine2D::crop_and_rotate`'s own documented sign convention
/// exactly (that function applies the *inverse*; this is its forward counterpart, used here only
/// to draw/hit-test the overlay in the same visually-rotated frame the image itself renders in).
fn rotate_forward(center: (f32, f32), p: (f32, f32), degrees: f32) -> (f32, f32) {
    let theta = degrees.to_radians();
    let (sin_t, cos_t) = theta.sin_cos();
    let (rx, ry) = (p.0 - center.0, p.1 - center.1);
    (
        center.0 + cos_t * rx - sin_t * ry,
        center.1 + sin_t * rx + cos_t * ry,
    )
}

/// Draws the crop/straighten overlay (rect outline, corner handles, rotate handle) and handles
/// every gesture on it: **Ctrl+drag** anywhere is the reference-line straighten gesture (distinct
/// from the two below); a plain drag on the small rotate handle above the rect is freeform
/// rotate; a plain drag on a corner handle resizes the rect; a plain drag inside the rect (and not
/// on a handle) pans it. `rect` is the on-screen area the current (full-canvas) render is painted
/// into -- see `screen_to_image`'s own doc comment for why the mapping can be anisotropic.
pub fn handle_viewport_gesture(
    ui: &mut egui::Ui,
    response: &egui::Response,
    rect: egui::Rect,
    develop: &mut DevelopView,
) {
    let source = develop.source_extent();
    let crop: CropParams = develop.stage_params(CROP);
    let crop_rect = crop.effective_rect(source);
    let center = crop_rect.center_absolute();
    let state_id = egui::Id::new(DRAG_STATE_ID);

    if response.drag_started() {
        let ctrl = ui.input(|i| i.modifiers.ctrl);
        let Some(pointer) = response.interact_pointer_pos() else {
            return;
        };
        let target = if ctrl {
            DragTarget::Straighten
        } else {
            let rotated_top = rotate_forward(
                center,
                (center.0, crop_rect.y - ROTATE_HANDLE_OFFSET_PX),
                crop.rotation_degrees,
            );
            let screen_rotate_handle = image_to_screen(rect, source, rotated_top);
            if pointer.distance(screen_rotate_handle) <= HANDLE_RADIUS_PX * 2.0 {
                DragTarget::Rotate
            } else if let Some(corner) =
                corner_hit(rect, source, &crop_rect, crop.rotation_degrees, pointer)
            {
                DragTarget::Corner(corner)
            } else {
                DragTarget::Pan
            }
        };
        ui.data_mut(|d| {
            d.insert_temp(
                state_id,
                DragState {
                    target,
                    start_pointer: pointer,
                    start_crop: crop,
                },
            )
        });
    }

    if response.dragged() {
        let Some(state) = ui.data(|d| d.get_temp::<DragState>(state_id)) else {
            return;
        };
        let Some(pointer) = response.interact_pointer_pos() else {
            return;
        };
        let start_img = screen_to_image(rect, source, state.start_pointer);
        let cur_img = screen_to_image(rect, source, pointer);
        let delta = (cur_img.0 - start_img.0, cur_img.1 - start_img.1);

        match state.target {
            DragTarget::Straighten => {
                // Only applied on release (matches "release to auto-rotate" in the ticket) --
                // nothing to do while still dragging.
            }
            DragTarget::Rotate => {
                // Computed in *image* space (via `screen_to_image`), not raw screen pixels --
                // `rect` can be anisotropically stretched relative to the source image (see
                // `screen_to_image`'s own doc comment), so a screen-space angle is not generally
                // the same as the image-space content rotation it's meant to drive.
                let start_img = screen_to_image(rect, source, state.start_pointer);
                let cur_img = screen_to_image(rect, source, pointer);
                let start_angle = (start_img.1 - center.1).atan2(start_img.0 - center.0);
                let cur_angle = (cur_img.1 - center.1).atan2(cur_img.0 - center.0);
                let delta_deg = (cur_angle - start_angle).to_degrees();
                let mut new_crop = state.start_crop;
                new_crop.set_rotation(state.start_crop.rotation_degrees + delta_deg);
                develop.set_stage_params(CROP, &new_crop);
            }
            DragTarget::Corner(idx) => {
                let start_rect = state.start_crop.effective_rect(source);
                // Inverse-rotate the image-space delta into the rect's own unrotated local frame
                // -- a corner drag should feel like resizing the rect along its own edges, not
                // the screen's, once it's been straightened.
                let theta = (-state.start_crop.rotation_degrees).to_radians();
                let (sin_t, cos_t) = theta.sin_cos();
                let local_dx = cos_t * delta.0 - sin_t * delta.1;
                let local_dy = sin_t * delta.0 + cos_t * delta.1;

                // Each corner drag moves exactly one edge per axis and keeps the *opposite* edge
                // fixed (idx 0=top-left, 1=top-right, 2=bottom-right, 3=bottom-left). Clamp both
                // the moving edge AND the fixed edge to the source bounds first (an adversarial
                // review of an earlier version of this fix caught that leaving the fixed edge
                // unclamped regresses the old code's unconditional containment guarantee -- a
                // stale/corrupt persisted `CropParams` with an out-of-bounds start_rect, e.g. from
                // an imported edit predating a source-resolution change, could no longer self-heal
                // via a corner drag), then derive that axis's size from the distance between them
                // (floored at 1.0). Deriving size this way, instead of clamping position and size
                // independently as the original pre-review version did, keeps the fixed edge from
                // drifting once an ordinary (in-bounds) drag crosses a source boundary, while still
                // guaranteeing the final rect stays within `[0, source]` even from corrupt input.
                let moves_left = matches!(idx, 0 | 3);
                let (x, width) = if moves_left {
                    let fixed_right = (start_rect.x + start_rect.width).clamp(0.0, source.0);
                    let moving_x = (start_rect.x + local_dx).clamp(0.0, source.0);
                    let width = (fixed_right - moving_x).max(1.0);
                    ((fixed_right - width).clamp(0.0, source.0), width)
                } else {
                    let fixed_left = start_rect.x.clamp(0.0, source.0);
                    let moving_right =
                        (start_rect.x + start_rect.width + local_dx).clamp(0.0, source.0);
                    let width = (moving_right - fixed_left).max(1.0);
                    (fixed_left, width)
                };

                let moves_top = matches!(idx, 0 | 1);
                let (y, height) = if moves_top {
                    let fixed_bottom = (start_rect.y + start_rect.height).clamp(0.0, source.1);
                    let moving_y = (start_rect.y + local_dy).clamp(0.0, source.1);
                    let height = (fixed_bottom - moving_y).max(1.0);
                    ((fixed_bottom - height).clamp(0.0, source.1), height)
                } else {
                    let fixed_top = start_rect.y.clamp(0.0, source.1);
                    let moving_bottom =
                        (start_rect.y + start_rect.height + local_dy).clamp(0.0, source.1);
                    let height = (moving_bottom - fixed_top).max(1.0);
                    (fixed_top, height)
                };

                let mut new_crop = state.start_crop;
                new_crop.x = x;
                new_crop.y = y;
                new_crop.width = width;
                new_crop.height = height;
                develop.set_stage_params(CROP, &new_crop);
            }
            DragTarget::Pan => {
                let mut new_rect = state.start_crop.effective_rect(source);
                new_rect.x =
                    (new_rect.x + delta.0).clamp(0.0, (source.0 - new_rect.width).max(0.0));
                new_rect.y =
                    (new_rect.y + delta.1).clamp(0.0, (source.1 - new_rect.height).max(0.0));
                let mut new_crop = state.start_crop;
                new_crop.x = new_rect.x;
                new_crop.y = new_rect.y;
                new_crop.width = new_rect.width;
                new_crop.height = new_rect.height;
                develop.set_stage_params(CROP, &new_crop);
            }
        }
    }

    if response.drag_stopped() {
        if let Some(state) = ui.data(|d| d.get_temp::<DragState>(state_id)) {
            if state.target == DragTarget::Straighten {
                if let Some(pointer) = response.interact_pointer_pos() {
                    let start_img = screen_to_image(rect, source, state.start_pointer);
                    let cur_img = screen_to_image(rect, source, pointer);
                    develop.straighten_from_drag(cur_img.0 - start_img.0, cur_img.1 - start_img.1);
                }
            }
        }
        ui.data_mut(|d| d.remove::<DragState>(state_id));
    }

    draw_crop_overlay(ui, rect, source, &crop);
}

/// Hit-tests the 4 corner handles (0=top-left, 1=top-right, 2=bottom-right, 3=bottom-left, in the
/// rect's own rotated frame) against a screen-space pointer position.
fn corner_hit(
    rect: egui::Rect,
    source: (f32, f32),
    crop_rect: &nicti_tapetum::geometry::CropRect,
    rotation_degrees: f32,
    pointer: egui::Pos2,
) -> Option<usize> {
    let center = crop_rect.center_absolute();
    let corners = [
        (crop_rect.x, crop_rect.y),
        (crop_rect.x + crop_rect.width, crop_rect.y),
        (
            crop_rect.x + crop_rect.width,
            crop_rect.y + crop_rect.height,
        ),
        (crop_rect.x, crop_rect.y + crop_rect.height),
    ];
    corners.iter().enumerate().find_map(|(i, &c)| {
        let rotated = rotate_forward(center, c, rotation_degrees);
        let screen = image_to_screen(rect, source, rotated);
        if pointer.distance(screen) <= HANDLE_RADIUS_PX * 2.0 {
            Some(i)
        } else {
            None
        }
    })
}

/// Draws the crop rect outline (rotated per `crop.rotation_degrees`), its 4 corner handles, and
/// the rotate handle above its top edge.
fn draw_crop_overlay(ui: &mut egui::Ui, rect: egui::Rect, source: (f32, f32), crop: &CropParams) {
    let crop_rect = crop.effective_rect(source);
    let center = crop_rect.center_absolute();
    let corners_img = [
        (crop_rect.x, crop_rect.y),
        (crop_rect.x + crop_rect.width, crop_rect.y),
        (
            crop_rect.x + crop_rect.width,
            crop_rect.y + crop_rect.height,
        ),
        (crop_rect.x, crop_rect.y + crop_rect.height),
    ];
    let painter = ui.painter_at(rect);
    let screen_points: Vec<egui::Pos2> = corners_img
        .iter()
        .map(|&c| {
            image_to_screen(
                rect,
                source,
                rotate_forward(center, c, crop.rotation_degrees),
            )
        })
        .collect();
    let stroke = egui::Stroke::new(1.5, egui::Color32::from_rgb(255, 210, 0));
    for i in 0..4 {
        painter.line_segment([screen_points[i], screen_points[(i + 1) % 4]], stroke);
    }
    for &p in &screen_points {
        painter.circle_filled(p, HANDLE_RADIUS_PX, egui::Color32::from_rgb(255, 210, 0));
    }
    let rotated_top = rotate_forward(
        center,
        (center.0, crop_rect.y - ROTATE_HANDLE_OFFSET_PX),
        crop.rotation_degrees,
    );
    let handle_screen = image_to_screen(rect, source, rotated_top);
    painter.line_segment(
        [
            image_to_screen(
                rect,
                source,
                rotate_forward(center, (center.0, crop_rect.y), crop.rotation_degrees),
            ),
            handle_screen,
        ],
        stroke,
    );
    painter.circle_filled(
        handle_screen,
        HANDLE_RADIUS_PX,
        egui::Color32::from_rgb(0, 200, 255),
    );
}

fn show_histogram(ui: &mut egui::Ui, develop: &DevelopView, frame: &FrameTexture) {
    let hist = develop.histogram(frame);
    let (rect, _response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 80.0), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, egui::Color32::from_gray(20));
    let max_count = *[
        hist.r.iter().max(),
        hist.g.iter().max(),
        hist.b.iter().max(),
    ]
    .into_iter()
    .flatten()
    .max()
    .unwrap_or(&1)
    .max(&1);
    let bin_width = rect.width() / 256.0;
    for (channel, color) in [
        (
            &hist.r,
            egui::Color32::from_rgba_unmultiplied(255, 60, 60, 140),
        ),
        (
            &hist.g,
            egui::Color32::from_rgba_unmultiplied(60, 255, 60, 140),
        ),
        (
            &hist.b,
            egui::Color32::from_rgba_unmultiplied(60, 60, 255, 140),
        ),
    ] {
        for (i, &count) in channel.iter().enumerate() {
            if count == 0 {
                continue;
            }
            let h = rect.height() * (count as f32 / max_count as f32).min(1.0);
            let x = rect.left() + i as f32 * bin_width;
            let bar = egui::Rect::from_min_max(
                egui::pos2(x, rect.bottom() - h),
                egui::pos2(x + bin_width.max(1.0), rect.bottom()),
            );
            painter.rect_filled(bar, 0.0, color);
        }
    }
}

/// A small preview of the tone-curve LUT (see `color::build_tone_curve_lut`) as a line from
/// bottom-left to top-right -- lets the user see the curve shape, not just the four slider values.
fn draw_curve_preview(ui: &mut egui::Ui, curve: &ToneCurveParams) {
    let lut = nicti_tapetum::color::build_tone_curve_lut(curve);
    let (rect, _response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 80.0), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, egui::Color32::from_gray(20));
    let points: Vec<egui::Pos2> = lut
        .iter()
        .enumerate()
        .map(|(i, &y)| {
            egui::pos2(
                rect.left() + rect.width() * (i as f32 / 255.0),
                rect.bottom() - rect.height() * y.clamp(0.0, 1.0),
            )
        })
        .collect();
    painter.add(egui::Shape::line(
        points,
        egui::Stroke::new(1.5, egui::Color32::WHITE),
    ));
}

/// The DCP camera-profile picker (#42): lists the installed Adobe profiles for this frame's
/// camera, plus "Matrix only" (the plain LibRaw color matrix). Hidden when none are installed.
fn show_camera_profile_picker(ui: &mut egui::Ui, develop: &mut DevelopView) {
    let choices = develop.profile_choices().to_vec();
    if choices.is_empty() {
        return;
    }
    let current = develop.camera_profile();
    let mut pick: Option<Option<usize>> = None;
    ui.horizontal(|ui| {
        ui.label("Profile");
        egui::ComboBox::from_id_salt("camera_profile")
            .selected_text(current.name.as_deref().unwrap_or("Matrix only"))
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(current.name.is_none(), "Matrix only (no profile)")
                    .clicked()
                {
                    pick = Some(None);
                }
                for (i, entry) in choices.iter().enumerate() {
                    let selected =
                        current.path.as_deref() == Some(entry.path.display().to_string().as_str());
                    if ui.selectable_label(selected, &entry.name).clicked() {
                        pick = Some(Some(i));
                    }
                }
            });
    });
    if let Some(pick) = pick {
        develop.select_camera_profile(pick.map(|i| &choices[i]));
    }
    if let Some(err) = &develop.profile_error {
        ui.colored_label(ui.visuals().error_fg_color, err);
    }
}

#[cfg(test)]
mod auto_hint_tests {
    use super::*;

    fn skipped(reason: AutoReason, low_confidence: bool) -> AutoApplied {
        AutoApplied::Skipped {
            reason,
            low_confidence,
        }
    }

    #[test]
    fn a_successful_apply_is_silent() {
        for op in [AutoOp::Tone, AutoOp::Straighten] {
            assert_eq!(hint_text(op, AutoApplied::Applied), None);
            assert_eq!(
                hint_text(
                    op,
                    AutoApplied::AppliedLowConfidence(AutoReason::AtypicalInput)
                ),
                None
            );
        }
    }

    #[test]
    fn straighten_hints_distinguish_nothing_found_from_too_uncertain() {
        let none = hint_text(AutoOp::Straighten, skipped(AutoReason::NoFeatures, false));
        let weak = hint_text(AutoOp::Straighten, skipped(AutoReason::WeakEvidence, true));
        assert_eq!(none, Some("No straight lines found"));
        assert_eq!(weak, Some("Uncertain angle, not applied"));
        assert_eq!(
            hint_text(AutoOp::Straighten, AutoApplied::Unchanged),
            Some("Already level")
        );
    }

    #[test]
    fn an_unchanged_tone_result_never_looks_like_a_failed_click() {
        assert_eq!(
            hint_text(AutoOp::Tone, AutoApplied::Unchanged),
            Some("Already at Auto settings")
        );
    }

    #[test]
    fn an_incomplete_decode_says_the_photo_is_loading_for_either_op() {
        for op in [AutoOp::Tone, AutoOp::Straighten] {
            assert_eq!(
                hint_text(op, skipped(AutoReason::DecodeIncomplete, false)),
                Some("Photo still loading")
            );
        }
    }

    fn tone(stops: f32) -> (ExposureParams, ToneParams) {
        (ExposureParams { stops }, ToneParams::default())
    }

    #[test]
    fn a_repeat_click_on_a_low_confidence_result_keeps_the_marker() {
        let mut ui = AutoHintUi::default();
        let low = AutoApplied::AppliedLowConfidence(AutoReason::AtypicalInput);
        ui.record(AutoOp::Tone, low, 7, tone(1.0));
        assert!(ui.tone_marker.is_some());
        ui.record(AutoOp::Tone, AutoApplied::Unchanged, 7, tone(1.0));
        assert!(ui.tone_marker.is_some(), "Unchanged must not clear it");
        let skipped = AutoApplied::Skipped {
            reason: AutoReason::DecodeIncomplete,
            low_confidence: false,
        };
        ui.record(AutoOp::Tone, skipped, 7, tone(1.0));
        assert!(ui.tone_marker.is_some(), "Skipped must not clear it");
    }

    #[test]
    fn a_confident_apply_clears_the_marker_and_straighten_never_touches_it() {
        let mut ui = AutoHintUi::default();
        let low = AutoApplied::AppliedLowConfidence(AutoReason::AtypicalInput);
        ui.record(AutoOp::Tone, low, 7, tone(1.0));
        ui.record(AutoOp::Straighten, AutoApplied::Applied, 7, tone(1.0));
        assert!(ui.tone_marker.is_some());
        ui.record(AutoOp::Tone, AutoApplied::Applied, 7, tone(2.0));
        assert!(ui.tone_marker.is_none());
    }

    #[test]
    fn a_hint_remembers_which_photo_it_was_for() {
        let mut ui = AutoHintUi::default();
        ui.record(AutoOp::Straighten, AutoApplied::Unchanged, 7, tone(0.0));
        assert_eq!(ui.hint.map(|h| h.3), Some(7));
    }
}
