//! Persisted open/closed state of the fur panel sections (#443): which Develop sections the user
//! left open survives a restart. Saved beside the catalog (`<catalog>.sections.json`, temp file +
//! rename like `preview_settings`); a missing or corrupt file just means the default layout.

use std::path::{Path, PathBuf};

use crate::fur::SectionStates;

/// Where the state lives: `<catalog file name>.sections.json`, next to the catalog.
pub fn file_for(catalog_path: &Path) -> PathBuf {
    let mut name = catalog_path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| "nicti".into());
    name.push(".sections.json");
    catalog_path.with_file_name(name)
}

/// The saved state, or the defaults when the file is absent, unreadable or corrupt.
pub fn load(catalog_path: &Path) -> SectionStates {
    std::fs::read_to_string(file_for(catalog_path))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Writes via a temp file + rename so a crash never leaves a half-written file.
pub fn save(catalog_path: &Path, states: &SectionStates) -> std::io::Result<()> {
    let path = file_for(catalog_path);
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string(states).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_or_corrupt_file_gives_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cat = dir.path().join("library.db");
        assert_eq!(load(&cat), SectionStates::default());
        std::fs::write(file_for(&cat), "{not json").unwrap();
        assert_eq!(load(&cat), SectionStates::default());
    }
}
