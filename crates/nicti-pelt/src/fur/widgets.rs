//! Lightroom-style chrome widgets: collapsible section headers, dividers, segmented controls and
//! icon buttons. The slider lives in `slider.rs`.
//!
//! Adapted from storytold/lightcraft@265248c `crates/ui-egui/src/widgets.rs`, Copyright (c) 2026
//! ArtCraft Team and the LightCraft contributors, MIT OR Apache-2.0 (see `docs/licensing.md`).
//! Changes: no i18n and no automation registry (#426 owns a harness), the section header owns its
//! open state, and the segmented control reports itself to screen readers.

use std::collections::BTreeMap;

use egui::{pos2, vec2, Align2, CornerRadius, Rect, Response, Sense, Stroke, StrokeKind, Ui};

use super::icons::{paint, Icon};
use super::slider::{hex, BAND_COLORS};
use super::tokens::Tokens;

/// Which sections the user opened or closed, by section id. Held in egui's memory while the app
/// runs; the app seeds it with [`install_section_states`] and writes it back whenever
/// [`take_changed_section_states`] reports a change, so the layout survives a restart.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct SectionStates(BTreeMap<String, bool>);

#[derive(Clone, Default)]
struct SectionMemory {
    states: SectionStates,
    dirty: bool,
}

fn section_memory_id() -> egui::Id {
    egui::Id::new("fur-section-states")
}

/// Seeds the remembered open/closed state (call once at startup).
pub fn install_section_states(ctx: &egui::Context, states: SectionStates) {
    ctx.data_mut(|d| {
        d.insert_temp(
            section_memory_id(),
            SectionMemory {
                states,
                dirty: false,
            },
        )
    });
}

/// The state if a section was toggled since the last call, for the app to persist.
pub fn take_changed_section_states(ctx: &egui::Context) -> Option<SectionStates> {
    ctx.data_mut(|d| {
        let mem = d.get_temp_mut_or_default::<SectionMemory>(section_memory_id());
        std::mem::take(&mut mem.dirty).then(|| mem.states.clone())
    })
}

/// A collapsible section: a header row (chevron + title) and `body` while open. The open state is
/// remembered per `id` (unique across the app) in [`SectionStates`], so it survives restarts once
/// the app persists it; `default_open` applies until the user first toggles the section.
pub fn section(ui: &mut Ui, id: &str, title: &str, default_open: bool, body: impl FnOnce(&mut Ui)) {
    let open = ui.data_mut(|d| {
        let mem = d.get_temp_mut_or_default::<SectionMemory>(section_memory_id());
        mem.states.0.get(id).copied().unwrap_or(default_open)
    });
    let resp = section_header(ui, title, open);
    let open = if resp.clicked() {
        ui.data_mut(|d| {
            let mem = d.get_temp_mut_or_default::<SectionMemory>(section_memory_id());
            mem.states.0.insert(id.to_owned(), !open);
            mem.dirty = true;
        });
        !open
    } else {
        open
    };
    if open {
        body(ui);
        ui.add_space(6.0);
    }
}

/// The header row on its own ("› Light"); the caller owns the open state.
pub fn section_header(ui: &mut Ui, title: &str, open: bool) -> Response {
    let t = Tokens::get(ui.ctx());
    let w = ui.available_width();
    let (r, resp) = ui.allocate_exact_size(vec2(w, t.section_h), Sense::click());
    resp.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::CollapsingHeader, true, open, title)
    });
    let p = ui.painter();
    if resp.hovered() {
        p.rect_filled(r, 0.0, t.chrome.gamma_multiply(1.06));
    }
    let chev = Rect::from_center_size(pos2(r.left() + 30.0, r.center().y), vec2(14.0, 14.0));
    paint(
        p,
        chev,
        if open {
            Icon::ChevronDown
        } else {
            Icon::ChevronRight
        },
        t.text_label,
    );
    p.text(
        pos2(r.left() + 44.0, r.center().y),
        Align2::LEFT_CENTER,
        title,
        t.semibold(14.0),
        t.text,
    );
    resp
}

