//! Turning LRC's drive-letter paths into paths on *this* machine (ADR-0061 Q1: every real root has
//! a drive-letter `absolutePath`, only some also carry a relative fallback), plus the optional
//! prefix remap a user supplies when the photos have moved since the catalog was written.

use std::path::{Path, PathBuf};

use nicti_lair::scruff::normalize_rel_path;

/// A user-supplied "this LRC path prefix now lives here" rule, applied before platform mapping.
/// Matching is on whole path segments, case-insensitive, separator-agnostic -- `D:\Photos` matches
/// `d:/photos/2026` but not `D:\PhotosOld`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootRemap {
    pub from: String,
    pub to: String,
}

/// `root` + `folder_rel` + `base_name.extension`, always forward-slashed (promoted from
/// `spikes/shed`'s `join_lrc_path`). Tolerates a root or folder with or without trailing
/// separators -- LRC's `pathFromRoot` normally ends in `/`, but naive concatenation of one that
/// doesn't glued a nested folder's last segment onto the file name (#158).
pub fn join_lrc_path(root: &str, folder_rel: &str, base_name: &str, extension: &str) -> String {
    let mut full = root.trim_end_matches(['/', '\\']).to_string();
    let folder_rel = folder_rel.trim_matches(['/', '\\']);
    if !folder_rel.is_empty() {
        full.push('/');
        full.push_str(folder_rel);
    }
    full.push('/');
    full.push_str(&file_name(base_name, extension));
    full
}

/// `base_name.extension`, or just `base_name` when the extension is empty.
pub fn file_name(base_name: &str, extension: &str) -> String {
    if extension.is_empty() {
        base_name.to_string()
    } else {
        format!("{base_name}.{extension}")
    }
}

/// The asset's path relative to its root folder, in the form `scruff` stores in `asset.rel_path`
/// (forward slashes, NFC, no leading/trailing slash).
pub fn rel_path_under_root(folder_rel: &str, base_name: &str, extension: &str) -> String {
    let joined = format!(
        "{}/{}",
        folder_rel.trim_matches(['/', '\\']),
        file_name(base_name, extension)
    );
    normalize_rel_path(Path::new(&joined))
}

fn segments(path: &str) -> Vec<String> {
    path.replace('\\', "/")
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
        .collect()
}

/// Applies the first matching remap to `path`, else returns it unchanged.
pub fn apply_remaps(path: &str, remaps: &[RootRemap]) -> String {
    let have = segments(path);
    for remap in remaps {
        let from = segments(&remap.from);
        if !from.is_empty() && have.len() >= from.len() && have[..from.len()] == from[..] {
            // Keep the original casing of the tail, which `have` has lowercased.
            let tail: Vec<&str> = path
                .split(['/', '\\'])
                .filter(|s| !s.is_empty())
                .skip(from.len())
                .collect();
            let mut out = remap.to.trim_end_matches(['/', '\\']).to_string();
            for seg in tail {
                out.push('/');
                out.push_str(seg);
            }
            return out;
        }
    }
    path.to_string()
}

/// `X:\...` / `X:/...` as `/mnt/x/...` (WSL's drive mounts). `None` when `path` isn't
/// drive-letter-rooted.
fn wsl_path(drive_letter_path: &str) -> Option<PathBuf> {
    let mut chars = drive_letter_path.chars();
    let letter = chars.next()?.to_ascii_lowercase();
    if !letter.is_ascii_lowercase() {
        return None;
    }
    let rest = chars.as_str().strip_prefix(':')?.replace('\\', "/");
    let rest = rest.strip_prefix('/').unwrap_or(&rest);
    Some(PathBuf::from(format!("/mnt/{letter}/{rest}")))
}

