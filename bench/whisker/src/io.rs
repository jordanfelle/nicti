//! Reads raw gray8 video (as produced by `ffmpeg -pix_fmt gray -f rawvideo`) into a
//! [`crate::FrameStream`]. See `bench/whisker/README.md` for the exact ffmpeg invocation.

use crate::FrameStream;
use std::fs;
use std::io;
use std::path::Path;

/// Reads a raw gray8 file into a [`FrameStream`]. `width`/`height` must match the ffmpeg crop
/// that produced the file. A trailing partial frame (a capture cut off mid-write) is dropped
/// rather than treated as an error, so an in-progress or slightly truncated capture still
/// analyzes the frames it does have.
pub fn read_frames_gray8(path: &Path, width: usize, height: usize) -> io::Result<FrameStream> {
    let frame_size = width * height;
    if frame_size == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "width and height must both be non-zero",
        ));
    }
    let bytes = fs::read(path)?;
    let n = bytes.len() / frame_size;
    let frames = (0..n)
        .map(|i| bytes[i * frame_size..(i + 1) * frame_size].to_vec())
        .collect();
    Ok(FrameStream {
        width,
        height,
        frames,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn reads_whole_frames_and_drops_trailing_partial() {
        let path = unique_temp_path();
        // 2 full 2x2 (4-byte) frames plus 2 stray trailing bytes.
        fs::File::create(&path)
            .unwrap()
            .write_all(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 9])
            .unwrap();
        let stream = read_frames_gray8(&path, 2, 2).unwrap();
        fs::remove_file(&path).unwrap();
        assert_eq!(stream.frames.len(), 2);
        assert_eq!(stream.frames[0], vec![1, 2, 3, 4]);
        assert_eq!(stream.frames[1], vec![5, 6, 7, 8]);
    }

    #[test]
    fn zero_dims_is_error() {
        let path = unique_temp_path();
        fs::File::create(&path).unwrap();
        let result = read_frames_gray8(&path, 0, 10);
        fs::remove_file(&path).unwrap();
        assert!(result.is_err());
    }

    // Minimal same-process tempfile helper so this crate doesn't need a `tempfile` dependency
    // just for two tests. Uses an atomic counter (not just PID) so parallel test threads never
    // collide on the same path.
    fn unique_temp_path() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("whisker-io-test-{}-{}", std::process::id(), n))
    }
}
