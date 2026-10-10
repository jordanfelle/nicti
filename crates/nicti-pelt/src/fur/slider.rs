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
    /// The shown value is `value * scale` (100 shows a `0..=1` fraction as a percentage).
    pub scale: f32,
    /// Logarithmic track (needs `min > 0`): equal thumb travel is an equal ratio. Values are not
    /// snapped to `step`, only clamped.
    pub log: bool,
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
            scale: 1.0,
            log: false,
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

    /// Shows the value times `scale`, to `decimals` places (`step` stays in value units).
    pub const fn scaled(mut self, scale: f32, decimals: usize) -> Self {
        self.scale = scale;
        self.decimals = decimals;
        self
    }

    /// A fraction shown as a whole-number percentage (`-1..=1` reads `-100..=100`), one
    /// percentage point per step.
    pub const fn percent(self) -> Self {
        let mut s = self.scaled(100.0, 0);
        s.step = 0.01;
        s
    }

    pub const fn log(mut self) -> Self {
        self.log = true;
        self
    }

    /// Whether the track is actually logarithmic: `log` needs a positive minimum, otherwise the
    /// spec behaves as an ordinary linear one (track, stepping and nudges alike).
    fn is_log(&self) -> bool {
        self.log && self.min > 0.0
    }

    /// The value as the row shows it: fixed decimals, `+` on positives when `signed`, and a plain
    /// `0` for anything that rounds to zero.
    pub fn display(&self, v: f32) -> String {
        let text = self.edit_text(v);
        let zero = text.parse::<f64>().is_ok_and(|n| n == 0.0);
        if zero {
            return format!("0{}", self.unit);
        }
        let sign = if self.signed && v > 0.0 { "+" } else { "" };
        format!("{sign}{text}{}", self.unit)
    }

    /// The bare number the typed-entry box starts from: scaled, fixed decimals, no sign or unit.
    pub fn edit_text(&self, v: f32) -> String {
        let text = format!("{:.*}", self.decimals, f64::from(v) * f64::from(self.scale));
        // No negative zero ("-0" for -0.001 shown to whole numbers).
        if text.parse::<f64>().is_ok_and(|n| n == 0.0) {
            text.trim_start_matches('-').to_owned()
        } else {
            text
        }
    }

    /// What the user typed as a value: the unit, a leading `+` and surrounding space are
    /// ignored, the number is rounded to the decimals the row shows (so what is stored is what is
    /// displayed) and clamped to the range. It is not snapped to `step`, so Temp and Rotation take
    /// exact values. `None` for anything that isn't a finite number.
    pub fn parse_input(&self, text: &str) -> Option<f32> {
        let t = text.trim();
        let t = t.strip_suffix(self.unit).unwrap_or(t).trim();
        let t = t.strip_prefix('+').unwrap_or(t);
        let n: f64 = t.parse().ok()?;
        let places = 10f64.powi(self.decimals.min(9) as i32);
        let n = (n * places).round() / places;
        let v = n / f64::from(self.scale.max(1e-9));
        v.is_finite()
            .then(|| v.clamp(f64::from(self.min), f64::from(self.max)) as f32)
    }
}

/// What a slider did this frame. `drag_started`/`drag_stopped` bracket one undo unit (a click or
/// keyboard nudge sets both), for a gesture-aware history; `DevelopDoc` (#324) groups by time window instead.
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
    if spec.is_log() {
        // 1/200 of the track's ratio per nudge.
        let ratio = (f64::from(spec.max) / f64::from(spec.min)).powf(f64::from(steps) / 200.0);
        return snap(spec, f64::from(value) * ratio);
    }
    let step = f64::from(spec.step).max(1e-9);
    let span = f64::from(spec.max - spec.min);
    let unit = (span / 200.0 / step).round().max(1.0) * step;
    snap(spec, f64::from(value) + unit * f64::from(steps))
}

fn snap(spec: &SliderSpec, v: f64) -> f32 {
    if spec.is_log() {
        return v.clamp(f64::from(spec.min), f64::from(spec.max)) as f32;
    }
    let step = f64::from(spec.step).max(1e-9);
    let snapped = (v / step).round() * step;
    snapped.clamp(f64::from(spec.min), f64::from(spec.max)) as f32
}

