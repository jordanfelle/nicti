//! The Develop view's right-side edit panel (#46): Basic tone, Tone Curve, HSL, Detail
//! (Sharpen/Noise Reduction), a live histogram, an Auto button, and the before/after toggle. Pure
//! UI glue over `render::DevelopView`'s `stage_params`/`set_stage_params`/`reset_stage` -- every
//! slider here reads/writes one `coat.rs` params struct, the same shape a catalog-backed edit
//! (once #31 lands persistence) will read too.

use nicti_tapetum::coat::{
    ExposureParams, HslBand, HslParams, NoiseReductionParams, SharpenParams, ToneCurveParams,
    ToneParams, VibranceParams, WbParams,
};
use nicti_tapetum::frame::FrameTexture;
use nicti_tapetum::stages::{
    EXPOSURE, HSL, NOISE_REDUCTION, SHARPEN, TONE, TONE_CURVE, VIBRANCE, WB,
};

use crate::render::DevelopView;

const HSL_BAND_NAMES: [&str; 8] = ["R", "O", "Y", "G", "A", "B", "P", "M"];

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
pub fn show(
    ui: &mut egui::Ui,
    develop: &mut DevelopView,
    current_frame: &FrameTexture,
    hsl_band_selected: &mut usize,
) {
    ui.heading("Develop");

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
            develop.apply_auto_tone();
        }
    });

    show_histogram(ui, develop, current_frame);

    ui.separator();
    ui.collapsing("Basic", |ui| {
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

    ui.separator();
    if ui.button("Reset all").clicked() {
        for id in [
            WB,
            EXPOSURE,
            TONE,
            TONE_CURVE,
            VIBRANCE,
            HSL,
            SHARPEN,
            NOISE_REDUCTION,
        ] {
            develop.reset_stage(id);
        }
    }
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
