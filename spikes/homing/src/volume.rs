//! #71's volume-identity candidates. `VolumeInfo` collects every field a candidate identity key
//! could be built from; `identity_key()` implements the ADR-0020 decision (NTFS 64-bit serial +
//! GPT partition GUID, when both are present) so the rest of the crate doesn't need to know the
//! Windows plumbing.
//!
//! Windows-only: the real enumeration lives behind `#[cfg(windows)]`. The stub keeps
//! `cargo clippy`/`cargo test` green on Linux CI (this repo's `clippy`/`test` jobs run
//! `--workspace`, and `homing` isn't path-gated out the way `den`/`pelt-*`/`retina` are, since it
//! doesn't compile a heavy native dependency -- see CLAUDE.md's package-map note).

use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct VolumeInfo {
    /// `\\?\Volume{GUID}\` -- assigned by the mount manager, **not stable across machines** and,
    /// per the spike's own findings (see `docs/research/homing-volume-identity.md`), not
    /// guaranteed stable across a reformat either. Recorded for completeness, not used as the
    /// identity key.
    pub volume_guid_path: Option<String>,
    /// Current mount point(s) -- drive letter (`H:\`) or a folder mount point.
    pub mount_points: Vec<String>,
    /// `GetVolumeInformationW`'s 32-bit `VolumeSerialNumber`. Survives a letter change and
    /// detach/reattach; does **not** survive a reformat (Windows regenerates it).
    pub serial_32: Option<u32>,
    /// `FSCTL_GET_NTFS_VOLUME_DATA`'s 64-bit `VolumeSerialNumber` -- NTFS-only (empty on
    /// exFAT/FAT32). Same survival profile as `serial_32` in every case tested, but a much
    /// larger keyspace (collision risk is negligible vs. the 32-bit serial).
    pub ntfs_serial_64: Option<u64>,
    /// GPT partition GUID from `IOCTL_DISK_GET_PARTITION_INFO_EX`, when the underlying disk is
    /// GPT-partitioned (not MBR). Tied to the *partition*, not the filesystem inside it --
    /// survives a reformat, since reformatting doesn't repartition.
    pub partition_guid: Option<String>,
    /// MBR signature + partition start offset, the non-GPT fallback identity. Weaker than a GPT
    /// GUID (a signature collision across two disks is far more plausible than a GUID collision)
    /// but still tied to the partition, not the filesystem.
    pub mbr_signature: Option<u32>,
    pub mbr_partition_offset: Option<u64>,
    pub label: Option<String>,
    pub total_bytes: Option<u64>,
    pub removable: bool,
    /// Content of a `.nicti-volume` marker file at the volume root, if one exists -- the
    /// candidate portable tie-breaker from ADR-0020's Decision section. `None` means either no
    /// marker file or (on a read-only volume) one couldn't be written.
    pub marker_uuid: Option<String>,
}

/// The identity key ADR-0020 selects: NTFS 64-bit serial + GPT partition GUID when both are
/// present, falling back to the 32-bit serial + MBR signature/offset on non-GPT disks. Returns
/// `None` when neither pairing is available (e.g. no partition info could be read at all) --
/// callers must treat that volume as unidentifiable, not silently skip it.
pub fn identity_key(v: &VolumeInfo) -> Option<String> {
    if let (Some(ntfs), Some(guid)) = (v.ntfs_serial_64, &v.partition_guid) {
        return Some(format!("ntfs64:{ntfs:016x}+gpt:{guid}"));
    }
    if let (Some(sig), Some(off)) = (v.mbr_signature, v.mbr_partition_offset) {
        if let Some(serial) = v.serial_32 {
            return Some(format!("mbr:{sig:08x}+{off:016x}+vsn:{serial:08x}"));
        }
    }
    None
}

