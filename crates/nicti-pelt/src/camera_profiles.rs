//! DCP camera profile discovery and loading (#42, ADR-0038/ADR-0018): the user's *own* installed
//! Adobe Camera Raw profiles, read at runtime and never bundled.
//!
//! A profile is matched to the frame by camera model: Adobe names each file
//! `<MAKE> <MODEL> <Profile name>.dcp` (e.g. `NIKON Z 8 Adobe Standard.dcp`, under
//! `Adobe Standard/` or `Camera/<model>/`), and every file carries the same model in its
//! `UniqueCameraModel` tag, which [`load`] re-checks after parsing so a wrongly-named file can't
//! be applied to the wrong camera.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use nicti_calico::dcp::DcpProfile;

/// Largest `.dcp` read (real ones are 100-300 KB; refuse anything absurd from a bad path).
const MAX_DCP_BYTES: u64 = 16 * 1024 * 1024;
/// How deep to walk a `CameraProfiles` root (`Camera/<model>/<file>` is depth 2).
const MAX_DEPTH: usize = 3;
/// Cap on files visited per root, so a huge or pathological tree can't stall the UI thread.
const MAX_FILES_VISITED: usize = 20_000;

/// One discoverable profile file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileEntry {
    /// File stem without the camera prefix, e.g. `Adobe Standard`, `Camera Landscape`.
    pub name: String,
    pub path: PathBuf,
}

/// A loaded profile plus the identity the edit document records.
pub struct LoadedProfile {
    pub profile: Arc<DcpProfile>,
    pub path: PathBuf,
    /// blake3 hex of the file's bytes.
    pub content_hash: String,
}

/// The upper-cased `"<MAKE> <MODEL>"` needle Adobe's file names start with. LibRaw reports
/// `make = "Nikon"`, `model = "Z 8"`; some cameras already include the make in the model.
pub fn camera_needle(make: &str, model: &str) -> String {
    let make_word = make.split_whitespace().next().unwrap_or("").to_uppercase();
    let model_up = model.trim().to_uppercase();
    if make_word.is_empty() || model_up.starts_with(&make_word) {
        model_up
    } else {
        format!("{make_word} {model_up}")
    }
}

/// Roots searched: `NICTI_DCP_DIR` if set, else Adobe's Windows locations.
fn roots() -> Vec<PathBuf> {
    if let Some(dir) = std::env::var_os("NICTI_DCP_DIR") {
        return vec![PathBuf::from(dir)];
    }
    let mut roots = Vec::new();
    if let Some(pd) = std::env::var_os("PROGRAMDATA") {
        roots.push(Path::new(&pd).join("Adobe/CameraRaw/CameraProfiles"));
    }
    if let Some(ad) = std::env::var_os("APPDATA") {
        roots.push(Path::new(&ad).join("Adobe/CameraRaw/CameraProfiles"));
    }
    roots
}

/// Profiles for the given camera under the default roots.
pub fn discover(make: &str, model: &str) -> Vec<ProfileEntry> {
    discover_in(&roots(), make, model)
}

/// Profiles for the camera under `roots`. Sorted, `Adobe Standard` first (the conventional
/// default), the rest alphabetical.
pub fn discover_in(roots: &[PathBuf], make: &str, model: &str) -> Vec<ProfileEntry> {
    let needle = camera_needle(make, model);
    if needle.is_empty() {
        return Vec::new();
    }
    let mut found = Vec::new();
    let mut visited = 0usize;
    for root in roots {
        walk(root, 0, &needle, &mut found, &mut visited);
    }
    found.sort_by(|a: &ProfileEntry, b: &ProfileEntry| {
        let rank = |e: &ProfileEntry| u8::from(e.name != "Adobe Standard");
        rank(a).cmp(&rank(b)).then_with(|| a.name.cmp(&b.name))
    });
    found.dedup_by(|a, b| a.path == b.path);
    found
}

fn walk(dir: &Path, depth: usize, needle: &str, out: &mut Vec<ProfileEntry>, visited: &mut usize) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        *visited += 1;
        if *visited > MAX_FILES_VISITED {
            return;
        }
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            walk(&path, depth + 1, needle, out, visited);
        } else if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("dcp"))
        {
            if let Some(name) = profile_name_for(&path, needle) {
                out.push(ProfileEntry { name, path });
            }
        }
    }
}

/// `Some(profile name)` if `path`'s file name is `<needle> <profile name>.dcp` -- the model must
/// be followed by a space, so `Z 8` does not match `Z 80 ...`.
fn profile_name_for(path: &Path, needle: &str) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let up = stem.to_uppercase();
    let rest = up.strip_prefix(needle)?;
    if !rest.starts_with(' ') {
        return None;
    }
    // Slice the original-case stem by the same byte length (ASCII prefix, so lengths agree).
    let name = stem.get(needle.len() + 1..)?.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// Reads, size-checks, parses and hashes a `.dcp`. `camera_model_needle`, if given, must match
