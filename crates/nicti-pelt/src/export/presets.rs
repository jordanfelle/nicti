//! Saved export presets (#57): `<catalog>.export-presets.json`, next to the catalog file (the same
//! sibling-file pattern `cache_settings` uses), written via temp file + rename.
//!
//! Three built-ins are always present and never written to disk; user presets can't reuse their
//! names. The file also remembers the last-used settings, so the dialog reopens where the user
//! left it. A missing or corrupt file simply means "built-ins only, default settings" -- it never
//! blocks exporting.

use std::path::{Path, PathBuf};

use nicti_preen::spec::{
    BitDepth, ExportPreset, ExportSpace, ExportSpec, FormatSpec, ResizeMode, ResizeSpec,
    Subsampling, TiffCompression,
};
use serde::{Deserialize, Serialize};

pub fn presets_file_for(catalog_path: &Path) -> PathBuf {
    let mut name = catalog_path.as_os_str().to_os_string();
    name.push(".export-presets.json");
    PathBuf::from(name)
}

/// The built-in presets.
pub fn builtin_presets() -> Vec<ExportPreset> {
    let preset = |name: &str, spec: ExportSpec| ExportPreset {
        name: name.to_string(),
        spec,
        ..ExportPreset::default()
    };
    vec![
        preset(
            "JPEG - full size, sRGB",
            ExportSpec {
                format: FormatSpec::Jpeg {
                    quality: 92,
                    subsampling: Subsampling::S420,
                },
                ..ExportSpec::default()
            },
        ),
        preset(
            "Web - 2048 px, sRGB",
            ExportSpec {
                format: FormatSpec::Jpeg {
                    quality: 80,
                    subsampling: Subsampling::S420,
                },
                resize: ResizeSpec {
                    mode: ResizeMode::LongEdge(2048),
                    dont_enlarge: true,
                },
                dpi: 72,
                ..ExportSpec::default()
            },
        ),
        preset(
            "TIFF - 16-bit, Adobe RGB",
            ExportSpec {
                format: FormatSpec::Tiff {
                    depth: BitDepth::Sixteen,
                    compression: TiffCompression::Deflate,
                },
                color_space: ExportSpace::AdobeRgb,
                ..ExportSpec::default()
            },
        ),
    ]
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct PresetFile {
    presets: Vec<ExportPreset>,
    last: Option<ExportSpec>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PresetError {
    EmptyName,
    ReservedName,
}

impl std::fmt::Display for PresetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PresetError::EmptyName => write!(f, "give the preset a name"),
            PresetError::ReservedName => write!(f, "that name is used by a built-in preset"),
        }
    }
}

/// The preset list plus the last-used settings.
#[derive(Default)]
pub struct PresetStore {
    user: Vec<ExportPreset>,
    pub last: ExportSpec,
}

