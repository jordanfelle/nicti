//! Ranged byte access, so the IFD walk reads just the bytes it needs instead of `fs::read`-ing
//! the whole file first — `docs/research/sniff-embedded-jpeg.md` measured whole-file-read cost as
//! the dominant factor, and seek-and-read as ~250x faster at p50 (see `preview-tiers` topic).
//!
//! Trimmed from `spikes/sniff/src/source.rs`: no cold/warm `FILE_FLAG_NO_BUFFERING` distinction
//! (that's a benchmarking concern, not a production-import one) — every read here goes through
//! the OS page cache normally.

use std::io;
use std::path::Path;

/// Ranged read access to a byte stream. `read_at(offset, len)` returns up to `len` bytes starting
/// at `offset`; a short read (EOF before `len` bytes) is not an error, matching `fs::read`'s
/// existing whole-file semantics for out-of-bounds trailing reads that `ifd.rs` already tolerates.
pub trait ByteSource {
    fn read_at(&mut self, offset: u64, len: usize) -> io::Result<Vec<u8>>;

    /// Total length of the underlying stream, when known. `Walker` uses this only for bounds
    /// checks; a source that can't cheaply know its length may return `None`.
    fn len_hint(&self) -> Option<u64> {
        None
    }
}

/// Wraps an in-memory buffer (tests, or a caller that already has the bytes in memory).
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

/// Ranged reads against an open file, with a prefetched head window. `read_at` uses
/// platform-appropriate positioned reads (`FileExt::read_at` on Unix, `FileExt::seek_read` on
/// Windows) — no seek-then-read race, safe to reuse across threads with separate handles.
pub struct FileSource {
    file: std::fs::File,
    len: u64,
    head: Vec<u8>,
}

impl FileSource {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = std::fs::File::open(path)?;
        let len = file.metadata()?.len();
        let mut source = FileSource {
            file,
            len,
            head: Vec::new(),
        };
        let head_len = HEAD_PREFETCH.min(len as usize);
        source.head = read_ranged(&source.file, 0, head_len)?;
        Ok(source)
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
        read_ranged(&self.file, offset, clamped_len)
    }

    fn len_hint(&self) -> Option<u64> {
        Some(self.len)
    }
}

#[cfg(unix)]
fn read_ranged(file: &std::fs::File, offset: u64, len: usize) -> io::Result<Vec<u8>> {
    use std::os::unix::fs::FileExt;
    let mut buf = vec![0u8; len];
    let n = file.read_at(&mut buf, offset)?;
    buf.truncate(n);
    Ok(buf)
}

#[cfg(windows)]
fn read_ranged(file: &std::fs::File, offset: u64, len: usize) -> io::Result<Vec<u8>> {
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
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &data).unwrap();

        let mut file_src = FileSource::open(tmp.path()).unwrap();
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
    }
}