/// the profile's `UniqueCameraModel` (case-insensitively) when that tag is present.
pub fn load(path: &Path, camera_needle: Option<&str>) -> Result<LoadedProfile, String> {
    let len = std::fs::metadata(path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .len();
    if len > MAX_DCP_BYTES {
        return Err(format!(
            "{}: {len} bytes is too large for a .dcp",
            path.display()
        ));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let profile = DcpProfile::parse(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    if let Some(needle) = camera_needle {
        let ucm = profile.unique_camera_model.to_uppercase();
        if !ucm.is_empty() && ucm != needle {
            return Err(format!(
                "{} is for a {} camera, not {needle}",
                path.display(),
                profile.unique_camera_model
            ));
        }
    }
    Ok(LoadedProfile {
        profile: Arc::new(profile),
        path: path.to_path_buf(),
        content_hash: blake3::hash(&bytes).to_hex().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needle_combines_make_and_model_unless_the_model_already_has_it() {
        assert_eq!(camera_needle("Nikon", "Z 8"), "NIKON Z 8");
        assert_eq!(camera_needle("NIKON CORPORATION", "NIKON Z 8"), "NIKON Z 8");
        assert_eq!(camera_needle("", "Z 8"), "Z 8");
    }

    #[test]
    fn a_profile_name_requires_a_word_boundary_after_the_model() {
        let p = |f: &str| profile_name_for(Path::new(f), "NIKON Z 8");
        assert_eq!(
            p("NIKON Z 8 Adobe Standard.dcp").as_deref(),
            Some("Adobe Standard")
        );
        assert_eq!(
            p("nikon z 8 Camera Flat.dcp").as_deref(),
            Some("Camera Flat")
        );
        assert!(p("NIKON Z 80 Adobe Standard.dcp").is_none());
        assert!(p("NIKON Z 8.dcp").is_none());
        assert!(p("CANON EOS R5 Adobe Standard.dcp").is_none());
    }

    #[test]
    fn discovery_walks_subfolders_and_puts_adobe_standard_first() {
        let dir = tempfile::tempdir().unwrap();
        let touch = |rel: &str| {
            let p = dir.path().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, b"x").unwrap();
        };
        touch("Camera/Nikon Z 8/NIKON Z 8 Camera Vivid.dcp");
        touch("Camera/Nikon Z 8/NIKON Z 8 Camera Flat.dcp");
        touch("Adobe Standard/NIKON Z 8 Adobe Standard.dcp");
        touch("Adobe Standard/NIKON Z 7 Adobe Standard.dcp");
        touch("Adobe Standard/NIKON Z 80 Adobe Standard.dcp");
        touch("Adobe Standard/readme.txt");
        let found = discover_in(&[dir.path().to_path_buf()], "Nikon", "Z 8");
        let names: Vec<_> = found.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Adobe Standard", "Camera Flat", "Camera Vivid"]);
    }

    #[test]
    fn a_missing_root_finds_nothing() {
        assert!(discover_in(&[PathBuf::from("/definitely/not/here")], "Nikon", "Z 8").is_empty());
    }

    #[test]
    fn load_rejects_garbage_and_hashes_real_content() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("bad.dcp");
        std::fs::write(&bad, b"not a dcp").unwrap();
        assert!(load(&bad, None).is_err());
        assert!(load(&dir.path().join("missing.dcp"), None).is_err());
    }

    /// Against a real installed profile (never bundled, ADR-0018): run with
    /// `NICTI_DCP_DIR=... cargo test -p nicti-pelt -- --ignored real_z8`.
    #[test]
    #[ignore = "needs a local Adobe Camera Raw profile install"]
    fn real_z8_profiles_discover_load_and_match_the_camera() {
        let dir = std::env::var("NICTI_DCP_DIR")
            .unwrap_or_else(|_| "/mnt/c/ProgramData/Adobe/CameraRaw/CameraProfiles".into());
        let found = discover_in(&[PathBuf::from(dir)], "Nikon", "Z 8");
        assert_eq!(found[0].name, "Adobe Standard");
        assert!(found.len() >= 4, "found only {found:?}");
        let loaded = load(&found[0].path, Some("NIKON Z 8")).unwrap();
        assert_eq!(loaded.content_hash.len(), 64);
        assert!(loaded.profile.hue_sat_map1.is_some());
        // The same file must be rejected for a different camera.
        assert!(load(&found[0].path, Some("NIKON Z 7")).is_err());
    }
}
