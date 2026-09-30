//! Tile geometry and painting shared by the survey and compare views (#32). The maths is pure
//! (fit a picture into a box, crop a zoomed view, lay N tiles into the biggest grid) so it is
//! tested without a window; `paint_tile` is the thin egui layer over it.

use egui::{Color32, Rect, Vec2};
use nicti_lair::AssetMeta;

use super::badges::paint_marks;

/// The largest rectangle of aspect `tex` (width, height in pixels) that fits inside `bounds`,
/// centred. A degenerate texture yields an empty rect at the centre rather than dividing by zero.
pub fn fit_rect(bounds: Rect, tex: [usize; 2]) -> Rect {
    let (w, h) = (tex[0] as f32, tex[1] as f32);
    if w <= 0.0 || h <= 0.0 || bounds.width() <= 0.0 || bounds.height() <= 0.0 {
        return Rect::from_center_size(bounds.center(), Vec2::ZERO);
    }
    let scale = (bounds.width() / w).min(bounds.height() / h);
    Rect::from_center_size(bounds.center(), Vec2::new(w * scale, h * scale))
}

/// How far a zoomed tile magnifies its picture, relative to fit.
pub const ZOOM: f32 = 3.0;

/// The largest pan (in image-fraction units, either axis) that keeps the zoomed window inside
/// the picture.
pub fn max_pan() -> f32 {
    0.5 - 0.5 / ZOOM
}

/// The UV rectangle a tile samples. Unzoomed: the whole picture. Zoomed: a `1/ZOOM`-wide window
/// centred at `0.5 + pan`, with `pan` clamped so the window never leaves the picture.
pub fn view_uv(zoomed: bool, pan: [f32; 2]) -> Rect {
    if !zoomed {
        return Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
    }
    let half = 0.5 / ZOOM;
    let limit = max_pan();
    let cx = 0.5 + pan[0].clamp(-limit, limit);
    let cy = 0.5 + pan[1].clamp(-limit, limit);
    Rect::from_min_max(
        egui::pos2(cx - half, cy - half),
        egui::pos2(cx + half, cy + half),
    )
}

/// New pan after the pointer moved by `delta` screen points over a picture drawn `shown` points
/// wide/tall. Dragging moves the picture with the pointer, so the window moves the other way.
pub fn pan_after_drag(pan: [f32; 2], delta: Vec2, shown: Vec2) -> [f32; 2] {
    if shown.x <= 0.0 || shown.y <= 0.0 {
        return pan;
    }
    let limit = max_pan();
    [
        (pan[0] - delta.x / (shown.x * ZOOM)).clamp(-limit, limit),
        (pan[1] - delta.y / (shown.y * ZOOM)).clamp(-limit, limit),
    ]
}

/// Columns and rows for `n` tiles in a `w` x `h` area that give the tiles the most room, with each
/// tile wanting aspect `aspect` (width / height). Ties prefer fewer columns.
pub fn tile_grid(n: usize, w: f32, h: f32, aspect: f32) -> (usize, usize) {
    if n == 0 {
        return (0, 0);
    }
    let mut best = (1, n);
    let mut best_area = -1.0f32;
    for cols in 1..=n {
        let rows = n.div_ceil(cols);
        let cell_w = w / cols as f32;
        let cell_h = h / rows as f32;
        // The picture that fits the cell.
        let pic_w = cell_w.min(cell_h * aspect);
        let area = pic_w * (pic_w / aspect);
        if area > best_area + 1e-3 {
            best_area = area;
            best = (cols, rows);
        }
    }
    best
}

/// What a tile draws.
pub struct TileStyle<'a> {
    pub label: Option<&'a str>,
    pub meta: Option<&'a AssetMeta>,
    /// The tile marks apply to.
    pub active: bool,
}

