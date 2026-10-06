//! Display color management state and its View-menu UI (#42, ADR-0042): the active monitor's
//! ICC profile, soft-proofing, and the gamut warning.
//!
//! Everything here is display-only. A change rebuilds a `nicti_calico::transform::
//! DisplayTransform` and pushes it into `ViewportResources`; nothing touches Tapetum's render
//! graph or caches, so toggling proofing or moving the window to another monitor costs no bake
//! work.

use nicti_calico::display_profile;
use nicti_calico::source_transform::SourceTransforms;
use nicti_calico::space::OutputSpace;
use nicti_calico::transform::{DisplayKind, DisplayProfile, DisplayTransform};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::sync::Arc;

use crate::viewport::ViewportResources;

pub struct ColorManagement {
    display: DisplayProfile,
    /// The baked display half of the transform for `display`. Building it probes the profile and
    /// may bake a LUT (6-90 ms), so it is cached and only rebuilt when the monitor profile
    /// changes -- toggling proofing or the gamut warning just swaps the cheap proof half.
    display_kind: Option<DisplayKind>,
    /// JPEG-sourced pixels (grid thumbnails, T0/T2 previews) -> `display` (#319). Rebuilt with the
    /// profile; shared with the worker threads that decode thumbnails.
    source: Arc<SourceTransforms>,
    /// Bumped whenever `display` changes, so anything cached through `source` (thumbnail textures,
    /// the loupe/tile preview textures) can tell it was converted for a stale monitor.
    generation: u64,
    /// Why the display profile fell back to sRGB, if it did -- shown in the menu.
    display_note: Option<String>,
    /// Last-seen monitor of the window, to detect a move to another monitor.
    monitor: Option<isize>,
    soft_proof: bool,
    proof_space: OutputSpace,
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
            display_kind: None,
            source: Arc::new(SourceTransforms::new(&DisplayProfile::Space(
                OutputSpace::Srgb,
            ))),
            generation: 0,
            display_note: None,
            monitor: None,
            soft_proof: false,
            proof_space: OutputSpace::Srgb,
            gamut_warn: false,
            dirty: true,
            error: None,
        }
    }

    /// The source -> display converter for JPEG-sourced pixels, valid for [`Self::generation`].
    pub fn source_transforms(&self) -> Arc<SourceTransforms> {
        self.source.clone()
    }

    /// Changes whenever the display profile does (monitor move, "Re-read display profile").
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The transform for the current settings. Pure (no GPU), so it is unit-tested directly.
    fn build_transform(&mut self) -> Result<DisplayTransform, String> {
        let kind = match &self.display_kind {
            Some(k) => k.clone(),
            None => {
                // A hostile or corrupt monitor profile must degrade to sRGB, not take down the UI
                // thread: `moxcms` indexes/unwraps on some malformed inputs.
                let display = self.display.clone();
                let k = std::panic::catch_unwind(move || DisplayTransform::build(&display, None))
                    .map_err(|_| "the color engine crashed on this profile".to_string())?
                    .map_err(|e| e.to_string())?
                    .kind;
                self.display_kind = Some(k.clone());
                k
            }
        };
        Ok(DisplayTransform {
            proof: self.soft_proof.then_some(self.proof_space),
            kind,
        })
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
            self.source = Arc::new(SourceTransforms::new(&self.display));
            self.generation += 1;
            self.display_kind = None;
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
                DisplayTransform::exact(OutputSpace::Srgb)
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
    use nicti_calico::transform::DisplayKind;

    #[test]
    fn default_is_a_direct_srgb_transform() {
        let mut cm = ColorManagement::new();
        let t = cm.build_transform().unwrap();
        assert!(t.proof.is_none());
        assert!(matches!(t.kind, DisplayKind::Space(OutputSpace::Srgb)));
    }

    #[test]
    fn soft_proof_sets_the_proof_space() {
        let mut cm = ColorManagement::new();
        cm.soft_proof = true;
        cm.proof_space = OutputSpace::AdobeRgb;
        let t = cm.build_transform().unwrap();
        assert_eq!(t.proof, Some(OutputSpace::AdobeRgb));
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
