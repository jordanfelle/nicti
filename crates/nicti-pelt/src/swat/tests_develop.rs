//! The Develop panel under the headless harness, with no GPU engine at all: `DevelopDoc` alone
//! carries every control the panel reads and writes, which is the point of the doc/engine split.

use egui_kittest::kittest::{NodeT, Queryable};
use nicti_pounce::Pounce;
use nicti_tapetum::coat::{
    CalibrationParams, ColorGradeParams, DefringeParams, ExposureParams, LensParams,
    PointColorParams, PointColorSample, PointCurveParams,
};
use nicti_tapetum::stages::{
    CALIBRATION, COLOR_GRADE, DEFRINGE, EXPOSURE, LENS, POINT_COLOR, POINT_CURVE,
};

use super::{click_at, double_click_at, harness, pass_time};
use crate::develop_panel::{self, AutoHintUi};
use crate::heal_tool::HealUi;
use crate::mask_panel::MaskUi;
use crate::render::DevelopDoc;

struct Panel {
    doc: DevelopDoc,
    hsl_band: usize,
    hint: AutoHintUi,
    heal: HealUi,
    mask: MaskUi,
    pounce: Pounce,
}

fn panel_harness() -> egui_kittest::Harness<'static, Panel> {
    harness(
        egui::vec2(320.0, 1200.0),
        |ui, p: &mut Panel| {
            develop_panel::show(
                ui,
                &mut p.doc,
                None,
                &mut p.hsl_band,
                &mut p.hint,
                &mut p.heal,
                &mut p.mask,
                &p.pounce,
            );
        },
        Panel {
            doc: DevelopDoc::new(),
            hsl_band: 0,
            hint: AutoHintUi::default(),
            heal: HealUi::new(),
            mask: MaskUi::new(),
            pounce: Pounce::new(0, 1, 1, || {}),
        },
    )
}

fn exposure(h: &egui_kittest::Harness<'_, Panel>) -> f32 {
    h.state().doc.stage_params::<ExposureParams>(EXPOSURE).stops
}

fn open_basic(h: &mut egui_kittest::Harness<'_, Panel>) {
    h.get_by_label("Basic").click();
    h.run_steps(3);
}

#[test]
fn without_an_engine_the_auto_buttons_are_disabled() {
    let mut h = panel_harness();
    assert!(
        h.get_by_label("Auto").accesskit_node().is_disabled(),
        "Auto tone renders and histograms, so it needs the engine"
    );
    // The before/after toggle is pure document state and stays live.
    assert!(!h
        .get_by_label("Showing: After")
        .accesskit_node()
        .is_disabled());

    // Auto-level lives in the Crop section, closed by default.
    h.get_by_label("Crop & Straighten").click();
    h.run_steps(3);
    assert!(h.get_by_label("Auto-level").accesskit_node().is_disabled());
}

#[test]
fn dragging_the_exposure_slider_edits_the_document() {
    let mut h = panel_harness();
    open_basic(&mut h);
    assert_eq!(exposure(&h), 0.0);
    let before = h.state().doc.document().clone();

    // The slider's node covers its track plus an 8 px grab margin either side. Exposure spans
    // -5..=5, so 80 % of the way along is +3 stops (snapped to the slider's step).
    let rect = h.get_by_label("Exposure").rect();
    let track = (rect.left() + 8.0)..(rect.right() - 8.0);
    let x = track.start + 0.8 * (track.end - track.start);
    click_at(&mut h, egui::pos2(x, rect.center().y));

    let stops = exposure(&h);
    assert!(
        (stops - 3.0).abs() < 0.1,
        "expected about +3 stops, got {stops}"
    );
    assert_ne!(
        h.state().doc.document(),
        &before,
        "the edit landed in the document the catalog persists"
    );
}

