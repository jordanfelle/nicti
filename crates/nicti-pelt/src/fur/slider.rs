//! Lightroom-style slider rows: label and value above a track with a hollow ring thumb, optional
//! gradient tracks (white balance, colour mixer), double-click reset, shift-drag fine adjust.
//!
//! Adapted from storytold/lightcraft@265248c `crates/ui-egui/src/widgets.rs`, Copyright (c) 2026
//! ArtCraft Team and the LightCraft contributors, MIT OR Apache-2.0 (see `docs/licensing.md`).
//! Changes: `SliderSpec` replaces LightCraft's `ControlSpec` (no i18n, no per-id special cases), the
//! value is written through a `&mut f32`, and shift-drag accumulates the unrounded delta so
//! integer-step sliders no longer stall under slow fine drags.

use egui::{pos2, vec2, Align2, Color32, Rect, Sense, Stroke, Ui};

use super::tokens::Tokens;

/// What the track behind the thumb looks like.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Track {
    Plain,
    /// Plain, with a tick at the midpoint (signed ranges).
    Centered,
    Temp,
    Tint,
    /// Colour-mixer band `0..8` (R, O, Y, G, A, B, P, M).
    Hue {
        band: u8,
    },
    Sat {
        band: u8,
    },
    Lum {
        band: u8,
    },
}

/// One slider's range, default and presentation.
#[derive(Clone, Copy, Debug)]
pub struct SliderSpec {
    /// Unique within a panel: it salts the widget id.
    pub id: &'static str,
    pub label: &'static str,
    pub min: f32,
    pub max: f32,
    pub default: f32,
    pub step: f32,
    pub decimals: usize,
    pub track: Track,
    /// Show a `+` on positive values.
    pub signed: bool,
    /// Appended to the shown value (for example `°`).
    pub unit: &'static str,
}

impl SliderSpec {
    pub const fn new(
        id: &'static str,
        label: &'static str,
        min: f32,
        max: f32,
        default: f32,
    ) -> Self {
        SliderSpec {
            id,
            label,
            min,
            max,
            default,
            step: 0.01,
            decimals: 2,
            track: Track::Plain,
            signed: false,
            unit: "",
        }
    }

    /// A `-1..=1` adjustment centred on zero, the shape most Develop controls have.
    pub const fn bipolar(id: &'static str, label: &'static str) -> Self {
        let mut s = Self::new(id, label, -1.0, 1.0, 0.0);
        s.track = Track::Centered;
        s.signed = true;
        s
    }

    pub const fn step(mut self, step: f32, decimals: usize) -> Self {
        self.step = step;
        self.decimals = decimals;
        self
    }

    pub const fn track(mut self, track: Track) -> Self {
        self.track = track;
        self
    }

    pub const fn signed(mut self) -> Self {
        self.signed = true;
        self
    }

    pub const fn unit(mut self, unit: &'static str) -> Self {
        self.unit = unit;
        self
    }

    /// The value as the row shows it: fixed decimals, `+` on positives when `signed`, and a plain
    /// `0` for anything that rounds to zero.
    pub fn display(&self, v: f32) -> String {
        let text = format!("{v:.*}", self.decimals);
        let zero = text.parse::<f64>().is_ok_and(|n| n == 0.0);
        if zero {
            return format!("0{}", self.unit);
        }
        let sign = if self.signed && v > 0.0 { "+" } else { "" };
        format!("{sign}{text}{}", self.unit)
    }
}

/// What a slider did this frame. `drag_started`/`drag_stopped` bracket one undo unit (a click or
/// keyboard nudge sets both), for when edits grow a history (#324).
#[derive(Default, Debug, Clone, Copy)]
pub struct SliderOut {
    pub changed: bool,
    pub drag_started: bool,
    pub drag_stopped: bool,
    pub reset: bool,
}

