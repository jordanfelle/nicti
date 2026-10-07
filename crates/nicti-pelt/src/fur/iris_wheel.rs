//! The Color Grading hue/saturation wheel (#432): a painted colour disc with a draggable handle.
//!
//! Adapted from storytold/lightcraft@265248c `crates/ui-egui/src/panels/edit.rs` (`grading`,
//! `paint_wheel`), Copyright (c) 2026 ArtCraft Team and the LightCraft contributors, MIT OR
//! Apache-2.0 (see `docs/licensing.md`). Changes: edits one `coat::GradeWheel` (hue in degrees,
//! sat 0..1) instead of LightCraft's 0..100 command-bus fields; the angle/radius maths is pure and
//! unit-tested; the painted hue is the same HSV hue `nicti_tapetum::oklab::wheel_direction` turns
//! into an OkLab direction, so the colour you pick is the colour the grade pushes toward.

use egui::epaint::{Mesh, Vertex};
use egui::{pos2, vec2, Color32, Sense, Shape, Stroke, Ui};
use nicti_tapetum::coat::GradeWheel;

use super::tokens::Tokens;

/// Triangle-fan segments the disc is painted with (the GPU interpolates between rim vertices).
const SEGMENTS: usize = 48;

/// What the user did to the wheel this frame.
#[derive(Debug, Default, Clone, Copy)]
pub struct WheelEdit {
    pub changed: bool,
    pub drag_started: bool,
    pub drag_stopped: bool,
}

/// Hue (degrees, 0 = +x/red, counter-clockwise on screen) and saturation (0..1, clamped) for a
/// pointer `(dx, dy_up)` away from the wheel's centre, on a wheel of `radius`.
pub fn hue_sat_from_offset(dx: f32, dy_up: f32, radius: f32) -> (f32, f32) {
    let hue = dy_up.atan2(dx).to_degrees().rem_euclid(360.0);
    let sat = (dx.hypot(dy_up) / radius.max(1e-3)).clamp(0.0, 1.0);
    (hue, sat)
}

/// The handle offset `(dx, dy_up)` for a hue/sat on a wheel of `radius` (inverse of
/// [`hue_sat_from_offset`] inside the disc).
pub fn offset_from_hue_sat(hue: f32, sat: f32, radius: f32) -> (f32, f32) {
    let a = hue.to_radians();
    (a.cos() * sat * radius, a.sin() * sat * radius)
}

/// HSV (hue degrees, s, v in 0..1) to an sRGB colour.
fn hsv(hue: f32, s: f32, v: f32) -> Color32 {
    let h = hue.rem_euclid(360.0) / 60.0;
    let x = v * s * (1.0 - (h % 2.0 - 1.0).abs());
    let c = v * s;
    let m = v - c;
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let q = |f: f32| ((f + m).clamp(0.0, 1.0) * 255.0).round() as u8;
    Color32::from_rgb(q(r), q(g), q(b))
}

/// Draws one wheel of `radius` (points) and edits `wheel`'s hue and saturation. Drag or click sets
/// them; double-click resets both to 0. `label` names the widget for screen readers/tests.
pub fn show(ui: &mut Ui, label: &str, radius: f32, wheel: &mut GradeWheel) -> WheelEdit {
    let t = Tokens::get(ui.ctx());
    let (rect, response) = ui.allocate_exact_size(
        vec2(radius * 2.0 + 8.0, radius * 2.0 + 8.0),
        Sense::click_and_drag(),
    );
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Other, true, format!("{label} wheel"))
    });
    let center = rect.center();
    let mut edit = WheelEdit::default();

    if response.double_clicked() {
        wheel.hue = 0.0;
        wheel.sat = 0.0;
        edit.changed = true;
    } else if response.dragged() || response.clicked() {
        if let Some(p) = response.interact_pointer_pos() {
            let (hue, sat) = hue_sat_from_offset(p.x - center.x, center.y - p.y, radius);
            wheel.hue = hue;
            wheel.sat = sat;
            edit.changed = true;
        }
    }
    edit.drag_started = response.drag_started();
    edit.drag_stopped = response.drag_stopped();

    let painter = ui.painter_at(rect);
    let mut mesh = Mesh::default();
    mesh.vertices.push(Vertex {
        pos: center,
        uv: egui::epaint::WHITE_UV,
        color: Color32::from_gray(128),
    });
    for i in 0..=SEGMENTS {
        let a = i as f32 / SEGMENTS as f32 * std::f32::consts::TAU;
        mesh.vertices.push(Vertex {
            pos: pos2(center.x + a.cos() * radius, center.y - a.sin() * radius),
            uv: egui::epaint::WHITE_UV,
            color: hsv(a.to_degrees(), 0.75, 0.8),
        });
    }
    for i in 0..SEGMENTS as u32 {
        mesh.indices.extend([0, i + 1, i + 2]);
    }
    painter.add(Shape::mesh(mesh));
    painter.circle_stroke(center, radius, Stroke::new(1.0, t.field_border));
    let (dx, dy_up) = offset_from_hue_sat(wheel.hue, wheel.sat, radius);
    let handle = pos2(center.x + dx, center.y - dy_up);
    painter.circle(
        handle,
        4.5,
        Color32::WHITE,
        Stroke::new(1.5, Color32::BLACK),
    );
    edit
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets_round_trip_through_hue_and_saturation() {
        for (hue, sat) in [(0.0, 1.0), (90.0, 0.5), (200.0, 0.25), (359.0, 0.9)] {
            let (dx, dy) = offset_from_hue_sat(hue, sat, 40.0);
            let (h, s) = hue_sat_from_offset(dx, dy, 40.0);
            assert!(
                (h - hue).abs() < 1e-3 && (s - sat).abs() < 1e-4,
                "{hue},{sat}"
            );
        }
    }

    #[test]
    fn dragging_outside_the_disc_clamps_saturation_but_keeps_the_hue() {
        let (h, s) = hue_sat_from_offset(0.0, 500.0, 40.0);
        assert!((h - 90.0).abs() < 1e-3 && s == 1.0);
    }

    #[test]
    fn the_cardinal_hues_match_the_paint() {
        // The painted rim at 0/120/240 degrees is red/green/blue-dominant.
        let (r, g, b) = (
            hsv(0.0, 1.0, 1.0),
            hsv(120.0, 1.0, 1.0),
            hsv(240.0, 1.0, 1.0),
        );
        assert!(r.r() == 255 && r.g() == 0 && r.b() == 0);
        assert!(g.g() == 255 && g.r() == 0);
        assert!(b.b() == 255 && b.r() == 0);
    }
}
