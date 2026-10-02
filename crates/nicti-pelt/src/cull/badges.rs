//! Marker badges (#32): how a photo's rating, pick/reject and label are drawn over a thumbnail.
//! Shared by the library grid's cells and the survey/compare tiles, so a mark looks the same
//! everywhere it can be made.

use egui::{Color32, Rect, Vec2};
use nicti_lair::AssetMeta;

use super::keys::{is_picked, is_rejected, Label};

/// Draws a photo's markers over its cell: a dimmed cell and red cross for a reject, stars and a
/// pick flag along the bottom, a colour strip for its label. Glyphs are the ones egui's bundled
/// emoji-icon-font has (★ ⚑ ✖) -- `Ubuntu-Light`/`Hack` have none of them.
pub fn paint_marks(painter: &egui::Painter, inner: Rect, meta: &AssetMeta) {
    if is_rejected(meta) {
        painter.rect_filled(inner, 2.0, Color32::from_black_alpha(140));
        painter.text(
            inner.right_top() + Vec2::new(-6.0, 4.0),
            egui::Align2::RIGHT_TOP,
            "\u{2716}",
            egui::FontId::proportional(18.0),
            Color32::from_rgb(0xe0, 0x4a, 0x4a),
        );
    }

    let font = egui::FontId::proportional(13.0);
    let mut x = inner.left() + 5.0;
    let y = inner.bottom() - 7.0;
    if let Some(stars) = meta.rating.filter(|r| (1..=5).contains(r)) {
        let text = "\u{2605}".repeat(stars as usize);
        let galley =
            painter.layout_no_wrap(text, font.clone(), Color32::from_rgb(0xf2, 0xc9, 0x3b));
        let backing = Rect::from_min_size(
            egui::pos2(x - 2.0, y - galley.size().y - 1.0),
            galley.size() + Vec2::new(4.0, 2.0),
        );
        painter.rect_filled(backing, 3.0, Color32::from_black_alpha(150));
        painter.galley(
            egui::pos2(x, y - galley.size().y),
            galley.clone(),
            Color32::WHITE,
        );
        x += galley.size().x + 8.0;
    }
    if is_picked(meta) {
        let galley = painter.layout_no_wrap(
            "\u{2691}".to_string(),
            font,
            Color32::from_rgb(0x5c, 0xd0, 0x6a),
        );
        let backing = Rect::from_min_size(
            egui::pos2(x - 2.0, y - galley.size().y - 1.0),
            galley.size() + Vec2::new(4.0, 2.0),
        );
        painter.rect_filled(backing, 3.0, Color32::from_black_alpha(150));
        painter.galley(egui::pos2(x, y - galley.size().y), galley, Color32::WHITE);
    }

    if let Some(label) = meta.label.as_deref().and_then(Label::from_name) {
        let strip = Rect::from_min_max(
            egui::pos2(inner.left(), inner.bottom() - 4.0),
            inner.right_bottom(),
        );
        painter.rect_filled(strip, 0.0, label.color());
    }
}

/// A small dot at the cell's top-left for a preview that doesn't match the photo's edits (#145):
/// amber = camera version while the render is made, blue = an older render being updated, grey
/// ring = the render lacks local adjustments. Drawn as shapes, not a glyph, so it needs no font.
pub fn paint_stale(painter: &egui::Painter, inner: Rect, badge: crate::eyeshine::Badge) {
    use crate::eyeshine::Badge;
    let center = inner.left_top() + Vec2::new(10.0, 10.0);
    let (fill, ring) = match badge {
        Badge::Stale => (Some(Color32::from_rgb(0xf2, 0xa3, 0x3b)), Color32::WHITE),
        Badge::Updating => (Some(Color32::from_rgb(0x4a, 0x9b, 0xe0)), Color32::WHITE),
        Badge::Partial => (None, Color32::from_rgb(0xc8, 0xc8, 0xc8)),
        Badge::CameraRendering | Badge::None => return,
    };
    painter.circle_filled(center, 8.0, Color32::from_black_alpha(150));
    match fill {
        Some(c) => {
            painter.circle_filled(center, 4.5, c);
        }
        None => {
            painter.circle_stroke(center, 4.5, egui::Stroke::new(1.5, ring));
        }
    }
}

/// A photo's markers as inline text, for a header row (the loupe): stars, pick/reject, label.
/// Draws nothing while the markers are still being read (`None`), never a guess.
pub fn show_marks_inline(ui: &mut egui::Ui, meta: Option<&AssetMeta>) {
    let Some(meta) = meta else {
        return;
    };
    if is_rejected(meta) {
        ui.colored_label(Color32::from_rgb(0xe0, 0x4a, 0x4a), "\u{2716} Rejected");
    }
    if let Some(stars) = meta.rating.filter(|r| (1..=5).contains(r)) {
        ui.colored_label(
            Color32::from_rgb(0xf2, 0xc9, 0x3b),
            "\u{2605}".repeat(stars as usize),
        );
    }
    if is_picked(meta) {
        ui.colored_label(Color32::from_rgb(0x5c, 0xd0, 0x6a), "\u{2691} Picked");
    }
    if let Some(label) = meta.label.as_deref() {
        match Label::from_name(label) {
            Some(l) => ui.colored_label(l.color(), label),
            None => ui.label(label),
        };
    }
}
