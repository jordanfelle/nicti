//! The point-curve editor (#432): a square graph where control points are dragged, added and
//! removed, drawing the Fritsch-Carlson curve that `nicti.point_curve` will apply.
//!
//! Interaction adapted from storytold/lightcraft@265248c `crates/ui-egui/src/panels/edit.rs`
//! (`curve_editor`), Copyright (c) 2026 ArtCraft Team and the LightCraft contributors, MIT OR
//! Apache-2.0 (see `docs/licensing.md`). Changes: the editor edits a plain `Vec<[f32; 2]>` rather
//! than going through LightCraft's command bus; the curve is drawn with the same
//! `nicti_calico::tonecurve::ToneCurve` the pipeline uses so the picture is the maths; endpoints
//! keep their x; the pure point operations are separated out and unit-tested.
//!
//! An empty list is the identity (as in `PointCurveParams`). The first edit materializes the two
//! endpoints so there is something to drag.

use egui::{pos2, vec2, Color32, CornerRadius, Sense, Stroke, StrokeKind, Ui};
use nicti_calico::tonecurve::ToneCurve;
use nicti_tapetum::coat::MAX_CURVE_POINTS;

use super::tokens::Tokens;

/// Radius (points) within which a press grabs an existing control point.
const HIT_RADIUS: f32 = 10.0;
/// Closest two control points may sit in x.
const MIN_GAP: f32 = 0.01;
/// Segments the curve is drawn with.
const CURVE_SEGMENTS: usize = 96;

/// The identity diagonal's two endpoints.
const DIAGONAL: [[f32; 2]; 2] = [[0.0, 0.0], [1.0, 1.0]];

/// Gives an empty (identity) list its two endpoints so it can be edited.
pub fn materialize(points: &mut Vec<[f32; 2]>) {
    if points.len() < 2 {
        *points = DIAGONAL.to_vec();
    }
}

/// The index of the control point nearest `p` (graph coordinates, 0..1, y up) within `radius_px`
/// on a graph `size_px` wide and tall, if any.
pub fn nearest_point(
    points: &[[f32; 2]],
    p: [f32; 2],
    size_px: [f32; 2],
    radius_px: f32,
) -> Option<usize> {
    points
        .iter()
        .enumerate()
        .map(|(i, q)| {
            let d = ((q[0] - p[0]) * size_px[0]).hypot((q[1] - p[1]) * size_px[1]);
            (i, d)
        })
        .filter(|&(_, d)| d <= radius_px)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(i, _)| i)
}

/// Inserts a point at `(x, y)` keeping the list sorted by x and returns its index, or `None` if it
/// would sit within [`MIN_GAP`] of a neighbour or the list is full.
pub fn insert_point(points: &mut Vec<[f32; 2]>, x: f32, y: f32) -> Option<usize> {
    materialize(points);
    if points.len() >= MAX_CURVE_POINTS {
        return None;
    }
    let x = x.clamp(0.0, 1.0);
    if points.iter().any(|p| (p[0] - x).abs() < MIN_GAP) {
        return None;
    }
    let at = points.partition_point(|p| p[0] < x);
    points.insert(at, [x, y.clamp(0.0, 1.0)]);
    Some(at)
}

/// Moves point `index` to `(x, y)`: y is clamped to 0..1, x stays between the neighbours (kept
/// [`MIN_GAP`] clear of them), and an endpoint keeps its x.
pub fn move_point(points: &mut [[f32; 2]], index: usize, x: f32, y: f32) {
    let n = points.len();
    if index >= n {
        return;
    }
    let y = y.clamp(0.0, 1.0);
    if index == 0 || index == n - 1 {
        points[index][1] = y;
        return;
    }
    let lo = points[index - 1][0] + MIN_GAP;
    let hi = points[index + 1][0] - MIN_GAP;
    points[index] = [x.clamp(lo.min(hi), hi.max(lo)), y];
}

/// Removes an interior point. Endpoints can't be removed; returns whether anything was removed.
pub fn remove_point(points: &mut Vec<[f32; 2]>, index: usize) -> bool {
    if index == 0 || index + 1 >= points.len() {
        return false;
    }
    points.remove(index);
    true
}

/// What the user did to the curve this frame.
#[derive(Debug, Default, Clone, Copy)]
pub struct CurveEdit {
    pub changed: bool,
    pub drag_started: bool,
    pub drag_stopped: bool,
}