#[cfg(windows)]
pub mod windows_impl {
    use super::VolumeInfo;
    use anyhow::{Context, Result};
    use std::fs;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, MAX_PATH};
    use windows_sys::Win32::Storage::FileSystem::{
        FindFirstVolumeW, FindNextVolumeW, FindVolumeClose, GetVolumeInformationW,
        GetVolumePathNamesForVolumeNameW,
    };
    use windows_sys::Win32::System::Ioctl::{
        FSCTL_GET_NTFS_VOLUME_DATA, IOCTL_DISK_GET_PARTITION_INFO_EX, NTFS_VOLUME_DATA_BUFFER,
        PARTITION_INFORMATION_EX, PARTITION_STYLE_GPT, PARTITION_STYLE_MBR,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

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
    pub fn enumerate() -> Result<Vec<VolumeInfo>> {
        let mut out = Vec::new();
        let mut buf = [0u16; MAX_PATH as usize];
        // SAFETY: buf is a valid, correctly-sized wide-char buffer; FindFirstVolumeW/
        // FindNextVolumeW/FindVolumeClose is the documented enumeration triple.
        let handle = unsafe { FindFirstVolumeW(buf.as_mut_ptr(), buf.len() as u32) };
        if handle.is_null() {
            return Err(io::Error::last_os_error()).context("FindFirstVolumeW failed");
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
        let file = fs::File::open(path).ok()?;
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
        let file = fs::File::open(path).ok()?;
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
                // SAFETY: PartitionStyle == MBR guarantees the union's Mbr arm is initialized.
                let mbr = unsafe { out.Anonymous.Mbr };
                Some((None, Some(mbr.Signature), Some(out.StartingOffset as u64)))
            }
            _ => Some((None, None, None)),
        }
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

    fn read_or_none(path: &str) -> Option<String> {
        fs::read_to_string(path).ok().map(|s| s.trim().to_string())
    }

    fn drive_type_is_removable(mount_point: &str) -> bool {
        use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, DRIVE_REMOVABLE};
        let wide_mp = wide(mount_point);
        unsafe { GetDriveTypeW(wide_mp.as_ptr()) == DRIVE_REMOVABLE }
    }

    /// Writes (or leaves alone, if present) a `.nicti-volume` marker containing a fresh UUID at
    /// the given mount point's root. Returns the UUID actually on disk (existing or newly
    /// written), or `None` if the volume is read-only.
    pub fn ensure_marker(mount_point: &str) -> Option<String> {
        let marker_path = format!("{mount_point}.nicti-volume");
        if let Some(existing) = read_or_none(&marker_path) {
            return Some(existing);
        }
        let id = format!("{:032x}", rand_u128());
        fs::write(&marker_path, &id).ok()?;
        Some(id)
    }

    fn rand_u128() -> u128 {
        // Not a production RNG choice -- spike-only, avoids pulling in `rand` for one call.
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        nanos ^ (std::process::id() as u128) << 64
    }
}

#[cfg(not(windows))]
pub mod windows_impl {
    use super::VolumeInfo;
    use anyhow::{bail, Result};

    /// Stub for non-Windows builds -- #71/ADR-0020 is Windows-only research (v1's actual target,
    /// per #4/E0's PRD sign-off); non-Windows volume identity is deferred to #73.
    pub fn enumerate() -> Result<Vec<VolumeInfo>> {
        bail!("volume enumeration is Windows-only (see #73 for non-Windows deferral)")
    }

    pub fn ensure_marker(_mount_point: &str) -> Option<String> {
        None
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

    #[test]
    fn prefers_ntfs_plus_gpt_when_both_present() {
        let vol = v(
            Some(0xdead_beef),
            Some("11111111-1111-1111-1111-111111111111"),
            None,
            None,
        );
        assert_eq!(
            identity_key(&vol),
            Some("ntfs64:00000000deadbeef+gpt:11111111-1111-1111-1111-111111111111".to_string())
        );
    }

    #[test]
    fn falls_back_to_mbr_plus_vsn_without_gpt() {
        let vol = v(None, None, Some((0xcafebabe, 1_048_576)), Some(0x1234_5678));
        assert_eq!(
            identity_key(&vol),
            Some("mbr:cafebabe+0000000000100000+vsn:12345678".to_string())
        );
    }

    #[test]
    fn no_identity_when_nothing_available() {
        let vol = v(None, None, None, None);
        assert_eq!(identity_key(&vol), None);
    }

    #[test]
    fn distinct_volumes_never_collide_by_construction() {
        let a = v(Some(1), Some("guid-a"), None, None);
        let b = v(Some(2), Some("guid-b"), None, None);
        assert_ne!(identity_key(&a), identity_key(&b));
    }
}
