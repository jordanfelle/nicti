//! Which drives/folders are archive locations (#72, ADR-0072).
//!
//! A folder moved *onto* an archive location is archived: its T0 thumbnails become `.thumb.jpg`
//! sidecars next to the RAWs and the catalog drops its copies. A move *out* to a non-archive
//! location reverses that. A whole drive is keyed by its **volume identity** (ADR-0071, #392), so the
//! flag survives a drive-letter change, a `\\?\` prefix or a symlinked path; any other location (a
//! folder, or a volume whose identity can't be read, which is every volume off Windows) keeps the
//! path-prefix rule, so nothing changes where identity is unavailable.

use std::path::{Path, PathBuf};

use std::sync::Arc;

use nicti_lair::thumb_sidecar::SidecarCodec;
use nicti_lair::volume::FolderVolume;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
struct Saved {
    archive_locations: Vec<String>,
    /// Identity keys of whole volumes flagged as archive drives (#392). Absent in older files.
    #[serde(default)]
    archive_volumes: Vec<String>,
    /// Thumbnail sidecar codec for folders archived from now on (#393). Absent in older files.
    #[serde(default, deserialize_with = "lenient_codec")]
    sidecar_codec: SidecarCodec,
}

/// An unknown or hand-edited codec value must not discard the archive-location list that shares
/// its file: fall back to the default instead of failing the whole parse.
fn lenient_codec<'de, D: serde::Deserializer<'de>>(d: D) -> Result<SidecarCodec, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    Ok(serde_json::from_value(v).unwrap_or_default())
}

/// Folder -> volume identity lookup; injectable so tests cover the identity paths off Windows.
type IdentifyFolder = Arc<dyn Fn(&Path) -> Option<FolderVolume> + Send + Sync>;

pub struct ArchiveDrives {
    file: Option<PathBuf>,
    locations: Vec<String>,
    /// Volume identity keys (`volume::identity_key`) of whole archive drives.
    volumes: Vec<String>,
    codec: SidecarCodec,
    identify: IdentifyFolder,
}

impl std::fmt::Debug for ArchiveDrives {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArchiveDrives")
            .field("locations", &self.locations)
            .field("volumes", &self.volumes)
            .field("codec", &self.codec)
            .finish_non_exhaustive()
    }
}

