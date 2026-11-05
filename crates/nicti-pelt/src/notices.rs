//! Third-party notices view (#689): the LGPL notice and CC BY-SA database attribution that the
//! bundled lens-correction library requires, shown in the app's settings panel. The text is the
//! same file the Windows installer ships (`assets/notices/`), embedded at build time so the view
//! can never drift from what the installer shows.

use eframe::egui;

/// The notices text, embedded from the repo-root `assets/notices/` directory.
pub const THIRD_PARTY_NOTICES: &str =
    include_str!("../../../assets/notices/THIRD-PARTY-NOTICES.txt");
/// Full licence texts, shown on request (they are long).
pub const LGPL_3_0: &str = include_str!("../../../assets/notices/LICENSE-LGPL-3.0.txt");
pub const LGPL_2_1: &str = include_str!("../../../assets/notices/LICENSE-LGPL-2.1.txt");
pub const GPL_3_0: &str = include_str!("../../../assets/notices/LICENSE-GPL-3.0.txt");

fn text_box(ui: &mut egui::Ui, id: &str, text: &str) {
    egui::ScrollArea::vertical()
        .id_salt(id)
        .max_height(240.0)
        .show(ui, |ui| {
            ui.add(egui::Label::new(egui::RichText::new(text).monospace()).wrap());
        });
}

/// Collapsed headings in the settings panel; nothing is laid out while closed.
pub fn show(ui: &mut egui::Ui) {
    ui.collapsing("Third-party notices", |ui| {
        text_box(ui, "notices_main", THIRD_PARTY_NOTICES);
        ui.collapsing("GNU LGPL v3.0 (full text)", |ui| {
            text_box(ui, "notices_lgpl", LGPL_3_0);
        });
        ui.collapsing("GNU LGPL v2.1 (full text, LibRaw)", |ui| {
            text_box(ui, "notices_lgpl21", LGPL_2_1);
        });
        ui.collapsing("GNU GPL v3.0 (incorporated by the LGPL)", |ui| {
            text_box(ui, "notices_gpl", GPL_3_0);
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notices_carry_the_obligations_the_licences_require() {
        // LGPL notice + the LensFun contributors' copyright line.
        assert!(THIRD_PARTY_NOTICES.contains("LGPL-3.0-or-later"));
        assert!(THIRD_PARTY_NOTICES.contains("WITHOUT ANY WARRANTY"));
        assert!(THIRD_PARTY_NOTICES.contains("Andrew Zabolotny"));
        assert!(THIRD_PARTY_NOTICES.contains("github.com/jordanfelle/nicti"));
        // CC BY-SA 3.0 attribution with a link to the licence.
        assert!(THIRD_PARTY_NOTICES.contains("https://creativecommons.org/licenses/by-sa/3.0/"));
        // The full texts are the real licences, not stubs.
        assert!(LGPL_3_0.contains("GNU LESSER GENERAL PUBLIC LICENSE"));
        assert!(LGPL_3_0.contains("Version 3, 29 June 2007"));
        assert!(GPL_3_0.contains("GNU GENERAL PUBLIC LICENSE"));
        // LibRaw (LGPL-2.1 arm of its dual licence): the notice names the fork, the chosen licence,
        // its copyright holders and where the source lives; the full text is the real LGPL-2.1.
        assert!(THIRD_PARTY_NOTICES.contains("LibRaw LLC"));
        assert!(THIRD_PARTY_NOTICES.contains("LGPL-2.1"));
        assert!(THIRD_PARTY_NOTICES.contains("github.com/yogthos/LibRaw"));
        assert!(THIRD_PARTY_NOTICES.contains("crates/nicti-cornea/vendor/LibRaw"));
        assert!(THIRD_PARTY_NOTICES.contains("Dave Coffin"));
        assert!(LGPL_2_1.contains("GNU LESSER GENERAL PUBLIC LICENSE"));
        assert!(LGPL_2_1.contains("Version 2.1, February 1999"));
    }
}
