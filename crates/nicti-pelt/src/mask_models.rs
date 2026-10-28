//! Which segmentation model a *new* AI mask uses, per target (#346). Persisted beside the catalog
//! (`<catalog>.mask-models.json`, temp file + rename like `preview_settings`). Only ever consulted
//! when a mask is created: the choice is written into that mask's recipe (`model_id` +
//! `model_version`), so changing it later never touches an existing mask.

use std::path::{Path, PathBuf};

use nicti_stalk::SegmentTarget;

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct MaskModelPrefs {
    /// `model_id` for Select Subject / Background; `None` = the build's default.
    pub subject: Option<String>,
    /// `model_id` for Select Sky.
    pub sky: Option<String>,
    #[serde(skip)]
    path: Option<PathBuf>,
}

/// Where the preference lives: `<catalog file name>.mask-models.json`, next to the catalog.
pub fn file_for(catalog_path: &Path) -> PathBuf {
    let mut name = catalog_path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| "nicti".into());
    name.push(".mask-models.json");
    catalog_path.with_file_name(name)
}

impl MaskModelPrefs {
    /// The saved choice, or the defaults when the file is absent, unreadable or corrupt (a bad file
    /// resets to the defaults rather than blocking mask creation). Remembers the file so
    /// [`Self::set`] can save.
    pub fn load(catalog_path: &Path) -> Self {
        let file = file_for(catalog_path);
        let mut prefs: MaskModelPrefs = std::fs::read_to_string(&file)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        prefs.path = Some(file);
        prefs
    }

    /// The `model_id` the user picked for `target`, if any.
    pub fn preferred(&self, target: SegmentTarget) -> Option<&str> {
        match target {
            SegmentTarget::Subject => self.subject.as_deref(),
            SegmentTarget::Sky => self.sky.as_deref(),
        }
    }

    /// Records (and saves) the choice for `target`; `None` goes back to the default. A save failure
    /// is returned for the panel to show; the choice still holds for this session.
    pub fn set(&mut self, target: SegmentTarget, model_id: Option<String>) -> Result<(), String> {
        match target {
            SegmentTarget::Subject => self.subject = model_id,
            SegmentTarget::Sky => self.sky = model_id,
        }
        self.save()
    }

    fn save(&self) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)
            .and_then(|()| std::fs::rename(&tmp, path))
            .map_err(|e| format!("Couldn't save the mask model choice: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_choice_is_per_target_saved_and_reloaded() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = dir.path().join("lib.nicti");
        let mut p = MaskModelPrefs::load(&catalog);
        assert_eq!(
            (
                p.preferred(SegmentTarget::Subject),
                p.preferred(SegmentTarget::Sky)
            ),
            (None, None)
        );
        p.set(SegmentTarget::Subject, Some("test.ai.small".into()))
            .unwrap();
        assert_eq!(p.preferred(SegmentTarget::Subject), Some("test.ai.small"));
        assert_eq!(p.preferred(SegmentTarget::Sky), None, "per target");
        let again = MaskModelPrefs::load(&catalog);
        assert_eq!(
            again.preferred(SegmentTarget::Subject),
            Some("test.ai.small")
        );
        // Back to the default.
        let mut again = again;
        again.set(SegmentTarget::Subject, None).unwrap();
        assert_eq!(
            MaskModelPrefs::load(&catalog).preferred(SegmentTarget::Subject),
            None
        );
        assert!(file_for(&catalog).ends_with("lib.nicti.mask-models.json"));
    }

    #[test]
    fn a_corrupt_or_foreign_file_resets_to_the_defaults_and_unknown_fields_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = dir.path().join("lib.nicti");
        std::fs::write(file_for(&catalog), "{ not json").unwrap();
        assert_eq!(MaskModelPrefs::load(&catalog).subject, None);
        std::fs::write(file_for(&catalog), r#"{"subject":"x.y","future":1}"#).unwrap();
        assert_eq!(
            MaskModelPrefs::load(&catalog).subject.as_deref(),
            Some("x.y")
        );
        // Without a file path (a test panel), set() still holds in memory and never errors.
        let mut p = MaskModelPrefs::default();
        p.set(SegmentTarget::Sky, Some("a.b".into())).unwrap();
        assert_eq!(p.preferred(SegmentTarget::Sky), Some("a.b"));
    }
}
