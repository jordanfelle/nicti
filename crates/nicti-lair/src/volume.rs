//! Volume identity (#510, ADR-0071), promoted from `spikes/homing/src/volume.rs`.
//!
//! [`identity_key`] is the ADR's decision: NTFS 64-bit serial + GPT partition GUID, falling back to
//! MBR signature + partition offset + 32-bit serial. [`identify`] finds the volume a folder lives on
//! and returns its key, label and `.nicti-volume` marker (read only: this never writes a file onto a
//! user's volume). The enumeration is Windows-only (v1's target); everywhere else [`identify`]
//! returns `None` and roots stay under `scruff::PLACEHOLDER_VOLUME_IDENTITY_KEY` (non-Windows
//! identity is #73). **The Windows half is unverified on this Linux host**: it is carried over from
//! the spike (which compiled and ran on Windows CI) with the anyhow/marker-writing pieces removed;
//! the pure parts below are unit-tested.

use std::path::Path;

/// Everything a candidate identity key could be built from (see ADR-0071's survival table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeInfo {
    /// `\\?\Volume{GUID}\`: per-machine, not an identity input.
    pub volume_guid_path: Option<String>,
    /// Drive letter (`H:\`) or folder mount point(s).
    pub mount_points: Vec<String>,
    pub serial_32: Option<u32>,
    pub ntfs_serial_64: Option<u64>,
    pub partition_guid: Option<String>,
    pub mbr_signature: Option<u32>,
    pub mbr_partition_offset: Option<u64>,
    pub label: Option<String>,
    pub total_bytes: Option<u64>,
    pub removable: bool,
    /// Content of a `.nicti-volume` file at the volume root, if one exists.
    pub marker_uuid: Option<String>,
}

/// The identity key ADR-0071 selects, or `None` when the volume is unidentifiable (callers must
/// then treat it as such, not silently skip it).
pub fn identity_key(v: &VolumeInfo) -> Option<String> {
    if let (Some(ntfs), Some(guid)) = (v.ntfs_serial_64, &v.partition_guid) {
        return Some(format!("ntfs64:{ntfs:016x}+gpt:{guid}"));
    }
    if let (Some(sig), Some(off), Some(serial)) =
        (v.mbr_signature, v.mbr_partition_offset, v.serial_32)
    {
        return Some(format!("mbr:{sig:08x}+{off:016x}+vsn:{serial:08x}"));
    }
    None
}

/// What registering a root needs to know about its volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeIdentity {
    pub identity_key: String,
    pub label: Option<String>,
    /// The `.nicti-volume` marker, the secondary signal feeding the catalog's
    /// `VolumeIdentityConflict` guard.
    pub marker_uuid: Option<String>,
}

/// A path normalised for mount-point prefix matching: Windows verbatim prefix (`\\?\`) removed,
/// one separator style, lower case (drive letters and NTFS names compare case-insensitively).
fn normalized(path: &str) -> String {
    let p = path.strip_prefix(r"\\?\").unwrap_or(path);
    p.replace('/', "\\").to_lowercase()
}

/// The volume `path` lives on: the one with the longest mount point that is a path-component
/// prefix of it (so `D:\mnt\photos` beats `D:\` for a folder under that mount). `None` when no
/// volume's mount point contains it.
pub fn volume_containing<'a>(volumes: &'a [VolumeInfo], path: &str) -> Option<&'a VolumeInfo> {
    let target = normalized(path);
    volumes
        .iter()
        .flat_map(|v| v.mount_points.iter().map(move |mp| (v, normalized(mp))))
        .filter(|(_, mp)| {
            let mp = mp.trim_end_matches('\\');
            target == mp
                || (target.starts_with(mp)
                    && (mp.is_empty() || target[mp.len()..].starts_with('\\')))
        })
        .max_by_key(|(_, mp)| mp.trim_end_matches('\\').len())
        .map(|(v, _)| v)
}