impl PresetStore {
    /// Loads `path`; a missing, unreadable or corrupt file yields the defaults.
    pub fn load(path: &Path) -> Self {
        let Some(file) = std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str::<PresetFile>(&t).ok())
        else {
            return Self::default();
        };
        let builtin = builtin_presets();
        // A hand-edited file can't shadow a built-in or hold two presets of one name.
        let mut user: Vec<ExportPreset> = Vec::new();
        for p in file.presets {
            let name = p.name.trim();
            if name.is_empty()
                || builtin.iter().any(|b| b.name == name)
                || user.iter().any(|u| u.name == name)
            {
                continue;
            }
            user.push(p);
        }
        PresetStore {
            user,
            last: file.last.unwrap_or_default(),
        }
    }

    /// Writes via a temp file + rename so a crash never leaves a half-written document.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let file = PresetFile {
            presets: self.user.clone(),
            last: Some(self.last.clone()),
        };
        let json = serde_json::to_string_pretty(&file).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)
    }

    /// Built-ins first, then the user's, in the order saved.
    pub fn all(&self) -> Vec<ExportPreset> {
        let mut all = builtin_presets();
        all.extend(self.user.iter().cloned());
        all
    }

    pub fn is_builtin(name: &str) -> bool {
        builtin_presets().iter().any(|p| p.name == name)
    }

    /// Saves `spec` under `name`, replacing a user preset of the same name.
    pub fn upsert(&mut self, name: &str, spec: ExportSpec) -> Result<(), PresetError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(PresetError::EmptyName);
        }
        if Self::is_builtin(name) {
            return Err(PresetError::ReservedName);
        }
        let preset = ExportPreset {
            name: name.to_string(),
            spec,
            ..ExportPreset::default()
        };
        match self.user.iter_mut().find(|p| p.name == name) {
            Some(existing) => *existing = preset,
            None => self.user.push(preset),
        }
        Ok(())
    }

    /// Deletes a user preset (built-ins can't be deleted). Returns whether one was removed.
    pub fn delete(&mut self, name: &str) -> bool {
        let before = self.user.len();
        self.user.retain(|p| p.name != name);
        self.user.len() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_presets_are_valid() {
        let presets = builtin_presets();
        assert_eq!(presets.len(), 3);
        for p in &presets {
            p.spec
                .validate()
                .unwrap_or_else(|e| panic!("{}: {e}", p.name));
        }
    }

    #[test]
    fn save_and_load_round_trip_user_presets_and_the_last_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = presets_file_for(&dir.path().join("catalog.db"));
        assert!(path
            .to_string_lossy()
            .ends_with("catalog.db.export-presets.json"));

        let mut store = PresetStore::default();
        let spec = ExportSpec {
            dpi: 150,
            ..ExportSpec::default()
        };
        store.upsert("Client proofs", spec.clone()).unwrap();
        store.last = ExportSpec {
            dpi: 96,
            ..ExportSpec::default()
        };
        store.save(&path).unwrap();
        assert!(!path.with_extension("json.tmp").exists());

        let loaded = PresetStore::load(&path);
        assert_eq!(loaded.last.dpi, 96);
        let all = loaded.all();
        assert_eq!(all.len(), 4);
        assert_eq!(all[3].name, "Client proofs");
        assert_eq!(all[3].spec, spec);
    }

    #[test]
    fn a_missing_or_corrupt_file_falls_back_to_the_built_ins() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.json");
        assert_eq!(PresetStore::load(&missing).all().len(), 3);
        let corrupt = dir.path().join("bad.json");
        std::fs::write(&corrupt, "{ not json").unwrap();
        let store = PresetStore::load(&corrupt);
        assert_eq!(store.all().len(), 3);
        assert_eq!(store.last, ExportSpec::default());
    }

    #[test]
    fn user_presets_cannot_shadow_a_built_in_or_reuse_a_name() {
        let mut store = PresetStore::default();
        assert_eq!(
            store.upsert("Web - 2048 px, sRGB", ExportSpec::default()),
            Err(PresetError::ReservedName)
        );
        assert_eq!(
            store.upsert("  ", ExportSpec::default()),
            Err(PresetError::EmptyName)
        );
        store.upsert("Mine", ExportSpec::default()).unwrap();
        store
            .upsert(
                "Mine",
                ExportSpec {
                    dpi: 200,
                    ..ExportSpec::default()
                },
            )
            .unwrap();
        assert_eq!(store.all().len(), 4, "second save replaced the first");
        assert_eq!(store.all()[3].spec.dpi, 200);
        assert!(store.delete("Mine"));
        assert!(!store.delete("Mine"));
        assert!(
            !store.delete("Web - 2048 px, sRGB"),
            "built-ins can't be deleted"
        );
        assert_eq!(store.all().len(), 3);
    }

    #[test]
    fn a_hand_edited_file_cannot_smuggle_in_duplicates_or_built_in_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.json");
        let dup = serde_json::json!({
            "presets": [
                { "name": "Web - 2048 px, sRGB" },
                { "name": "A" }, { "name": "A" }, { "name": "" }
            ]
        });
        std::fs::write(&path, dup.to_string()).unwrap();
        let names: Vec<_> = PresetStore::load(&path)
            .all()
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names.iter().filter(|n| *n == "A").count(), 1);
        assert_eq!(names.len(), 4);
    }
}