/// Shift-drag: the unrounded value after a pointer move of `dx` points, at a tenth of the normal
/// speed. Kept unrounded between frames; rounding it each frame would swallow every move smaller
/// than half a step.
/// Position of `v` along the track, `0..=1`.
fn to_frac(spec: &SliderSpec, v: f64) -> f64 {
    let (min, max) = (f64::from(spec.min), f64::from(spec.max));
    let f = if spec.is_log() {
        (v.max(min) / min).ln() / (max / min).ln().max(1e-9)
    } else {
        (v - min) / (max - min).max(1e-9)
    };
    f.clamp(0.0, 1.0)
}

/// The value at track position `f` (`0..=1`).
fn from_frac(spec: &SliderSpec, f: f64) -> f64 {
    let (min, max) = (f64::from(spec.min), f64::from(spec.max));
    let f = f.clamp(0.0, 1.0);
    if spec.is_log() {
        min * (max / min).powf(f)
    } else {
        min + f * (max - min)
    }
}

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

/// Width of the clickable value text at the right of a row's label line.
const VALUE_BOX_W: f32 = 64.0;

/// A slider row. Double-click the value to type one; double-click the label or track to reset; shift-drag is a fine adjustment;
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
    // The value text on the right: double-click to type an exact number. Declared after the label
    // so it wins where they overlap (a double-click there edits instead of resetting).
    let value_rect = Rect::from_min_max(
        pos2(label_rect.right() - VALUE_BOX_W, label_rect.top()),
        label_rect.max,
    );
    let value_resp = ui.interact(
        value_rect,
        id.with("value"),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    let edit_id = id.with("edit");
    let mut editing = ui.data(|d| d.get_temp::<String>(edit_id));
    let mut start_edit = false;
    if enabled && editing.is_none() && value_resp.double_clicked() {
        editing = Some(spec.edit_text(*value));
        start_edit = true;
    }

    let mut out = SliderOut::default();
    let width = track_rect.width();
    let to_x = |v: f32| track_rect.left() + to_frac(spec, f64::from(v)) as f32 * width;
    let from_x = |x: f32| from_frac(spec, f64::from((x - track_rect.left()) / width));
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
                // Accumulated as a track fraction, so a log track fine-drags by ratio.
                let acc = ui
                    .data(|d| d.get_temp::<f64>(fine_id))
                    .unwrap_or_else(|| to_frac(spec, f64::from(current)));
                let acc = fine_accumulate(acc, dx, 1.0, width).clamp(0.0, 1.0);
                ui.data_mut(|d| d.insert_temp(fine_id, acc));
                snap(spec, from_frac(spec, acc))
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

    // typed entry: Enter or focus loss commits, Escape cancels
    let mut typed = None;
    if let Some(mut text) = editing {
        let te_id = id.with("textedit");
        let r = ui.put(
            value_rect,
            egui::TextEdit::singleline(&mut text)
                .id(te_id)
                .font(t.font(12.5))
                .horizontal_align(egui::Align::Max)
                .desired_width(value_rect.width()),
        );
        if start_edit {
            r.request_focus();
        }
        if r.gained_focus() {
            if let Some(mut st) = egui::TextEdit::load_state(ui.ctx(), te_id) {
                let end = egui::text::CCursor::new(text.chars().count());
                st.cursor.set_char_range(Some(egui::text::CCursorRange::two(
                    egui::text::CCursor::new(0),
                    end,
                )));
                st.store(ui.ctx(), te_id);
            }
        }
        // Escape cancels. egui drops a TextEdit's focus on Escape before this runs, so detect the
        // key itself (not focus), and consume it so Esc-to-commit tools don't also act on it.
        let escape = !start_edit
            && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        // An edit that is no longer focused (and didn't just start) is over: committed when focus
        // was lost this frame, otherwise abandoned, e.g. the row was not drawn while it was open
        // (section collapsed, selection changed) and the box came back unfocused. Memory focus,
        // not `has_focus()`: an OS window blur must not end the edit.
        let ended = !start_edit && !ui.memory(|m| m.has_focus(te_id));
        if !enabled || escape || ended {
            ui.data_mut(|d| d.remove_temp::<String>(edit_id));
            if enabled && !escape && r.lost_focus() {
                typed = spec.parse_input(&text);
            }
        } else {
            ui.data_mut(|d| d.insert_temp(edit_id, text));
        }
    }
    if let Some(nv) = typed.filter(|nv| (nv - *value).abs() > f32::EPSILON) {
        *value = nv;
        out.changed = true;
        out.drag_started = true;
        out.drag_stopped = true;
    }
    let editing_now = ui.data(|d| d.get_temp::<String>(edit_id)).is_some();

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
    if !editing_now {
        p.text(
            label_rect.right_center(),
            Align2::RIGHT_CENTER,
            spec.display(v),
            t.font(12.5),
            text_c,
        );
    }
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
    fn percent_scales_display_and_input() {
        let pct = SliderSpec::bipolar("t.pct", "Contrast").percent();
        assert_eq!(pct.display(0.5), "+50");
        assert_eq!(pct.display(-1.0), "-100");
        assert_eq!(pct.edit_text(-0.25), "-25");
        assert_eq!(pct.edit_text(-0.001), "0", "no negative zero in the box");
        assert_eq!(pct.parse_input("40"), Some(0.4));
        assert_eq!(pct.parse_input(" +40 "), Some(0.4));
        assert_eq!(pct.parse_input("250"), Some(1.0), "clamped to the range");
    }

    #[test]
    fn typed_input_is_exact_clamped_and_rejects_junk() {
        // Not snapped to the 50-step grid: Temp takes exactly what was typed.
        assert_eq!(TEMP.parse_input("5523"), Some(5523.0));
        assert_eq!(TEMP.parse_input("99999"), Some(50000.0));
        assert_eq!(TEMP.parse_input("1"), Some(2000.0));
        let rot = SliderSpec::new("t.rot", "Rotation", -45.0, 45.0, 0.0)
            .step(0.1, 1)
            .unit("\u{b0}");
        assert_eq!(rot.parse_input("-3.2\u{b0}"), Some(-3.2));
        assert_eq!(
            rot.parse_input("-3.26"),
            Some(-3.3),
            "rounded to the shown decimals"
        );
        assert_eq!(rot.parse_input("12.5"), Some(12.5));
        for bad in ["", "abc", "1,5", "nan", "inf", "--3"] {
            assert_eq!(rot.parse_input(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn log_track_maps_by_ratio_and_does_not_snap() {
        let size = SliderSpec::new("t.size", "Size", 0.002, 0.5, 0.05)
            .scaled(1.0, 3)
            .log();
        assert!((to_frac(&size, 0.002)).abs() < 1e-9);
        assert!((to_frac(&size, 0.5) - 1.0).abs() < 1e-9);
        // The geometric mean sits mid-track.
        let mid = (0.002_f64 * 0.5).sqrt();
        assert!((to_frac(&size, mid) - 0.5).abs() < 1e-6);
        assert!((from_frac(&size, 0.5) - mid).abs() < 1e-6);
        assert_eq!(
            snap(&size, 0.0123),
            0.0123,
            "log values are clamped, not stepped"
        );
        assert_eq!(snap(&size, 9.0), 0.5);
        let up = nudged(&size, 0.01, 1.0);
        assert!(
            up > 0.01 && up < 0.0105,
            "one nudge is ~1/200 of the ratio, got {up}"
        );
    }

    /// Double-clicking the value text opens a box; typing a number and pressing Enter sets it
    /// exactly, and the row reports one undo unit.
    #[test]
    fn double_clicking_the_value_types_an_exact_number() {
        let ctx = super::super::tokens::testing::themed_ctx();
        let mut v = 5500.0_f32;
        let mut t = 0.0;
        let mut outs = Vec::new();
        let mut frame = |events: Vec<egui::Event>, v: &mut f32| {
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
            let mut o = None;
            let mut out = ctx.run_ui(input, |ui| {
                o = Some(slider(ui, &TEMP, v, true));
            });
            out.textures_delta.clear();
            outs.push(o.unwrap());
        };
        // The value text is at the right end of the label line (row top + ~13).
        let at = egui::pos2(800.0 - 40.0, 13.0);
        let click = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        };
        frame(vec![egui::Event::PointerMoved(at)], &mut v);
        for _ in 0..2 {
            frame(vec![click(true)], &mut v);
            frame(vec![click(false)], &mut v);
        }
        frame(vec![egui::Event::Text("6523".into())], &mut v);
        assert_eq!(v, 5500.0, "nothing is applied until Enter");
        frame(
            vec![egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Default::default(),
            }],
            &mut v,
        );
        frame(vec![], &mut v);
        assert_eq!(v, 6523.0);
        assert!(outs
            .iter()
            .any(|o| o.changed && o.drag_started && o.drag_stopped));
        assert!(
            !outs.iter().any(|o| o.reset),
            "the value box does not reset"
        );
    }

    /// Escape cancels a typed edit: the value is unchanged and the box closes.
    #[test]
    fn escape_cancels_a_typed_edit() {
        let ctx = super::super::tokens::testing::themed_ctx();
        let mut v = 5500.0_f32;
        let mut t = 0.0;
        let mut frame = |events: Vec<egui::Event>, v: &mut f32| {
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
                slider(ui, &TEMP, v, true);
            });
            out.textures_delta.clear();
        };
        let at = egui::pos2(760.0, 13.0);
        let click = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        };
        frame(vec![egui::Event::PointerMoved(at)], &mut v);
        for _ in 0..2 {
            frame(vec![click(true)], &mut v);
            frame(vec![click(false)], &mut v);
        }
        frame(vec![egui::Event::Text("6523".into())], &mut v);
        frame(
            vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Default::default(),
            }],
            &mut v,
        );
        frame(vec![], &mut v);
        assert_eq!(v, 5500.0, "Escape discards the typed number");
    }

    /// An edit left open while the row isn't drawn (section collapsed) or is disabled never
    /// commits, and doesn't come back as a stuck box: later typing changes nothing.
    #[test]
    fn an_abandoned_or_disabled_edit_commits_nothing() {
        // `mode`: 0 = hide the row after opening the box, 1 = disable it after opening the box.
        let run = |mode: u8| {
            let ctx = super::super::tokens::testing::themed_ctx();
            let root = std::cell::Cell::new(None);
            let mut v = 5500.0_f32;
            let mut t = 0.0;
            let mut frame = |events: Vec<egui::Event>, v: &mut f32, draw: bool, enabled: bool| {
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
                    root.set(Some(ui.id()));
                    if draw {
                        slider(ui, &TEMP, v, enabled);
                    }
                });
                out.textures_delta.clear();
            };
            let at = egui::pos2(760.0, 13.0);
            let click = |pressed| egui::Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: Default::default(),
            };
            let key = |k| egui::Event::Key {
                key: k,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Default::default(),
            };
            frame(vec![egui::Event::PointerMoved(at)], &mut v, true, true);
            for _ in 0..2 {
                frame(vec![click(true)], &mut v, true, true);
                frame(vec![click(false)], &mut v, true, true);
            }
            frame(vec![egui::Event::Text("6523".into())], &mut v, true, true);
            // The edit is open with typed text; now the row goes away / is disabled.
            frame(vec![], &mut v, mode == 1, false);
            frame(vec![key(egui::Key::Enter)], &mut v, mode == 1, false);
            // Back to normal: the stale edit must not resurface and commit anything.
            frame(vec![], &mut v, true, true);
            let edit_id = root.get().unwrap().with(TEMP.id).with("edit");
            let stuck = ctx.data(|d| d.get_temp::<String>(edit_id)).is_some();
            assert!(!stuck, "the abandoned edit left a box open");
            frame(vec![egui::Event::Text("9999".into())], &mut v, true, true);
            frame(vec![key(egui::Key::Enter)], &mut v, true, true);
            frame(vec![], &mut v, true, true);
            v
        };
        assert_eq!(run(0), 5500.0, "hidden row");
        assert_eq!(run(1), 5500.0, "disabled row");
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