/// Thin divider between groups.
pub fn divider(ui: &mut Ui) {
    let t = Tokens::get(ui.ctx());
    let w = ui.available_width();
    let (r, _) = ui.allocate_exact_size(vec2(w, 1.0), Sense::hover());
    ui.painter().rect_filled(r, 0.0, t.divider);
}

/// A segmented control: `labels` share the available width equally, `per_row` per row. Returns
/// the clicked index.
pub fn segmented(
    ui: &mut Ui,
    labels: &[&str],
    active: Option<usize>,
    per_row: usize,
) -> Option<usize> {
    let t = Tokens::get(ui.ctx());
    let per_row = per_row.max(1);
    let gap = 4.0;
    let w = ui.available_width();
    let seg_w = ((w - gap * (per_row as f32 - 1.0)) / per_row as f32).max(24.0);
    let mut clicked = None;
    for (row_i, row) in labels.chunks(per_row).enumerate() {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for (j, label) in row.iter().enumerate() {
                let i = row_i * per_row + j;
                let is_active = active == Some(i);
                let (r, resp) = ui.allocate_exact_size(vec2(seg_w, 24.0), Sense::click());
                resp.widget_info(|| {
                    egui::WidgetInfo::selected(egui::WidgetType::Button, true, is_active, *label)
                });
                let fill = if is_active {
                    t.pressed
                } else if resp.hovered() {
                    t.hover
                } else {
                    t.button
                };
                let p = ui.painter();
                p.rect(
                    r,
                    CornerRadius::same(4),
                    fill,
                    Stroke::new(1.0, t.button_border),
                    StrokeKind::Inside,
                );
                p.text(
                    r.center(),
                    Align2::CENTER_CENTER,
                    *label,
                    t.semibold(11.5),
                    t.text,
                );
                if resp.clicked() {
                    clicked = Some(i);
                }
            }
        });
    }
    clicked
}

/// An icon-only button. `active` draws the selected background.
pub fn icon_button(
    ui: &mut Ui,
    icon: Icon,
    size: egui::Vec2,
    active: bool,
    enabled: bool,
    tooltip: &str,
) -> Response {
    let t = Tokens::get(ui.ctx());
    let (r, resp) = ui.allocate_exact_size(
        size,
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    resp.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Button, enabled, active, tooltip)
    });
    let p = ui.painter();
    let sq = Rect::from_center_size(r.center(), vec2(32.0f32.min(size.x), 32.0f32.min(size.y)));
    if active {
        p.rect_filled(sq, 4.0, t.tool_active);
    } else if resp.hovered() && enabled {
        p.rect_filled(sq, 4.0, t.hover.gamma_multiply(0.7));
    }
    let c = if !enabled {
        t.text_disabled
    } else if active {
        t.pick
    } else if resp.hovered() {
        t.text
    } else {
        t.icon
    };
    let glyph = (size.x.min(size.y) * 0.62).min(22.0);
    paint(
        p,
        Rect::from_center_size(r.center(), vec2(glyph, glyph)),
        icon,
        c,
    );
    if tooltip.is_empty() {
        resp
    } else {
        resp.on_hover_text(tooltip)
    }
}

/// The colour mixer's band picker: one dot per band in the band's own colour (Red, Orange, Yellow,
/// Green, Aqua, Blue, Purple, Magenta), the selected one ringed. Returns the clicked index. Each
/// dot reports its band name to screen readers (and the headless harness).
pub fn band_dots(ui: &mut Ui, names: &[&str; 8], active: usize) -> Option<usize> {
    let t = Tokens::get(ui.ctx());
    let mut clicked = None;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        for (i, name) in names.iter().enumerate() {
            let (r, resp) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::click());
            resp.widget_info(|| {
                egui::WidgetInfo::selected(egui::WidgetType::Button, true, i == active, *name)
            });
            let p = ui.painter();
            p.circle_filled(r.center(), 8.0, hex(BAND_COLORS[i]));
            if i == active {
                p.circle_stroke(r.center(), 10.0, Stroke::new(1.5, t.text));
            } else if resp.hovered() {
                p.circle_stroke(r.center(), 10.0, Stroke::new(1.0, t.text_dim));
            }
            if resp.clicked() {
                clicked = Some(i);
            }
        }
    });
    clicked
}

