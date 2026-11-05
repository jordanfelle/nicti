//! Which drives/folders are archive locations (#72, ADR-0072).
//!
//! A folder moved *onto* an archive location is archived: its T0 thumbnails become `.thumb.jpg`
//! sidecars next to the RAWs and the catalog drops its copies. A move *out* to a non-archive
//! location reverses that. A whole drive is keyed by its **volume identity** (ADR-0071, #392), so the
//! flag survives a drive-letter change, a `\\?\` prefix or a symlinked path; any other location (a
//! folder, or a volume whose identity can't be read, which is every volume off Windows) keeps the
//! path-prefix rule, so nothing changes where identity is unavailable.

use std::path::{Path, PathBuf};

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nicti_lair::thumb_sidecar::SidecarCodec;
use nicti_lair::volume::FolderVolume;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
struct Saved {
    archive_locations: Vec<String>,
    /// Whole volumes flagged as archive drives (#392). Absent in older files; a junk element is
    /// skipped rather than discarding the rest of the file.
    #[serde(default, deserialize_with = "lenient_volumes")]
    archive_volumes: Vec<ArchiveVolume>,
    /// Thumbnail sidecar codec for folders archived from now on (#393). Absent in older files.
    #[serde(default, deserialize_with = "lenient_codec")]
    sidecar_codec: SidecarCodec,
}

/// A whole drive flagged as an archive: its identity key, and the path it was flagged under (kept
/// so an offline drive can still be recognised, and unflagged, by that path).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ArchiveVolume {
    key: String,
    path: String,
}

fn lenient_volumes<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<ArchiveVolume>, D::Error> {
    let v = Vec::<serde_json::Value>::deserialize(d).unwrap_or_default();
    Ok(v.into_iter()
        .filter_map(|e| serde_json::from_value(e).ok())
        .collect())
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
    /// Whole archive drives (identity key + the path they were flagged under).
    volumes: Vec<ArchiveVolume>,
    codec: SidecarCodec,
    identify: IdentifyFolder,
    /// Recent lookups by normalised path. The folder panel asks about every drive on every frame
    /// and a real lookup enumerates the volumes (disk I/O), so answers are reused for
    /// [`LOOKUP_TTL`]; [`ArchiveDrives::set`] always looks fresh.
    cache: Mutex<HashMap<String, (Instant, Option<FolderVolume>)>>,
}

