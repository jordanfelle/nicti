//! #373: keyword tagging in the Library toolbar. A text box tags the targeted photos (the grid's
//! selection, else the cursor photo) with a keyword path (`Events|Anthrocon`, created on demand);
//! chips list the cursor photo's keywords, each with an `x` that untags it from that photo only.
//! Every write goes through `XmpMeta` so it reaches the sidecar the same way marker edits do.

use nicti_lair::{CatalogStore, Keyword};

use crate::xmp_sync::XmpMeta;

#[derive(Default)]
pub struct KeywordUi {
    input: String,
    error: Option<String>,
    /// The cursor photo's keywords, read once per (photo, write) rather than once per frame.
    shown: Option<(i64, Vec<(i64, String)>)>,
}

impl KeywordUi {
    /// Draws the row. Returns `true` when a write changed the catalog's keyword links, so the
    /// caller can refresh the filter bar's options and facet counts.
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        meta: &XmpMeta,
        store: &dyn CatalogStore,
        target_count: usize,
        targets: &dyn Fn() -> Vec<i64>,
        cursor: Option<i64>,
    ) -> bool {
        let mut changed = false;
        if self.shown.as_ref().map(|(id, _)| *id) != cursor {
            self.shown = None;
        }
        if self.shown.is_none() {
            if let Some(id) = cursor {
                match chips(store, id) {
                    Ok(kws) => self.shown = Some((id, kws)),
                    Err(e) => self.error = Some(e),
                }
            }
        }
        let mut untag: Option<i64> = None;
        let mut submit = false;
        ui.horizontal_wrapped(|ui| {
            ui.label("Keywords:");
            let edit = ui.add_enabled(
                target_count > 0,
                egui::TextEdit::singleline(&mut self.input)
                    .hint_text("Name or Parent|Child")
                    .desired_width(180.0),
            );
            let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let clicked = ui
                .add_enabled(
                    target_count > 0 && !self.input.trim().is_empty(),
                    egui::Button::new(format!("Tag {}", target_count)),
                )
                .on_hover_text("Add this keyword to the selected photos (or the cursor photo).")
                .clicked();
            submit = enter || clicked;
            if let Some((_, kws)) = &self.shown {
                for (kid, label) in kws {
                    if ui
                        .small_button(format!("{label} \u{2715}"))
                        .on_hover_text("Remove this keyword from the cursor photo.")
                        .clicked()
                    {
                        untag = Some(*kid);
                    }
                }
            }
        });
        if submit && target_count > 0 {
            match meta.tag_path(&targets(), &self.input) {
                Ok(tagged) => {
                    self.input.clear();
                    self.error = None;
                    self.shown = None;
                    changed = tagged.is_some();
                }
                Err(e) => self.error = Some(e),
            }
        }
        if let (Some(id), Some(photo)) = (untag, cursor) {
            match meta.untag(&[photo], id) {
                Ok(()) => {
                    self.error = None;
                    self.shown = None;
                    changed = true;
                }
                Err(e) => self.error = Some(e),
            }
        }
        if let Some(e) = &self.error {
            ui.colored_label(
                egui::Color32::LIGHT_RED,
                format!("Keyword write failed: {e}"),
            );
        }
        changed
    }
}

/// `(id, "Parent \u{203A} Child")` for each keyword on `asset_id`. `Keyword::path` is an id path
/// (`/3/7/`), so the readable form is rebuilt from the parent chain.
fn chips(store: &dyn CatalogStore, asset_id: i64) -> Result<Vec<(i64, String)>, String> {
    let e = |e: nicti_lair::CatalogError| e.to_string();
    let tagged = store.keywords_for(asset_id).map_err(e)?;
    if tagged.is_empty() {
        return Ok(Vec::new());
    }
    let all: std::collections::HashMap<i64, Keyword> = store
        .list_keywords()
        .map_err(e)?
        .into_iter()
        .map(|k| (k.id, k))
        .collect();
    Ok(tagged
        .iter()
        .map(|k| {
            let mut names = vec![k.name.clone()];
            let mut parent = k.parent_id;
            // Bounded: a corrupt parent cycle must not hang the UI thread.
            for _ in 0..64 {
                let Some(p) = parent.and_then(|id| all.get(&id)) else {
                    break;
                };
                names.push(p.name.clone());
                parent = p.parent_id;
            }
            names.reverse();
            (k.id, names.join(" \u{203A} "))
        })
        .collect())
}
