//! `.thumb.*` sidecar files: the on-disk home of an archived folder's T0 grid thumbnails
//! (#72, ADR-0072). A sidecar sits next to its RAW and carries the same bytes the catalog's
//! `preview` table holds for the active-edit tier, so an archive drive can be browsed without the
//! central cache.
//!
//! The name keeps the RAW's full file name (`DSC_1234.NEF` -> `DSC_1234.NEF.thumb.jpg`), unlike
//! the LRC-style `.xmp` sidecar: a `.NEF` and `.NRW` sharing a stem must not collide.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::Preview;

/// Sidecar encoding. Only JPEG for now: T0 is already a JPEG, so exporting is a byte copy with no
/// re-encode and no new dependency (ADR-0143 rejected lossy WebP; AVIF is a later option).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SidecarCodec {
    #[default]
    Jpeg,
}

impl SidecarCodec {
    /// Everything after the RAW's own file name.
    pub fn suffix(self) -> &'static str {
        match self {
            SidecarCodec::Jpeg => ".thumb.jpg",
        }
    }
}

/// `true` for a file name that is one of our sidecars, so importers never mistake it for an asset.
pub fn is_sidecar_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.ends_with(".thumb.jpg") || n.ends_with(".thumb-tmp")
}

pub fn sidecar_path(raw: &Path, codec: SidecarCodec) -> PathBuf {
    let mut name = raw.file_name().unwrap_or_default().to_os_string();
    name.push(codec.suffix());
    raw.with_file_name(name)
}

/// Writes `preview`'s bytes next to `raw`, atomically (temp file in the same folder, then rename).
pub fn export(raw: &Path, preview: &Preview, codec: SidecarCodec) -> io::Result<()> {
    let path = sidecar_path(raw, codec);
    let tmp = path.with_file_name(format!(
        ".{}.thumb-tmp",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    fs::write(&tmp, &preview.bytes)?;
    if let Err(e) = fs::rename(&tmp, &path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// The sidecar next to `raw`, if one exists and holds a JPEG. A missing, empty or non-JPEG file
/// reads as `None` (never trust a truncated write).
pub fn read(raw: &Path, codec: SidecarCodec) -> Option<Preview> {
    let bytes = fs::read(sidecar_path(raw, codec)).ok()?;
    if !bytes.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let dims = jpeg_dimensions(&bytes);
    Some(Preview {
        width: dims.map(|d| d.0),
        height: dims.map(|d| d.1),
        bytes,
    })
}

pub fn remove(raw: &Path, codec: SidecarCodec) {
    let _ = fs::remove_file(sidecar_path(raw, codec));
}

/// Pixel size from the first SOF marker, without decoding.
pub fn jpeg_dimensions(b: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2;
    while i + 4 <= b.len() {
        if b[i] != 0xFF {
            return None;
        }
        let marker = b[i + 1];
        if marker == 0xFF {
            i += 1;
            continue;
        }
        if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            if i + 9 > b.len() {
                return None;
            }
            let h = u16::from_be_bytes([b[i + 5], b[i + 6]]) as u32;
            let w = u16::from_be_bytes([b[i + 7], b[i + 8]]) as u32;
            return Some((w, h));
        }
        if marker == 0xD8 || (0xD0..=0xD7).contains(&marker) || marker == 0x01 {
            i += 2;
            continue;
        }
        let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
        i += 2 + len;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jpeg(w: u16, h: u16) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0, 0];
        v.extend([0xFF, 0xC0, 0x00, 0x0B, 8]);
        v.extend(h.to_be_bytes());
        v.extend(w.to_be_bytes());
        v.extend([1, 1, 0x11, 0]);
        v.extend([0xFF, 0xD9]);
        v
    }

    #[test]
    fn name_keeps_full_raw_name() {
        let p = sidecar_path(Path::new("/a/DSC_1.NEF"), SidecarCodec::Jpeg);
        assert_eq!(p, Path::new("/a/DSC_1.NEF.thumb.jpg"));
        assert_ne!(
            p,
            sidecar_path(Path::new("/a/DSC_1.NRW"), SidecarCodec::Jpeg)
        );
    }

    #[test]
    fn export_read_remove_round_trip() {
        let d = tempfile::tempdir().unwrap();
        let raw = d.path().join("DSC_1.NEF");
        let pv = Preview {
            width: Some(40),
            height: Some(30),
            bytes: jpeg(40, 30),
        };
        export(&raw, &pv, SidecarCodec::Jpeg).unwrap();
        let back = read(&raw, SidecarCodec::Jpeg).unwrap();
        assert_eq!(back.bytes, pv.bytes);
        assert_eq!((back.width, back.height), (Some(40), Some(30)));
        assert_eq!(fs::read_dir(d.path()).unwrap().count(), 1, "no temp left");
        remove(&raw, SidecarCodec::Jpeg);
        assert!(read(&raw, SidecarCodec::Jpeg).is_none());
    }

    #[test]
    fn truncated_or_non_jpeg_reads_as_none() {
        let d = tempfile::tempdir().unwrap();
        let raw = d.path().join("a.NEF");
        fs::write(sidecar_path(&raw, SidecarCodec::Jpeg), b"").unwrap();
        assert!(read(&raw, SidecarCodec::Jpeg).is_none());
        fs::write(sidecar_path(&raw, SidecarCodec::Jpeg), b"nope").unwrap();
        assert!(read(&raw, SidecarCodec::Jpeg).is_none());
    }

    #[test]
    fn recognises_own_names() {
        assert!(is_sidecar_name("DSC_1.NEF.thumb.jpg"));
        assert!(is_sidecar_name("DSC_1.NEF.THUMB.JPG"));
        assert!(!is_sidecar_name("DSC_1.jpg"));
    }
}