#[test]
fn double_clicking_a_slider_resets_it() {
    let mut h = panel_harness();
    open_basic(&mut h);
    h.state_mut()
        .doc
        .set_stage_params(EXPOSURE, &ExposureParams { stops: 2.0 });
    h.run_steps(2);
    // Off-centre on purpose: a single click on the track jumps the value to that point (+3 here),
    // and only the double-click reset lands on the default 0, so this can't pass by coincidence.
    let rect = h.get_by_label("Exposure").rect();
    let at = egui::pos2(rect.right() - 20.0, rect.center().y);
    click_at(&mut h, at);
    assert!(
        exposure(&h) > 3.0,
        "a single click moves the thumb to the click"
    );
    // Let the double-click window lapse, or this click and the next two would read as a triple.
    pass_time(&mut h, 0.5);
    double_click_at(&mut h, at);
    assert_eq!(exposure(&h), 0.0, "double-click restores the default");
}

#[test]
fn the_before_after_toggle_flips_the_document_flag() {
    let mut h = panel_harness();
    assert!(!h.state().doc.show_before);
    h.get_by_label("Showing: After").click();
    h.run_steps(2);
    assert!(h.state().doc.show_before);
    assert!(
        h.query_by_label("Showing: Before").is_some(),
        "the button relabels itself"
    );
}

fn open_lens(h: &mut egui_kittest::Harness<'_, Panel>) {
    h.get_by_label("Lens Corrections").click();
    h.run_steps(3);
}

#[test]
fn the_lens_toggle_writes_remove_ca_and_a_photo_without_a_profile_has_no_profile_switch() {
    let mut h = panel_harness();
    open_lens(&mut h);
    assert!(!h.state().doc.stage_params::<LensParams>(LENS).remove_ca);
    h.get_by_label("Remove chromatic aberration").click();
    h.run_steps(2);
    let lens = h.state().doc.stage_params::<LensParams>(LENS);
    assert!(lens.remove_ca);
    assert!(
        lens.embedded_profile,
        "the profile switch stays at its default"
    );
    // The synthetic frame carries no DNG opcodes, so offering the switch would be a dead control.
    assert!(
        h.query_by_label("Use embedded lens profile").is_none(),
        "no embedded profile, no switch"
    );
}

#[test]
fn the_defringe_slider_edits_the_document_and_reset_all_clears_the_lens_stages() {
    let mut h = panel_harness();
    open_lens(&mut h);
    assert!(h
        .state()
        .doc
        .stage_params::<DefringeParams>(DEFRINGE)
        .is_noop());
    // Purple Amount spans 0..=1: a click 60 % of the way along lands near 0.6.
    let rect = h.get_by_label("Purple Amount").rect();
    let track = (rect.left() + 8.0)..(rect.right() - 8.0);
    let x = track.start + 0.6 * (track.end - track.start);
    click_at(&mut h, egui::pos2(x, rect.center().y));
    let amount = h
        .state()
        .doc
        .stage_params::<DefringeParams>(DEFRINGE)
        .purple_amount;
    assert!(
        (amount - 0.6).abs() < 0.1,
        "expected about 0.6, got {amount}"
    );
    // The green channel and both hue windows are untouched.
    let d = h.state().doc.stage_params::<DefringeParams>(DEFRINGE);
    assert_eq!(d.green_amount, 0.0);
    assert_eq!((d.purple_hue_lo, d.purple_hue_hi), (0.3, 0.7));

    h.state_mut().doc.set_stage_params(
        LENS,
        &LensParams {
            remove_ca: true,
            embedded_profile: true,
            ..Default::default()
        },
    );
    h.run_steps(2);
    h.get_by_label("Reset all").click();
    h.run_steps(2);
    // Reset removes the entries; the sections re-read their (default) values on the next frame.
    assert!(!h.state().doc.stage_params::<LensParams>(LENS).remove_ca);
    assert!(h
        .state()
        .doc
        .stage_params::<DefringeParams>(DEFRINGE)
        .is_noop());
}

