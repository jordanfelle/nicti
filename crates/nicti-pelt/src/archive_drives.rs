//! Which drives/folders are archive locations (#72, ADR-0072).
//!
//! A folder moved *onto* an archive location is archived: its T0 thumbnails become `.thumb.jpg`
//! sidecars next to the RAWs and the catalog drops its copies. A move *out* to a non-archive
//! location reverses that. Keyed by path prefix (a drive root like `D:\`) because the app doesn't
//! track real volume identity yet (ADR-0071's code is spike-only); swapping this for a volume
//! lookup is a one-function change in [`ArchiveDrives::is_archive`].

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
struct Saved {
    archive_locations: Vec<String>,
}

#[derive(Debug, Default)]
pub struct ArchiveDrives {
    file: Option<PathBuf>,
    locations: Vec<String>,
}

/// `<catalog>.archive-drives.json`, beside the catalog.
pub fn file_for(catalog_path: &Path) -> PathBuf {
    let mut name = catalog_path.as_os_str().to_os_string();
    name.push(".archive-drives.json");
    PathBuf::from(name)
}

/// Comparison form of a path: forward slashes, no trailing separator, case-folded on Windows.
fn norm(p: &str) -> String {
    let s = p.replace('\\', "/");
    let s = s.trim_end_matches('/').to_string();
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s
    }
}

impl ArchiveDrives {
    /// Loads the saved list; a missing or corrupt file means "no archive locations".
    pub fn load(catalog_path: &Path) -> Self {
        let file = file_for(catalog_path);
        let locations = std::fs::read_to_string(&file)
            .ok()
            .and_then(|t| serde_json::from_str::<Saved>(&t).ok())
            .map(|s| s.archive_locations)
            .unwrap_or_default();
        ArchiveDrives {
            file: Some(file),
            locations,
        }
    }

    #[cfg(test)]
    pub fn with_locations(locations: &[&str]) -> Self {
        ArchiveDrives {
            file: None,
            locations: locations.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// `true` if `path` is, or is inside, an archive location.
    pub fn is_archive(&self, path: &Path) -> bool {
        let p = norm(&path.to_string_lossy());
        self.locations.iter().any(|l| {
            let l = norm(l);
            p == l || p.starts_with(&format!("{l}/"))
        })
    }

    pub fn is_location(&self, location: &str) -> bool {
        let l = norm(location);
        self.locations.iter().any(|x| norm(x) == l)
    }

    /// Adds or removes `location` and saves. The in-memory change sticks even if the save fails.
    pub fn set(&mut self, location: &str, archive: bool) -> std::io::Result<()> {
        let l = norm(location);
        self.locations.retain(|x| norm(x) != l);
        if archive {
            self.locations.push(location.to_string());
        }
        self.locations.sort();
        let Some(file) = &self.file else {
            return Ok(());
        };
        let tmp = file.with_extension("json.tmp");
        std::fs::write(
            &tmp,
            serde_json::to_string(&Saved {
                archive_locations: self.locations.clone(),
            })?,
        )?;
        std::fs::rename(&tmp, file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_prefix_on_component_boundaries() {
        let a = ArchiveDrives::with_locations(&["D:\\", "/mnt/archive"]);
        assert!(a.is_archive(Path::new("D:\\events\\x")));
        assert!(a.is_archive(Path::new("/mnt/archive/x")));
        assert!(a.is_archive(Path::new("/mnt/archive")));
        assert!(!a.is_archive(Path::new("/mnt/archive2/x")));
        assert!(!a.is_archive(Path::new("C:\\events")));
    }

    #[test]
    fn persists_and_reloads() {
        let d = tempfile::tempdir().unwrap();
        let cat = d.path().join("cat.db");
        let mut a = ArchiveDrives::load(&cat);
        assert!(!a.is_archive(Path::new("/mnt/a/x")));
        a.set("/mnt/a", true).unwrap();
        assert!(ArchiveDrives::load(&cat).is_archive(Path::new("/mnt/a/x")));
        a.set("/mnt/a/", false).unwrap();
        assert!(!ArchiveDrives::load(&cat).is_archive(Path::new("/mnt/a/x")));
    }
}
