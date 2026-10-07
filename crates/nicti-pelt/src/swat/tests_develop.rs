//! The Develop panel under the headless harness, with no GPU engine at all: `DevelopDoc` alone
//! carries every control the panel reads and writes, which is the point of the doc/engine split.

use egui_kittest::kittest::{NodeT, Queryable};
use nicti_pounce::Pounce;
use nicti_tapetum::coat::{DefringeParams, ExposureParams, LensParams};
use nicti_tapetum::stages::{DEFRINGE, EXPOSURE, LENS};

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

#[test]
fn develop_panel_snapshot() {
    let mut h = panel_harness();
    open_basic(&mut h);
    h.snapshot("develop_panel");
}
