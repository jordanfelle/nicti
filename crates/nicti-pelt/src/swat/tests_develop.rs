//! The Develop panel under the headless harness, with no GPU engine at all: `DevelopDoc` alone
//! carries every control the panel reads and writes, which is the point of the doc/engine split.

use egui_kittest::kittest::{NodeT, Queryable};
use nicti_pounce::Pounce;
use nicti_tapetum::coat::ExposureParams;
use nicti_tapetum::stages::EXPOSURE;

use super::{click_at, harness};
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
    let h = panel_harness();
    for label in ["Auto", "Auto-level"] {
        // Auto-level lives inside the (closed) Crop section, so only Auto is always on screen.
        if let Some(button) = h.query_by_label(label) {
            assert!(
                button.accesskit_node().is_disabled(),
                "{label} needs the GPU engine"
            );
        }
    }
    assert!(
        h.get_by_label("Auto").accesskit_node().is_disabled(),
        "Auto tone renders and histograms, so it needs the engine"
    );
    // The before/after toggle is pure document state and stays live.
    assert!(!h
        .get_by_label("Showing: After")
        .accesskit_node()
        .is_disabled());
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
    let centre = h.get_by_label("Exposure").rect().center();
    click_at(&mut h, centre);
    click_at(&mut h, centre);
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

#[test]
fn develop_panel_snapshot() {
    let mut h = panel_harness();
    open_basic(&mut h);
    h.snapshot("develop_panel");
}
