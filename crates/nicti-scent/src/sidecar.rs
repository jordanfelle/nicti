//! RAW-file `.xmp` sidecar naming and atomic writes.
//!
//! LRC replaces the RAW file's own extension rather than appending `.xmp` to
//! the full filename (`DSC_1234.NEF` -> `DSC_1234.xmp`, not
//! `DSC_1234.NEF.xmp`) -- confirmed against real Z8 sidecars in
//! `spikes/litter/tests/real_nef_cross_check.rs`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The sidecar path LRC itself would use for `raw_path`.
pub fn sidecar_path(raw_path: &Path) -> PathBuf {
    raw_path.with_extension("xmp")
}

/// Writes `contents` to `path` atomically: write to a temp file in the same
/// directory, then rename over the destination. A crash or concurrent LRC
/// read mid-write can never observe a half-written sidecar this way -- an
/// in-place `fs::write` can, since it truncates before writing.
pub fn atomic_write(path: &Path, contents: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?
        .to_string_lossy()
        .into_owned();
    let tmp_path = dir.join(format!(".{file_name}.scent-tmp"));
    fs::write(&tmp_path, contents)?;
    if let Err(e) = fs::rename(&tmp_path, path) {
        // e.g. Windows refusing to replace a sidecar LRC has open: don't leave the temp behind.
        let _ = fs::remove_file(&tmp_path);
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_path_replaces_extension() {
        assert_eq!(
            sidecar_path(Path::new("DSC_1234.NEF")),
            PathBuf::from("DSC_1234.xmp")
        );
        assert_eq!(
            sidecar_path(Path::new("/a/b/photo.dng")),
            PathBuf::from("/a/b/photo.xmp")
        );
    }

    #[test]
    fn atomic_write_leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("photo.xmp");
        atomic_write(&path, b"hello").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello");
        let leftover: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("scent-tmp"))
            .collect();
        assert!(leftover.is_empty(), "temp file left behind: {leftover:?}");
    }

    #[test]
    fn atomic_write_overwrites_existing_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("photo.xmp");
        atomic_write(&path, b"first").unwrap();
        atomic_write(&path, b"second").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "second");
    }
}