/// Draws and edits one channel's curve. `id` keys the drag state, `color` is the curve colour and
/// `label` names the widget for screen readers/tests.
pub fn show(
    ui: &mut Ui,
    id: egui::Id,
    label: &str,
    color: Color32,
    points: &mut Vec<[f32; 2]>,
) -> CurveEdit {
    let t = Tokens::get(ui.ctx());
    let side = ui.available_width().clamp(120.0, 260.0);
    let (rect, response) = ui.allocate_exact_size(vec2(side, side), Sense::click_and_drag());
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Other, true, format!("{label} curve"))
    });
    let size = [rect.width(), rect.height()];
    let to_graph = |p: egui::Pos2| -> [f32; 2] {
        [
            ((p.x - rect.left()) / rect.width()).clamp(0.0, 1.0),
            (1.0 - (p.y - rect.top()) / rect.height()).clamp(0.0, 1.0),
        ]
    };
    let to_screen =
        |g: [f32; 2]| pos2(rect.left() + g[0] * size[0], rect.bottom() - g[1] * size[1]);
    let drag_id = id.with("curve-drag");
    let mut edit = CurveEdit::default();

    // Press-drag: grab a point, or make one under the pointer and drag that.
    if response.drag_started() {
        // egui reports drag_started after the pointer has crossed the drag threshold, so anchor
        // the grab at the press position.
        if let Some(press) = ui.input(|i| i.pointer.press_origin()) {
            let g = to_graph(press);
            let grabbed = nearest_point(points, g, size, HIT_RADIUS).or_else(|| {
                let at = insert_point(points, g[0], g[1]);
                edit.changed |= at.is_some();
                at
            });
            ui.data_mut(|d| d.insert_temp(drag_id, grabbed));
            edit.drag_started = true;
        }
    }
    let dragging: Option<usize> = ui.data(|d| d.get_temp::<Option<usize>>(drag_id)).flatten();
    if response.dragged() {
        if let (Some(i), Some(p)) = (dragging, response.interact_pointer_pos()) {
            let g = to_graph(p);
            materialize(points);
            move_point(points, i, g[0], g[1]);
            edit.changed = true;
        }
    }
    if response.drag_stopped() {
        ui.data_mut(|d| d.insert_temp::<Option<usize>>(drag_id, None));
        edit.drag_stopped = true;
    }
    // Click on empty graph adds a point; double-click on a point deletes it.
    if let Some(p) = response.interact_pointer_pos() {
        let g = to_graph(p);
        if response.double_clicked() {
            if let Some(i) = nearest_point(points, g, size, HIT_RADIUS) {
                edit.changed |= remove_point(points, i);
            }
        } else if response.clicked() && nearest_point(points, g, size, HIT_RADIUS).is_none() {
            edit.changed |= insert_point(points, g[0], g[1]).is_some();
        }
    }
    response.context_menu(|ui| {
        if ui.button("Reset this curve").clicked() {
            points.clear();
            edit.changed = true;
            ui.close();
        }
    });

    // Paint.
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, CornerRadius::same(2), t.canvas);
    for k in 1..4 {
        let f = k as f32 / 4.0;
        let grid = Stroke::new(1.0, Color32::from_gray(52));
        painter.line_segment(
            [
                pos2(rect.left() + f * size[0], rect.top()),
                pos2(rect.left() + f * size[0], rect.bottom()),
            ],
            grid,
        );
        painter.line_segment(
            [
                pos2(rect.left(), rect.top() + f * size[1]),
                pos2(rect.right(), rect.top() + f * size[1]),
            ],
            grid,
        );
    }
    painter.line_segment(
        [to_screen([0.0, 0.0]), to_screen([1.0, 1.0])],
        Stroke::new(1.0, Color32::from_gray(70)),
    );
    if points.len() >= 2 {
        let pts: Vec<(f64, f64)> = points
            .iter()
            .map(|p| (f64::from(p[0]), f64::from(p[1])))
            .collect();
        // Strictly increasing x is this editor's invariant, but a hand-edited document may not
        // honour it; draw nothing rather than panic in `ToneCurve::new`.
        if pts.windows(2).all(|w| w[1].0 > w[0].0) {
            let curve = ToneCurve::new(&pts);
            let line: Vec<egui::Pos2> = (0..=CURVE_SEGMENTS)
                .map(|i| {
                    let x = i as f64 / CURVE_SEGMENTS as f64;
                    to_screen([x as f32, curve.eval(x).clamp(0.0, 1.0) as f32])
                })
                .collect();
            painter.add(egui::Shape::line(line, Stroke::new(2.0, color)));
        }
        let hovered = response
            .hover_pos()
            .and_then(|p| nearest_point(points, to_graph(p), size, HIT_RADIUS));
        for (i, p) in points.iter().enumerate() {
            let active = Some(i) == dragging || Some(i) == hovered;
            painter.circle(
                to_screen(*p),
                if active { 5.5 } else { 4.0 },
                if active { Color32::WHITE } else { t.canvas },
                Stroke::new(1.5, color),
            );
        }
        if let Some(i) = dragging.filter(|&i| i < points.len()) {
            let p = points[i];
            painter.text(
                rect.left_top() + vec2(6.0, 4.0),
                egui::Align2::LEFT_TOP,
                format!("{} / {}", (p[0] * 255.0).round(), (p[1] * 255.0).round()),
                t.font(11.0),
                t.text_label,
            );
        }
    }
    painter.rect_stroke(
        rect,
        CornerRadius::same(2),
        Stroke::new(1.0, t.field_border),
        StrokeKind::Inside,
    );
    if response.hovered() || dragging.is_some() {
        ui.output_mut(|o| {
            o.cursor_icon = if dragging.is_some() {
                egui::CursorIcon::Grabbing
            } else {
                egui::CursorIcon::Crosshair
            }
        });
    }
    edit
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_edit_materializes_the_endpoints() {
        let mut p = Vec::new();
        assert_eq!(insert_point(&mut p, 0.5, 0.7), Some(1));
        assert_eq!(p, vec![[0.0, 0.0], [0.5, 0.7], [1.0, 1.0]]);
    }

    #[test]
    fn insert_keeps_x_sorted_and_refuses_crowding_and_overflow() {
        let mut p = vec![[0.0, 0.0], [1.0, 1.0]];
        insert_point(&mut p, 0.7, 0.5);
        insert_point(&mut p, 0.3, 0.2);
        assert_eq!(
            p.iter().map(|q| q[0]).collect::<Vec<_>>(),
            [0.0, 0.3, 0.7, 1.0]
        );
        assert_eq!(
            insert_point(&mut p, 0.305, 0.9),
            None,
            "within MIN_GAP of 0.3"
        );
        let mut full: Vec<[f32; 2]> = (0..MAX_CURVE_POINTS)
            .map(|i| [i as f32 / (MAX_CURVE_POINTS - 1) as f32, 0.5])
            .collect();
        assert_eq!(insert_point(&mut full, 0.0123, 0.5), None);
    }

    #[test]
    fn moving_a_point_stays_between_its_neighbours() {
        let mut p = vec![[0.0, 0.0], [0.4, 0.4], [0.8, 0.8], [1.0, 1.0]];
        move_point(&mut p, 1, 0.95, 0.4);
        assert!(
            p[1][0] < p[2][0] - MIN_GAP + 1e-6 && p[1][0] > 0.7,
            "{:?}",
            p[1]
        );
        move_point(&mut p, 1, -3.0, 2.0);
        assert!(p[1][0] > p[0][0] && p[1][1] == 1.0);
    }

    #[test]
    fn endpoints_keep_their_x_and_cannot_be_removed() {
        let mut p = vec![[0.0, 0.0], [0.5, 0.5], [1.0, 1.0]];
        move_point(&mut p, 0, 0.4, 0.3);
        move_point(&mut p, 2, 0.6, 0.9);
        assert_eq!((p[0], p[2]), ([0.0, 0.3], [1.0, 0.9]));
        assert!(!remove_point(&mut p, 0));
        assert!(!remove_point(&mut p, 2));
        assert!(remove_point(&mut p, 1));
        assert_eq!(p.len(), 2);
    }

    #[test]
    fn nearest_point_uses_pixel_distance_not_graph_distance() {
        let p = [[0.0, 0.0], [0.5, 0.5], [1.0, 1.0]];
        // 4 px right of the middle point on a 200 px graph is within a 10 px radius...
        assert_eq!(
            nearest_point(&p, [0.52, 0.5], [200.0, 200.0], 10.0),
            Some(1)
        );
        // ...but the same graph distance on a 2000 px graph is not.
        assert_eq!(nearest_point(&p, [0.52, 0.5], [2000.0, 2000.0], 10.0), None);
    }
}