#[cfg(test)]
mod tests {
    use super::super::tokens::testing::{run_frame, themed_ctx};
    use super::*;

    #[test]
    fn section_draws_its_body_only_when_open() {
        let mut open_runs = 0;
        run_frame(&themed_ctx(), |ui| {
            section(ui, "basic", "Basic", true, |_| open_runs += 1);
        });
        assert_eq!(open_runs, 1, "default_open draws the body");

        let mut closed_runs = 0;
        run_frame(&themed_ctx(), |ui| {
            section(ui, "basic", "Basic", false, |_| closed_runs += 1);
        });
        assert_eq!(closed_runs, 0, "a closed section skips the body");
    }

    /// A click on the header toggles the section, and the state persists to the next frame.
    #[test]
    fn clicking_the_header_toggles_and_persists() {
        let ctx = themed_ctx();
        let mut bodies = Vec::new();
        let mut t = 0.0;
        let mut frame = |events: Vec<egui::Event>| {
            t += 0.02;
            let input = egui::RawInput {
                time: Some(t),
                events,
                ..Default::default()
            };
            let mut shown = false;
            let mut out = ctx.run_ui(input, |ui| {
                section(ui, "s", "Section", false, |_| shown = true);
            });
            out.textures_delta.clear();
            bodies.push(shown);
        };
        let at = egui::pos2(60.0, 20.0);
        let click = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        };
        frame(vec![egui::Event::PointerMoved(at)]);
        frame(vec![click(true)]);
        frame(vec![click(false)]);
        frame(vec![]);
        assert_eq!(
            bodies,
            [false, false, true, true],
            "opened by the click, then stays open"
        );
    }

    #[test]
    fn segmented_and_icon_button_draw() {
        let mut out = None;
        run_frame(&themed_ctx(), |ui| {
            out = segmented(ui, &["R", "O", "Y", "G", "A", "B", "P", "M"], Some(2), 8);
            let _ = icon_button(
                ui,
                Icon::BeforeAfter,
                vec2(24.0, 24.0),
                false,
                true,
                "Before / after",
            );
            divider(ui);
        });
        assert_eq!(out, None, "no click, no selection change");
    }

    /// Toggling a section reports the new state once for the app to save, and a fresh context
    /// seeded with it (a restart) draws the section as the user left it.
    #[test]
    fn section_state_survives_a_restart() {
        let ctx = themed_ctx();
        let at = egui::pos2(60.0, 20.0);
        let click = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        };
        let mut t = 0.0;
        let mut frame = |ctx: &egui::Context, events: Vec<egui::Event>| {
            t += 0.02;
            let input = egui::RawInput {
                time: Some(t),
                events,
                ..Default::default()
            };
            let mut shown = false;
            let mut out = ctx.run_ui(input, |ui| {
                section(ui, "s", "Section", false, |_| shown = true);
            });
            out.textures_delta.clear();
            shown
        };
        frame(&ctx, vec![egui::Event::PointerMoved(at)]);
        assert_eq!(
            take_changed_section_states(&ctx),
            None,
            "nothing toggled yet"
        );
        frame(&ctx, vec![click(true)]);
        frame(&ctx, vec![click(false)]);
        let saved = take_changed_section_states(&ctx).expect("the toggle is reported");
        assert_eq!(take_changed_section_states(&ctx), None, "reported once");

        let text = serde_json::to_string(&saved).unwrap();
        let restored: SectionStates = serde_json::from_str(&text).unwrap();
        let fresh = themed_ctx();
        install_section_states(&fresh, restored);
        assert!(frame(&fresh, vec![]), "reopened after the restart");
        assert!(
            !frame(&themed_ctx(), vec![]),
            "an unseeded context uses the default"
        );
    }
}