/// This machine's path for an LRC root: remaps first, then -- off Windows -- the WSL drive mount
/// for a drive-letter path (on Windows a drive-letter path already *is* the native path; `/mnt/x`
/// means nothing to a Windows process). An already-absolute Unix path passes through, which is what
/// lets tests point a fixture root at a temp directory.
pub fn resolve_root_path(lrc_absolute: &str, remaps: &[RootRemap]) -> PathBuf {
    // LRC keeps a trailing separator (`D:\Photos\`); the Import button registers the folder as
    // typed, without one. `ensure_root` matches the exact string, so normalise or the same folder
    // becomes two roots and every file is ingested twice.
    let remapped = apply_remaps(lrc_absolute, remaps);
    let trimmed = remapped.trim_end_matches(['/', '\\']);
    let remapped = if trimmed.is_empty() || trimmed.ends_with(':') {
        remapped
    } else {
        trimmed.to_string()
    };
    if cfg!(target_os = "windows") {
        return PathBuf::from(remapped);
    }
    wsl_path(&remapped)
        .or_else(|| remapped.starts_with('/').then(|| PathBuf::from(&remapped)))
        .unwrap_or_else(|| PathBuf::from(remapped))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_tolerates_missing_trailing_separators() {
        assert_eq!(
            join_lrc_path("D:\\Photos\\", "sub/dir", "test", "NEF"),
            "D:\\Photos/sub/dir/test.NEF"
        );
        assert_eq!(
            join_lrc_path("D:/Photos", "sub/dir/", "test", "NEF"),
            "D:/Photos/sub/dir/test.NEF"
        );
        assert_eq!(
            join_lrc_path("D:/Photos", "", "test", "NEF"),
            "D:/Photos/test.NEF"
        );
        assert_eq!(
            join_lrc_path("D:/Photos", "a", "test", ""),
            "D:/Photos/a/test"
        );
    }

    #[test]
    fn rel_path_is_nfc_forward_slashed_and_untrimmed_of_nothing_else() {
        assert_eq!(
            rel_path_under_root("2026\\Event/", "IMG_1", "NEF"),
            "2026/Event/IMG_1.NEF"
        );
        assert_eq!(rel_path_under_root("", "IMG_1", "NEF"), "IMG_1.NEF");
        // NFD "e + combining acute" composes to NFC, exactly as ingest stores it.
        assert_eq!(
            rel_path_under_root("caf\u{65}\u{301}", "a", "NEF"),
            "caf\u{e9}/a.NEF"
        );
    }

    #[test]
    fn remap_matches_whole_segments_case_insensitively_and_keeps_tail_case() {
        let remaps = [RootRemap {
            from: "D:\\Photos".into(),
            to: "/data/pics".into(),
        }];
        assert_eq!(
            apply_remaps("d:/photos/2026/Event", &remaps),
            "/data/pics/2026/Event"
        );
        assert_eq!(apply_remaps("D:\\Photos", &remaps), "/data/pics");
        assert_eq!(
            apply_remaps("D:\\PhotosOld\\x", &remaps),
            "D:\\PhotosOld\\x"
        );
        assert_eq!(apply_remaps("E:\\Other", &remaps), "E:\\Other");
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn a_trailing_separator_is_dropped_so_the_root_matches_the_import_button() {
        assert_eq!(
            resolve_root_path("D:\\Photos\\", &[]),
            PathBuf::from("/mnt/d/Photos")
        );
        assert_eq!(resolve_root_path("/tmp/x/", &[]), PathBuf::from("/tmp/x"));
        assert_eq!(resolve_root_path("D:\\", &[]), PathBuf::from("/mnt/d/"));
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn drive_letters_map_to_wsl_mounts_and_unix_paths_pass_through() {
        assert_eq!(
            resolve_root_path("C:\\Photos\\2026", &[]),
            PathBuf::from("/mnt/c/Photos/2026")
        );
        assert_eq!(resolve_root_path("/tmp/x", &[]), PathBuf::from("/tmp/x"));
        let remaps = [RootRemap {
            from: "D:\\".into(),
            to: "/mnt/g".into(),
        }];
        assert_eq!(
            resolve_root_path("D:\\Photos", &remaps),
            PathBuf::from("/mnt/g/Photos")
        );
    }
}