/// Paints one tile: a dark backing, the picture (`uv` crop of `texture`), its badges, an optional
/// caption, and an outline when active. `None` texture draws "no preview".
pub fn paint_tile(
    ui: &egui::Ui,
    rect: Rect,
    texture: Option<&egui::TextureHandle>,
    uv: Rect,
    style: &TileStyle<'_>,
) -> Rect {
    let painter = ui.painter_at(rect);
    let visuals = ui.visuals();
    painter.rect_filled(rect, 2.0, visuals.extreme_bg_color);
    let inner = rect.shrink(4.0);

    let picture = match texture {
        Some(t) => {
            let img = fit_rect(inner, t.size());
            painter.image(t.id(), img, uv, Color32::WHITE);
            img
        }
        None => {
            painter.text(
                inner.center(),
                egui::Align2::CENTER_CENTER,
                "no preview",
                egui::FontId::proportional(13.0),
                visuals.weak_text_color(),
            );
            inner
        }
    };
    if let Some(meta) = style.meta {
        paint_marks(&painter, picture, meta);
    }
    if let Some(label) = style.label {
        painter.text(
            inner.left_top() + Vec2::new(6.0, 4.0),
            egui::Align2::LEFT_TOP,
            label,
            egui::FontId::proportional(13.0),
            Color32::from_white_alpha(220),
        );
    }
    if style.active {
        painter.rect_stroke(
            rect,
            2.0,
            egui::Stroke::new(2.5, visuals.selection.stroke.color),
            egui::StrokeKind::Inside,
        );
    }
    picture
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::pos2;

    fn rect(w: f32, h: f32) -> Rect {
        Rect::from_min_size(pos2(0.0, 0.0), Vec2::new(w, h))
    }

    #[test]
    fn a_picture_fits_by_its_limiting_side_and_is_centred() {
        // A 3:2 picture in a 300x300 box is width-limited.
        let r = fit_rect(rect(300.0, 300.0), [600, 400]);
        assert_eq!((r.width(), r.height()), (300.0, 200.0));
        assert_eq!(r.center(), pos2(150.0, 150.0));
        // A portrait in a wide box is height-limited.
        let r = fit_rect(rect(900.0, 300.0), [400, 600]);
        assert_eq!((r.width().round(), r.height()), (200.0, 300.0));
    }

    #[test]
    fn a_degenerate_texture_or_box_never_divides_by_zero() {
        assert_eq!(fit_rect(rect(100.0, 100.0), [0, 10]).size(), Vec2::ZERO);
        assert_eq!(fit_rect(rect(0.0, 100.0), [10, 10]).size(), Vec2::ZERO);
    }

    #[test]
    fn unzoomed_shows_the_whole_picture() {
        let uv = view_uv(false, [0.4, -0.4]);
        assert_eq!((uv.min, uv.max), (pos2(0.0, 0.0), pos2(1.0, 1.0)));
    }

    #[test]
    fn zoomed_shows_a_centred_window_and_clamps_the_pan_inside_the_picture() {
        let centred = view_uv(true, [0.0, 0.0]);
        assert!((centred.width() - 1.0 / ZOOM).abs() < 1e-6);
        assert!((centred.center().x - 0.5).abs() < 1e-6);
        // An absurd pan is clamped so the window's edge sits exactly on the picture's edge.
        let far = view_uv(true, [9.0, -9.0]);
        assert!((far.max.x - 1.0).abs() < 1e-5);
        assert!(far.min.y.abs() < 1e-5);
        assert!(far.min.x >= 0.0 && far.max.y <= 1.0);
    }

    #[test]
    fn dragging_moves_the_picture_with_the_pointer_and_stops_at_the_edge() {
        let shown = Vec2::new(300.0, 200.0);
        // Pointer right -> picture right -> the window moves left (negative pan).
        let p = pan_after_drag([0.0, 0.0], Vec2::new(30.0, 0.0), shown);
        assert!(p[0] < 0.0 && p[1] == 0.0);
        // A huge drag pins at the limit instead of leaving the picture.
        let p = pan_after_drag([0.0, 0.0], Vec2::new(-1e6, 1e6), shown);
        assert!((p[0] - max_pan()).abs() < 1e-6 && (p[1] + max_pan()).abs() < 1e-6);
        // A zero-size picture ignores the drag.
        assert_eq!(
            pan_after_drag([0.1, 0.2], Vec2::new(5.0, 5.0), Vec2::ZERO),
            [0.1, 0.2]
        );
    }

    #[test]
    fn the_survey_grid_uses_the_layout_that_gives_pictures_the_most_room() {
        // A wide window fits four 3:2 photos side by side better than a 2x2.
        assert_eq!(tile_grid(4, 1600.0, 300.0, 1.5), (4, 1));
        // A squarer one prefers 2x2.
        assert_eq!(tile_grid(4, 900.0, 700.0, 1.5), (2, 2));
        assert_eq!(tile_grid(1, 900.0, 700.0, 1.5), (1, 1));
        assert_eq!(tile_grid(0, 900.0, 700.0, 1.5), (0, 0));
        // Every layout has room for every tile.
        for n in 1..=16 {
            let (c, r) = tile_grid(n, 1200.0, 800.0, 1.5);
            assert!(c * r >= n, "{n} tiles in {c}x{r}");
        }
    }
}