/// #381: the Calibration section's sliders write `nicti.calibration` (bipolar: the middle of the
/// track is 0, a click 75 % along lands near +0.5), and Reset all clears them.
#[test]
fn the_calibration_sliders_edit_the_document_and_reset_all_clears_them() {
    let mut h = panel_harness();
    open_section(&mut h, "Calibration");
    assert!(h
        .state()
        .doc
        .stage_params::<CalibrationParams>(CALIBRATION)
        .is_noop());
    let rect = h.get_by_label("Shadows Tint").rect();
    let track = (rect.left() + 8.0)..(rect.right() - 8.0);
    let x = track.start + 0.75 * (track.end - track.start);
    click_at(&mut h, egui::pos2(x, rect.center().y));
    let c = h.state().doc.stage_params::<CalibrationParams>(CALIBRATION);
    assert!(
        (c.shadow_tint - 0.5).abs() < 0.15,
        "expected about +0.5, got {}",
        c.shadow_tint
    );
    // The other sliders are untouched and the stage is a real, non-default entry now.
    assert_eq!((c.red_hue, c.blue_sat), (0.0, 0.0));
    assert!(!c.is_noop());

    h.get_by_label("Reset all").click();
    h.run_steps(2);
    assert!(h
        .state()
        .doc
        .stage_params::<CalibrationParams>(CALIBRATION)
        .is_noop());
}

#[test]
fn develop_panel_snapshot() {
    let mut h = panel_harness();
    open_basic(&mut h);
    h.snapshot("develop_panel");
}

fn open_section(h: &mut egui_kittest::Harness<'_, Panel>, title: &str) {
    h.get_by_label(title).click();
    h.run_steps(3);
}

/// A point inside a widget's rect at fractions `(fx, fy_up)` of its width and height (y up, as
/// the curve graph is drawn).
fn at_fraction(rect: egui::Rect, fx: f32, fy_up: f32) -> egui::Pos2 {
    egui::pos2(
        rect.left() + fx * rect.width(),
        rect.bottom() - fy_up * rect.height(),
    )
}

#[test]
fn clicking_the_rgb_curve_adds_a_point_and_double_clicking_it_removes_it() {
    let mut h = panel_harness();
    open_section(&mut h, "Tone Curve");
    h.get_by_label("RGB").click();
    h.run_steps(3);
    assert!(h
        .state()
        .doc
        .stage_params::<PointCurveParams>(POINT_CURVE)
        .is_noop());

    let rect = h.get_by_label("RGB curve").rect();
    click_at(&mut h, at_fraction(rect, 0.5, 0.75));
    let master = h
        .state()
        .doc
        .stage_params::<PointCurveParams>(POINT_CURVE)
        .master;
    assert_eq!(master.len(), 3, "endpoints plus the new point: {master:?}");
    assert!(
        (master[1][0] - 0.5).abs() < 0.03 && (master[1][1] - 0.75).abs() < 0.03,
        "the point lands where clicked: {:?}",
        master[1]
    );

    pass_time(&mut h, 0.5);
    double_click_at(&mut h, at_fraction(rect, 0.5, 0.75));
    let master = h
        .state()
        .doc
        .stage_params::<PointCurveParams>(POINT_CURVE)
        .master;
    // Back to just the endpoints is the identity, which is stored as nothing at all.
    assert!(master.is_empty(), "double-click removed it: {master:?}");
    assert!(h
        .state()
        .doc
        .stage_params::<PointCurveParams>(POINT_CURVE)
        .is_noop());
}

#[test]
fn point_curves_are_edited_per_channel() {
    let mut h = panel_harness();
    open_section(&mut h, "Tone Curve");
    h.get_by_label("Blue").click();
    h.run_steps(3);
    let rect = h.get_by_label("Blue curve").rect();
    click_at(&mut h, at_fraction(rect, 0.3, 0.6));
    let c = h.state().doc.stage_params::<PointCurveParams>(POINT_CURVE);
    assert_eq!(c.blue.len(), 3);
    assert!(c.master.is_empty() && c.red.is_empty() && c.green.is_empty());
}

#[test]
fn clicking_a_band_dot_selects_that_hsl_band() {
    let mut h = panel_harness();
    open_section(&mut h, "HSL");
    assert_eq!(h.state().hsl_band, 0);
    h.get_by_label("Blue").click();
    h.run_steps(3);
    assert_eq!(h.state().hsl_band, 5);
}