/// The identity of `v`, or `None` when it has no usable key.
pub fn identity_of(v: &VolumeInfo) -> Option<VolumeIdentity> {
    Some(VolumeIdentity {
        identity_key: identity_key(v)?,
        label: v.label.clone(),
        marker_uuid: v.marker_uuid.clone(),
    })
}

/// A reusable folder-to-volume lookup (see [`identifier`]).
pub type Identify = Box<dyn Fn(&Path) -> Option<VolumeIdentity> + Send + Sync>;

/// An `identify` that enumerates the volumes once and answers every path from that one snapshot
/// (a startup migration over many roots). Always `None` off Windows.
pub fn identifier() -> Identify {
    #[cfg(windows)]
    {
        let volumes = windows_impl::enumerate().ok();
        Box::new(move |path| {
            let volumes = volumes.as_ref()?;
            let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            volume_containing(volumes, &canon.to_string_lossy()).and_then(identity_of)
        })
    }
    #[cfg(not(windows))]
    {
        Box::new(|_| None)
    }
}

/// A folder's volume identity plus whether the folder is that volume's mount root itself (a whole
/// drive, as opposed to a folder on it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderVolume {
    pub identity: VolumeIdentity,
    pub is_mount_root: bool,
}

/// [`FolderVolume`] for `path` from a volume snapshot: pure, so it is testable without Windows.
pub fn folder_volume(volumes: &[VolumeInfo], path: &str) -> Option<FolderVolume> {
    let volume = volume_containing(volumes, path)?;
    let identity = identity_of(volume)?;
    let target = normalized(path);
    let target = target.trim_end_matches('\\');
    let is_mount_root = volume
        .mount_points
        .iter()
        .any(|mp| normalized(mp).trim_end_matches('\\') == target);
    Some(FolderVolume {
        identity,
        is_mount_root,
    })
}

