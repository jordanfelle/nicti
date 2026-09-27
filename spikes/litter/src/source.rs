//! Ranged byte access for the IFD walk, adapted from `spikes/sniff/src/source.rs` (#28/#29).
//! Unlike sniff, this spike does no cold/warm I/O benchmarking, so the Windows
//! `FILE_FLAG_NO_BUFFERING` path and the `cold` flag are dropped -- every read here is a plain
//! buffered read, which is all `nef.rs`'s tag/preview extraction needs.

use std::io;
use std::path::Path;

pub trait ByteSource {
    fn read_at(&mut self, offset: u64, len: usize) -> io::Result<Vec<u8>>;

    fn len_hint(&self) -> Option<u64> {
        None
    }
}

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

/// Same head-prefetch window as sniff's `FileSource` -- the IFD walk's touched regions sit within
/// the first ~256KB of a NEF/DNG in practice, so one head read serves most of a walk.
const HEAD_PREFETCH: usize = 256 * 1024;

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
    fn file_source_matches_slice_source_on_same_bytes() {
        let data: Vec<u8> = (0u8..=255).cycle().take(300_000).collect();
        let tmp = std::env::temp_dir().join(format!("litter-source-test-{}", std::process::id()));
        std::fs::write(&tmp, &data).unwrap();

        let mut file_src = FileSource::open(&tmp).unwrap();
        let mut slice_src = SliceSource::new(&data);

        assert_eq!(
            file_src.read_at(100, 50).unwrap(),
            slice_src.read_at(100, 50).unwrap()
        );
        assert_eq!(
            file_src.read_at(280_000, 1000).unwrap(),
            slice_src.read_at(280_000, 1000).unwrap()
        );

        std::fs::remove_file(&tmp).ok();
    }
}
