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
    let mut found = discover_all_in(roots, make, model);
    // The same profile installed under both PROGRAMDATA and APPDATA would list twice.
    found.dedup_by(|a, b| a.name.eq_ignore_ascii_case(&b.name));
    found
}

/// Like [`discover_in`] but keeps every installed copy of a same-named profile (only the very same
/// file collapses), in the same order -- the LRC import tries each until one loads.
fn discover_all_in(roots: &[PathBuf], make: &str, model: &str) -> Vec<ProfileEntry> {
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
    found.dedup_by(|a, b| a.path == b.path);
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

/// Reloads the DCP profile an edit document refers to (its `WORKING_SPACE` entry), verifying the
/// file still hashes to what the document recorded.
///
/// - `Ok(None)`: the document selects no profile.
/// - `Ok(Some(_))`: the profile, ready for `spine::resolve_inputs`.
/// - `Err`: the document selects a profile that is missing, unreadable, for another camera, or
///   has changed on disk since the edit was made. Callers decide what that means -- Develop shows
///   the message and renders with the plain matrix; **export fails that photo**, because silently
///   rendering different colors than the user edited would be worse.
pub fn load_for_document(
    doc: &nicti_pawprint::EditDocument,
    make: &str,
    model: &str,
) -> Result<Option<Arc<DcpProfile>>, String> {
    let chosen: nicti_tapetum::coat::CameraProfileParams =
        match doc.stages.get(nicti_tapetum::stages::WORKING_SPACE) {
            Some(entry) => nicti_tapetum::coat::parse(&entry.params),
            None => return Ok(None),
        };
    let Some(want_hash) = chosen.content_hash else {
        return Ok(None);
    };
    let Some(path) = chosen.path else {
        return Err(
            "the edit selects a camera profile but doesn't record where it came from".into(),
        );
    };
    let needles = camera_needles(make, model);
    let loaded = load(Path::new(&path), Some(&needles))?;
    if loaded.content_hash != want_hash {
        return Err(format!(
            "camera profile {path} has changed on disk since this photo was edited"
        ));
    }
    Ok(Some(loaded.profile))
}

// --- Adobe Raw "Look" .xmp profiles (#321) ----------------------------------------------------

/// Largest Look `.xmp` read (real ones are ~100 KB; the decoder also caps the decompressed size).
const MAX_LOOK_BYTES: u64 = 16 * 1024 * 1024;

/// One discoverable Look `.xmp` (not camera-specific, unlike a DCP).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LookEntry {
    /// File stem, e.g. `Adobe Vivid`.
    pub name: String,
    pub path: PathBuf,
}

/// A loaded Look plus the identity the edit document records.
pub struct LoadedLook {
    pub look: Arc<nicti_calico::xmp_profile::LookProfile>,
    pub path: PathBuf,
    /// blake3 hex of the file's bytes.
    pub content_hash: String,
}

/// Roots searched for Look profiles: `NICTI_LOOK_XMP_DIR` if set, else Adobe's Windows
/// `CameraRaw/Settings/Adobe/Profiles` locations.
fn look_roots() -> Vec<PathBuf> {
    if let Some(dir) = std::env::var_os("NICTI_LOOK_XMP_DIR") {
        return vec![PathBuf::from(dir)];
    }
    let mut roots = Vec::new();
    for var in ["PROGRAMDATA", "APPDATA"] {
        if let Some(base) = std::env::var_os(var) {
            roots.push(Path::new(&base).join("Adobe/CameraRaw/Settings/Adobe/Profiles"));
        }
    }
    roots
}

/// Look profiles under the default roots, sorted by name.
pub fn discover_looks() -> Vec<LookEntry> {
    discover_looks_in(&look_roots())
}

pub fn discover_looks_in(roots: &[PathBuf]) -> Vec<LookEntry> {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<LookEntry>, visited: &mut usize) {
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
                walk(&path, depth + 1, out, visited);
            } else if path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("xmp"))
            {
                if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
                    out.push(LookEntry {
                        name: name.to_string(),
                        path,
                    });
                }
            }
        }
    }
    let mut found = Vec::new();
    for root in roots {
        let mut visited = 0usize;
        walk(root, 0, &mut found, &mut visited);
    }
    // Only the same file (e.g. one root listed twice) collapses: two distinct Looks may share a
    // name, and the picker identifies the selection by path.
    found.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.path.cmp(&b.path)));
    found.dedup_by(|a, b| a.path == b.path);
    found
}