/// How long a volume lookup is reused. Drive letters change on a replug, not between frames.
const LOOKUP_TTL: Duration = Duration::from_secs(5);

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
            cache: Mutex::new(HashMap::new()),
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

    /// The volume `path` is on, cached for [`LOOKUP_TTL`].
    fn lookup(&self, path: &Path) -> Option<FolderVolume> {
        let key = norm(&path.to_string_lossy());
        let now = Instant::now();
        if let Ok(cache) = self.cache.lock() {
            if let Some((at, found)) = cache.get(&key) {
                if now.duration_since(*at) < LOOKUP_TTL {
                    return found.clone();
                }
            }
        }
        let found = (self.identify)(path);
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(key, (now, found.clone()));
        }
        found
    }

    /// Whether `path` is on a flagged volume. Never looks anything up while nothing is flagged.
    fn volume_flagged(&self, path: &Path) -> bool {
        !self.volumes.is_empty()
            && self.lookup(path).is_some_and(|fv| {
                self.volumes
                    .iter()
                    .any(|v| v.key == fv.identity.identity_key)
            })
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
            || (!self.volumes.is_empty()
                && self.lookup(Path::new(location)).is_some_and(|fv| {
                    fv.is_mount_root
                        && self.volumes.iter().any(|v| v.key == fv.identity.identity_key)
                }))
    }

    /// Adds or removes `location` and saves. The in-memory change sticks even if the save fails.
    pub fn set(&mut self, location: &str, archive: bool) -> std::io::Result<()> {
        let l = norm(location);
        if archive && l.is_empty() {
            return Err(std::io::Error::other(
                "the filesystem root can't be an archive drive; pick a mounted drive or folder",
            ));
        }
        // A whole, identifiable drive is flagged by identity *and* by the path it was flagged
        // under, so while it is offline (or unreadable) the old path rule still recognises it and
        // it can still be unflagged. Always looked up fresh.
        self.cache.lock().map(|mut c| c.clear()).ok();
        let drive = (self.identify)(Path::new(location)).filter(|fv| fv.is_mount_root);
        self.locations.retain(|x| norm(x) != l);
        self.volumes.retain(|v| norm(&v.path) != l);
        if let Some(fv) = &drive {
            let key = &fv.identity.identity_key;
            // Drop everything else on record for this same disk (another letter, an older path
            // entry), so one click flags or unflags it as a whole.
            self.volumes.retain(|v| v.key != *key);
            let same_disk = |p: &str| {
                (self.identify)(Path::new(p))
                    .is_some_and(|f| f.is_mount_root && f.identity.identity_key == *key)
            };
            let stale: Vec<String> = self
                .locations
                .iter()
                .filter(|x| same_disk(x))
                .cloned()
                .collect();
            self.locations.retain(|x| !stale.contains(x));
        }
        if archive {
            self.locations.push(location.to_string());
            if let Some(fv) = drive {
                self.volumes.push(ArchiveVolume {
                    key: fv.identity.identity_key,
                    path: location.to_string(),
                });
            }
        }
        self.locations.sort();
        self.volumes.sort_by(|a, b| a.key.cmp(&b.key));
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
        // A folder on the flagged disk is archive, but it is not itself the flagged *location*.
        assert!(a.is_archive(Path::new("E:\\events")));
        assert!(!a.is_location("E:\\events"));
        assert_eq!(a.volumes.len(), 1);
        // Unflagging under the new letter clears the disk, whatever letter it was flagged under.
        a.set("E:\\", false).unwrap();
        assert!(!a.is_archive(Path::new("D:\\events\\x")));
        assert!(!a.is_archive(Path::new("E:\\events\\x")));
        assert!(a.locations.is_empty() && a.volumes.is_empty());
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
    fn an_offline_flagged_drive_is_still_an_archive_and_can_still_be_unflagged() {
        let mut a = ArchiveDrives::default().with_identify(fake_identify);
        a.set("D:\\", true).unwrap();
        // Unplugged: identity can't be read, so only the saved path speaks, as it always did.
        a.identify = Arc::new(|_| None);
        a.cache.lock().unwrap().clear();
        assert!(a.is_archive(Path::new("D:\\events\\x")));
        assert!(a.is_location("D:\\"));
        a.set("D:\\", false).unwrap();
        assert!(a.locations.is_empty() && a.volumes.is_empty());
        // Back online: it is no longer an archive.
        a.identify = Arc::new(fake_identify);
        a.cache.lock().unwrap().clear();
        assert!(!a.is_archive(Path::new("E:\\x")));
    }

    #[test]
    fn older_path_entries_for_one_disk_are_replaced_as_a_whole() {
        // Two letters of the same disk were flagged by path before identity existed.
        let mut a = ArchiveDrives::with_locations(&["D:\\", "E:\\", "C:\\keep"])
            .with_identify(fake_identify);
        a.set("D:\\", true).unwrap();
        assert_eq!(a.locations, ["C:\\keep", "D:\\"]);
        assert_eq!(a.volumes.len(), 1);
        a.set("D:\\", false).unwrap();
        assert_eq!(a.locations, ["C:\\keep"]);
        assert!(!a.is_archive(Path::new("E:\\x")));
    }

    #[test]
    fn volumes_persist_survive_junk_and_a_host_without_identity_ignores_them() {
        let d = tempfile::tempdir().unwrap();
        let cat = d.path().join("cat.db");
        let mut a = ArchiveDrives::load(&cat).with_identify(fake_identify);
        a.set("D:\\", true).unwrap();
        let again = ArchiveDrives::load(&cat).with_identify(fake_identify);
        assert!(again.is_archive(Path::new("E:\\x")));
        // No identity (the default lookup off Windows): the saved path rule still answers.
        #[cfg(not(windows))]
        {
            let plain = ArchiveDrives::load(&cat);
            assert!(!plain.is_archive(Path::new("E:\\x")));
            assert!(plain.is_archive(Path::new("D:\\x")));
        }
        // A junk element costs only itself; an older file still loads.
        std::fs::write(
            file_for(&cat),
            r#"{"archive_locations":["/a"],"archive_volumes":[123,null,{"key":"K1","path":"D:\\"}]}"#,
        )
        .unwrap();
        let b = ArchiveDrives::load(&cat);
        assert!(b.is_location("/a") && b.volumes.len() == 1);
        std::fs::write(file_for(&cat), r#"{"archive_locations":["/a"]}"#).unwrap();
        assert!(ArchiveDrives::load(&cat).is_location("/a"));
    }

    #[test]
    fn volume_lookups_are_cached_and_never_made_while_nothing_is_flagged() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let mut a = ArchiveDrives::default().with_identify(move |p| {
            counter.fetch_add(1, Ordering::SeqCst);
            fake_identify(p)
        });
        // Nothing flagged: the folder panel's per-frame questions cost no lookup at all.
        for _ in 0..50 {
            assert!(!a.is_location("D:\\"));
            assert!(!a.is_archive(Path::new("D:\\x")));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        a.set("D:\\", true).unwrap();
        let after_set = calls.load(Ordering::SeqCst);
        // Flagged: repeated per-frame questions about the same paths reuse one lookup each.
        for _ in 0..50 {
            assert!(a.is_location("E:\\"));
            assert!(a.is_archive(Path::new("E:\\x")));
        }
        assert!(calls.load(Ordering::SeqCst) - after_set <= 2);
    }
}
