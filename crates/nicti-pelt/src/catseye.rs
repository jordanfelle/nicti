//! The Point Color eyedropper (#432): while `Tool::PointColor` owns the viewport, a click on the
//! photo adds a Point Color sample of the colour under the pointer and hands the viewport back to
//! the crop tool. The sample's colour is read from the rendered frame under the pointer (#454), so
//! a click lands on the colour the preview shows however heavily the photo is edited. It is the
//! frame's final colour: grain, a vignette and Point Color samples already set also act on it, but
//! the soft-box ranges are wide enough that none of that moves the pick out of its colour family.
//!
//! Named for the cat's-eye reflector: it picks the colour the light lands on.

use std::sync::Arc;

use nicti_tapetum::coat::{PointColorParams, PointColorSample, MAX_POINT_COLORS};
use nicti_tapetum::frame::{read_frame_region, FrameTexture};
use nicti_tapetum::gpu::GpuContext;
use nicti_tapetum::oklab;
use nicti_tapetum::stages::POINT_COLOR;

use crate::heal_tool::{HealUi, Tool};
use crate::mask_edit::Thumb;
use crate::mask_panel::to_norm;
use crate::render::DevelopDoc;

const SELECTED_ID: &str = "catseye-selected";
const THUMB_ID: &str = "catseye-thumb";
const VISUALIZE_ID: &str = "catseye-visualize";

/// Whether "Visualize range" is ticked.
pub fn visualize(ctx: &egui::Context) -> bool {
    ctx.data(|d| d.get_temp::<bool>(egui::Id::unique(VISUALIZE_ID)))
        .unwrap_or(false)
}

pub fn set_visualize(ctx: &egui::Context, on: bool) {
    ctx.data_mut(|d| d.insert_temp(egui::Id::unique(VISUALIZE_ID), on));
}

/// Half the side of the square window the pick averages, in rendered pixels (a click on grain or
/// noise should not pick the noise up): a 5x5 window.
const SAMPLE_RADIUS: i64 = 2;

/// The window `(x, y, width, height)` around normalized `(nx, ny)` on a `width x height` frame,
/// clipped to the frame.
fn sample_window(width: u32, height: u32, nx: f32, ny: f32) -> (u32, u32, u32, u32) {
    let at = |n: f32, len: u32| -> (u32, u32) {
        let c = (n * len as f32)
            .floor()
            .clamp(0.0, len.saturating_sub(1) as f32) as i64;
        let lo = (c - SAMPLE_RADIUS).max(0);
        let hi = (c + SAMPLE_RADIUS).min(i64::from(len) - 1);
        (lo as u32, (hi - lo + 1) as u32)
    };
    let (x, w) = at(nx, width);
    let (y, h) = at(ny, height);
    (x, y, w, h)
}

/// The mean RGB of `pixels`, the readback of a [`sample_window`].
fn mean_rgb(pixels: &[[f32; 4]]) -> [f32; 3] {
    let n = pixels.len().max(1) as f32;
    let mut acc = [0.0f32; 3];
    for p in pixels {
        for (a, v) in acc.iter_mut().zip(p) {
            *a += v;
        }
    }
    acc.map(|v| v / n)
}

/// The rendered frame at normalized `(nx, ny)`, as linear working-space (ProPhoto) RGB.
pub fn sample_rendered(gpu: &GpuContext, frame: &FrameTexture, nx: f32, ny: f32) -> [f32; 3] {
    let (x, y, w, h) = sample_window(frame.extent.width, frame.extent.height, nx, ny);
    mean_rgb(&read_frame_region(gpu, frame, x, y, w, h))
}

/// Which sample the Point Color section is editing (clamped by the caller to the live count).
pub fn selected(ctx: &egui::Context) -> usize {
    ctx.data(|d| d.get_temp::<usize>(egui::Id::unique(SELECTED_ID)))
        .unwrap_or(0)
}

pub fn set_selected(ctx: &egui::Context, index: usize) {
    ctx.data_mut(|d| d.insert_temp(egui::Id::unique(SELECTED_ID), index));
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
    ctx.data(|d| d.get_temp::<Option<String>>(egui::Id::unique(STATUS_ID)))
        .flatten()
}

pub fn set_status(ctx: &egui::Context, message: Option<String>) {
    ctx.data_mut(|d| d.insert_temp(egui::Id::unique(STATUS_ID), message));
}

/// The small as-shot thumbnail of the open photo, rebuilt when the photo changes. The Point Color
/// pick no longer reads it (it samples the rendered frame); the targeted-adjustment tool does.
pub(crate) fn thumb_for(ctx: &egui::Context, develop: &DevelopDoc) -> Arc<Thumb> {
    let id = egui::Id::unique(THUMB_ID);
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
    gpu: &GpuContext,
    frame: &FrameTexture,
) {
    if response.hovered() {
        ui.output_mut(|o| o.cursor_icon = egui::CursorIcon::Crosshair);
    }
    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        heal.tool = Tool::Idle;
        return;
    }
    if !response.clicked() {
        return;
    }
    let Some(p) = response.interact_pointer_pos() else {
        return;
    };
    // The frame on screen is the one sampled, so the click's position on it is the position in it.
    let n = to_norm(rect, develop.display_extent(), p);
    let working = sample_rendered(gpu, frame, n[0], n[1]);
    let mut params: PointColorParams = develop.stage_params(POINT_COLOR);
    match add_sample(&mut params, sample_from_working(working)) {
        Some(i) => {
            develop.set_stage_params(POINT_COLOR, &params);
            set_selected(ui.ctx(), i);
            heal.tool = Tool::Idle;
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
    fn the_sample_window_is_five_square_and_clipped_to_the_frame() {
        assert_eq!(sample_window(100, 50, 0.5, 0.5), (48, 23, 5, 5));
        assert_eq!(sample_window(100, 50, 0.0, 0.0), (0, 0, 3, 3));
        assert_eq!(sample_window(100, 50, 1.0, 1.0), (97, 47, 3, 3));
        assert_eq!(sample_window(1, 1, 0.7, 0.2), (0, 0, 1, 1));
        let w = sample_window(100, 50, f32::NAN, 2.0);
        assert!(
            w.0 + w.2 <= 100 && w.1 + w.3 <= 50 && w.2 > 0 && w.3 > 0,
            "{w:?}"
        );
    }

    #[test]
    fn the_pick_is_the_mean_of_the_window() {
        let px = [[0.2, 0.4, 0.0, 1.0], [0.4, 0.0, 0.2, 1.0]];
        assert_eq!(mean_rgb(&px), [0.3, 0.2, 0.1]);
    }

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
