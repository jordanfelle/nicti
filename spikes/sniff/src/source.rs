//! Ranged byte access, so the IFD walk and tier extraction can read just the bytes they need
//! instead of `fs::read`-ing the whole file first. `docs/research/sniff-embedded-jpeg.md`'s
//! throughput section found the whole-file-read cost dominates every number it measured --
//! this module is what a targeted-read implementation actually needs.
//!
//! `Walker` (see `ifd.rs`) is generic over `ByteSource` so the exact same walk logic runs
//! against an in-memory slice (tests, `inventory`'s whole-file mode) or a ranged file handle
//! (the `--io ranged` bench modes) without duplicating the IFD-parsing code.

use std::io;
use std::path::Path;

/// Ranged read access to a byte stream. `read_at(offset, len)` returns up to `len` bytes starting
/// at `offset`; a short read (EOF before `len` bytes) is not an error, matching `fs::read`'s
/// existing whole-file semantics for out-of-bounds trailing reads that `ifd.rs` already tolerates.
pub trait ByteSource {
    fn read_at(&mut self, offset: u64, len: usize) -> io::Result<Vec<u8>>;

    /// Total length of the underlying stream, when known. `Walker` uses this only for bounds
    /// messages; a source that can't cheaply know its length may return `None`.
    fn len_hint(&self) -> Option<u64> {
        None
    }
}

/// Wraps an in-memory buffer (already-read test fixtures, or `inventory`'s whole-file mode where
/// nothing is saved by going ranged since the caller already paid for the full read).
pub struct SliceSource<'a> {
    data: &'a [u8],
}

impl<'a> SliceSource<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        SliceSource { data }
    }
}

impl ByteSource for SliceSource<'_> {
    fn read_at(&mut self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let start = offset.min(self.data.len() as u64) as usize;
        let end = (start + len).min(self.data.len());
        Ok(self.data[start..end].to_vec())
    }

    fn len_hint(&self) -> Option<u64> {
        Some(self.data.len() as u64)
    }
}

/// A small read-ahead window: the IFD walk touches a handful of small, usually-clustered regions
/// (the header, IFD0, a SubIFD array, the MakerNote's inner IFDs) that in practice sit within the
/// first ~256KB of a NEF/DNG, so one head read serves most of a walk without falling back to a
/// second positioned read per tag.
const HEAD_PREFETCH: usize = 256 * 1024;

/// Ranged reads against an open file. Windows uses `FileExt::seek_read` (no seek-then-read race:
/// each call is independently positioned, safe to reuse across threads with separate handles).
/// `cold` requests sector-aligned `FILE_FLAG_NO_BUFFERING` reads on Windows, reusing `bench.rs`'s
/// `AlignedBuf`; elsewhere it's a best-effort no-op, matching `bench.rs`'s existing `read_cold`
/// caveat that only the Windows-native reference-machine run produces trustworthy cold numbers.
pub struct FileSource {
    file: std::fs::File,
    len: u64,
    cold: bool,
    head: Vec<u8>,
}

impl FileSource {
    pub fn open(path: &Path, cold: bool) -> io::Result<Self> {
        let file = std::fs::File::open(path)?;
        let len = file.metadata()?.len();
        let mut source = FileSource {
            file,
            len,
            cold,
            head: Vec::new(),
        };
        let head_len = HEAD_PREFETCH.min(len as usize);
        source.head = source.read_at_uncached(0, head_len)?;
        Ok(source)
    }

    fn read_at_uncached(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        if self.cold {
            read_cold_ranged(&self.file, offset, len)
        } else {
            read_warm_ranged(&self.file, offset, len)
        }
    }
}

impl ByteSource for FileSource {
    fn read_at(&mut self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        if offset + (len as u64) <= self.head.len() as u64 {
            let start = offset as usize;
            let end = start + len;
            return Ok(self.head[start..end].to_vec());
        }
        let clamped_len = len.min((self.len.saturating_sub(offset)) as usize);
        if clamped_len == 0 {
            return Ok(Vec::new());
        }
        self.read_at_uncached(offset, clamped_len)
    }

    fn len_hint(&self) -> Option<u64> {
        Some(self.len)
    }
}

#[cfg(unix)]
fn read_warm_ranged(file: &std::fs::File, offset: u64, len: usize) -> io::Result<Vec<u8>> {
    use std::os::unix::fs::FileExt;
    let mut buf = vec![0u8; len];
    let n = file.read_at(&mut buf, offset)?;
    buf.truncate(n);
    Ok(buf)
}

#[cfg(windows)]
fn read_warm_ranged(file: &std::fs::File, offset: u64, len: usize) -> io::Result<Vec<u8>> {
    use std::os::windows::fs::FileExt;
    let mut buf = vec![0u8; len];
    let mut total = 0usize;
    // seek_read can return a short read; loop until len bytes are filled or EOF.
    while total < len {
        let n = file.seek_read(&mut buf[total..], offset + total as u64)?;
        if n == 0 {
            break;
        }
        total += n;
    }
    buf.truncate(total);
    Ok(buf)
}

#[cfg(not(windows))]
fn read_cold_ranged(file: &std::fs::File, offset: u64, len: usize) -> io::Result<Vec<u8>> {
    // No NO_BUFFERING-equivalent used here; matches docs/benchmarks.md's own rule that cold
    // numbers only come from the Windows-native reference-machine run, not WSL.
    read_warm_ranged(file, offset, len)
}

#[cfg(windows)]
fn read_cold_ranged(file: &std::fs::File, offset: u64, len: usize) -> io::Result<Vec<u8>> {
    use crate::bench::read_cold_range;
    read_cold_range(file, offset, len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_source_reads_exact_range() {
        let data = b"0123456789".to_vec();
        let mut src = SliceSource::new(&data);
        assert_eq!(src.read_at(2, 4).unwrap(), b"2345");
    }

    #[test]
    fn slice_source_clamps_short_read_at_eof() {
        let data = b"0123456789".to_vec();
        let mut src = SliceSource::new(&data);
        assert_eq!(src.read_at(8, 10).unwrap(), b"89");
    }

    #[test]
    fn slice_source_out_of_bounds_offset_returns_empty() {
        let data = b"0123456789".to_vec();
        let mut src = SliceSource::new(&data);
        assert_eq!(src.read_at(100, 4).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn file_source_matches_slice_source_on_same_bytes() {
        let data: Vec<u8> = (0u8..=255).cycle().take(300_000).collect();
        let tmp = std::env::temp_dir().join(format!("sniff-source-test-{}", std::process::id()));
        std::fs::write(&tmp, &data).unwrap();

        let mut file_src = FileSource::open(&tmp, false).unwrap();
        let mut slice_src = SliceSource::new(&data);

        // Inside the head-prefetch window.
        assert_eq!(
            file_src.read_at(100, 50).unwrap(),
            slice_src.read_at(100, 50).unwrap()
        );
        // Past the head-prefetch window -- exercises the fallback ranged read.
        assert_eq!(
            file_src.read_at(280_000, 1000).unwrap(),
            slice_src.read_at(280_000, 1000).unwrap()
        );
        // Straddling EOF.
        assert_eq!(
            file_src.read_at(299_990, 100).unwrap(),
            slice_src.read_at(299_990, 100).unwrap()
        );

        std::fs::remove_file(&tmp).ok();
    }
}
