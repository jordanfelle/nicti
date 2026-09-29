//! Display color management state and its View-menu UI (#42, ADR-0042): the active monitor's
//! ICC profile, soft-proofing, and the gamut warning.
//!
//! Everything here is display-only. A change rebuilds a `nicti_calico::transform::
//! DisplayTransform` and pushes it into `ViewportResources`; nothing touches Tapetum's render
//! graph or caches, so toggling proofing or moving the window to another monitor costs no bake
//! work.

use nicti_calico::display_profile;
use nicti_calico::space::OutputSpace;
use nicti_calico::transform::{DisplayProfile, DisplayTransform, ProofSettings, RenderingIntent};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::viewport::ViewportResources;

pub struct ColorManagement {
    display: DisplayProfile,
    /// Why the display profile fell back to sRGB, if it did -- shown in the menu.
    display_note: Option<String>,
    /// Last-seen monitor of the window, to detect a move to another monitor.
    monitor: Option<isize>,
    soft_proof: bool,
    proof_space: OutputSpace,
    intent: RenderingIntent,
    gamut_warn: bool,
    /// The transform must be rebuilt and re-pushed to the GPU.
    dirty: bool,
    /// Last build failure, shown in the menu; the display falls back to plain sRGB meanwhile.
    error: Option<String>,
}

impl Default for ColorManagement {
    fn default() -> Self {
        Self::new()
    }
}

impl ColorManagement {
    pub fn new() -> Self {
        Self {
            display: DisplayProfile::Space(OutputSpace::Srgb),
            display_note: None,
            monitor: None,
            soft_proof: false,
            proof_space: OutputSpace::Srgb,
            intent: RenderingIntent::RelativeColorimetric,
            gamut_warn: false,
            dirty: true,
            error: None,
        }
    }

    /// The transform for the current settings. Pure (no GPU), so it is unit-tested directly.
    fn build_transform(&self) -> Result<DisplayTransform, String> {
        let proof = self.soft_proof.then_some(ProofSettings {
            space: self.proof_space,
            intent: self.intent,
        });
        DisplayTransform::build(&self.display, proof).map_err(|e| e.to_string())
    }

    /// The gamut overlay only means something while proofing.
    fn gamut_warn_active(&self) -> bool {
        self.gamut_warn && self.soft_proof
    }

    /// Shift+S toggles the gamut warning (Lightroom Classic's shortcut). Skipped while a text
    /// field has keyboard focus.
    pub fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        let pressed = ctx.input(|i| i.modifiers.shift_only() && i.key_pressed(egui::Key::S));
        if pressed && self.soft_proof {
            self.gamut_warn = !self.gamut_warn;
            self.dirty = true;
        }
    }

    pub fn show_menu(&mut self, ui: &mut egui::Ui) {
        ui.menu_button("Color", |ui| {
            let mut changed = false;
            changed |= ui.checkbox(&mut self.soft_proof, "Soft proof").changed();
            ui.add_enabled_ui(self.soft_proof, |ui| {
                egui::ComboBox::from_label("Proof space")
                    .selected_text(self.proof_space.name())
                    .show_ui(ui, |ui| {
                        for s in OutputSpace::ALL {
                            changed |= ui
                                .selectable_value(&mut self.proof_space, s, s.name())
                                .changed();
                        }
                    });
                egui::ComboBox::from_label("Intent")
                    .selected_text(intent_name(self.intent))
                    .show_ui(ui, |ui| {
                        for i in [
                            RenderingIntent::RelativeColorimetric,
                            RenderingIntent::Perceptual,
                        ] {
                            changed |= ui
                                .selectable_value(&mut self.intent, i, intent_name(i))
                                .changed();
                        }
                    });
                changed |= ui
                    .checkbox(&mut self.gamut_warn, "Gamut warning (Shift+S)")
                    .changed();
            });
            ui.separator();
            let profile_label = match &self.display {
                DisplayProfile::Space(s) => format!("Display: {} (default)", s.name()),
                DisplayProfile::Icc(_) => "Display: monitor ICC profile".to_string(),
            };
            ui.label(profile_label);
            if let Some(note) = &self.display_note {
                ui.weak(note);
            }
            if let Some(err) = &self.error {
                ui.colored_label(ui.visuals().error_fg_color, err);
            }
            if ui.button("Re-read display profile").clicked() {
                // Force a re-resolve on the next `sync`.
                self.monitor = None;
                self.dirty = true;
                ui.close();
            }
            if changed {
                self.dirty = true;
            }
        });
    }

    /// Call once per frame: follows the window across monitors, and re-pushes the transform to
    /// the GPU when anything changed.
    pub fn sync(&mut self, frame: &eframe::Frame) {
        let hwnd = native_window_handle(frame);
        let monitor = display_profile::current_monitor(hwnd);
        if monitor != self.monitor || (self.monitor.is_none() && self.dirty) {
            let (profile, note) = display_profile::resolve(hwnd);
            self.display = profile;
            self.display_note = note;
            self.monitor = monitor;
            self.dirty = true;
        }
        if !self.dirty {
            return;
        }
        self.dirty = false;
        let transform = match self.build_transform() {
            Ok(t) => {
                self.error = None;
                t
            }
            Err(e) => {
                self.error = Some(format!("color transform failed, using sRGB: {e}"));
                DisplayTransform::Direct(OutputSpace::Srgb)
            }
        };
        let Some(rs) = frame.wgpu_render_state() else {
            return;
        };
        if let Some(res) = rs
            .renderer
            .write()
            .callback_resources
            .get_mut::<ViewportResources>()
        {
            res.set_display_transform(&rs.device, &rs.queue, &transform, self.gamut_warn_active());
        }
    }
}

fn intent_name(i: RenderingIntent) -> &'static str {
    match i {
        RenderingIntent::Perceptual => "Perceptual",
        RenderingIntent::RelativeColorimetric => "Relative colorimetric",
        RenderingIntent::Saturation => "Saturation",
        RenderingIntent::AbsoluteColorimetric => "Absolute colorimetric",
    }
}

/// The native window handle as an integer (Windows HWND), or `None` if unavailable.
fn native_window_handle(frame: &eframe::Frame) -> Option<isize> {
    match frame.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_a_direct_srgb_transform() {
        let cm = ColorManagement::new();
        assert!(matches!(
            cm.build_transform(),
            Ok(DisplayTransform::Direct(OutputSpace::Srgb))
        ));
    }

    #[test]
    fn soft_proof_builds_a_lut() {
        let mut cm = ColorManagement::new();
        cm.soft_proof = true;
        cm.proof_space = OutputSpace::AdobeRgb;
        assert!(matches!(cm.build_transform(), Ok(DisplayTransform::Lut(_))));
    }

    #[test]
    fn gamut_warning_is_inert_without_proofing() {
        let mut cm = ColorManagement::new();
        cm.gamut_warn = true;
        assert!(!cm.gamut_warn_active());
        cm.soft_proof = true;
        assert!(cm.gamut_warn_active());
    }
}
