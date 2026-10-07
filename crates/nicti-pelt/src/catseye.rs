//! The Point Color eyedropper (#432): while `Tool::PointColor` owns the viewport, a click on the
//! photo adds a Point Color sample of the colour under the pointer and hands the viewport back to
//! the crop tool. The sample's colour is read from the same small as-shot thumbnail the mask
//! colour eyedropper uses (`mask_edit::Thumb`), so it ignores edits already applied -- a heavily
//! tone-edited photo samples close to, not exactly at, what the preview shows. The soft-box ranges
//! are wide enough that this lands the sample on the right colour family; sampling the rendered
//! frame is a follow-up if it proves too coarse.
//!
//! Named for the cat's-eye reflector: it picks the colour the light lands on.

use std::sync::Arc;

use nicti_tapetum::coat::{PointColorParams, PointColorSample, MAX_POINT_COLORS};
use nicti_tapetum::oklab;
use nicti_tapetum::stages::POINT_COLOR;

use crate::heal_tool::{HealUi, Tool};
use crate::mask_edit::Thumb;
use crate::mask_panel::to_norm;
use crate::render::DevelopDoc;

const SELECTED_ID: &str = "catseye-selected";
const THUMB_ID: &str = "catseye-thumb";

/// Which sample the Point Color section is editing (clamped by the caller to the live count).
pub fn selected(ctx: &egui::Context) -> usize {
    ctx.data(|d| d.get_temp::<usize>(egui::Id::new(SELECTED_ID)))
        .unwrap_or(0)
}

pub fn set_selected(ctx: &egui::Context, index: usize) {
    ctx.data_mut(|d| d.insert_temp(egui::Id::new(SELECTED_ID), index));
}

/// A new sample of the colour `working` (linear ProPhoto RGB), every adjustment at its neutral.
pub fn sample_from_working(working: [f32; 3]) -> PointColorSample {
    let lab = oklab::lab_from_prophoto(working);
    PointColorSample {
        lum: lab[0],
        chroma: lab[1].hypot(lab[2]),
        hue: lab[2].atan2(lab[1]).to_degrees().rem_euclid(360.0),
        ..PointColorSample::default()
    }
    .sanitized()
}

/// Appends `sample`, returning its index, or `None` when all [`MAX_POINT_COLORS`] are used.
pub fn add_sample(params: &mut PointColorParams, sample: PointColorSample) -> Option<usize> {
    let at = usize::from(params.count);
    if at >= MAX_POINT_COLORS {
        return None;
    }
    params.samples[at] = sample;
    params.count += 1;
    Some(at)
}

/// Removes sample `index`, shifting the later ones down. Returns whether anything was removed.
pub fn remove_sample(params: &mut PointColorParams, index: usize) -> bool {
    let n = usize::from(params.count).min(MAX_POINT_COLORS);
    if index >= n {
        return false;
    }
    params.samples.copy_within(index + 1..n, index);
    params.samples[n - 1] = PointColorSample::default();
    params.count = (n - 1) as u8;
    true
}

const STATUS_ID: &str = "catseye-status";

/// The last pick problem (shown under the Point Color section), if any.
pub fn status(ctx: &egui::Context) -> Option<String> {
    ctx.data(|d| d.get_temp::<Option<String>>(egui::Id::new(STATUS_ID)))
        .flatten()
}

pub fn set_status(ctx: &egui::Context, message: Option<String>) {
    ctx.data_mut(|d| d.insert_temp(egui::Id::new(STATUS_ID), message));
}

fn thumb_for(ctx: &egui::Context, develop: &DevelopDoc) -> Arc<Thumb> {
    let id = egui::Id::new(THUMB_ID);
    let key = develop.frame_key();
    if let Some((k, t)) = ctx.data(|d| d.get_temp::<(u64, Arc<Thumb>)>(id)) {
        if k == key {
            return t;
        }
    }
    let t = Arc::new(Thumb::build(develop.frame_arc()));
    ctx.data_mut(|d| d.insert_temp(id, (key, Arc::clone(&t))));
    t
}

/// Handles the photo area while the eyedropper is active: a click samples, Escape cancels.
pub fn handle_viewport(
    ui: &mut egui::Ui,
    response: &egui::Response,
    rect: egui::Rect,
    develop: &mut DevelopDoc,
    heal: &mut HealUi,
) {
    if response.hovered() {
        ui.output_mut(|o| o.cursor_icon = egui::CursorIcon::Crosshair);
    }
    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        heal.tool = Tool::Crop;
        return;
    }
    if !response.clicked() {
        return;
    }
    let Some(p) = response.interact_pointer_pos() else {
        return;
    };
    let source = develop.source_extent();
    let n = to_norm(rect, source, p);
    let working = thumb_for(ui.ctx(), develop).sample_working(n[0], n[1]);
    let mut params: PointColorParams = develop.stage_params(POINT_COLOR);
    match add_sample(&mut params, sample_from_working(working)) {
        Some(i) => {
            develop.set_stage_params(POINT_COLOR, &params);
            set_selected(ui.ctx(), i);
            heal.tool = Tool::Crop;
        }
        None => set_status(
            ui.ctx(),
            Some(format!(
                "Point Color holds at most {MAX_POINT_COLORS} samples; delete one first."
            )),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_picked_colour_becomes_a_neutral_sample_of_that_colour() {
        let s = sample_from_working([0.4, 0.1, 0.1]);
        assert!(s.is_noop(), "a fresh sample must not change the photo yet");
        assert!(s.chroma > 0.02 && s.lum > 0.2 && s.lum < 0.9);
        // A reddish colour sits in the red/orange hue range.
        assert!(s.hue < 60.0 || s.hue > 330.0, "hue {}", s.hue);
    }

    #[test]
    fn samples_are_added_up_to_the_cap_and_removed_with_the_rest_shifting_down() {
        let mut p = PointColorParams::default();
        for i in 0..MAX_POINT_COLORS {
            let s = PointColorSample {
                hue: i as f32,
                ..Default::default()
            };
            assert_eq!(add_sample(&mut p, s), Some(i));
        }
        assert_eq!(add_sample(&mut p, PointColorSample::default()), None);
        assert!(remove_sample(&mut p, 2));
        assert_eq!(p.count as usize, MAX_POINT_COLORS - 1);
        assert_eq!(p.samples[2].hue, 3.0, "later samples shift down");
        assert_eq!(p.samples[MAX_POINT_COLORS - 1], PointColorSample::default());
        assert!(
            !remove_sample(&mut p, MAX_POINT_COLORS - 1),
            "past the live count"
        );
    }
}
