//! Saved develop presets (#52): `<catalog>.develop-presets.json`, next to the catalog file -- the
//! same sibling-file pattern as `export::presets` and `cache_settings`, written via temp file +
//! rename.
//!
//! A preset is a name plus the stage entries it sets. There are no built-ins. A missing or corrupt
//! file simply means "no presets"; it never blocks the app.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use nicti_pawprint::StageEntry;
use serde::{Deserialize, Serialize};

pub fn presets_file_for(catalog_path: &Path) -> PathBuf {
    let mut name = catalog_path.as_os_str().to_os_string();
    name.push(".develop-presets.json");
    PathBuf::from(name)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DevelopPreset {
    pub name: String,
    pub stages: BTreeMap<String, StageEntry>,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum PresetError {
    #[error("a preset needs a name")]
    EmptyName,
    #[error("a preset called \"{0}\" already exists")]
    Duplicate(String),
    #[error("there's no preset called \"{0}\"")]
    Unknown(String),
    #[error("nothing is checked, so the preset would be empty")]
    Empty,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PresetFile {
    #[serde(default)]
    presets: Vec<DevelopPreset>,
}

#[derive(Debug, Default, PartialEq)]
pub struct PresetStore {
    presets: Vec<DevelopPreset>,
}

impl PresetStore {
    /// Loads `path`; a missing, unreadable or corrupt file yields no presets.
    pub fn load(path: &Path) -> Self {
        let Some(file) = std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str::<PresetFile>(&t).ok())
        else {
            return Self::default();
        };
        // A hand-edited file can't hold an unnamed preset or two of one name.
        let mut presets: Vec<DevelopPreset> = Vec::new();
        for mut p in file.presets {
            p.name = p.name.trim().to_string();
            if !p.name.is_empty() && !presets.iter().any(|q| q.name == p.name) {
                presets.push(p);
            }
        }
        Self { presets }
    }

    /// Writes via a temp file + rename so a crash never leaves a half-written document.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let file = PresetFile {
            presets: self.presets.clone(),
        };
        let json = serde_json::to_string_pretty(&file).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)
    }

    pub fn all(&self) -> &[DevelopPreset] {
        &self.presets
    }

    pub fn get(&self, name: &str) -> Option<&DevelopPreset> {
        self.presets.iter().find(|p| p.name == name)
    }

    /// Adds a preset; refuses an empty or already-used name rather than silently replacing it.
    pub fn add(
        &mut self,
        name: &str,
        stages: BTreeMap<String, StageEntry>,
    ) -> Result<(), PresetError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(PresetError::EmptyName);
        }
        if stages.is_empty() {
            return Err(PresetError::Empty);
        }
        if self.get(name).is_some() {
            return Err(PresetError::Duplicate(name.to_string()));
        }
        self.presets.push(DevelopPreset {
            name: name.to_string(),
            stages,
        });
        Ok(())
    }

    pub fn rename(&mut self, from: &str, to: &str) -> Result<(), PresetError> {
        let to = to.trim();
        if to.is_empty() {
            return Err(PresetError::EmptyName);
        }
        if from != to && self.get(to).is_some() {
            return Err(PresetError::Duplicate(to.to_string()));
        }
        let p = self
            .presets
            .iter_mut()
            .find(|p| p.name == from)
            .ok_or_else(|| PresetError::Unknown(from.to_string()))?;
        p.name = to.to_string();
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> Result<(), PresetError> {
        let before = self.presets.len();
        self.presets.retain(|p| p.name != name);
        if self.presets.len() == before {
            return Err(PresetError::Unknown(name.to_string()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn stages(v: f64) -> BTreeMap<String, StageEntry> {
        BTreeMap::from([(
            "nicti.exposure".to_string(),
            StageEntry {
                schema_version: 1,
                params: json!({ "ev": v }),
            },
        )])
    }

    #[test]
    fn the_file_sits_next_to_the_catalog() {
        let p = presets_file_for(Path::new("/x/catalog.db"));
        assert!(p
            .to_string_lossy()
            .ends_with("catalog.db.develop-presets.json"));
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = presets_file_for(&dir.path().join("catalog.db"));
        let mut store = PresetStore::default();
        store.add("Stage light", stages(1.0)).unwrap();
        store.add("Outdoor", stages(-0.5)).unwrap();
        store.save(&path).unwrap();

        let back = PresetStore::load(&path);
        assert_eq!(back, store);
        assert_eq!(back.all()[0].name, "Stage light", "saved order is kept");
    }

    #[test]
    fn a_missing_or_corrupt_file_gives_no_presets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.json");
        assert_eq!(PresetStore::load(&path), PresetStore::default());
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(PresetStore::load(&path), PresetStore::default());
    }

    #[test]
    fn a_hand_edited_file_cant_hold_blank_or_duplicate_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.json");
        let file = PresetFile {
            presets: vec![
                DevelopPreset {
                    name: "  ".into(),
                    stages: stages(1.0),
                },
                DevelopPreset {
                    name: "A".into(),
                    stages: stages(1.0),
                },
                DevelopPreset {
                    name: "A".into(),
                    stages: stages(2.0),
                },
            ],
        };
        std::fs::write(&path, serde_json::to_string(&file).unwrap()).unwrap();
        let store = PresetStore::load(&path);
        assert_eq!(store.all().len(), 1);
        assert_eq!(store.all()[0].stages, stages(1.0), "first one wins");
    }

    #[test]
    fn add_refuses_empty_duplicate_and_contentless_presets() {
        let mut store = PresetStore::default();
        assert_eq!(store.add(" ", stages(1.0)), Err(PresetError::EmptyName));
        assert_eq!(store.add("A", BTreeMap::new()), Err(PresetError::Empty));
        store.add("A", stages(1.0)).unwrap();
        assert_eq!(
            store.add(" A ", stages(2.0)),
            Err(PresetError::Duplicate("A".into()))
        );
        assert_eq!(store.get("A").unwrap().stages, stages(1.0));
    }

    #[test]
    fn rename_and_remove() {
        let mut store = PresetStore::default();
        store.add("A", stages(1.0)).unwrap();
        store.add("B", stages(2.0)).unwrap();
        assert_eq!(
            store.rename("A", "B"),
            Err(PresetError::Duplicate("B".into()))
        );
        store.rename("A", "C").unwrap();
        assert!(store.get("A").is_none() && store.get("C").is_some());
        assert_eq!(
            store.rename("nope", "D"),
            Err(PresetError::Unknown("nope".into()))
        );
        store.remove("C").unwrap();
        assert_eq!(store.remove("C"), Err(PresetError::Unknown("C".into())));
    }
}
