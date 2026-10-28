//! Camera-card detection (#548, ADR-0436 section 5): which mounted volumes look like a card with
//! photos on it, i.e. have a top-level `DCIM` folder.
//!
//! Pure functions over a list of mount roots, so they are testable without a card. Probing a
//! volume that is mid spin-up (or a drive letter with no media) can block in `read_dir`, so callers
//! run [`scan_cards`] off the UI thread (a Pounce job) and cache the result; this module never
//! touches the UI or keeps state.

use std::fs;
use std::path::{Path, PathBuf};

/// A mounted volume with a `DCIM` folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardVolume {
    /// The volume's mount root (`E:\`, `/media/me/NIKON Z 8`).
    pub mount: PathBuf,
    /// Its `DCIM` folder, as spelled on the card.
    pub dcim: PathBuf,
    /// A display name: the mount's last path component, or the whole mount for a drive root.
    pub label: String,
}

/// The `DCIM` folder directly under `mount`, matched case-insensitively (cameras differ and FAT is
/// case-preserving). A symlink named DCIM is not followed: the card's contents are untrusted.
fn find_dcim(mount: &Path) -> Option<PathBuf> {
    for entry in fs::read_dir(mount).ok()?.flatten() {
        if !entry
            .file_name()
            .to_string_lossy()
            .eq_ignore_ascii_case("dcim")
        {
            continue;
        }
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            return Some(entry.path());
        }
    }
    None
}

/// The cards among `mounts`, in the order given. Unreadable mounts are skipped. Probes sequentially
/// with no timeout: a stale network mount can stall it, so call it off the UI thread.
pub fn scan_cards(mounts: &[PathBuf]) -> Vec<CardVolume> {
    mounts
        .iter()
        .filter_map(|mount| {
            let dcim = find_dcim(mount)?;
            let label = mount
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| mount.display().to_string());
            Some(CardVolume {
                mount: mount.clone(),
                dcim,
                label,
            })
        })
        .collect()
}

/// **Can block**: on Windows this probes every drive letter (an empty reader, a disconnected network
/// drive or a spinning-up card reader can take tens of seconds), so run it, like [`scan_cards`], off
/// the UI thread. It does not tell removable from fixed volumes; an internal drive with a DCIM folder
/// is listed too.
///
/// Where removable volumes are mounted on this platform: drive letters on Windows (the required
/// platform), and the usual per-user and system mount parents elsewhere.
pub fn default_mount_roots() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        ('A'..='Z')
            .map(|c| PathBuf::from(format!("{c}:\\")))
            .filter(|p| p.exists())
            .collect()
    }
    #[cfg(not(windows))]
    {
        let user = std::env::var("USER").unwrap_or_default();
        let parents = [
            PathBuf::from("/Volumes"),
            PathBuf::from("/media").join(&user),
            PathBuf::from("/run/media").join(&user),
            PathBuf::from("/media"),
            PathBuf::from("/mnt"),
        ];
        let mut roots = Vec::new();
        for parent in parents {
            let Ok(read) = fs::read_dir(&parent) else {
                continue;
            };
            let mut found: Vec<PathBuf> = read
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| e.path())
                .collect();
            found.sort();
            roots.extend(found);
        }
        // `/media/$USER` and `/run/media/$USER` are often the same place: list each volume once.
        let mut seen = std::collections::HashSet::new();
        roots.retain(|r| seen.insert(fs::canonicalize(r).unwrap_or_else(|_| r.clone())));
        roots
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_volume_with_a_dcim_folder_is_a_card_whatever_the_case() {
        let tmp = tempfile::tempdir().unwrap();
        for (name, dcim) in [("a", "DCIM"), ("b", "dcim"), ("c", "Dcim")] {
            fs::create_dir_all(tmp.path().join(name).join(dcim)).unwrap();
        }
        let mounts: Vec<PathBuf> = ["a", "b", "c"].iter().map(|n| tmp.path().join(n)).collect();
        let cards = scan_cards(&mounts);
        assert_eq!(cards.len(), 3);
        assert_eq!(cards[1].label, "b");
        assert!(cards[1].dcim.ends_with("dcim"));
    }

    #[test]
    fn volumes_without_a_real_dcim_folder_are_not_cards() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("plain/photos")).unwrap();
        // A *file* called DCIM is not a card.
        fs::create_dir_all(tmp.path().join("file")).unwrap();
        fs::write(tmp.path().join("file/DCIM"), b"x").unwrap();
        // DCIM nested one level down is not a card root.
        fs::create_dir_all(tmp.path().join("nested/sub/DCIM")).unwrap();
        let mounts: Vec<PathBuf> = ["plain", "file", "nested", "missing"]
            .iter()
            .map(|n| tmp.path().join(n))
            .collect();
        assert!(scan_cards(&mounts).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_named_dcim_is_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("elsewhere")).unwrap();
        fs::create_dir_all(tmp.path().join("vol")).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("elsewhere"), tmp.path().join("vol/DCIM"))
            .unwrap();
        assert!(scan_cards(&[tmp.path().join("vol")]).is_empty());
    }
}
