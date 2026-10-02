//! Rendered-preview settings (#145): which photos get a rendered screen preview and which views
//! use it. Persisted beside the catalog (`<catalog>.previews.json`, temp file + rename like
//! `cache_settings`'s cap) so people can trade preview fidelity against background work.

use std::path::{Path, PathBuf};

use crate::eyeshine::RenderPolicy;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct PreviewSettings {
    pub policy: RenderPolicy,
    /// Use rendered previews in the loupe's pre-decode fallback.
    pub loupe: bool,
    /// ...in Library grid cells.
    pub grid: bool,
}

impl Default for PreviewSettings {
    fn default() -> Self {
        PreviewSettings {
            policy: RenderPolicy::default(),
            loupe: true,
            grid: true,
        }
    }
}

impl PreviewSettings {
    /// The policy a given surface effectively runs under: `Off` when that surface is switched off.
    pub fn policy_for(&self, surface: Surface) -> RenderPolicy {
        let on = match surface {
            Surface::Loupe => self.loupe,
            Surface::Grid => self.grid,
        };
        if on {
            self.policy
        } else {
            RenderPolicy::Off
        }
    }
}

impl PreviewSettings {
    /// The policy for work not tied to one view (a render queued after an edit is saved): the
    /// chosen policy while any view uses rendered previews, else `Off`.
    pub fn effective_policy(&self) -> RenderPolicy {
        if self.loupe || self.grid {
            self.policy
        } else {
            RenderPolicy::Off
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    Loupe,
    Grid,
}

/// Where the settings live: `<catalog file name>.previews.json`, next to the catalog.
pub fn settings_file_for(catalog_path: &Path) -> PathBuf {
    let mut name = catalog_path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| "nicti".into());
    name.push(".previews.json");
    catalog_path.with_file_name(name)
}

/// The saved settings, or the defaults when the file is absent, unreadable or corrupt (a bad
/// file never disables previews -- it just resets to the defaults).
pub fn load(catalog_path: &Path) -> PreviewSettings {
    std::fs::read_to_string(settings_file_for(catalog_path))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Writes via a temp file + rename so a crash never leaves a half-written document.
pub fn save(catalog_path: &Path, settings: &PreviewSettings) -> std::io::Result<()> {
    let path = settings_file_for(catalog_path);
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string(settings).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)
}

/// The Library view's "Rendered previews" panel. Returns `true` when a setting changed (the caller
/// persists it with [`save`] and reacts, e.g. dropping cached renders when switched to `Off`).
pub fn show(ui: &mut egui::Ui, settings: &mut PreviewSettings) -> bool {
    let before = *settings;
    ui.collapsing("Rendered previews", |ui| {
        ui.label(
            "Edited photos are shown from a render of their edits instead of the camera's \
             preview. \"All photos\" also renders unedited photos so every preview uses Nicti's \
             own colour (costs a RAW decode per photo, in the background).",
        );
        egui::ComboBox::from_label("Render previews for")
            .selected_text(policy_name(settings.policy))
            .show_ui(ui, |ui| {
                for p in [
                    RenderPolicy::Off,
                    RenderPolicy::EditedOnly,
                    RenderPolicy::All,
                ] {
                    ui.selectable_value(&mut settings.policy, p, policy_name(p));
                }
            });
        ui.add_enabled_ui(settings.policy != RenderPolicy::Off, |ui| {
            ui.checkbox(&mut settings.loupe, "Use in the loupe");
            ui.checkbox(
                &mut settings.grid,
                "Flag stale previews in the library grid",
            );
        });
    });
    *settings != before
}

fn policy_name(p: RenderPolicy) -> &'static str {
    match p {
        RenderPolicy::Off => "Off (camera previews only)",
        RenderPolicy::EditedOnly => "Edited photos",
        RenderPolicy::All => "All photos",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("cat.db")
    }

    #[test]
    fn defaults_render_edited_photos_everywhere() {
        let s = PreviewSettings::default();
        assert_eq!(s.policy, RenderPolicy::EditedOnly);
        assert!(s.loupe && s.grid);
    }

    #[test]
    fn a_missing_file_gives_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(&catalog(&dir)), PreviewSettings::default());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let s = PreviewSettings {
            policy: RenderPolicy::All,
            loupe: true,
            grid: false,
        };
        save(&catalog(&dir), &s).unwrap();
        assert_eq!(load(&catalog(&dir)), s);
        assert!(settings_file_for(&catalog(&dir)).ends_with("cat.db.previews.json"));
    }

    #[test]
    fn a_corrupt_file_or_unknown_policy_gives_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = settings_file_for(&catalog(&dir));
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(load(&catalog(&dir)), PreviewSettings::default());
        std::fs::write(&path, r#"{"policy":"Sometimes"}"#).unwrap();
        assert_eq!(load(&catalog(&dir)), PreviewSettings::default());
    }

    #[test]
    fn missing_fields_fall_back_per_field() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            settings_file_for(&catalog(&dir)),
            r#"{"policy":"Off","grid":false}"#,
        )
        .unwrap();
        let s = load(&catalog(&dir));
        assert_eq!(s.policy, RenderPolicy::Off);
        assert!(s.loupe && !s.grid);
    }

    #[test]
    fn a_switched_off_surface_runs_under_off() {
        let s = PreviewSettings {
            policy: RenderPolicy::All,
            grid: false,
            ..PreviewSettings::default()
        };
        assert_eq!(s.policy_for(Surface::Loupe), RenderPolicy::All);
        assert_eq!(s.policy_for(Surface::Grid), RenderPolicy::Off);
    }
}