/// [`folder_volume`] for a real folder; `None` where [`identify`] has no answer (not Windows, the
/// volume isn't mounted, its partition info can't be read).
pub fn identify_folder(path: &Path) -> Option<FolderVolume> {
    #[cfg(windows)]
    {
        let volumes = windows_impl::enumerate().ok()?;
        let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        folder_volume(&volumes, &canon.to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        None
    }
}

/// The volume identity of the folder `path`, or `None` when it can't be established (not Windows,
/// the volume isn't mounted, or its partition info can't be read: the caller keeps the placeholder).
pub fn identify(path: &Path) -> Option<VolumeIdentity> {
    #[cfg(windows)]
    {
        let volumes = windows_impl::enumerate().ok()?;
        let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        volume_containing(&volumes, &canon.to_string_lossy()).and_then(identity_of)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        None
    }
}

#[cfg(windows)]
pub mod windows_impl {
    use super::VolumeInfo;
    use std::fs;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{HANDLE, MAX_PATH};
    use windows_sys::Win32::Storage::FileSystem::{
        FindFirstVolumeW, FindNextVolumeW, FindVolumeClose, GetVolumeInformationW,
        GetVolumePathNamesForVolumeNameW,
    };
    use windows_sys::Win32::System::Ioctl::{
        DRIVE_LAYOUT_INFORMATION_EX, FSCTL_GET_NTFS_VOLUME_DATA, IOCTL_DISK_GET_DRIVE_LAYOUT_EX,
        IOCTL_DISK_GET_PARTITION_INFO_EX, IOCTL_STORAGE_GET_DEVICE_NUMBER, NTFS_VOLUME_DATA_BUFFER,
        PARTITION_INFORMATION_EX, PARTITION_STYLE_GPT, PARTITION_STYLE_MBR, STORAGE_DEVICE_NUMBER,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    /// A handle for metadata IOCTLs only: access 0 and full sharing. `File::open` asks for
    /// GENERIC_READ, which Windows refuses on a raw volume or physical drive for a non-elevated
    /// user; every query below (`FSCTL_GET_NTFS_VOLUME_DATA`, the partition/layout/device-number
    /// IOCTLs) is `FILE_ANY_ACCESS`, so a zero-access handle is enough and works as a standard user.
    fn open_query(path: &str) -> io::Result<fs::File> {
        use std::os::windows::fs::OpenOptionsExt;
        const SHARE_ALL: u32 = 0x1 | 0x2 | 0x4; // FILE_SHARE_READ | WRITE | DELETE
        fs::OpenOptions::new()
            .access_mode(0)
            .share_mode(SHARE_ALL)
            .open(path)
    }

    /// Keeps Windows from raising its "there is no disk in the drive" box for an unready
    /// removable drive while volumes are probed (thread-scoped; restored on drop).
    struct NoCriticalErrorBox(u32);

    impl NoCriticalErrorBox {
        fn new() -> Self {
            use windows_sys::Win32::System::Diagnostics::Debug::{
                SetThreadErrorMode, SEM_FAILCRITICALERRORS,
            };
            let mut old = 0u32;
            // SAFETY: `old` is a valid out pointer; failure just leaves the previous mode in place.
            unsafe { SetThreadErrorMode(SEM_FAILCRITICALERRORS, &mut old) };
            Self(old)
        }
    }

    impl Drop for NoCriticalErrorBox {
        fn drop(&mut self) {
            use windows_sys::Win32::System::Diagnostics::Debug::SetThreadErrorMode;
            // SAFETY: restores the mode saved in `new`.
            unsafe { SetThreadErrorMode(self.0, std::ptr::null_mut()) };
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    /// Enumerates every mounted volume via `FindFirstVolumeW`/`FindNextVolumeW`, then fills in
    /// each volume's identity fields. Errors reading one volume's partition info are recorded as
    /// `None` fields rather than aborting the whole enumeration -- a locked/offline volume
    /// shouldn't hide every other volume's data.
    pub fn enumerate() -> io::Result<Vec<VolumeInfo>> {
        let _no_dialogs = NoCriticalErrorBox::new();
        let mut out = Vec::new();
        let mut buf = [0u16; MAX_PATH as usize];
        // SAFETY: buf is a valid, correctly-sized wide-char buffer; FindFirstVolumeW/
        // FindNextVolumeW/FindVolumeClose is the documented enumeration triple.
        let handle = unsafe { FindFirstVolumeW(buf.as_mut_ptr(), buf.len() as u32) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        loop {
            let guid_path = String::from_utf16_lossy(
                &buf[..buf.iter().position(|&c| c == 0).unwrap_or(buf.len())],
            );
            out.push(volume_info_for(&guid_path));

            let ok = unsafe { FindNextVolumeW(handle, buf.as_mut_ptr(), buf.len() as u32) };
            if ok == 0 {
                break;
            }
        }
        unsafe { FindVolumeClose(handle) };
        Ok(out)
    }

    fn mount_points_for(guid_path: &str) -> Vec<String> {
        let wide_guid = wide(guid_path);
        let mut buf = vec![0u16; 4096];
        let mut needed: u32 = 0;
        let ok = unsafe {
            GetVolumePathNamesForVolumeNameW(
                wide_guid.as_ptr(),
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut needed,
            )
        };
        if ok == 0 {
            return Vec::new();
        }
        // The buffer is a sequence of NUL-terminated wide strings, itself terminated by an extra
        // NUL -- split on NUL and drop empty tail entries.
        buf.truncate(needed.max(1) as usize);
        String::from_utf16_lossy(&buf)
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect()
    }

    fn volume_info_for(guid_path: &str) -> VolumeInfo {
        let mount_points = mount_points_for(guid_path);
        let (serial_32, label, total_bytes) = volume_information(guid_path);
        let ntfs_serial_64 = ntfs_volume_data(guid_path);
        let marker_uuid = mount_points
            .first()
            .and_then(|mp| read_or_none(&format!("{mp}.nicti-volume")));
        let (partition_guid, mbr_signature, mbr_partition_offset) =
            partition_info(guid_path).unwrap_or((None, None, None));
        let removable = mount_points
            .first()
            .map(|mp| drive_type_is_removable(mp))
            .unwrap_or(false);

        VolumeInfo {
            volume_guid_path: Some(guid_path.trim_end_matches('\\').to_string()),
            mount_points,
            serial_32,
            ntfs_serial_64,
            partition_guid,
            mbr_signature,
            mbr_partition_offset,
            label,
            total_bytes,
            removable,
            marker_uuid,
        }
    }

    fn volume_information(guid_path: &str) -> (Option<u32>, Option<String>, Option<u64>) {
        let wide_path = wide(guid_path);
        let mut label_buf = [0u16; 256];
        let mut serial: u32 = 0;
        let mut max_component: u32 = 0;
        let mut flags: u32 = 0;
        let ok = unsafe {
            GetVolumeInformationW(
                wide_path.as_ptr(),
                label_buf.as_mut_ptr(),
                label_buf.len() as u32,
                &mut serial,
                &mut max_component,
                &mut flags,
                std::ptr::null_mut(),
                0,
            )
        };
        if ok == 0 {
            return (None, None, None);
        }
        let label_end = label_buf
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(label_buf.len());
        let label = if label_end > 0 {
            Some(String::from_utf16_lossy(&label_buf[..label_end]))
        } else {
            None
        };
        (Some(serial), label, None)
    }

    fn ntfs_volume_data(guid_path: &str) -> Option<u64> {
        // `FSCTL_GET_NTFS_VOLUME_DATA` needs a genuine volume-device handle, not a directory
        // handle -- an earlier draft opened the mount point directly (`fs::File::open(mount_point)`,
        // e.g. `"H:\\"`), which is wrong on two counts: `CreateFileW` on a directory path needs
        // `FILE_FLAG_BACKUP_SEMANTICS` at minimum (plain `std::fs::File::open` doesn't set it), and
        // per Microsoft's own FSCTL documentation the control code itself requires a volume handle
        // (`\\.\H:` or `\\?\Volume{GUID}`), not a directory handle, regardless of that flag. Opening
        // the trimmed GUID path instead (same pattern `partition_info` below already uses
        // successfully) sidesteps both problems in one fix.
        let path = guid_path.trim_end_matches('\\');
        let file = open_query(path).ok()?;
        let handle: HANDLE = file.as_raw_handle() as HANDLE;
        let mut out = NTFS_VOLUME_DATA_BUFFER::default();
        let mut returned: u32 = 0;
        let ok = unsafe {
            DeviceIoControl(
                handle,
                FSCTL_GET_NTFS_VOLUME_DATA,
                std::ptr::null(),
                0,
                &mut out as *mut _ as *mut _,
                std::mem::size_of::<NTFS_VOLUME_DATA_BUFFER>() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return None;
        }
        // VolumeSerialNumber is an i64 in the windows-sys binding; NTFS serials are conventionally
        // displayed/stored unsigned.
        Some(out.VolumeSerialNumber as u64)
    }

    fn partition_info(guid_path: &str) -> Option<(Option<String>, Option<u32>, Option<u64>)> {
        let path = guid_path.trim_end_matches('\\');
        let file = open_query(path).ok()?;
        let handle: HANDLE = file.as_raw_handle() as HANDLE;
        let mut out = PARTITION_INFORMATION_EX::default();
        let mut returned: u32 = 0;
        let ok = unsafe {
            DeviceIoControl(
                handle,
                IOCTL_DISK_GET_PARTITION_INFO_EX,
                std::ptr::null(),
                0,
                &mut out as *mut _ as *mut _,
                std::mem::size_of::<PARTITION_INFORMATION_EX>() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return None;
        }
        match out.PartitionStyle {
            PARTITION_STYLE_GPT => {
                // SAFETY: PartitionStyle == GPT guarantees the union's Gpt arm is initialized.
                let gpt = unsafe { out.Anonymous.Gpt };
                let guid = format_guid(&gpt.PartitionId);
                Some((Some(guid), None, None))
            }
            PARTITION_STYLE_MBR => {
                // `PARTITION_INFORMATION_EX`'s MBR arm (`PARTITION_INFORMATION_MBR`) has no
                // signature field at all -- only `PartitionType`/`BootIndicator`/
                // `RecognizedPartition`/`HiddenSectors`/`PartitionId`. An earlier draft read a
                // nonexistent `mbr.Signature` here (caught by real Windows CI, not by this
                // sandbox, which can't compile this module at all). The MBR disk signature is a
                // whole-*disk* property, not a per-partition one: it lives on
                // `DRIVE_LAYOUT_INFORMATION_MBR`, returned by `IOCTL_DISK_GET_DRIVE_LAYOUT_EX`
                // against the owning `\\.\PhysicalDriveN` device handle, found via
                // `IOCTL_STORAGE_GET_DEVICE_NUMBER` on this same volume handle.
                let signature = disk_number_for(guid_path).and_then(mbr_disk_signature);
                Some((None, signature, Some(out.StartingOffset as u64)))
            }
            _ => Some((None, None, None)),
        }
    }

    fn disk_number_for(guid_path: &str) -> Option<u32> {
        let path = guid_path.trim_end_matches('\\');
        let file = open_query(path).ok()?;
        let handle: HANDLE = file.as_raw_handle() as HANDLE;
        let mut out = STORAGE_DEVICE_NUMBER {
            DeviceType: 0,
            DeviceNumber: 0,
            PartitionNumber: 0,
        };
        let mut returned: u32 = 0;
        let ok = unsafe {
            DeviceIoControl(
                handle,
                IOCTL_STORAGE_GET_DEVICE_NUMBER,
                std::ptr::null(),
                0,
                &mut out as *mut _ as *mut _,
                std::mem::size_of::<STORAGE_DEVICE_NUMBER>() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return None;
        }
        Some(out.DeviceNumber)
    }

    fn mbr_disk_signature(disk_number: u32) -> Option<u32> {
        let device_path = format!(r"\\.\PhysicalDrive{disk_number}");
        let file = open_query(&device_path).ok()?;
        let handle: HANDLE = file.as_raw_handle() as HANDLE;

        // `IOCTL_DISK_GET_DRIVE_LAYOUT_EX`'s response is variable-length (a fixed header
        // followed by a `PartitionEntry` array sized by the disk's actual partition count).
        // Microsoft's own guidance for this IOCTL is explicit that a too-small buffer isn't
        // guaranteed to report back a usable required size (no reliable
        // ERROR_INSUFFICIENT_BUFFER-plus-exact-size contract) -- the documented recovery is to
        // retry with a doubled buffer on any failure, not to size it once from a fixed guess (an
        // earlier draft's bug: a 4-partition guess that a real MBR disk with more partitions
        // could exceed, silently losing the signature and, with it, `identity_key`). This also
        // fixes a real alignment bug in that same earlier draft: `Vec<u8>` only guarantees
        // `u8`-alignment, so casting its pointer to `*const DRIVE_LAYOUT_INFORMATION_EX` isn't
        // sound even when it happens to work in practice -- `Vec<DRIVE_LAYOUT_INFORMATION_EX>`
        // guarantees the buffer's alignment actually matches what the struct needs.
        const MAX_ATTEMPTS: u32 = 8;
        let mut element_count: usize = 1
            + (4 * std::mem::size_of::<PARTITION_INFORMATION_EX>())
                .div_ceil(std::mem::size_of::<DRIVE_LAYOUT_INFORMATION_EX>());
        for _ in 0..MAX_ATTEMPTS {
            let mut buf: Vec<DRIVE_LAYOUT_INFORMATION_EX> = (0..element_count)
                .map(|_| DRIVE_LAYOUT_INFORMATION_EX::default())
                .collect();
            let byte_len = element_count * std::mem::size_of::<DRIVE_LAYOUT_INFORMATION_EX>();
            let mut returned: u32 = 0;
            let ok = unsafe {
                DeviceIoControl(
                    handle,
                    IOCTL_DISK_GET_DRIVE_LAYOUT_EX,
                    std::ptr::null(),
                    0,
                    buf.as_mut_ptr() as *mut _,
                    byte_len as u32,
                    &mut returned,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                // Don't try to distinguish "buffer too small" from any other failure via
                // GetLastError -- this IOCTL doesn't reliably map to ERROR_INSUFFICIENT_BUFFER,
                // per Microsoft's own docs. Just grow and retry; give up after MAX_ATTEMPTS.
                element_count *= 2;
                continue;
            }
            // SAFETY: DeviceIoControl succeeded; `buf`'s first element is a valid, aligned,
            // fully-initialized `DRIVE_LAYOUT_INFORMATION_EX` (the header fields this function
            // reads are always at the start of the response, regardless of partition count).
            let layout = &buf[0];
            // DRIVE_LAYOUT_INFORMATION_EX::PartitionStyle is `u32`, unlike
            // PARTITION_INFORMATION_EX::PartitionStyle (`PARTITION_STYLE`, an `i32` alias) above
            // -- two different windows-sys struct defs for what the Win32 API treats as the same
            // logical enum, hence the cast.
            if layout.PartitionStyle != PARTITION_STYLE_MBR as u32 {
                return None;
            }
            // SAFETY: PartitionStyle == MBR guarantees the union's Mbr arm is initialized.
            return Some(unsafe { layout.Anonymous.Mbr }.Signature);
        }
        None
    }

    fn format_guid(guid: &windows_sys::core::GUID) -> String {
        format!(
            "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            guid.data1,
            guid.data2,
            guid.data3,
            guid.data4[0],
            guid.data4[1],
            guid.data4[2],
            guid.data4[3],
            guid.data4[4],
            guid.data4[5],
            guid.data4[6],
            guid.data4[7],
        )
    }

    /// The marker file's content, trimmed. Read at most 256 bytes (a real marker is a short id): a
    /// huge or hostile file must not be pulled into memory and stored. Not UTF-8 or empty is `None`.
    fn read_or_none(path: &str) -> Option<String> {
        use std::io::Read;
        let mut buf = Vec::new();
        fs::File::open(path)
            .ok()?
            .take(256)
            .read_to_end(&mut buf)
            .ok()?;
        let text = String::from_utf8(buf).ok()?;
        let text = text.trim();
        (!text.is_empty()).then(|| text.to_string())
    }

    fn drive_type_is_removable(mount_point: &str) -> bool {
        // DRIVE_REMOVABLE lives in Win32::System::WindowsProgramming, not
        // Win32::Storage::FileSystem where GetDriveTypeW itself is defined -- an earlier draft
        // guessed the wrong module (caught by real Windows CI, not this sandbox).
        use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
        use windows_sys::Win32::System::WindowsProgramming::DRIVE_REMOVABLE;
        let wide_mp = wide(mount_point);
        unsafe { GetDriveTypeW(wide_mp.as_ptr()) == DRIVE_REMOVABLE }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(
        ntfs: Option<u64>,
        gpt: Option<&str>,
        mbr: Option<(u32, u64)>,
        vsn: Option<u32>,
    ) -> VolumeInfo {
        VolumeInfo {
            volume_guid_path: None,
            mount_points: vec![],
            serial_32: vsn,
            ntfs_serial_64: ntfs,
            partition_guid: gpt.map(String::from),
            mbr_signature: mbr.map(|(s, _)| s),
            mbr_partition_offset: mbr.map(|(_, o)| o),
            label: None,
            total_bytes: None,
            removable: false,
            marker_uuid: None,
        }
    }

    fn mounted(label: &str, mount: &[&str], ntfs: u64, gpt: &str) -> VolumeInfo {
        VolumeInfo {
            mount_points: mount.iter().map(|m| m.to_string()).collect(),
            label: Some(label.into()),
            ..v(Some(ntfs), Some(gpt), None, None)
        }
    }

    #[test]
    fn a_folder_volume_knows_whether_the_folder_is_the_whole_drive() {
        let vols = vec![
            mounted(
                "Archive",
                &["D:\\"],
                1,
                "aaaaaaaa-1111-1111-1111-111111111111",
            ),
            mounted(
                "Work",
                &["C:\\", "C:\\mnt\\work"],
                2,
                "bbbbbbbb-1111-1111-1111-111111111111",
            ),
        ];
        let root = folder_volume(&vols, "D:\\").unwrap();
        assert!(root.is_mount_root);
        // Verbatim prefix, forward slashes, case and a trailing separator all still mean the root.
        assert!(folder_volume(&vols, r"\\?\d:/").unwrap().is_mount_root);
        let inside = folder_volume(&vols, "D:\\events").unwrap();
        assert!(!inside.is_mount_root);
        assert_eq!(inside.identity, root.identity);
        // A folder mount point of another volume is that volume's root, not C:'s.
        let mounted_dir = folder_volume(&vols, "C:\\mnt\\work\\").unwrap();
        assert!(mounted_dir.is_mount_root);
        // Outside every volume, or a volume with no usable key: no answer.
        assert!(folder_volume(&vols, "Z:\\x").is_none());
        let bare = vec![VolumeInfo {
            mount_points: vec!["F:\\".into()],
            ..v(None, None, None, None)
        }];
        assert!(folder_volume(&bare, "F:\\").is_none());
    }

    #[test]
    fn prefers_ntfs_plus_gpt_and_falls_back_to_mbr() {
        assert_eq!(
            identity_key(&v(
                Some(0xdead_beef),
                Some("11111111-1111-1111-1111-111111111111"),
                None,
                None
            )),
            Some("ntfs64:00000000deadbeef+gpt:11111111-1111-1111-1111-111111111111".into())
        );
        assert_eq!(
            identity_key(&v(
                None,
                None,
                Some((0xcafebabe, 1_048_576)),
                Some(0x1234_5678)
            )),
            Some("mbr:cafebabe+0000000000100000+vsn:12345678".into())
        );
        // An MBR signature and offset without the 32-bit serial is not a key (it would not be
        // unique per filesystem), and nothing at all is unidentifiable.
        assert_eq!(identity_key(&v(None, None, Some((1, 2)), None)), None);
        assert_eq!(identity_key(&v(None, None, None, None)), None);
        assert_ne!(
            identity_key(&v(Some(1), Some("a"), None, None)),
            identity_key(&v(Some(2), Some("b"), None, None))
        );
    }

    #[test]
    fn the_volume_containing_a_path_is_the_longest_mount_point_prefix() {
        let vols = [
            mounted("C", &["C:\\"], 1, "g1"),
            mounted("Photos", &["D:\\"], 2, "g2"),
            mounted("Archive", &["D:\\mnt\\archive\\"], 3, "g3"),
        ];
        let label = |p: &str| volume_containing(&vols, p).and_then(|v| v.label.clone());
        assert_eq!(label("C:\\Users\\me"), Some("C".into()));
        assert_eq!(
            label("d:/photos/2026"),
            Some("Photos".into()),
            "case and separators"
        );
        assert_eq!(
            label(r"\\?\D:\photos"),
            Some("Photos".into()),
            "verbatim prefix"
        );
        assert_eq!(
            label("D:\\mnt\\archive\\raw"),
            Some("Archive".into()),
            "nested mount wins"
        );
        assert_eq!(
            label("D:\\mnt\\archive"),
            Some("Archive".into()),
            "the mount point itself"
        );
        assert_eq!(
            label("D:\\mnt\\archive2"),
            Some("Photos".into()),
            "a sibling is not inside it"
        );
        assert_eq!(label("E:\\x"), None, "no volume there");
        assert_eq!(label(""), None);
    }

    #[test]
    fn identity_of_carries_the_label_and_marker_and_needs_a_key() {
        let mut vol = mounted("Photos", &["D:\\"], 2, "g2");
        vol.marker_uuid = Some("abc".into());
        let id = identity_of(&vol).unwrap();
        assert_eq!(id.label.as_deref(), Some("Photos"));
        assert_eq!(id.marker_uuid.as_deref(), Some("abc"));
        assert!(identity_of(&v(None, None, None, None)).is_none());
    }

    #[cfg(not(windows))]
    #[test]
    fn off_windows_nothing_is_identified() {
        assert!(identify(Path::new("/tmp")).is_none());
    }
}