impl Default for ArchiveDrives {
    fn default() -> Self {
        ArchiveDrives {
            file: None,
            locations: Vec::new(),
            volumes: Vec::new(),
            codec: SidecarCodec::default(),
            identify: Arc::new(nicti_lair::volume::identify_folder),
        }
    }
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
        let saved = std::fs::read_to_string(&file)
            .ok()
            .and_then(|t| serde_json::from_str::<Saved>(&t).ok())
            .unwrap_or_default();
        ArchiveDrives {
            file: Some(file),
            locations: saved.archive_locations,
            volumes: saved.archive_volumes,
            codec: saved.sidecar_codec,
            ..ArchiveDrives::default()
        }
    }

    #[cfg(test)]
    pub fn with_locations(locations: &[&str]) -> Self {
        ArchiveDrives {
            locations: locations.iter().map(|s| s.to_string()).collect(),
            ..ArchiveDrives::default()
        }
    }

    #[cfg(test)]
    fn with_identify(
        mut self,
        f: impl Fn(&Path) -> Option<FolderVolume> + Send + Sync + 'static,
    ) -> Self {
        self.identify = Arc::new(f);
        self
    }

    /// The saved identity key of the volume `path` is on, when that volume is flagged.
    fn volume_flagged(&self, path: &Path) -> bool {
        !self.volumes.is_empty()
            && (self.identify)(path)
                .is_some_and(|fv| self.volumes.contains(&fv.identity.identity_key))
    }

    /// `true` if `path` is, or is inside, an archive location.
    pub fn is_archive(&self, path: &Path) -> bool {
        if self.volume_flagged(path) {
            return true;
        }
        let p = norm(&path.to_string_lossy());
        self.locations.iter().any(|l| {
            let l = norm(l);
            // A bare "/" normalises to "" and would match every path: not a usable location.
            !l.is_empty() && (p == l || p.starts_with(&format!("{l}/")))
        })
    }

    pub fn is_location(&self, location: &str) -> bool {
        let l = norm(location);
        self.locations.iter().any(|x| norm(x) == l)
            // A flagged whole drive reads as flagged under whatever letter it has now.
            || (self.volume_flagged(Path::new(location))
                && (self.identify)(Path::new(location)).is_some_and(|fv| fv.is_mount_root))
    }

    /// Adds or removes `location` and saves. The in-memory change sticks even if the save fails.
    pub fn set(&mut self, location: &str, archive: bool) -> std::io::Result<()> {
        let l = norm(location);
        if archive && l.is_empty() {
            return Err(std::io::Error::other(
                "the filesystem root can't be an archive drive; pick a mounted drive or folder",
            ));
        }
        // A whole, identifiable drive is flagged by volume identity (drops any older path entry for
        // the same location); everything else keeps the path rule.
        let drive = (self.identify)(Path::new(location)).filter(|fv| fv.is_mount_root);
        self.locations.retain(|x| norm(x) != l);
        if let Some(fv) = drive {
            let key = fv.identity.identity_key;
            self.volumes.retain(|k| *k != key);
            if archive {
                self.volumes.push(key);
            }
            self.volumes.sort();
        } else if archive {
            self.locations.push(location.to_string());
        }
        self.locations.sort();
        self.save()
    }

    /// Codec for the thumbnail sidecars written when a folder is archived.
    pub fn sidecar_codec(&self) -> SidecarCodec {
        self.codec
    }

    /// Sets the sidecar codec and saves. The in-memory change sticks even if the save fails.
    pub fn set_sidecar_codec(&mut self, codec: SidecarCodec) -> std::io::Result<()> {
        self.codec = codec;
        self.save()
    }

    fn save(&self) -> std::io::Result<()> {
        let Some(file) = &self.file else {
            return Ok(());
        };
        let tmp = file.with_extension("json.tmp");
        std::fs::write(
            &tmp,
            serde_json::to_string(&Saved {
                archive_locations: self.locations.clone(),
                archive_volumes: self.volumes.clone(),
                sidecar_codec: self.codec,
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
    fn the_filesystem_root_is_not_a_usable_location() {
        let mut a = ArchiveDrives::with_locations(&["/"]);
        assert!(!a.is_archive(Path::new("/home/x")));
        assert!(a.set("/", true).is_err());
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

    #[test]
    fn codec_persists_and_an_old_file_loads_as_jpeg() {
        let d = tempfile::tempdir().unwrap();
        let cat = d.path().join("c.db");
        std::fs::write(file_for(&cat), r#"{"archive_locations":["/a"]}"#).unwrap();
        let mut a = ArchiveDrives::load(&cat);
        assert_eq!(a.sidecar_codec(), SidecarCodec::Jpeg);
        assert!(a.is_location("/a"));
        a.set_sidecar_codec(SidecarCodec::Avif).unwrap();
        let b = ArchiveDrives::load(&cat);
        assert_eq!(b.sidecar_codec(), SidecarCodec::Avif);
        assert!(b.is_location("/a"), "locations survive a codec save");
        std::fs::write(
            file_for(&cat),
            r#"{"archive_locations":["/a"],"sidecar_codec":"jxl"}"#,
        )
        .unwrap();
        let c = ArchiveDrives::load(&cat);
        assert!(c.is_location("/a"), "an unknown codec keeps the locations");
        assert_eq!(c.sidecar_codec(), SidecarCodec::Jpeg);
    }

    /// A fake Windows: `D:\` and `E:\` are the *same* volume (K1) under two drive letters, `C:\`
    /// is another (K2); only a bare drive root is a mount root.
    fn fake_identify(path: &Path) -> Option<FolderVolume> {
        let p = path.to_string_lossy().replace('/', "\\").to_lowercase();
        let key = match p.chars().next()? {
            'd' | 'e' => "K1",
            'c' => "K2",
            _ => return None,
        };
        Some(FolderVolume {
            identity: nicti_lair::volume::VolumeIdentity {
                identity_key: key.into(),
                label: None,
                marker_uuid: None,
            },
            is_mount_root: p.trim_end_matches('\\').len() == 2,
        })
    }

    #[test]
    fn a_flagged_drive_follows_its_volume_across_a_drive_letter_change() {
        let mut a = ArchiveDrives::default().with_identify(fake_identify);
        a.set("D:\\", true).unwrap();
        assert!(a.is_archive(Path::new("D:\\events\\x")));
        // The same disk now shows up as E:, and a different disk keeps its own answer.
        assert!(a.is_archive(Path::new("E:\\events\\x")));
        assert!(a.is_location("E:\\"));
        assert!(!a.is_archive(Path::new("C:\\events")));
        // It is recorded by identity, not as a path.
        assert!(a.locations.is_empty() && a.volumes == ["K1"]);
        // Unflagging under the new letter clears the volume.
        a.set("E:\\", false).unwrap();
        assert!(!a.is_archive(Path::new("D:\\events\\x")));
    }

    #[test]
    fn a_folder_on_an_identifiable_drive_stays_a_path_rule() {
        let mut a = ArchiveDrives::default().with_identify(fake_identify);
        a.set("D:\\events", true).unwrap();
        assert!(a.volumes.is_empty());
        assert!(a.is_archive(Path::new("D:\\events\\x")));
        assert!(!a.is_archive(Path::new("D:\\other")));
        // Not followed to E:: a folder is not a drive.
        assert!(!a.is_archive(Path::new("E:\\events\\x")));
    }

    #[test]
    fn an_older_path_entry_for_a_drive_is_replaced_by_its_identity_when_reflagged() {
        let mut a = ArchiveDrives::with_locations(&["D:\\"]).with_identify(fake_identify);
        assert!(a.is_archive(Path::new("D:\\x")));
        a.set("D:\\", true).unwrap();
        assert!(a.locations.is_empty() && a.volumes == ["K1"]);
        assert!(a.is_archive(Path::new("E:\\x")));
    }

    #[test]
    fn volumes_persist_and_an_unidentifiable_host_ignores_them() {
        let d = tempfile::tempdir().unwrap();
        let cat = d.path().join("cat.db");
        let mut a = ArchiveDrives::load(&cat).with_identify(fake_identify);
        a.set("D:\\", true).unwrap();
        let again = ArchiveDrives::load(&cat).with_identify(fake_identify);
        assert!(again.is_archive(Path::new("E:\\x")));
        // The default lookup finds no identity off Windows: the saved key matches nothing, and the
        // path rule is untouched.
        let plain = ArchiveDrives::load(&cat);
        #[cfg(not(windows))]
        assert!(!plain.is_archive(Path::new("E:\\x")));
        let _ = plain;
        // A file written before volumes existed still loads.
        std::fs::write(file_for(&cat), r#"{"archive_locations":["/a"]}"#).unwrap();
        assert!(ArchiveDrives::load(&cat).is_location("/a"));
    }
}