/// `value` moved by `steps` keyboard nudges: one nudge is about 1/200 of the range, rounded to a
/// whole number of the control's steps.
pub fn nudged(spec: &SliderSpec, value: f32, steps: f32) -> f32 {
    let step = f64::from(spec.step).max(1e-9);
    let span = f64::from(spec.max - spec.min);
    let unit = (span / 200.0 / step).round().max(1.0) * step;
    snap(spec, f64::from(value) + unit * f64::from(steps))
}

fn snap(spec: &SliderSpec, v: f64) -> f32 {
    let step = f64::from(spec.step).max(1e-9);
    let snapped = (v / step).round() * step;
    snapped.clamp(f64::from(spec.min), f64::from(spec.max)) as f32
}

/// Shift-drag: the unrounded value after a pointer move of `dx` points, at a tenth of the normal
/// speed. Kept unrounded between frames; rounding it each frame would swallow every move smaller
/// than half a step.
fn fine_accumulate(acc: f64, dx: f32, span: f64, track_width: f32) -> f64 {
    acc + f64::from(dx) * span / f64::from(track_width.max(1.0)) * 0.1
}

pub(super) fn hex(s: &str) -> Color32 {
    let v = u32::from_str_radix(s.trim_start_matches('#'), 16).unwrap_or(0x80_80_80);
    Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

fn lerp(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let f = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
    Color32::from_rgb(f(a.r(), b.r()), f(a.g(), b.g()), f(a.b(), b.b()))
}

/// Band colours for the 8 colour-mixer bands.
pub const BAND_COLORS: [&str; 8] = [
    "#aa0000", "#bb6600", "#aaaa00", "#009900", "#00aaaa", "#0044cc", "#7722aa", "#aa0077",
];

fn track_stops(track: Track) -> Option<Vec<Color32>> {
    Some(match track {
        Track::Plain | Track::Centered => return None,
        Track::Temp => [
            "#193961", "#47566a", "#60666d", "#70705b", "#737339", "#75742a",
        ]
        .iter()
        .map(|h| hex(h))
        .collect(),
        Track::Tint => [
            "#246123", "#2c552c", "#394739", "#4a3d4c", "#6b336b", "#872c87",
        ]
        .iter()
        .map(|h| hex(h))
        .collect(),
        Track::Hue { band } => {
            let i = usize::from(band) % 8;
            let prev = hex(BAND_COLORS[(i + 7) % 8]);
            let next = hex(BAND_COLORS[(i + 1) % 8]);
            let me = hex(BAND_COLORS[i]);
            vec![
                prev.gamma_multiply(0.7),
                lerp(prev, me, 0.5).gamma_multiply(0.7),
                me.gamma_multiply(0.7),
                lerp(me, next, 0.5).gamma_multiply(0.7),
                next.gamma_multiply(0.7),
            ]
        }
        Track::Sat { band } => {
            let me = hex(BAND_COLORS[usize::from(band) % 8]);
            vec![hex("#454343"), lerp(hex("#454343"), me, 0.6), me]
        }
        Track::Lum { band } => {
            let c = hex(BAND_COLORS[usize::from(band) % 8]);
            vec![
                c.gamma_multiply(0.2),
                c.gamma_multiply(0.55),
                lerp(c, Color32::WHITE, 0.35),
                lerp(c, Color32::WHITE, 0.6),
            ]
        }
    })
}

fn paint_track(ui: &Ui, rect: Rect, track: Track, t: &Tokens, thumb_x: f32, ring: f32) {
    let p = ui.painter();
    let y = rect.center().y;
    let h = 1.0;
    if let Some(stops) = track_stops(track) {
        let n = stops.len().max(2) - 1;
        let steps = 48;
        for i in 0..steps {
            let (a, b) = (i as f32 / steps as f32, (i + 1) as f32 / steps as f32);
            let seg = (a * n as f32).floor() as usize;
            let local = a * n as f32 - seg as f32;
            let c = lerp(stops[seg.min(n)], stops[(seg + 1).min(n)], local);
            let x0 = rect.left() + rect.width() * a;
            let x1 = rect.left() + rect.width() * b;
            p.rect_filled(
                Rect::from_min_max(pos2(x0, y - h - 0.5), pos2(x1 + 0.5, y + h + 0.5)),
                0.0,
                c,
            );
        }
    } else {
        p.rect_filled(
            Rect::from_min_max(pos2(rect.left(), y - h), pos2(rect.right(), y + h)),
            1.0,
            t.track,
        );
        if track == Track::Centered {
            let x = rect.center().x;
            p.line_segment(
                [pos2(x, y - 4.0), pos2(x, y + 4.0)],
                Stroke::new(1.0, t.track),
            );
        }
    }
    // gap around the hollow thumb
    p.rect_filled(
        Rect::from_center_size(pos2(thumb_x, y), vec2(ring * 2.0 + 4.0, 6.0)),
        0.0,
        t.chrome,
    );
}

/// A slider row. Double-click the label or track to reset; shift-drag is a fine adjustment;
/// ArrowUp/Down while hovering nudge (shift: five times as much).
pub fn slider(ui: &mut Ui, spec: &SliderSpec, value: &mut f32, enabled: bool) -> SliderOut {
    let t = Tokens::get(ui.ctx());
    let w = ui.available_width();
    let (row, _) = ui.allocate_exact_size(vec2(w, t.slider_row_h), Sense::hover());
    let pad_l = 24.0;
    let pad_r = 22.0;
    let label_rect = Rect::from_min_size(
        pos2(row.left() + pad_l, row.top() + 4.0),
        vec2((w - pad_l - pad_r).max(1.0), 18.0),
    );
    let track_rect = Rect::from_min_max(
        pos2(row.left() + pad_l, row.top() + 22.0),
        pos2(
            (row.right() - pad_r).max(row.left() + pad_l + 1.0),
            row.top() + 40.0,
        ),
    );
    let id = ui.id().with(spec.id);
    let resp = ui.interact(
        track_rect.expand2(vec2(8.0, 2.0)),
        id,
        if enabled {
            Sense::click_and_drag()
        } else {
            Sense::hover()
        },
    );
    let current = *value;
    resp.widget_info(|| egui::WidgetInfo::slider(enabled, f64::from(current), spec.label));
    let label_resp = ui.interact(label_rect, id.with("label"), Sense::click());

    let mut out = SliderOut::default();
    let (min, max) = (f64::from(spec.min), f64::from(spec.max));
    let span = (max - min).max(1e-9);
    let width = track_rect.width();
    let to_x =
        |v: f32| track_rect.left() + ((f64::from(v) - min) / span).clamp(0.0, 1.0) as f32 * width;
    let from_x = |x: f32| min + f64::from(((x - track_rect.left()) / width).clamp(0.0, 1.0)) * span;
    let fine_id = id.with("fine");
    let mut new_value = None;

    if enabled && (resp.double_clicked() || label_resp.double_clicked()) {
        out.reset = true;
        new_value = Some(spec.default);
    } else if enabled {
        if resp.drag_started() {
            out.drag_started = true;
        }
        let pointer = resp.interact_pointer_pos();
        if let Some(p) = pointer.filter(|_| resp.dragged() || resp.drag_started()) {
            let (shift, dx, press) = ui.input(|i| {
                (
                    i.modifiers.shift,
                    i.pointer.delta().x,
                    i.pointer.press_origin(),
                )
            });
            let nv = if shift {
                let acc = ui
                    .data(|d| d.get_temp::<f64>(fine_id))
                    .unwrap_or_else(|| f64::from(current));
                let acc = fine_accumulate(acc, dx, span, width).clamp(min, max);
                ui.data_mut(|d| d.insert_temp(fine_id, acc));
                snap(spec, acc)
            } else {
                ui.data_mut(|d| d.remove_temp::<f64>(fine_id));
                // `drag_started` fires only after the pointer crossed the drag threshold, so the
                // press point (not the current position) is where the user meant to grab.
                let x = if resp.drag_started() {
                    press.map_or(p.x, |o| o.x)
                } else {
                    p.x
                };
                snap(spec, from_x(x))
            };
            if (nv - current).abs() > f32::EPSILON {
                new_value = Some(nv);
            }
        } else if let Some(p) = pointer.filter(|_| resp.clicked()) {
            new_value = Some(snap(spec, from_x(p.x)));
            out.drag_started = true;
            out.drag_stopped = true;
        }
        if resp.drag_stopped() {
            out.drag_stopped = true;
            ui.data_mut(|d| d.remove_temp::<f64>(fine_id));
        }
        // ↑ / ↓ while the pointer rests on the row nudge the value (⇧: five times as much)
        // Not while a text field has keyboard focus: the arrows belong to it.
        if new_value.is_none()
            && ui.rect_contains_pointer(row)
            && !ui.ctx().egui_wants_keyboard_input()
        {
            let (up, down, shift) = ui.input_mut(|i| {
                let shift = i.modifiers.shift;
                let m = if shift {
                    egui::Modifiers::SHIFT
                } else {
                    egui::Modifiers::NONE
                };
                (
                    i.consume_key(m, egui::Key::ArrowUp),
                    i.consume_key(m, egui::Key::ArrowDown),
                    shift,
                )
            });
            if up || down {
                let steps = if up { 1.0 } else { -1.0 } * if shift { 5.0 } else { 1.0 };
                let nv = nudged(spec, current, steps);
                if (nv - current).abs() > f32::EPSILON {
                    new_value = Some(nv);
                    out.drag_started = true;
                    out.drag_stopped = true;
                }
            }
        }
    }
    if let Some(nv) = new_value.filter(|nv| (nv - current).abs() > f32::EPSILON) {
        *value = nv;
        out.changed = true;
    }

    // paint
    let v = *value;
    let hovered = resp.hovered() || resp.dragged();
    let text_c = if enabled {
        t.text_label
    } else {
        t.text_disabled
    };
    let p = ui.painter();
    p.text(
        label_rect.left_center(),
        Align2::LEFT_CENTER,
        spec.label,
        t.font(12.5),
        text_c,
    );
    p.text(
        label_rect.right_center(),
        Align2::RIGHT_CENTER,
        spec.display(v),
        t.font(12.5),
        text_c,
    );
    let ring = 7.0;
    let tx = to_x(v);
    paint_track(ui, track_rect, spec.track, &t, tx, ring);
    let ring_c = if !enabled {
        t.text_disabled
    } else if hovered {
        t.thumb_hover
    } else {
        t.thumb
    };
    let p = ui.painter();
    let centre = pos2(tx, track_rect.center().y);
    p.circle_filled(centre, ring, t.chrome);
    p.circle_stroke(centre, ring, Stroke::new(2.0, ring_c));
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXPOSURE: SliderSpec = SliderSpec::new("t.exposure", "Exposure", -5.0, 5.0, 0.0)
        .step(0.01, 2)
        .signed();
    const INT: SliderSpec = SliderSpec::new("t.int", "Tint", -150.0, 150.0, 0.0).step(1.0, 0);
    const TEMP: SliderSpec =
        SliderSpec::new("t.temp", "Temp", 2000.0, 50000.0, 5500.0).step(50.0, 0);

    #[test]
    fn nudges_are_a_sensible_step() {
        assert!((nudged(&EXPOSURE, 0.0, 1.0) - 0.05).abs() < 1e-6);
        // 300 / 200 = 1.5 steps rounds to a 2-step nudge on the integer Tint range.
        assert_eq!(nudged(&INT, 10.0, -5.0), 0.0);
        assert_eq!(nudged(&TEMP, 6500.0, 1.0), 6750.0);
        assert_eq!(nudged(&INT, 149.0, 5.0), 150.0, "clamped");
    }

    #[test]
    fn display_formats_sign_zero_and_unit() {
        assert_eq!(EXPOSURE.display(0.5), "+0.50");
        assert_eq!(EXPOSURE.display(-0.5), "-0.50");
        assert_eq!(EXPOSURE.display(0.001), "0", "rounds to zero");
        assert_eq!(EXPOSURE.display(-0.001), "0", "no negative zero");
        assert_eq!(INT.display(12.0), "12");
        let rot = SliderSpec::new("t.rot", "Rotation", -45.0, 45.0, 0.0)
            .step(0.1, 1)
            .unit("\u{b0}");
        assert_eq!(rot.display(3.5), "3.5\u{b0}");
        assert_eq!(rot.display(0.0), "0\u{b0}");
    }

    /// The regression for LightCraft's stalling fine drag: moving 1 point per frame at a tenth
    /// speed is far below half an integer step on this range, so rounding each frame (their
    /// behaviour) never leaves the starting value, while accumulating does.
    #[test]
    fn fine_drag_accumulates_below_one_step_per_frame() {
        let span = f64::from(INT.max - INT.min);
        let width = 220.0;
        let mut acc = 0.0;
        let mut per_frame_rounded = 0.0_f64;
        for _ in 0..40 {
            acc = fine_accumulate(acc, 1.0, span, width);
            per_frame_rounded = (fine_accumulate(per_frame_rounded, 1.0, span, width)).round();
        }
        assert_eq!(
            per_frame_rounded, 0.0,
            "per-frame rounding stalls (old behaviour)"
        );
        assert_eq!(
            snap(&INT, acc),
            5.0,
            "accumulated: 40 * 300/220 * 0.1 = 5.45 -> 5"
        );
    }

    /// Drives `slider()` through a real press, many small moves and a release, with or without
    /// shift held, and returns the final value. Moves are 1 point per frame.
    fn drag(spec: &SliderSpec, start: f32, shift: bool, frames: usize) -> f32 {
        let ctx = super::super::tokens::testing::themed_ctx();
        let mods = egui::Modifiers {
            shift,
            ..Default::default()
        };
        let (x0, y) = (300.0, 36.0);
        let mut v = start;
        let mut t = 0.0;
        let mut run = |events: Vec<egui::Event>, v: &mut f32| {
            t += 0.02;
            let input = egui::RawInput {
                time: Some(t),
                // Without a screen rect egui assumes a huge one and the track gets ~10k points wide.
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 600.0),
                )),
                events,
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| {
                slider(ui, spec, v, true);
            });
            out.textures_delta.clear();
        };
        let at = |x: f32| egui::pos2(x, y);
        run(
            vec![
                egui::Event::ModifiersChanged(mods),
                egui::Event::PointerMoved(at(x0)),
            ],
            &mut v,
        );
        run(
            vec![egui::Event::PointerButton {
                pos: at(x0),
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: mods,
            }],
            &mut v,
        );
        for i in 1..=frames {
            run(vec![egui::Event::PointerMoved(at(x0 + i as f32))], &mut v);
        }
        run(
            vec![egui::Event::PointerButton {
                pos: at(x0 + frames as f32),
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: mods,
            }],
            &mut v,
        );
        v
    }

    /// Through the real widget: 60 one-point moves with shift held cover 60/738 of the track at a
    /// tenth speed (~2.4 steps on Tint's integer range). Rounding per frame would stay at the
    /// start; an un-accumulated absolute mapping would jump by ~24 steps.
    #[test]
    fn shift_drag_through_the_widget_accumulates_fine_movement() {
        let fine = drag(&INT, 0.0, true, 60);
        assert!(
            (1.0..=4.0).contains(&fine),
            "fine drag moved by a few steps, got {fine}"
        );
        let coarse = drag(&INT, 0.0, false, 60);
        // Absolute mapping: the thumb lands under the pointer (x=360 of a ~738pt track), far from
        // the starting 0 and from the few steps a fine drag moves.
        assert!(
            coarse.abs() > 10.0,
            "an unshifted drag follows the pointer, got {coarse}"
        );
    }

    /// ArrowUp over a slider nudges it, unless a text field has keyboard focus (then the arrow
    /// belongs to the field).
    #[test]
    fn arrow_nudge_yields_to_a_focused_text_field() {
        let nudge = |focus_text: bool| {
            let ctx = super::super::tokens::testing::themed_ctx();
            let mut v = 0.0_f32;
            let mut text = String::new();
            let mut t = 0.0;
            let mut frame = |events: Vec<egui::Event>, v: &mut f32, text: &mut String| {
                t += 0.02;
                let input = egui::RawInput {
                    time: Some(t),
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(800.0, 600.0),
                    )),
                    events,
                    ..Default::default()
                };
                let mut out = ctx.run_ui(input, |ui| {
                    if focus_text {
                        ui.text_edit_singleline(text).request_focus();
                    }
                    slider(ui, &INT, v, true);
                });
                out.textures_delta.clear();
            };
            frame(vec![], &mut v, &mut text);
            // The slider row sits below the text field when there is one; hover well inside it.
            let row_y = if focus_text { 60.0 } else { 36.0 };
            frame(
                vec![egui::Event::PointerMoved(egui::pos2(300.0, row_y))],
                &mut v,
                &mut text,
            );
            frame(
                vec![egui::Event::Key {
                    key: egui::Key::ArrowUp,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: Default::default(),
                }],
                &mut v,
                &mut text,
            );
            v
        };
        assert_ne!(nudge(false), 0.0, "the arrow nudges a hovered slider");
        assert_eq!(nudge(true), 0.0, "a focused text field keeps the arrow");
    }

    #[test]
    fn snap_clamps_and_rounds_to_step() {
        assert_eq!(snap(&TEMP, 6523.0), 6500.0);
        assert_eq!(snap(&TEMP, 6530.0), 6550.0);
        assert_eq!(snap(&TEMP, 99999.0), 50000.0);
        assert_eq!(snap(&INT, -151.0), -150.0);
    }

    #[test]
    fn every_track_has_stops_or_is_flat() {
        for band in 0..8u8 {
            assert!(track_stops(Track::Hue { band }).is_some());
            assert!(track_stops(Track::Sat { band }).is_some());
            assert!(track_stops(Track::Lum { band }).is_some());
        }
        assert!(track_stops(Track::Temp).is_some());
        assert!(track_stops(Track::Tint).is_some());
        assert!(track_stops(Track::Plain).is_none());
        assert!(track_stops(Track::Centered).is_none());
    }

    /// A slider row draws, reports its value to a screen reader, and leaves the value alone
    /// without input.
    #[test]
    fn slider_draws_and_describes_itself() {
        let ctx = egui::Context::default();
        super::super::tokens::install_fonts(&ctx);
        ctx.enable_accesskit();
        let mut v = 0.5_f32;
        let mut found = Vec::new();
        for _ in 0..3 {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                let o = slider(ui, &EXPOSURE, &mut v, true);
                assert!(!o.changed);
            });
            out.textures_delta.clear();
            if let Some(update) = out.platform_output.accesskit_update.take() {
                found = update
                    .nodes
                    .iter()
                    .map(|(_, n)| {
                        (
                            format!("{:?}", n.role()),
                            n.label().unwrap_or_default().to_string(),
                            n.numeric_value(),
                        )
                    })
                    .collect();
            }
        }
        assert_eq!(v, 0.5);
        assert!(
            found
                .iter()
                .any(|(r, l, n)| r == "Slider" && l == "Exposure" && *n == Some(0.5)),
            "{found:?}"
        );
    }
}