/// Reads, size-checks, parses and hashes a Look `.xmp`.
pub fn load_look(path: &Path) -> Result<LoadedLook, String> {
    let len = std::fs::metadata(path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .len();
    if len > MAX_LOOK_BYTES {
        return Err(format!(
            "{}: {len} bytes is too large for a look profile",
            path.display()
        ));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|e| format!("{}: not UTF-8 text: {e}", path.display()))?;
    let look =
        nicti_calico::xmp_profile::parse(text).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(LoadedLook {
        look: Arc::new(look),
        path: path.to_path_buf(),
        content_hash: blake3::hash(&bytes).to_hex().to_string(),
    })
}

/// Reloads the Look `.xmp` an edit document refers to (`CameraProfileParams.look`), verifying the
/// file still hashes to what the document recorded. Same contract as [`load_for_document`]:
/// `Ok(None)` for no look, `Err` when it is missing or changed (export fails the photo).
pub fn load_look_for_document(
    doc: &nicti_pawprint::EditDocument,
) -> Result<Option<Arc<nicti_calico::xmp_profile::LookProfile>>, String> {
    let chosen: nicti_tapetum::coat::CameraProfileParams =
        match doc.stages.get(nicti_tapetum::stages::WORKING_SPACE) {
            Some(entry) => nicti_tapetum::coat::parse(&entry.params),
            None => return Ok(None),
        };
    let Some(look_ref) = chosen.look else {
        return Ok(None);
    };
    let loaded = load_look(Path::new(&look_ref.path))?;
    if loaded.content_hash != look_ref.content_hash {
        return Err(format!(
            "look profile {} has changed on disk since this photo was edited",
            look_ref.path
        ));
    }
    Ok(Some(loaded.look))
}

// --- LRC import: camera-profile name -> installed profile (#381) -------------------------------

/// Resolves the `CameraProfile` name LRC stored for a photo to the user's own installed `.dcp` or
/// Adobe Raw Look, for `nicti-stray`'s import job.
///
/// - A **DCP** wins when the name matches an installed profile for that camera
///   (`Camera Landscape`, `Adobe Standard` ...), compared case-insensitively.
/// - Otherwise a **Look** (`Adobe Vivid`, `Adobe Color` ...) matched by file name, layered on that
///   camera's `Adobe Standard` DCP -- a Look needs a DCP underneath it
///   (`spine::resolve_inputs`), and `Adobe Standard` is the base Adobe's own Raw profiles use.
/// - Anything else, or a file that no longer loads, is `None`: the import reports the name as
///   missing rather than substituting a different profile.
///
/// Discovery reads the disk, so each lookup is cheap but not free; the import job caches per
/// (camera, name).
#[derive(Debug, Clone)]
pub struct LrcProfileResolver {
    dcp_roots: Vec<PathBuf>,
    look_roots: Vec<PathBuf>,
}

impl Default for LrcProfileResolver {
    /// The same roots the Develop panel's pickers search (`NICTI_DCP_DIR`/`NICTI_LOOK_XMP_DIR`
    /// override Adobe's Windows locations).
    fn default() -> Self {
        Self {
            dcp_roots: roots(),
            look_roots: look_roots(),
        }
    }
}

impl LrcProfileResolver {
    /// A resolver over explicit roots (tests).
    #[cfg(test)]
    pub fn with_roots(dcp_roots: Vec<PathBuf>, look_roots: Vec<PathBuf>) -> Self {
        Self {
            dcp_roots,
            look_roots,
        }
    }

    fn dcp(
        &self,
        make: &str,
        model: &str,
        name: &str,
    ) -> Option<nicti_tapetum::coat::CameraProfileParams> {
        let needles = camera_needles(make, model);
        // The same profile name can be installed twice (per-user and system folders); an
        // unreadable first copy must not hide a valid second one.
        discover_all_in(&self.dcp_roots, make, model)
            .into_iter()
            .filter(|e| e.name.eq_ignore_ascii_case(name))
            .find_map(|entry| {
                let loaded = load(&entry.path, Some(&needles)).ok()?;
                Some(nicti_tapetum::coat::CameraProfileParams {
                    name: Some(loaded.profile.name.clone()),
                    path: Some(loaded.path.display().to_string()),
                    content_hash: Some(loaded.content_hash),
                    look: None,
                })
            })
    }
}

impl nicti_stray::ProfileResolver for LrcProfileResolver {
    fn resolve(
        &self,
        make: &str,
        model: &str,
        name: &str,
    ) -> Option<nicti_tapetum::coat::CameraProfileParams> {
        // No camera, no `.dcp` (they are matched by model), and so no Look either.
        if camera_needles(make, model).is_empty() {
            return None;
        }
        if let Some(found) = self.dcp(make, model, name) {
            return Some(found);
        }
        let (entry, look) = discover_looks_in(&self.look_roots)
            .into_iter()
            .filter(|e| e.name.eq_ignore_ascii_case(name))
            .find_map(|e| load_look(&e.path).ok().map(|l| (e, l)))?;
        let mut base = self.dcp(make, model, "Adobe Standard")?;
        base.look = Some(nicti_tapetum::coat::LookRef {
            name: entry.name,
            path: look.path.display().to_string(),
            content_hash: look.content_hash,
        });
        Some(base)
    }
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

    #[test]
    fn looks_are_discovered_by_xmp_extension_and_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("Adobe Raw");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("Adobe Vivid.xmp"), "x").unwrap();
        std::fs::write(sub.join("Adobe Color.xmp"), "x").unwrap();
        std::fs::write(sub.join("readme.txt"), "x").unwrap();
        let found = discover_looks_in(&[dir.path().to_path_buf()]);
        let names: Vec<_> = found.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Adobe Color", "Adobe Vivid"]);
    }

    #[test]
    fn a_missing_or_garbage_look_fails_to_load_instead_of_panicking() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_look(&dir.path().join("nope.xmp")).is_err());
        let bad = dir.path().join("bad.xmp");
        std::fs::write(&bad, "<not-xmp/>").unwrap();
        assert!(load_look(&bad).is_err());
    }

    mod lrc_resolver {
        use super::*;
        use nicti_calico::dcp::testing::synthetic_dcp_bytes;
        use nicti_calico::xmp_profile::testing::synthetic_valid_xmp;
        use nicti_stray::ProfileResolver;

        struct Fixture {
            _dir: tempfile::TempDir,
            resolver: LrcProfileResolver,
        }

        /// A Z 8 with `Adobe Standard` + `Camera Landscape` DCPs, an unrelated Z 7 DCP, and two
        /// Looks (one valid, one garbage) in separate trees.
        fn fixture(with_standard: bool) -> Fixture {
            let dir = tempfile::tempdir().unwrap();
            let dcps = dir.path().join("CameraProfiles");
            let looks = dir.path().join("Profiles");
            std::fs::create_dir_all(dcps.join("Camera/Nikon Z 8")).unwrap();
            std::fs::create_dir_all(dcps.join("Adobe Standard")).unwrap();
            std::fs::create_dir_all(dcps.join("Camera/Nikon Z 6 2")).unwrap();
            std::fs::create_dir_all(&looks).unwrap();
            let write = |rel: &str, name: &str, model: &str| {
                std::fs::write(
                    dcps.join(rel),
                    synthetic_dcp_bytes(model, name, None, None, true),
                )
                .unwrap();
            };
            if with_standard {
                write(
                    "Adobe Standard/NIKON Z 8 Adobe Standard.dcp",
                    "Adobe Standard",
                    "NIKON Z 8",
                );
            }
            write(
                "Camera/Nikon Z 8/NIKON Z 8 Camera Landscape.dcp",
                "Camera Landscape",
                "NIKON Z 8",
            );
            write(
                "Adobe Standard/NIKON Z 7 Adobe Standard.dcp",
                "Adobe Standard",
                "NIKON Z 7",
            );
            // A Z 6 II: Adobe spells it "Z 6 2". A Z 6 must never be handed this file.
            write(
                "Camera/Nikon Z 6 2/NIKON Z 6 2 Camera Flat.dcp",
                "Camera Flat",
                "NIKON Z 6_2",
            );
            std::fs::write(
                looks.join("Adobe Vivid.xmp"),
                synthetic_valid_xmp("Adobe Vivid"),
            )
            .unwrap();
            std::fs::write(looks.join("Adobe Broken.xmp"), "<not-xmp/>").unwrap();
            Fixture {
                resolver: LrcProfileResolver::with_roots(vec![dcps], vec![looks]),
                _dir: dir,
            }
        }

        /// CodeRabbit: the same profile installed twice, the first copy unreadable, must still resolve.
        #[test]
        fn an_unreadable_first_copy_does_not_hide_a_valid_second_one() {
            let dir = tempfile::tempdir().unwrap();
            let (bad_root, good_root) = (dir.path().join("a"), dir.path().join("b"));
            for root in [&bad_root, &good_root] {
                std::fs::create_dir_all(root).unwrap();
            }
            let file = "NIKON Z 8 Camera Landscape.dcp";
            std::fs::write(bad_root.join(file), b"not a dcp").unwrap();
            std::fs::write(
                good_root.join(file),
                synthetic_dcp_bytes("NIKON Z 8", "Camera Landscape", None, None, true),
            )
            .unwrap();
            // Same-named Look twice too: a garbage one in the first root, a valid one in the second.
            let (bad_look, good_look) = (dir.path().join("la"), dir.path().join("lb"));
            for root in [&bad_look, &good_look] {
                std::fs::create_dir_all(root).unwrap();
            }
            std::fs::write(bad_look.join("Adobe Vivid.xmp"), "<not-xmp/>").unwrap();
            std::fs::write(
                good_look.join("Adobe Vivid.xmp"),
                synthetic_valid_xmp("Adobe Vivid"),
            )
            .unwrap();
            std::fs::write(
                good_root.join("NIKON Z 8 Adobe Standard.dcp"),
                synthetic_dcp_bytes("NIKON Z 8", "Adobe Standard", None, None, true),
            )
            .unwrap();
            let r = LrcProfileResolver::with_roots(
                vec![bad_root, good_root],
                vec![bad_look, good_look],
            );
            let dcp = r.resolve("NIKON CORPORATION", "NIKON Z 8", "Camera Landscape");
            assert_eq!(dcp.unwrap().name.as_deref(), Some("Camera Landscape"));
            let look = r.resolve("NIKON CORPORATION", "NIKON Z 8", "Adobe Vivid");
            assert!(look.unwrap().look.is_some());
        }

        #[test]
        fn a_named_dcp_resolves_case_insensitively_with_its_identity() {
            let f = fixture(true);
            let p = f
                .resolver
                .resolve("NIKON CORPORATION", "NIKON Z 8", "camera LANDSCAPE")
                .expect("installed");
            assert_eq!(p.name.as_deref(), Some("Camera Landscape"));
            assert!(p.path.as_deref().unwrap().ends_with("Camera Landscape.dcp"));
            assert_eq!(p.content_hash.as_deref().map(str::len), Some(64));
            assert!(p.look.is_none());
        }

        #[test]
        fn a_look_resolves_on_top_of_the_cameras_adobe_standard() {
            let f = fixture(true);
            let p = f
                .resolver
                .resolve("NIKON CORPORATION", "NIKON Z 8", "Adobe Vivid")
                .expect("installed");
            assert_eq!(p.name.as_deref(), Some("Adobe Standard"));
            let look = p.look.expect("look layered");
            assert_eq!(look.name, "Adobe Vivid");
            assert_eq!(look.content_hash.len(), 64);
        }

        #[test]
        fn nothing_is_guessed_when_a_piece_is_missing() {
            let f = fixture(true);
            let r = &f.resolver;
            // Unknown name, a Look that does not parse, a camera with no such DCP, no camera at all.
            assert!(r
                .resolve("NIKON CORPORATION", "NIKON Z 8", "Camera Portrait")
                .is_none());
            assert!(r
                .resolve("NIKON CORPORATION", "NIKON Z 8", "Adobe Broken")
                .is_none());
            assert!(r
                .resolve("NIKON CORPORATION", "NIKON Z 7", "Camera Landscape")
                .is_none());
            assert!(r.resolve("", "", "Camera Landscape").is_none());
            // A Look without that camera's Adobe Standard underneath has nothing to layer on.
            let no_base = fixture(false);
            assert!(no_base
                .resolver
                .resolve("NIKON CORPORATION", "NIKON Z 8", "Adobe Vivid")
                .is_none());
        }

        #[test]
        fn a_short_model_does_not_pick_up_a_longer_models_profile() {
            let f = fixture(true);
            // "Z 8" must not match "Z 80..." and the Z 7's file is never offered for a Z 8.
            assert!(f
                .resolver
                .resolve("NIKON CORPORATION", "NIKON Z 7", "Adobe Standard")
                .is_some());
            let z8 = f
                .resolver
                .resolve("NIKON CORPORATION", "NIKON Z 8", "Adobe Standard")
                .unwrap();
            assert!(z8.path.unwrap().contains("Z 8"));
            // The Z 6 II's "Camera Flat" belongs to the Z 6 II only, never to a Z 6 or a Z 8.
            let r = &f.resolver;
            assert!(r
                .resolve("NIKON CORPORATION", "NIKON Z 6", "Camera Flat")
                .is_none());
            assert!(r
                .resolve("NIKON CORPORATION", "NIKON Z 8", "Camera Flat")
                .is_none());
            assert!(r
                .resolve("NIKON CORPORATION", "NIKON Z 6_2", "Camera Flat")
                .is_some());
        }
    }
}
