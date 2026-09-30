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
    presets: Vec<serde_json::Value>,
}

#[derive(Debug, Default, PartialEq)]
pub struct PresetStore {
    presets: Vec<DevelopPreset>,
    /// Set when `load` couldn't use everything in an existing file (unreadable, not valid JSON, or
    /// entries dropped). The first `save` copies that file to `<path>.bad` before replacing it.
    backup_first: bool,
}

/// `<path>.bad`, or `.bad1`, `.bad2`, ... -- the first that doesn't exist yet, so a second
/// problem never overwrites the first backup.
fn backup_path_for(path: &Path) -> PathBuf {
    let with = |suffix: &str| {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        PathBuf::from(name)
    };
    let first = with(".bad");
    if !first.exists() {
        return first;
    }
    (1u32..)
        .map(|n| with(&format!(".bad{n}")))
        .find(|p| !p.exists())
        .expect("an unused backup name exists")
}

impl PresetStore {
    /// Loads `path`; a missing file yields no presets. Anything in an existing file that can't be
    /// used (it can't be read, isn't valid JSON, or holds a malformed, unnamed or duplicate
    /// preset) is skipped rather than failing the app -- but the next [`Self::save`] first copies
    /// the original to `<path>.bad`, so a hand-edit or newer-schema file is never silently lost.
    pub fn load(path: &Path) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(_) => {
                return Self {
                    backup_first: true,
                    ..Self::default()
                }
            }
        };
        let Ok(file) = serde_json::from_str::<PresetFile>(&text) else {
            return Self {
                backup_first: true,
                ..Self::default()
            };
        };
        let mut backup_first = false;
        let mut presets: Vec<DevelopPreset> = Vec::new();
        for value in file.presets {
            let Ok(mut p) = serde_json::from_value::<DevelopPreset>(value) else {
                backup_first = true;
                continue;
            };
            p.name = p.name.trim().to_string();
            if p.name.is_empty() || presets.iter().any(|q| q.name == p.name) {
                backup_first = true;
                continue;
            }
            presets.push(p);
        }
        Self {
            presets,
            backup_first,
        }
    }

    /// Writes via a temp file + rename so a crash never leaves a half-written document. If `load`
    /// left something unusable behind, that file is copied aside first; if the copy fails, nothing
    /// is overwritten and the error is returned.
    pub fn save(&mut self, path: &Path) -> std::io::Result<()> {
        if self.backup_first {
            if path.exists() {
                std::fs::copy(path, backup_path_for(path))?;
            }
            self.backup_first = false;
        }
        let file = serde_json::json!({ "presets": self.presets });
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
    fn a_missing_file_gives_no_presets() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            PresetStore::load(&dir.path().join("p.json")),
            PresetStore::default()
        );
    }

    #[test]
    fn a_corrupt_file_is_set_aside_so_the_next_save_cant_destroy_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.json");
        std::fs::write(&path, "{ not json").unwrap();

        let mut store = PresetStore::load(&path);
        assert!(store.all().is_empty());
        store.add("New", stages(1.0)).unwrap();
        store.save(&path).unwrap();

        let bad = dir.path().join("p.json.bad");
        assert_eq!(std::fs::read_to_string(bad).unwrap(), "{ not json");
        assert_eq!(PresetStore::load(&path).all().len(), 1);
    }

    #[test]
    fn one_malformed_preset_is_skipped_not_the_whole_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.json");
        let good = serde_json::to_value(DevelopPreset {
            name: "Good".into(),
            stages: stages(1.0),
        })
        .unwrap();
        let text = serde_json::json!({ "presets": [ { "name": "Bad", "stages": 7 }, good ] });
        std::fs::write(&path, text.to_string()).unwrap();

        let mut store = PresetStore::load(&path);
        assert_eq!(store.all().len(), 1);
        assert_eq!(store.all()[0].name, "Good");

        // The dropped entry isn't lost: the original is copied aside before the save replaces it.
        store.save(&path).unwrap();
        let backup = std::fs::read_to_string(dir.path().join("p.json.bad")).unwrap();
        assert!(backup.contains("\"Bad\""));
        assert!(!std::fs::read_to_string(&path).unwrap().contains("\"Bad\""));
    }

    #[test]
    fn an_unreadable_file_is_backed_up_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.json");
        // UTF-16 with a BOM (what Notepad's "Unicode" save writes) is not valid UTF-8.
        let original: Vec<u8> = vec![0xFF, 0xFE, b'{', 0, b'}', 0];
        std::fs::write(&path, &original).unwrap();

        let mut store = PresetStore::load(&path);
        assert_eq!(store.all().len(), 0);
        store.add("New", stages(1.0)).unwrap();
        store.save(&path).unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("p.json.bad")).unwrap(),
            original
        );
    }

    #[test]
    fn a_second_problem_never_overwrites_the_first_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.json");
        for (n, text) in ["{ first", "{ second"].into_iter().enumerate() {
            std::fs::write(&path, text).unwrap();
            let mut store = PresetStore::load(&path);
            store.add("New", stages(1.0)).unwrap();
            store.save(&path).unwrap();
            assert!(dir
                .path()
                .join(if n == 0 { "p.json.bad" } else { "p.json.bad1" })
                .exists());
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("p.json.bad")).unwrap(),
            "{ first"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("p.json.bad1")).unwrap(),
            "{ second"
        );
    }

    #[test]
    fn a_hand_edited_file_cant_hold_blank_or_duplicate_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.json");
        let entry = |name: &str, v: f64| {
            serde_json::to_value(DevelopPreset {
                name: name.into(),
                stages: stages(v),
            })
            .unwrap()
        };
        let file = serde_json::json!({
            "presets": [entry("  ", 1.0), entry("A", 1.0), entry("A", 2.0)]
        });
        std::fs::write(&path, file.to_string()).unwrap();
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
