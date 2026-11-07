//! Friendly volume names for the folder panel (#775): the OS volume label where there is one
//! (`/dev/disk/by-label` on Linux, the volume label on Windows) instead of only the mount path.
//! The parsing is pure and tested; the reads (`/proc`, `/dev`, the Windows volume enumeration) run
//! on a worker like the rest of the panel's probes, never on the UI thread.

// The mount-table parsing is only called on non-Windows; it is still unit-tested everywhere.
#![cfg_attr(windows, allow(dead_code))]

use std::collections::HashMap;

/// `\040`-style octal escapes as `/proc/self/mounts` writes a space, tab, newline or backslash.
fn unescape_mounts(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let octal = b
            .get(i + 1..i + 4)
            .filter(|d| d.iter().all(|c| (b'0'..=b'7').contains(c)));
        match (b[i], octal) {
            (b'\\', Some(d)) => {
                let v = d.iter().fold(0u32, |a, c| a * 8 + u32::from(c - b'0'));
                out.push(v as u8);
                i += 4;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `\x20`-style hex escapes as udev writes a `/dev/disk/by-label` name.
fn unescape_by_label(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = (b[i] == b'\\' && b.get(i + 1) == Some(&b'x'))
            .then(|| {
                s.get(i + 2..i + 4)
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
            })
            .flatten();
        match hex {
            Some(v) => {
                out.push(v);
                i += 4;
            }
            None => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `(device, mount point)` for every line of a `/proc/self/mounts` text.
pub fn parse_mounts(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let mut f = line.split_whitespace();
            let (dev, mount) = (f.next()?, f.next()?);
            dev.starts_with('/')
                .then(|| (unescape_mounts(dev), unescape_mounts(mount)))
        })
        .collect()
}

/// Mount point -> label, joining the mount table with the label table by device path. `by_label`
/// is `(escaped udev name, resolved device path)`. A mount with no label is absent; the first
/// mount of a device wins its label only if no other mount already has it.
pub fn labels_by_mount(
    mounts: &[(String, String)],
    by_label: &[(String, String)],
) -> HashMap<String, String> {
    let by_dev: HashMap<&str, String> = by_label
        .iter()
        .map(|(name, dev)| (dev.as_str(), unescape_by_label(name)))
        .filter(|(_, l)| !l.trim().is_empty())
        .collect();
    let mut out = HashMap::new();
    for (dev, mount) in mounts {
        if let Some(label) = by_dev.get(dev.as_str()) {
            out.entry(mount.clone()).or_insert_with(|| label.clone());
        }
    }
    out
}

/// The labels of the volumes mounted right now, keyed by mount path (the panel's drive path:
/// `D:\` on Windows, `/mnt/d` elsewhere). Best effort: empty when nothing can be read.
pub fn volume_labels() -> HashMap<String, String> {
    #[cfg(windows)]
    {
        nicti_lair::volume::windows_impl::enumerate()
            .map(|volumes| {
                volumes
                    .into_iter()
                    .filter_map(|v| {
                        let label = v.label.filter(|l| !l.trim().is_empty())?;
                        Some(
                            v.mount_points
                                .into_iter()
                                .map(move |mp| (mp, label.clone())),
                        )
                    })
                    .flatten()
                    .collect()
            })
            .unwrap_or_default()
    }
    #[cfg(not(windows))]
    {
        let Ok(mounts) = std::fs::read_to_string("/proc/self/mounts") else {
            return HashMap::new();
        };
        let Ok(rd) = std::fs::read_dir("/dev/disk/by-label") else {
            return HashMap::new();
        };
        let by_label: Vec<(String, String)> = rd
            .flatten()
            .filter_map(|e| {
                let dev = std::fs::canonicalize(e.path()).ok()?;
                Some((
                    e.file_name().to_string_lossy().into_owned(),
                    dev.to_string_lossy().into_owned(),
                ))
            })
            .collect();
        // A mount's source may itself be a symlink (`/dev/mapper/x`): resolve it to the same
        // device node the label link points at.
        let mounts: Vec<(String, String)> = parse_mounts(&mounts)
            .into_iter()
            .map(|(dev, mp)| {
                let resolved = std::fs::canonicalize(&dev)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or(dev);
                (resolved, mp)
            })
            .collect();
        labels_by_mount(&mounts, &by_label)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mount_table_is_parsed_with_escaped_spaces_and_skips_virtual_filesystems() {
        let text = "/dev/sdb1 /mnt/My\\040Drive ext4 rw 0 0\n\
                    proc /proc proc rw 0 0\n\
                    /dev/sda2 / ext4 rw 0 0\n";
        assert_eq!(
            parse_mounts(text),
            vec![
                ("/dev/sdb1".into(), "/mnt/My Drive".into()),
                ("/dev/sda2".into(), "/".into())
            ]
        );
    }

    #[test]
    fn a_label_is_found_by_device_and_unescaped() {
        let mounts = vec![
            ("/dev/sdb1".to_string(), "/mnt/cache".to_string()),
            ("/dev/sdc1".to_string(), "/mnt/other".to_string()),
        ];
        let by_label = vec![
            ("Photo\\x20Archive".to_string(), "/dev/sdb1".to_string()),
            ("   ".to_string(), "/dev/sdc1".to_string()),
        ];
        let got = labels_by_mount(&mounts, &by_label);
        assert_eq!(
            got.get("/mnt/cache").map(String::as_str),
            Some("Photo Archive")
        );
        assert!(!got.contains_key("/mnt/other"), "a blank label is no label");
    }

    #[test]
    fn a_malformed_escape_is_kept_literally() {
        assert_eq!(unescape_mounts("a\\0zb"), "a\\0zb");
        assert_eq!(unescape_by_label("a\\xZZb"), "a\\xZZb");
        assert_eq!(unescape_by_label("a\\x"), "a\\x");
    }
}
