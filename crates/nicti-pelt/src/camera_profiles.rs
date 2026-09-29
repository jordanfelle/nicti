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

/// The upper-cased camera names Adobe's files may start with, most specific first. LibRaw
/// reports `make = "Nikon"`, `model = "Z 8"`; some cameras already put the make in the model, some
/// have a multi-word make Adobe spells out (`OM Digital Solutions OM-1`), and LibRaw spells
/// `Z 6_2` where Adobe writes `Z 6 2`.
pub fn camera_needles(make: &str, model: &str) -> Vec<String> {
    let model_up = model.trim().replace('_', " ").to_uppercase();
    let make_up = make.trim().to_uppercase();
    let make_word = make_up.split_whitespace().next().unwrap_or("").to_string();
    let mut out: Vec<String> = Vec::new();
    let mut push = |n: String| {
        if !n.is_empty() && !out.contains(&n) {
            out.push(n);
        }
    };
    // The model alone if it already carries the make ("NIKON Z 8"), plus the make-prefixed forms:
    // first word ("NIKON Z 8") and the full make ("OM DIGITAL SOLUTIONS OM-1"). Extra candidates
    // are harmless -- a needle only matches a file whose name actually starts with it.
    if make_word.is_empty() || model_up.starts_with(&make_word) {
        push(model_up.clone());
    }
    if !make_word.is_empty() {
        push(format!("{make_word} {model_up}"));
        push(format!("{make_up} {model_up}"));
    }
    out
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
    let needles = camera_needles(make, model);
    if needles.is_empty() {
        return Vec::new();
    }
    let mut found = Vec::new();
    for root in roots {
        // The visit cap is per root, as documented, so a huge root can't starve the others.
        let mut visited = 0usize;
        walk(root, 0, &needles, &mut found, &mut visited);
    }
    found.sort_by(|a: &ProfileEntry, b: &ProfileEntry| {
        let rank = |e: &ProfileEntry| u8::from(e.name != "Adobe Standard");
        rank(a).cmp(&rank(b)).then_with(|| a.name.cmp(&b.name))
    });
    // The same profile installed under both PROGRAMDATA and APPDATA would list twice.
    found.dedup_by(|a, b| a.path == b.path || a.name.eq_ignore_ascii_case(&b.name));
    found
}

fn walk(
    dir: &Path,
    depth: usize,
    needles: &[String],
    out: &mut Vec<ProfileEntry>,
    visited: &mut usize,
) {
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
            walk(&path, depth + 1, needles, out, visited);
        } else if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("dcp"))
        {
            if let Some(name) = needles.iter().find_map(|n| profile_name_for(&path, n)) {
                out.push(ProfileEntry { name, path });
            }
        }
    }
}

/// `Some(profile name)` if `path`'s file name is `<needle> <profile name>.dcp`.
///
/// Adobe's profile names all begin `Adobe ` (`Adobe Standard`) or `Camera ` (`Camera Landscape`),
/// and requiring that is what stops a short model from claiming a longer one's files: `Z 6` must
/// not list `NIKON Z 6 2 Camera Flat` (the Z 6II), `EOS 5D` must not list `EOS 5D Mark II ...`,
/// `EOS R5` must not list the `R5 C`, and `Z 8` must not list `Z 80 ...`.
fn profile_name_for(path: &Path, needle: &str) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let up = stem.to_uppercase();
    let rest = up.strip_prefix(needle)?.strip_prefix(' ')?;
    if !(rest.starts_with("ADOBE ") || rest.starts_with("CAMERA ")) {
        return None;
    }
    // Slice the original-case stem by the same byte length (ASCII prefix, so lengths agree).
    let name = stem.get(needle.len() + 1..)?.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// Reads, size-checks, parses and hashes a `.dcp`. `camera_needles`, if given, must include
/// the profile's `UniqueCameraModel` (case-insensitively) when that tag is present.
pub fn load(path: &Path, camera_needles: Option<&[String]>) -> Result<LoadedProfile, String> {
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
    if let Some(needles) = camera_needles {
        let ucm = profile
            .unique_camera_model
            .trim()
            .replace('_', " ")
            .to_uppercase();
        if !ucm.is_empty() && !needles.contains(&ucm) {
            return Err(format!(
                "{} is for a {} camera, not {}",
                path.display(),
                profile.unique_camera_model,
                needles.first().map_or("this", String::as_str)
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
    fn needles_combine_make_and_model_unless_the_model_already_has_it() {
        assert_eq!(camera_needles("Nikon", "Z 8"), ["NIKON Z 8"]);
        assert_eq!(
            camera_needles("NIKON CORPORATION", "NIKON Z 8")[0],
            "NIKON Z 8"
        );
        assert_eq!(camera_needles("", "Z 8"), ["Z 8"]);
    }

    #[test]
    fn needles_cover_multi_word_makes_and_libraws_underscore_models() {
        let om = camera_needles("OM Digital Solutions", "OM-1");
        assert!(
            om.contains(&"OM DIGITAL SOLUTIONS OM-1".to_string()),
            "{om:?}"
        );
        assert_eq!(camera_needles("Nikon", "Z 6_2"), ["NIKON Z 6 2"]);
    }

    #[test]
    fn a_short_model_does_not_claim_a_longer_models_profiles() {
        let p = |f: &str, needle: &str| profile_name_for(Path::new(f), needle).is_some();
        // Nikon Z 6 vs Z 6II (Adobe writes "Z 6 2"); Canon 5D vs 5D Mark II; R5 vs R5 C.
        assert!(!p("NIKON Z 6 2 Camera Flat.dcp", "NIKON Z 6"));
        assert!(p("NIKON Z 6 Camera Flat.dcp", "NIKON Z 6"));
        assert!(!p(
            "CANON EOS 5D Mark II Adobe Standard.dcp",
            "CANON EOS 5D"
        ));
        assert!(p("CANON EOS 5D Adobe Standard.dcp", "CANON EOS 5D"));
        assert!(!p("CANON EOS R5 C Adobe Standard.dcp", "CANON EOS R5"));
        assert!(p("CANON EOS R5 Adobe Standard.dcp", "CANON EOS R5"));
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
        assert!(p("NIKON Z 8 Something Else.dcp").is_none());
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
        touch("Camera/Nikon Z 8/NIKON Z 8 Camera Flat.dcp"); // same profile listed twice
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
        let loaded = load(&found[0].path, Some(&["NIKON Z 8".to_string()])).unwrap();
        assert_eq!(loaded.content_hash.len(), 64);
        assert!(loaded.profile.hue_sat_map1.is_some());
        // A short model must not claim a longer one's files (Z 6 vs Z 6II's "Z 6 2 ..."; 5D vs
        // 5D Mark II): every listed profile must actually load for that camera.
        let root = [PathBuf::from(
            std::env::var("NICTI_DCP_DIR")
                .unwrap_or_else(|_| "/mnt/c/ProgramData/Adobe/CameraRaw/CameraProfiles".into()),
        )];
        for (make, model) in [("Nikon", "Z 6"), ("Canon", "EOS 5D"), ("Canon", "EOS R5")] {
            let needles = camera_needles(make, model);
            for entry in discover_in(&root, make, model) {
                assert!(
                    load(&entry.path, Some(&needles)).is_ok(),
                    "{make} {model} listed a profile for another camera: {entry:?}"
                );
            }
        }
        // The same file must be rejected for a different camera.
        assert!(load(&found[0].path, Some(&["NIKON Z 7".to_string()])).is_err());
    }
}