#[test]
fn the_grading_wheel_sets_hue_and_saturation_and_double_click_resets_them() {
    let mut h = panel_harness();
    open_section(&mut h, "Color Grading");
    // Midtones is the default wheel.
    let rect = h.get_by_label("Midtones wheel").rect();
    // Halfway out along +x is hue 0 (red) at saturation 0.5 (the wheel's radius is 56).
    let at = egui::pos2(rect.center().x + 28.0, rect.center().y);
    click_at(&mut h, at);
    let g = h.state().doc.stage_params::<ColorGradeParams>(COLOR_GRADE);
    assert!(
        (g.midtones.sat - 0.5).abs() < 0.08,
        "saturation {}",
        g.midtones.sat
    );
    assert!(
        g.midtones.hue < 8.0 || g.midtones.hue > 352.0,
        "hue {}",
        g.midtones.hue
    );
    assert_eq!(g.shadows.sat, 0.0, "only the selected wheel changed");

    pass_time(&mut h, 0.5);
    double_click_at(&mut h, at);
    let g = h.state().doc.stage_params::<ColorGradeParams>(COLOR_GRADE);
    assert_eq!((g.midtones.hue, g.midtones.sat), (0.0, 0.0));
}

#[test]
fn point_color_samples_are_edited_and_deleted_from_the_panel() {
    let mut h = panel_harness();
    let mut pc = PointColorParams {
        count: 1,
        ..Default::default()
    };
    pc.samples[0] = PointColorSample {
        lum: 0.6,
        chroma: 0.12,
        hue: 40.0,
        ..Default::default()
    };
    h.state_mut().doc.set_stage_params(POINT_COLOR, &pc);
    open_section(&mut h, "Point Color");
    assert!(h.query_by_label("Point color sample 1").is_some());

    // Hue Shift spans -1..=1: 80 % of the way along is about +0.6.
    let rect = h.get_by_label("Hue Shift").rect();
    let track = (rect.left() + 8.0)..(rect.right() - 8.0);
    let x = track.start + 0.8 * (track.end - track.start);
    click_at(&mut h, egui::pos2(x, rect.center().y));
    let s = h.state().doc.stage_params::<PointColorParams>(POINT_COLOR);
    assert!(
        (s.samples[0].hue_shift - 0.6).abs() < 0.12,
        "hue shift {}",
        s.samples[0].hue_shift
    );
    assert_eq!(
        s.samples[0].hue, 40.0,
        "the sampled colour itself is untouched"
    );

    h.get_by_label("Delete sample").click();
    h.run_steps(3);
    let s = h.state().doc.stage_params::<PointColorParams>(POINT_COLOR);
    assert_eq!(s.count, 0);
    assert!(h.query_by_label("Point color sample 1").is_none());
}

#[test]
fn the_eyedropper_arms_the_point_color_tool_and_reset_all_clears_the_new_stages() {
    let mut h = panel_harness();
    open_section(&mut h, "Point Color");
    assert!(!h.state().heal.point_color_active());
    h.get_by_label("Pick a colour from the photo").click();
    h.run_steps(3);
    assert!(h.state().heal.point_color_active());

    h.state_mut().doc.set_stage_params(
        COLOR_GRADE,
        &ColorGradeParams {
            global: nicti_tapetum::coat::GradeWheel {
                hue: 10.0,
                sat: 0.4,
                lum: 0.0,
            },
            ..Default::default()
        },
    );
    h.state_mut().doc.set_stage_params(
        POINT_CURVE,
        &PointCurveParams {
            red: vec![[0.0, 0.1], [1.0, 0.9]],
            ..Default::default()
        },
    );
    h.run_steps(2);
    h.get_by_label("Reset all").click();
    h.run_steps(2);
    assert!(h
        .state()
        .doc
        .stage_params::<ColorGradeParams>(COLOR_GRADE)
        .is_noop());
    assert!(h
        .state()
        .doc
        .stage_params::<PointCurveParams>(POINT_CURVE)
        .is_noop());
}
