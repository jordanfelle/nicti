//! The targeted-adjustment tool (#453): while `Tool::Targeted` owns the viewport, a vertical drag
//! on the photo moves the Tone Curve point (master channel) or the HSL band value that the pixel
//! under the pointer belongs to -- LRC's TAT. Dragging up raises the value, down lowers it.
//!
//! Like Point Color (`catseye`), the pixel is read from the small as-shot thumbnail, so the band
//! or curve position is picked from the photo as shot, not the rendered frame. The hue and the
//! curve x use the same maths the renderer does (HSV hue of the working RGB; cube-root of the
//! pixel), but they ignore edits already applied: with a large exposure or tone edit the grabbed
//! tonal region is approximate.

use nicti_calico::tonecurve::ToneCurve;
use nicti_tapetum::coat::{HslParams, PointCurveParams, MAX_CURVE_POINTS};
use nicti_tapetum::color::hsl_hue_degrees;
use nicti_tapetum::stages::{HSL, POINT_CURVE};

use crate::catseye::thumb_for;
use crate::heal_tool::{HealUi, Tool};
use crate::mask_panel::to_norm;
use crate::render::DevelopDoc;

/// What a drag edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Curve,
    Hue,
    Saturation,
    Luminance,
}

impl Mode {
    pub const ALL: [(Mode, &'static str); 4] = [
        (Mode::Curve, "Curve"),
        (Mode::Hue, "Hue"),
        (Mode::Saturation, "Sat"),
        (Mode::Luminance, "Lum"),
    ];
}

const MODE_ID: &str = "targeted-mode";
const GRAB_ID: &str = "targeted-grab";
/// Hue bands are centred 45 degrees apart starting at red (see `HslParams`); the hue is the
/// renderer's own (`hsl_hue_degrees`), not Oklch hue.
const BAND_SPAN_DEG: f32 = 45.0;
/// A press within this x distance of an existing curve point grabs it instead of adding one.
const GRAB_RADIUS: f32 = 0.04;
/// One pixel of vertical drag is half a step (LRC's `-dy * 0.5`); a step is 1/100 of an HSL
/// slider and 1/255 of the curve's range.
const STEPS_PER_PX: f32 = 0.5;
const HSL_STEP: f32 = 0.01;
const CURVE_STEP: f32 = 1.0 / 255.0;

pub fn mode(ctx: &egui::Context) -> Mode {
    ctx.data(|d| d.get_temp::<Mode>(egui::Id::unique(MODE_ID)))
        .unwrap_or(Mode::Curve)
}

pub fn set_mode(ctx: &egui::Context, mode: Mode) {
    ctx.data_mut(|d| d.insert_temp(egui::Id::unique(MODE_ID), mode));
}

/// Mode buttons, shown under the tool switch while the tool is active.
pub fn mode_picker(ui: &mut egui::Ui) {
    let current = mode(ui.ctx());
    ui.horizontal(|ui| {
        ui.label("Drag edits");
        for (m, label) in Mode::ALL {
            if ui.selectable_label(current == m, label).clicked() {
                set_mode(ui.ctx(), m);
            }
        }
    });
}

/// The HSL band (0 = red .. 7 = magenta) whose centre is nearest `hue_deg`.
pub fn band_for_hue(hue_deg: f32) -> usize {
    ((hue_deg.rem_euclid(360.0) / BAND_SPAN_DEG).round() as usize) % 8
}

/// Where a pixel sits on the point curve's x axis: the renderer indexes the curve by the
/// cube-root of each channel, so the channel mean's cube-root is the master curve's position.
fn curve_x(working: [f32; 3]) -> f32 {
    let mean = (working[0] + working[1] + working[2]) / 3.0;
    mean.max(0.0).cbrt().clamp(0.0, 1.0)
}

/// A vertical drag of `dy` screen pixels (down positive) as a signed number of steps.
fn steps(dy: f32) -> f32 {
    -dy * STEPS_PER_PX
}

/// Moves the master-curve point nearest `x` by `delta_y`, adding one on the curve at `x` when none
/// is within [`GRAB_RADIUS`]. `points` must be sorted and strictly increasing (sanitized).
/// Returns the grabbed point's x, so a drag keeps hold of it.
pub fn nudge_curve(points: &mut Vec<[f32; 2]>, x: f32, delta_y: f32) -> f32 {
    if points.len() < 2 {
        *points = vec![[0.0, 0.0], [1.0, 1.0]];
    }
    let nearest = points
        .iter()
        .enumerate()
        .min_by(|a, b| (a.1[0] - x).abs().total_cmp(&(b.1[0] - x).abs()))
        .map(|(i, p)| (i, (p[0] - x).abs()));
    let i = match nearest {
        Some((i, d)) if d <= GRAB_RADIUS || points.len() >= MAX_CURVE_POINTS => i,
        _ => {
            // Insert on the existing curve's height at x, so the new point starts as a no-op.
            let at = points.partition_point(|p| p[0] < x);
            let y = evaluate(points, x);
            points.insert(at, [x, y]);
            at
        }
    };
    points[i][1] = (points[i][1] + delta_y).clamp(0.0, 1.0);
    let x = points[i][0];
    if points.iter().all(|p| (p[0] - p[1]).abs() < 1e-6) {
        points.clear();
    }
    x
}

/// The curve's height at `x`, as the renderer's monotone spline draws it.
fn evaluate(points: &[[f32; 2]], x: f32) -> f32 {
    let pts: Vec<(f64, f64)> = points
        .iter()
        .map(|p| (f64::from(p[0]), f64::from(p[1])))
        .collect();
    ToneCurve::new(&pts).eval(f64::from(x)) as f32
}

/// Adds `delta` (normalized, so 0.01 is one slider step) to `band`'s hue/sat/lum value.
pub fn nudge_hsl(params: &mut HslParams, band: usize, mode: Mode, delta: f32) {
    let b = &mut params.bands[band.min(7)];
    let v = match mode {
        Mode::Hue => &mut b.hue,
        Mode::Saturation => &mut b.saturation,
        Mode::Luminance => &mut b.luminance,
        Mode::Curve => return,
    };
    *v = (*v + delta).clamp(-1.0, 1.0);
}

/// What the press landed on, kept for the rest of the drag.
#[derive(Clone, Copy)]
enum Grab {
    Curve {
        x: f32,
    },
    Band(usize),
    /// The pressed pixel had nothing to grab (a grey in an HSL mode).
    None,
}

/// Handles the photo area while the tool is active: press picks, vertical drag edits, Esc exits.
pub fn handle_viewport(
    ui: &mut egui::Ui,
    response: &egui::Response,
    rect: egui::Rect,
    develop: &mut DevelopDoc,
    heal: &mut HealUi,
) {
    if response.hovered() {
        ui.output_mut(|o| o.cursor_icon = egui::CursorIcon::ResizeVertical);
    }
    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        heal.tool = Tool::Idle;
        return;
    }
    let grab_id = egui::Id::unique(GRAB_ID);
    let mode = mode(ui.ctx());
    if response.drag_started() {
        let Some(p) = response.interact_pointer_pos() else {
            return;
        };
        let n = develop.display_to_source_norm(to_norm(rect, develop.display_extent(), p));
        let working = thumb_for(ui.ctx(), develop).sample_working(n[0], n[1]);
        let grab = match mode {
            Mode::Curve => {
                let x = curve_x(working);
                if !x.is_finite() {
                    return;
                }
                Grab::Curve { x }
            }
            _ => hsl_hue_degrees(working).map_or(Grab::None, |h| Grab::Band(band_for_hue(h))),
        };
        ui.ctx().data_mut(|d| d.insert_temp(grab_id, grab));
    }
    if !response.dragged() {
        return;
    }
    let dy = response.drag_delta().y;
    if dy == 0.0 {
        return;
    }
    let Some(grab) = ui.ctx().data(|d| d.get_temp::<Grab>(grab_id)) else {
        return;
    };
    match grab {
        Grab::Curve { x } => {
            let mut curves: PointCurveParams = develop.stage_params(POINT_CURVE);
            // The stored document is untrusted: edit the cleaned master list.
            curves.master = curves.sanitized().master;
            let held = nudge_curve(&mut curves.master, x, steps(dy) * CURVE_STEP);
            develop.set_stage_params(POINT_CURVE, &curves);
            // Keep hold of the same point as it moves.
            ui.ctx()
                .data_mut(|d| d.insert_temp(grab_id, Grab::Curve { x: held }));
        }
        Grab::Band(band) => {
            let mut hsl: HslParams = develop.stage_params(HSL);
            nudge_hsl(&mut hsl, band, mode, steps(dy) * HSL_STEP);
            develop.set_stage_params(HSL, &hsl);
        }
        Grab::None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hue_maps_to_the_nearest_band_and_wraps_at_red() {
        assert_eq!(band_for_hue(0.0), 0);
        assert_eq!(band_for_hue(350.0), 0, "just below 360 is still red");
        assert_eq!(band_for_hue(100.0), 2, "yellow-green rounds to yellow");
        assert_eq!(band_for_hue(120.0), 3);
        assert_eq!(band_for_hue(320.0), 7);
        assert_eq!(band_for_hue(-10.0), 0);
    }

    #[test]
    fn primaries_pick_the_band_the_renderer_weights_highest() {
        let band = |rgb| band_for_hue(hsl_hue_degrees(rgb).unwrap());
        assert_eq!(band([0.8, 0.02, 0.02]), 0, "red");
        assert_eq!(
            band([0.8, 0.8, 0.02]),
            1,
            "HSV 60 deg is nearer Orange (45) than Yellow (90)"
        );
        assert_eq!(band([0.8, 0.5, 0.02]), 1, "orange");
        assert_eq!(band([0.02, 0.8, 0.02]), 3, "green");
        assert_eq!(band([0.02, 0.02, 0.8]), 5, "blue");
        assert_eq!(hsl_hue_degrees([0.4, 0.4, 0.4]), None, "grey has no band");
    }

    #[test]
    fn inserting_on_a_three_point_curve_does_not_reshape_it() {
        let mut pts = vec![[0.0, 0.0], [0.5, 0.7], [1.0, 1.0]];
        let before = evaluate(&pts, 0.25);
        nudge_curve(&mut pts, 0.25, 0.0);
        assert_eq!(pts.len(), 4);
        assert!((evaluate(&pts, 0.25) - before).abs() < 1e-5);
        assert!((pts[1][1] - before).abs() < 1e-5);
    }

    #[test]
    fn a_multi_point_curve_dragged_back_onto_the_diagonal_clears() {
        let mut pts = vec![[0.0, 0.0], [0.5, 0.6], [1.0, 1.0]];
        nudge_curve(&mut pts, 0.5, -0.1);
        assert!(pts.is_empty());
    }

    #[test]
    fn curve_x_is_the_cube_root_of_the_pixel() {
        assert!((curve_x([0.125; 3]) - 0.5).abs() < 1e-6);
        assert_eq!(curve_x([-1.0, 0.0, 0.0]), 0.0);
        assert_eq!(curve_x([8.0; 3]), 1.0);
    }

    #[test]
    fn dragging_up_raises_and_dragging_down_lowers() {
        assert!(steps(-4.0) > 0.0 && steps(4.0) < 0.0);
        assert_eq!(steps(-4.0), 2.0);
    }

    #[test]
    fn hsl_nudge_moves_only_the_chosen_band_and_value_and_clamps() {
        let mut p = HslParams::default();
        nudge_hsl(&mut p, 3, Mode::Saturation, 0.25);
        assert_eq!(p.bands[3].saturation, 0.25);
        assert_eq!(p.bands[3].hue, 0.0);
        assert_eq!(p.bands[2].saturation, 0.0);
        nudge_hsl(&mut p, 3, Mode::Saturation, 5.0);
        assert_eq!(p.bands[3].saturation, 1.0);
        nudge_hsl(&mut p, 3, Mode::Curve, 0.5);
        assert_eq!(p.bands[3].saturation, 1.0, "curve mode never touches HSL");
    }

    #[test]
    fn a_first_curve_drag_adds_a_point_on_the_identity_and_moves_it() {
        let mut pts = Vec::new();
        let held = nudge_curve(&mut pts, 0.5, 0.1);
        assert_eq!(held, 0.5);
        assert_eq!(pts, vec![[0.0, 0.0], [0.5, 0.6], [1.0, 1.0]]);
    }

    #[test]
    fn a_press_near_an_existing_point_grabs_it_instead_of_adding_one() {
        let mut pts = vec![[0.0, 0.0], [0.5, 0.5], [1.0, 1.0]];
        let held = nudge_curve(&mut pts, 0.52, -0.1);
        assert_eq!(held, 0.5, "holds the grabbed point's own x");
        assert_eq!(pts.len(), 3);
        assert!((pts[1][1] - 0.4).abs() < 1e-6);
    }

    #[test]
    fn dragging_a_point_back_to_the_diagonal_clears_the_curve() {
        let mut pts = vec![[0.0, 0.0], [1.0, 0.9]];
        nudge_curve(&mut pts, 1.0, 0.1);
        assert!(pts.is_empty(), "identity is stored as nothing");
    }

    #[test]
    fn curve_values_stay_inside_the_unit_square() {
        let mut pts = Vec::new();
        nudge_curve(&mut pts, 0.5, 9.0);
        assert!(pts.iter().all(|p| (0.0..=1.0).contains(&p[1])));
    }
}
