//! Design tokens (colours, metrics, fonts) and the app-wide egui theme.
//!
//! Adapted from storytold/lightcraft@265248c `crates/ui-egui/src/theme.rs`, Copyright (c) 2026
//! ArtCraft Team and the LightCraft contributors, MIT OR Apache-2.0 (see `docs/licensing.md`).
//! Changes: dropped the Japanese craft-fonts fallback and the `lightcraft_engine` dependency;
//! Inter is bundled from the upstream rsms/inter 4.1 release (SIL OFL 1.1).

use std::sync::Arc;

use egui::{Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Stroke, Visuals};

pub const FONT_SEMIBOLD: &str = "semibold";

#[derive(Clone, Copy, Debug)]
pub struct Tokens {
    /// Side panels and bars.
    pub chrome: Color32,
    /// Photo canvas.
    pub canvas: Color32,
    pub divider: Color32,
    pub inset: Color32,
    pub field: Color32,
    pub field_border: Color32,
    pub button: Color32,
    pub button_border: Color32,
    pub hover: Color32,
    pub pressed: Color32,
    pub tool_active: Color32,
    pub text: Color32,
    pub text_label: Color32,
    pub text_dim: Color32,
    pub text_disabled: Color32,
    /// Icon glyphs in the chrome.
    pub icon: Color32,
    pub track: Color32,
    pub thumb: Color32,
    pub thumb_hover: Color32,
    pub accent: Color32,
    pub pick: Color32,
    // metrics (points)
    pub slider_row_h: f32,
    pub section_h: f32,
}

impl Default for Tokens {
    fn default() -> Self {
        Tokens {
            chrome: Color32::from_rgb(0x2d, 0x2d, 0x2d),
            canvas: Color32::from_rgb(0x1c, 0x1c, 0x1c),
            divider: Color32::from_rgb(0x1c, 0x1c, 0x1c),
            inset: Color32::from_rgb(0x23, 0x23, 0x23),
            field: Color32::from_rgb(0x22, 0x22, 0x22),
            field_border: Color32::from_rgb(0x3c, 0x3c, 0x3c),
            button: Color32::from_rgb(0x24, 0x24, 0x24),
            button_border: Color32::from_rgb(0x4c, 0x4c, 0x4c),
            hover: Color32::from_rgb(0x3a, 0x3a, 0x3a),
            pressed: Color32::from_rgb(0x3f, 0x3f, 0x3f),
            tool_active: Color32::from_rgb(0x3f, 0x3f, 0x3f),
            text: Color32::from_rgb(0xe2, 0xe2, 0xe2),
            text_label: Color32::from_rgb(0xbc, 0xbc, 0xbc),
            text_dim: Color32::from_rgb(0x8e, 0x8e, 0x8e),
            text_disabled: Color32::from_rgb(0x5c, 0x5c, 0x5c),
            icon: Color32::from_rgb(0x9a, 0x9a, 0x9a),
            track: Color32::from_rgb(0x5a, 0x5a, 0x5a),
            thumb: Color32::from_rgb(0xa0, 0xa0, 0xa0),
            thumb_hover: Color32::from_rgb(0xe0, 0xe0, 0xe0),
            accent: Color32::from_rgb(0x01, 0x65, 0xdd),
            pick: Color32::from_rgb(0xf0, 0xf0, 0xf0),
            slider_row_h: 45.0,
            section_h: 52.0,
        }
    }
}

impl Tokens {
    /// The tokens [`apply`] installed on `ctx`, or the defaults if it hasn't run (tests).
    pub fn get(ctx: &egui::Context) -> Tokens {
        ctx.data(|d| d.get_temp::<Tokens>(egui::Id::NULL))
            .unwrap_or_default()
    }

    pub fn font(&self, size: f32) -> FontId {
        FontId::proportional(size)
    }

    pub fn semibold(&self, size: f32) -> FontId {
        FontId::new(size, FontFamily::Name(FONT_SEMIBOLD.into()))
    }
}

/// Inter for Latin text with egui's bundled fonts behind it, so symbols and emoji the remaining
/// glyph-based UI still uses keep rendering.
pub fn install_fonts(ctx: &egui::Context) {
    ctx.set_fonts(font_definitions());
}

fn font_definitions() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        "Inter".into(),
        Arc::new(FontData::from_static(include_bytes!(
            "../../assets/fonts/Inter-Regular.ttf"
        ))),
    );
    fonts.font_data.insert(
        "Inter-SemiBold".into(),
        Arc::new(FontData::from_static(include_bytes!(
            "../../assets/fonts/Inter-SemiBold.ttf"
        ))),
    );
    let defaults: Vec<String> = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let mut prop = vec!["Inter".to_string()];
    prop.extend(defaults.iter().cloned());
    fonts.families.insert(FontFamily::Proportional, prop);
    let mut semi = vec!["Inter-SemiBold".to_string()];
    semi.extend(defaults);
    fonts
        .families
        .insert(FontFamily::Name(FONT_SEMIBOLD.into()), semi);
    fonts
}

/// Installs the tokens, visuals and text styles app-wide.
pub fn apply(ctx: &egui::Context) {
    let t = Tokens::default();
    ctx.data_mut(|d| d.insert_temp(egui::Id::NULL, t));
    let mut v = Visuals::dark();
    v.panel_fill = t.chrome;
    v.window_fill = t.chrome;
    v.extreme_bg_color = t.field;
    v.faint_bg_color = t.inset;
    v.window_stroke = Stroke::new(1.0, t.button_border);
    v.window_corner_radius = CornerRadius::same(6);
    v.menu_corner_radius = CornerRadius::same(6);
    v.selection.bg_fill = t.accent.gamma_multiply(0.6);
    v.selection.stroke = Stroke::new(1.0, t.accent);
    v.override_text_color = Some(t.text_label);
    v.popup_shadow = egui::epaint::Shadow {
        offset: [0, 4],
        blur: 16,
        spread: 0,
        color: Color32::from_black_alpha(140),
    };
    v.window_shadow = egui::epaint::Shadow {
        offset: [0, 8],
        blur: 30,
        spread: 0,
        color: Color32::from_black_alpha(160),
    };
    let w = &mut v.widgets;
    for (wv, fill) in [
        (&mut w.noninteractive, t.chrome),
        (&mut w.inactive, t.button),
        (&mut w.hovered, t.hover),
        (&mut w.active, t.pressed),
        (&mut w.open, t.hover),
    ] {
        wv.bg_fill = fill;
        wv.weak_bg_fill = fill;
        wv.corner_radius = CornerRadius::same(4);
        wv.fg_stroke = Stroke::new(1.0, t.text_label);
    }
    w.noninteractive.bg_stroke = Stroke::new(1.0, t.divider);
    w.inactive.bg_stroke = Stroke::new(1.0, t.button_border);
    ctx.set_visuals(v);
    ctx.global_style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(6.0, 4.0);
        s.spacing.button_padding = egui::vec2(8.0, 3.0);
        s.spacing.interact_size.y = 22.0;
        s.text_styles
            .insert(egui::TextStyle::Body, FontId::proportional(13.0));
        s.text_styles
            .insert(egui::TextStyle::Button, FontId::proportional(13.0));
        s.text_styles
            .insert(egui::TextStyle::Small, FontId::proportional(11.0));
        s.text_styles.insert(
            egui::TextStyle::Heading,
            FontId::new(16.0, FontFamily::Name(FONT_SEMIBOLD.into())),
        );
        s.animation_time = 0.08;
    });
}

/// Shared by the `fur` tests: a context with the app's fonts and theme, and a frame runner that
/// discards the texture upload (egui panics if a `TexturesDelta` is dropped unapplied).
#[cfg(test)]
pub(super) mod testing {
    pub fn themed_ctx() -> egui::Context {
        let ctx = egui::Context::default();
        super::install_fonts(&ctx);
        super::apply(&ctx);
        ctx
    }

    pub fn run_frame(ctx: &egui::Context, add_contents: impl FnMut(&mut egui::Ui)) {
        let mut out = ctx.run_ui(egui::RawInput::default(), add_contents);
        out.textures_delta.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{run_frame, themed_ctx};
    use super::*;

    #[test]
    fn tokens_fall_back_to_defaults_before_apply() {
        let ctx = egui::Context::default();
        assert_eq!(Tokens::get(&ctx).chrome, Tokens::default().chrome);
    }

    #[test]
    fn apply_and_fonts_install_and_run_a_frame() {
        let ctx = themed_ctx();
        assert_eq!(Tokens::get(&ctx).accent, Tokens::default().accent);
        // Fonts only bind on the first frame; a panic there (an unbound family) is what this guards.
        run_frame(&ctx, |ui| {
            ui.label(egui::RichText::new("Develop").font(Tokens::get(ui.ctx()).semibold(14.0)));
            ui.heading("Heading uses the semibold family");
            ui.label("Exposure \u{26A0}");
        });
    }
}
