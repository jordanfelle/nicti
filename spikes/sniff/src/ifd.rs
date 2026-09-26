//! Minimal TIFF/EXIF/Nikon-MakerNote IFD walker.
//!
//! Purpose-built for #28: find every embedded JPEG preview/thumbnail in a NEF or DNG file
//! (JPEGInterchangeFormat/Length pairs, wherever they live: IFD0, IFD1, a SubIFD, or inside the
//! Nikon MakerNote's PreviewIFD) without depending on LibRaw or rawler. Not a general-purpose TIFF
//! library -- it reads only the tags Sniff needs and is deliberately tolerant of malformed input
//! (bounds-checked, cycle-guarded), since it will be pointed at thousands of real camera files.
//!
//! Generic over `ByteSource` (#29): every read here is a small, explicit range (the header, one
//! IFD's entries, an external offset array, the MakerNote's 18-byte header) rather than a
//! whole-file slice, so `Walker<FileSource>` performs the walk as a handful of positioned reads
//! instead of paying for the full file's I/O just to locate a preview -- the pessimistic bound
//! `docs/research/sniff-embedded-jpeg.md` measured and flagged as the biggest lever available.
//! `Walker<SliceSource>` (tests, and any caller that already has the bytes in memory) behaves
//! identically, just backed by a slice instead of a file handle.

use crate::source::ByteSource;
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteOrder {
    Little,
    Big,
}

impl ByteOrder {
    fn u16(self, b: &[u8]) -> u16 {
        match self {
            ByteOrder::Little => u16::from_le_bytes([b[0], b[1]]),
            ByteOrder::Big => u16::from_be_bytes([b[0], b[1]]),
        }
    }
    fn u32(self, b: &[u8]) -> u32 {
        match self {
            ByteOrder::Little => u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            ByteOrder::Big => u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct IfdEntry {
    tag: u16,
    field_type: u16,
    count: u32,
    value_or_offset_raw: [u8; 4],
}

impl IfdEntry {
    fn type_size(&self) -> usize {
        match self.field_type {
            1 | 2 | 6 | 7 => 1, // BYTE, ASCII, SBYTE, UNDEFINED
            3 | 8 => 2,         // SHORT, SSHORT
            4 | 9 | 11 => 4,    // LONG, SLONG, FLOAT
            5 | 10 | 12 => 8,   // RATIONAL, SRATIONAL, DOUBLE
            _ => 1,
        }
    }

    fn value_len(&self) -> u64 {
        self.type_size() as u64 * self.count as u64
    }

    /// A single-value SHORT or LONG, resolved from the inline 4 bytes.
    fn as_u32(&self, bo: ByteOrder) -> u32 {
        match self.field_type {
            3 => bo.u16(&self.value_or_offset_raw[0..2]) as u32,
            4 => bo.u32(&self.value_or_offset_raw[0..4]),
            _ => bo.u32(&self.value_or_offset_raw[0..4]),
        }
    }

    /// Offset field, for tags whose value is a pointer regardless of nominal type.
    fn as_offset(&self, bo: ByteOrder) -> u32 {
        bo.u32(&self.value_or_offset_raw[0..4])
    }
}

pub const TAG_NEW_SUBFILE_TYPE: u16 = 0x00FE;
pub const TAG_COMPRESSION: u16 = 0x0103;
pub const TAG_IMAGE_WIDTH: u16 = 0x0100;
pub const TAG_IMAGE_LENGTH: u16 = 0x0101;
pub const TAG_STRIP_OFFSETS: u16 = 0x0111;
pub const TAG_STRIP_BYTE_COUNTS: u16 = 0x0117;
pub const TAG_SUB_IFDS: u16 = 0x014A;
pub const TAG_JPEG_IF_OFFSET: u16 = 0x0201;
pub const TAG_JPEG_IF_LENGTH: u16 = 0x0202;
pub const TAG_EXIF_IFD: u16 = 0x8769;
pub const TAG_MAKER_NOTE: u16 = 0x927C;
pub const TAG_NIKON_PREVIEW_IFD: u16 = 0x0011;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewSource {
    /// IFD0's own JPEGInterchangeFormat pair (rare on modern bodies, common on old ones).
    Ifd0,
    /// The classic EXIF thumbnail IFD (IFD0's "next IFD" pointer).
    ThumbnailIfd,
    /// A NewSubfileType SubIFD (DNG-style reduced-resolution preview).
    SubIfd(usize),
    /// The Nikon MakerNote's PreviewIFD ("JpgFromRaw").
    NikonPreviewIfd,
}

#[derive(Debug, Clone)]
pub struct EmbeddedJpeg {
    pub source: PreviewSource,
    /// Absolute byte offset into the *file*, already resolved (MakerNote-relative offsets are
    /// rebased here so callers never need to know about the Nikon quirk).
    pub file_offset: u64,
    pub byte_len: u64,
    /// From the IFD's own ImageWidth/ImageLength tags, when present (not decoded from the JPEG).
    pub declared_width: Option<u32>,
    pub declared_height: Option<u32>,
    pub new_subfile_type: Option<u32>,
}

#[derive(Debug, thiserror::Error)]
pub enum IfdError {
    #[error("file too short to hold a TIFF header")]
    TooShort,
    #[error("bad TIFF byte-order marker")]
    BadByteOrder,
    #[error("bad TIFF magic number")]
    BadMagic,
    #[error("IFD offset {0} out of bounds")]
    OffsetOutOfBounds(u64),
    #[error("IFD entry count would read past end of buffer")]
    TruncatedIfd,
    #[error("IFD offset cycle detected at {0}")]
    Cycle(u64),
    #[error("too many IFDs visited (possible malicious/corrupt file)")]
    TooManyIfds,
    #[error("I/O error reading source: {0}")]
    Io(String),
}

impl From<std::io::Error> for IfdError {
    fn from(e: std::io::Error) -> Self {
        IfdError::Io(e.to_string())
    }
}

const MAX_IFDS_VISITED: usize = 512;

pub struct Walker<S: ByteSource> {
    source: S,
    bo: ByteOrder,
    ifd0_off: u32,
    visited: HashSet<u64>,
    ifds_visited: usize,
    /// Total stream length, when the source can report it cheaply -- used only for the
    /// `file_offset >= len` bounds check `jpeg_pair`/`strip_jpeg` already performed against
    /// `self.data.len()` before this became ranged-read-based.
    len: Option<u64>,
}

impl<S: ByteSource> Walker<S> {
    pub fn new(mut source: S) -> Result<Self, IfdError> {
        let header = source.read_at(0, 8)?;
        if header.len() < 8 {
            return Err(IfdError::TooShort);
        }
        let bo = match &header[0..2] {
            b"II" => ByteOrder::Little,
            b"MM" => ByteOrder::Big,
            _ => return Err(IfdError::BadByteOrder),
        };
        let magic = bo.u16(&header[2..4]);
        if magic != 42 {
            return Err(IfdError::BadMagic);
        }
        let ifd0_off = bo.u32(&header[4..8]);
        let len = source.len_hint();
        Ok(Walker {
            source,
            bo,
            ifd0_off,
            visited: HashSet::new(),
            ifds_visited: 0,
            len,
        })
    }

    fn ifd0_offset(&self) -> u32 {
        self.ifd0_off
    }

    /// Reads an arbitrary byte range from the underlying source. For callers (e.g. `bench.rs`'s
    /// tier-selection logic) that need to inspect or decode an `EmbeddedJpeg`'s bytes after
    /// `find_embedded_jpegs` has returned its offset/len, without re-walking the IFD tree.
    pub fn read_range(&mut self, offset: u64, len: usize) -> Result<Vec<u8>, IfdError> {
        Ok(self.source.read_at(offset, len)?)
    }

    /// Reads one IFD at `offset` (relative to `base`, which is 0 for the main TIFF header and
    /// >0 for a Nikon-MakerNote-embedded TIFF header). Returns (entries, next_ifd_offset).
    fn read_ifd(&mut self, base: u64, offset: u32) -> Result<(Vec<IfdEntry>, u32), IfdError> {
        let abs = base + offset as u64;
        if !self.visited.insert(abs) {
            return Err(IfdError::Cycle(abs));
        }
        self.ifds_visited += 1;
        if self.ifds_visited > MAX_IFDS_VISITED {
            return Err(IfdError::TooManyIfds);
        }
        let count_bytes = self.source.read_at(abs, 2)?;
        if count_bytes.len() < 2 {
            return Err(IfdError::OffsetOutOfBounds(abs));
        }
        let count = self.bo.u16(&count_bytes) as usize;
        let body_len = count
            .checked_mul(12)
            .and_then(|n| n.checked_add(4))
            .ok_or(IfdError::TruncatedIfd)?;
        let body = self.source.read_at(abs + 2, body_len)?;
        if body.len() < body_len {
            return Err(IfdError::TruncatedIfd);
        }
        let mut entries = Vec::with_capacity(count);
        for i in 0..count {
            let e = &body[i * 12..i * 12 + 12];
            let mut raw = [0u8; 4];
            raw.copy_from_slice(&e[8..12]);
            entries.push(IfdEntry {
                tag: self.bo.u16(&e[0..2]),
                field_type: self.bo.u16(&e[2..4]),
                count: self.bo.u32(&e[4..8]),
                value_or_offset_raw: raw,
            });
        }
        let next = self.bo.u32(&body[count * 12..count * 12 + 4]);
        Ok((entries, next))
    }

    fn find_entry(entries: &[IfdEntry], tag: u16) -> Option<IfdEntry> {
        entries.iter().find(|e| e.tag == tag).copied()
    }

    fn in_bounds(&self, file_offset: u64) -> bool {
        match self.len {
            Some(len) => file_offset < len,
            // Unknown total length (a source that can't report one): trust the offset: the
            // eventual actual read (outside this walker's scope) will fail on a bad one anyway.
            None => true,
        }
    }

    fn clamp_len(&self, file_offset: u64, byte_len: u64) -> u64 {
        match self.len {
            Some(len) => byte_len.min(len.saturating_sub(file_offset)),
            None => byte_len,
        }
    }

    /// Resolves a JPEGInterchangeFormat(Offset)/Length pair, if both present, rebased onto
    /// `base` (0 for the main file, the MakerNote's inner-TIFF base for a Nikon PreviewIFD).
    fn jpeg_pair(&self, entries: &[IfdEntry], base: u64) -> Option<(u64, u64)> {
        let off = Self::find_entry(entries, TAG_JPEG_IF_OFFSET)?;
        let len = Self::find_entry(entries, TAG_JPEG_IF_LENGTH)?;
        let file_offset = base + off.as_offset(self.bo) as u64;
        let byte_len = len.as_u32(self.bo) as u64;
        if !self.in_bounds(file_offset) {
            return None;
        }
        Some((file_offset, self.clamp_len(file_offset, byte_len)))
    }

    fn declared_dims(&self, entries: &[IfdEntry]) -> (Option<u32>, Option<u32>) {
        let w = Self::find_entry(entries, TAG_IMAGE_WIDTH).map(|e| e.as_u32(self.bo));
        let h = Self::find_entry(entries, TAG_IMAGE_LENGTH).map(|e| e.as_u32(self.bo));
        (w, h)
    }

    /// Also covers the DNG "old-style JPEG compression" case: Compression==6 with a
    /// StripOffsets/StripByteCounts single strip (some DNG preview SubIFDs use this instead of
    /// the classic JPEGInterchangeFormat pair).
    fn strip_jpeg(&self, entries: &[IfdEntry], base: u64) -> Option<(u64, u64)> {
        let compression = Self::find_entry(entries, TAG_COMPRESSION)?.as_u32(self.bo);
        if compression != 6 && compression != 7 {
            return None;
        }
        let offsets = Self::find_entry(entries, TAG_STRIP_OFFSETS)?;
        let counts = Self::find_entry(entries, TAG_STRIP_BYTE_COUNTS)?;
        if offsets.count != 1 || counts.count != 1 {
            // Multi-strip/tiled: out of scope for this spike (that's the full-res raw image
            // data, not a single-shot JPEG preview).
            return None;
        }
        let file_offset = base + offsets.as_offset(self.bo) as u64;
        let byte_len = counts.as_u32(self.bo) as u64;
        if !self.in_bounds(file_offset) {
            return None;
        }
        Some((file_offset, self.clamp_len(file_offset, byte_len)))
    }

    fn emit_if_jpeg(
        &self,
        entries: &[IfdEntry],
        base: u64,
        source: PreviewSource,
        out: &mut Vec<EmbeddedJpeg>,
    ) {
        let pair = self
            .jpeg_pair(entries, base)
            .or_else(|| self.strip_jpeg(entries, base));
        let Some((file_offset, byte_len)) = pair else {
            return;
        };
        if byte_len < 2 {
            return;
        }
        let (w, h) = self.declared_dims(entries);
        let nsft = Self::find_entry(entries, TAG_NEW_SUBFILE_TYPE).map(|e| e.as_u32(self.bo));
        out.push(EmbeddedJpeg {
            source,
            file_offset,
            byte_len,
            declared_width: w,
            declared_height: h,
            new_subfile_type: nsft,
        });
    }

    /// Walks IFD0, its "next IFD" chain, every SubIFD, and (if present) the Nikon MakerNote's
    /// PreviewIFD, collecting every embedded JPEG it finds along the way.
    pub fn find_embedded_jpegs(&mut self) -> Result<Vec<EmbeddedJpeg>, IfdError> {
        let mut out = Vec::new();
        let ifd0_off = self.ifd0_offset();
        let (ifd0, next) = self.read_ifd(0, ifd0_off)?;
        self.emit_if_jpeg(&ifd0, 0, PreviewSource::Ifd0, &mut out);

        // "Next IFD" chain off IFD0 (classic thumbnail IFD1, occasionally more).
        let mut next_off = next;
        while next_off != 0 {
            let (entries, following) = match self.read_ifd(0, next_off) {
                Ok(v) => v,
                Err(IfdError::Cycle(_)) => break,
                Err(e) => return Err(e),
            };
            self.emit_if_jpeg(&entries, 0, PreviewSource::ThumbnailIfd, &mut out);
            next_off = following;
        }

        // SubIFDs (tag 0x14A): an array of offsets, each pointing at a full IFD.
        if let Some(sub_entry) = Self::find_entry(&ifd0, TAG_SUB_IFDS) {
            let offsets = self.read_offset_array(&sub_entry)?;
            for (i, off) in offsets.into_iter().enumerate() {
                let (entries, _) = match self.read_ifd(0, off) {
                    Ok(v) => v,
                    Err(IfdError::Cycle(_)) => continue,
                    Err(e) => return Err(e),
                };
                self.emit_if_jpeg(&entries, 0, PreviewSource::SubIfd(i), &mut out);
            }
        }

        // ExifIFD -> MakerNote -> Nikon PreviewIFD.
        if let Some(exif_entry) = Self::find_entry(&ifd0, TAG_EXIF_IFD) {
            let exif_off = exif_entry.as_offset(self.bo);
            if let Ok((exif_entries, _)) = self.read_ifd(0, exif_off) {
                if let Some(mn) = Self::find_entry(&exif_entries, TAG_MAKER_NOTE) {
                    self.walk_nikon_maker_note(&mn, &mut out)?;
                }
            }
        }

        Ok(out)
    }

    fn read_offset_array(&mut self, entry: &IfdEntry) -> Result<Vec<u32>, IfdError> {
        let n = entry.count as usize;
        if n == 0 {
            return Ok(Vec::new());
        }
        if entry.value_len() <= 4 {
            // Fits inline (n<=1 for LONG); just the one value.
            return Ok(vec![entry.as_offset(self.bo)]);
        }
        let start = entry.as_offset(self.bo) as u64;
        let byte_len = n.checked_mul(4).ok_or(IfdError::TruncatedIfd)?;
        let raw = self.source.read_at(start, byte_len)?;
        if raw.len() < byte_len {
            return Err(IfdError::TruncatedIfd);
        }
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            out.push(self.bo.u32(&raw[i * 4..i * 4 + 4]));
        }
        Ok(out)
    }

    /// Nikon MakerNote layout: `"Nikon\0"` (6 bytes), a 2-byte format version, 2 reserved bytes,
    /// then a *second, embedded* TIFF header. Every offset inside the MakerNote's own IFD tree --
    /// including the PreviewIFD it points to via tag 0x11, and that PreviewIFD's own
    /// JPEGInterchangeFormat offset -- is relative to the start of that inner TIFF header, not to
    /// the start of the file or the start of the MakerNote data. This is the one genuinely
    /// nonstandard piece of the whole walk; every other IFD in this file uses file-absolute
    /// offsets (base 0).
    fn walk_nikon_maker_note(
        &mut self,
        mn_entry: &IfdEntry,
        out: &mut Vec<EmbeddedJpeg>,
    ) -> Result<(), IfdError> {
        let mn_off = mn_entry.as_offset(self.bo) as u64;
        // "Nikon\0" (6) + 2 version + 2 reserved + inner TIFF header (2 byte-order + 2 magic +
        // 4 ifd-offset = 8) = 18 bytes, one read covers the whole check.
        let mn_data = match self.source.read_at(mn_off, 18) {
            Ok(d) => d,
            Err(_) => return Ok(()),
        };
        if mn_data.len() < 18 || &mn_data[0..6] != b"Nikon\0" {
            return Ok(());
        }
        let inner_header = &mn_data[10..18];
        let inner_bo = match &inner_header[0..2] {
            b"II" => ByteOrder::Little,
            b"MM" => ByteOrder::Big,
            _ => return Ok(()),
        };
        if inner_bo.u16(&inner_header[2..4]) != 42 {
            return Ok(());
        }
        let maker_base = mn_off + 10;
        let ifd_off = inner_bo.u32(&inner_header[4..8]);

        let (mn_ifd, _) = match self.read_ifd(maker_base, ifd_off) {
            Ok(v) => v,
            Err(_) => return Ok(()),
        };
        let Some(preview_entry) = Self::find_entry(&mn_ifd, TAG_NIKON_PREVIEW_IFD) else {
            return Ok(());
        };
        let preview_off = preview_entry.as_offset(self.bo);
        let (preview_ifd, _) = match self.read_ifd(maker_base, preview_off) {
            Ok(v) => v,
            Err(_) => return Ok(()),
        };
        self.emit_if_jpeg(
            &preview_ifd,
            maker_base,
            PreviewSource::NikonPreviewIfd,
            out,
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{FileSource, SliceSource};

    fn walk(data: &[u8]) -> Walker<SliceSource<'_>> {
        Walker::new(SliceSource::new(data)).expect("valid header")
    }

    /// `Walker<FileSource>` (ranged reads) must find the exact same embedded JPEGs as
    /// `Walker<SliceSource>` (whole-buffer reads) on the same bytes -- the core correctness
    /// requirement #29's seek-and-read implementation depends on: going ranged must never change
    /// *what* is found, only how many bytes it costs to find it.
    #[test]
    fn file_source_and_slice_source_agree_on_nikon_maker_note_file() {
        let mut b = FileBuilder::new();
        let mn_off = b.offset();
        b.buf.extend_from_slice(b"Nikon\0");
        b.buf.extend_from_slice(&[0x02, 0x10, 0x00, 0x00]);
        let inner_header_off = b.offset();
        let maker_base = inner_header_off;
        b.buf.extend_from_slice(&[b'I', b'I', 42, 0]);
        b.buf.extend_from_slice(&8u32.to_le_bytes());

        let (_mn_ifd_off, mn_value_positions) =
            b.append_ifd(&[(TAG_NIKON_PREVIEW_IFD, TY_LONG, 1, 0)], 0);
        let preview_ifd_off = b.offset();
        let (_preview_ifd_start, preview_value_positions) = b.append_ifd(
            &[
                (TAG_IMAGE_WIDTH, TY_SHORT, 1, 1920),
                (TAG_IMAGE_LENGTH, TY_SHORT, 1, 1280),
                (TAG_JPEG_IF_OFFSET, TY_LONG, 1, 0),
                (TAG_JPEG_IF_LENGTH, TY_LONG, 1, FAKE_JPEG.len() as u32),
            ],
            0,
        );
        b.patch_u32(mn_value_positions[0], preview_ifd_off - maker_base);
        let jpeg_off = b.append_bytes(FAKE_JPEG);
        b.patch_u32(preview_value_positions[2], jpeg_off - maker_base);

        let (exif_ifd_off, _) = b.append_ifd(&[(TAG_MAKER_NOTE, 7, 1, mn_off)], 0);
        let (ifd0_off, _) = b.append_ifd(&[(TAG_EXIF_IFD, TY_LONG, 1, exif_ifd_off)], 0);
        let data = b.finish(ifd0_off);

        let tmp = std::env::temp_dir().join(format!(
            "sniff-ifd-parity-test-{}-{}",
            std::process::id(),
            jpeg_off
        ));
        std::fs::write(&tmp, &data).unwrap();

        let mut slice_walker = Walker::new(SliceSource::new(&data)).expect("slice header");
        let slice_jpegs = slice_walker.find_embedded_jpegs().expect("slice walk");

        let mut file_walker =
            Walker::new(FileSource::open(&tmp, false).expect("open")).expect("file header");
        let file_jpegs = file_walker.find_embedded_jpegs().expect("file walk");

        std::fs::remove_file(&tmp).ok();

        assert_eq!(slice_jpegs.len(), file_jpegs.len());
        for (s, f) in slice_jpegs.iter().zip(file_jpegs.iter()) {
            assert_eq!(s.source, f.source);
            assert_eq!(s.file_offset, f.file_offset);
            assert_eq!(s.byte_len, f.byte_len);
            assert_eq!(s.declared_width, f.declared_width);
            assert_eq!(s.declared_height, f.declared_height);
        }

        // And the extracted JPEG bytes themselves must be byte-identical, via each walker's own
        // `read_range` -- not just the offset/len bookkeeping.
        let s = &slice_jpegs[0];
        let f = &file_jpegs[0];
        let slice_bytes = slice_walker
            .read_range(s.file_offset, s.byte_len as usize)
            .unwrap();
        let file_bytes = file_walker
            .read_range(f.file_offset, f.byte_len as usize)
            .unwrap();
        assert_eq!(slice_bytes, file_bytes);
        assert_eq!(slice_bytes, FAKE_JPEG);
    }

    /// Builds a little-endian TIFF file byte-by-byte, tracking absolute offsets as it goes so
    /// tests never hardcode a magic-number offset -- every offset used is derived from
    /// `buf.len()` at the moment it matters, the same way the real file layout would emerge.
    struct FileBuilder {
        buf: Vec<u8>,
    }

    impl FileBuilder {
        fn new() -> Self {
            // "II" + magic 42 + IFD0 offset placeholder (patched by `finish_header`).
            FileBuilder {
                buf: vec![b'I', b'I', 42, 0, 0, 0, 0, 0],
            }
        }

        fn offset(&self) -> u32 {
            self.buf.len() as u32
        }

        fn set_ifd0_offset(&mut self, off: u32) {
            self.buf[4..8].copy_from_slice(&off.to_le_bytes());
        }

        fn append_bytes(&mut self, bytes: &[u8]) -> u32 {
            let off = self.offset();
            self.buf.extend_from_slice(bytes);
            off
        }

        /// Writes one IFD (`count` header, N 12-byte entries, `next` trailer). Every entry's
        /// value must already fit inline (<=4 bytes) -- external array values aren't needed by
        /// these fixtures. Returns (ifd_offset, absolute offset of each entry's 4-byte value
        /// field, in the same order as `entries`), so callers can patch a value in after
        /// appending data that didn't exist yet when the entry was written (e.g. a JPEG offset
        /// pointing at bytes appended later).
        fn append_ifd(&mut self, entries: &[(u16, u16, u32, u32)], next: u32) -> (u32, Vec<u32>) {
            let ifd_off = self.offset();
            self.buf
                .extend_from_slice(&(entries.len() as u16).to_le_bytes());
            let mut value_positions = Vec::with_capacity(entries.len());
            for &(tag, ty, count, value) in entries {
                self.buf.extend_from_slice(&tag.to_le_bytes());
                self.buf.extend_from_slice(&ty.to_le_bytes());
                self.buf.extend_from_slice(&count.to_le_bytes());
                value_positions.push(self.offset());
                self.buf.extend_from_slice(&value.to_le_bytes());
            }
            self.buf.extend_from_slice(&next.to_le_bytes());
            (ifd_off, value_positions)
        }

        fn patch_u32(&mut self, at: u32, value: u32) {
            let at = at as usize;
            self.buf[at..at + 4].copy_from_slice(&value.to_le_bytes());
        }

        fn finish(mut self, ifd0_off: u32) -> Vec<u8> {
            self.set_ifd0_offset(ifd0_off);
            self.buf
        }
    }

    const TY_SHORT: u16 = 3;
    const TY_LONG: u16 = 4;

    const FAKE_JPEG: &[u8] = b"\xFF\xD8FAKEDATA\xFF\xD9";

    #[test]
    fn finds_ifd0_jpeg_pair() {
        let mut b = FileBuilder::new();
        let jpeg_off = b.append_bytes(FAKE_JPEG);
        let (ifd0_off, _) = b.append_ifd(
            &[
                (TAG_IMAGE_WIDTH, TY_SHORT, 1, 200),
                (TAG_IMAGE_LENGTH, TY_SHORT, 1, 100),
                (TAG_JPEG_IF_OFFSET, TY_LONG, 1, jpeg_off),
                (TAG_JPEG_IF_LENGTH, TY_LONG, 1, FAKE_JPEG.len() as u32),
            ],
            0,
        );
        let data = b.finish(ifd0_off);

        let mut walker = walk(&data);
        let jpegs = walker.find_embedded_jpegs().expect("walk");
        assert_eq!(jpegs.len(), 1);
        let j = &jpegs[0];
        assert_eq!(j.source, PreviewSource::Ifd0);
        assert_eq!(j.file_offset, jpeg_off as u64);
        assert_eq!(j.byte_len, FAKE_JPEG.len() as u64);
        assert_eq!(j.declared_width, Some(200));
        assert_eq!(j.declared_height, Some(100));
    }

    #[test]
    fn finds_thumbnail_via_next_ifd_chain() {
        let mut b = FileBuilder::new();
        let jpeg_off = b.append_bytes(FAKE_JPEG);
        // IFD1 (thumbnail): just the JPEG pair.
        let (ifd1_off, _) = b.append_ifd(
            &[
                (TAG_JPEG_IF_OFFSET, TY_LONG, 1, jpeg_off),
                (TAG_JPEG_IF_LENGTH, TY_LONG, 1, FAKE_JPEG.len() as u32),
            ],
            0,
        );
        // IFD0: no JPEG tags of its own, "next" points at IFD1.
        let (ifd0_off, _) = b.append_ifd(&[(TAG_IMAGE_WIDTH, TY_SHORT, 1, 8256)], ifd1_off);
        let data = b.finish(ifd0_off);

        let mut walker = walk(&data);
        let jpegs = walker.find_embedded_jpegs().expect("walk");
        assert_eq!(jpegs.len(), 1);
        assert_eq!(jpegs[0].source, PreviewSource::ThumbnailIfd);
        assert_eq!(jpegs[0].file_offset, jpeg_off as u64);
    }

    #[test]
    fn finds_dng_style_subifd_strip_jpeg() {
        let mut b = FileBuilder::new();
        let jpeg_off = b.append_bytes(FAKE_JPEG);
        // A DNG-style reduced-resolution preview SubIFD: NewSubfileType=1, old-style JPEG
        // compression (6), single-strip StripOffsets/StripByteCounts instead of the
        // JPEGInterchangeFormat pair.
        let (sub_ifd_off, _) = b.append_ifd(
            &[
                (TAG_NEW_SUBFILE_TYPE, TY_LONG, 1, 1),
                (TAG_COMPRESSION, TY_SHORT, 1, 6),
                (TAG_STRIP_OFFSETS, TY_LONG, 1, jpeg_off),
                (TAG_STRIP_BYTE_COUNTS, TY_LONG, 1, FAKE_JPEG.len() as u32),
                (TAG_IMAGE_WIDTH, TY_SHORT, 1, 1024),
                (TAG_IMAGE_LENGTH, TY_SHORT, 1, 768),
            ],
            0,
        );
        let (ifd0_off, _) = b.append_ifd(&[(TAG_SUB_IFDS, TY_LONG, 1, sub_ifd_off)], 0);
        let data = b.finish(ifd0_off);

        let mut walker = walk(&data);
        let jpegs = walker.find_embedded_jpegs().expect("walk");
        assert_eq!(jpegs.len(), 1);
        let j = &jpegs[0];
        assert_eq!(j.source, PreviewSource::SubIfd(0));
        assert_eq!(j.new_subfile_type, Some(1));
        assert_eq!(j.declared_width, Some(1024));
        assert_eq!(j.file_offset, jpeg_off as u64);
    }

    #[test]
    fn finds_nikon_maker_note_preview_ifd() {
        let mut b = FileBuilder::new();

        // Placeholder MakerNote-blob layout, built in the same order a real Nikon file uses:
        // "Nikon\0" + 2 version bytes + 2 reserved bytes, then an embedded TIFF header whose own
        // offsets (including the preview IFD's JPEG offset) are relative to *this* header's own
        // start, not the file's.
        let mn_off = b.offset();
        b.buf.extend_from_slice(b"Nikon\0");
        b.buf.extend_from_slice(&[0x02, 0x10, 0x00, 0x00]);
        let inner_header_off = b.offset();
        assert_eq!(inner_header_off, mn_off + 10);
        let maker_base = inner_header_off; // offsets inside are relative to here.

        // Inner TIFF header: mn_ifd starts immediately after these 8 bytes, i.e. at relative
        // offset 8.
        b.buf.extend_from_slice(&[b'I', b'I', 42, 0]);
        b.buf.extend_from_slice(&8u32.to_le_bytes());
        assert_eq!(b.offset(), inner_header_off + 8);

        // mn_ifd: one entry, the NikonPreviewIFD pointer, also maker_base-relative.
        let preview_ifd_rel_off_placeholder = 0u32; // patched once we know where preview_ifd lands
        let (_mn_ifd_off, mn_value_positions) = b.append_ifd(
            &[(
                TAG_NIKON_PREVIEW_IFD,
                TY_LONG,
                1,
                preview_ifd_rel_off_placeholder,
            )],
            0,
        );

        // preview_ifd: ImageWidth/Length plus a JPEGInterchangeFormat pair, all maker_base-
        // relative. The offset value is patched in after the JPEG bytes are appended below.
        let preview_ifd_off = b.offset();
        let (_preview_ifd_start, preview_value_positions) = b.append_ifd(
            &[
                (TAG_IMAGE_WIDTH, TY_SHORT, 1, 1920),
                (TAG_IMAGE_LENGTH, TY_SHORT, 1, 1280),
                (TAG_JPEG_IF_OFFSET, TY_LONG, 1, 0), // placeholder
                (TAG_JPEG_IF_LENGTH, TY_LONG, 1, FAKE_JPEG.len() as u32),
            ],
            0,
        );

        // Now that preview_ifd's final position is known, patch mn_ifd's pointer to it.
        b.patch_u32(mn_value_positions[0], preview_ifd_off - maker_base);

        // Append the actual JPEG bytes after the MakerNote structure, and patch preview_ifd's
        // JPEGInterchangeFormat offset to point at them (maker_base-relative, per the Nikon
        // quirk -- this offset is NOT relative to the JPEG's containing IFD or to the file).
        let jpeg_off = b.append_bytes(FAKE_JPEG);
        b.patch_u32(preview_value_positions[2], jpeg_off - maker_base);

        // ExifIFD -> MakerNote, and IFD0 -> ExifIFD, both ordinary file-absolute offsets.
        let (exif_ifd_off, _) = b.append_ifd(&[(TAG_MAKER_NOTE, 7 /* UNDEFINED */, 1, mn_off)], 0);
        let (ifd0_off, _) = b.append_ifd(&[(TAG_EXIF_IFD, TY_LONG, 1, exif_ifd_off)], 0);
        let data = b.finish(ifd0_off);

        let mut walker = walk(&data);
        let jpegs = walker.find_embedded_jpegs().expect("walk");
        assert_eq!(jpegs.len(), 1);
        let j = &jpegs[0];
        assert_eq!(j.source, PreviewSource::NikonPreviewIfd);
        assert_eq!(j.file_offset, jpeg_off as u64);
        assert_eq!(j.byte_len, FAKE_JPEG.len() as u64);
        assert_eq!(j.declared_width, Some(1920));
        assert_eq!(j.declared_height, Some(1280));
    }

    #[test]
    fn rejects_too_short_buffer() {
        assert!(matches!(
            Walker::new(SliceSource::new(&[0u8; 4])),
            Err(IfdError::TooShort)
        ));
    }

    #[test]
    fn rejects_bad_byte_order_marker() {
        let data = [b'X', b'X', 42, 0, 0, 0, 0, 8];
        assert!(matches!(
            Walker::new(SliceSource::new(&data)),
            Err(IfdError::BadByteOrder)
        ));
    }

    #[test]
    fn detects_self_referencing_ifd_cycle() {
        // IFD0's "next" pointer points back at itself.
        let mut b = FileBuilder::new();
        let ifd0_off = b.offset();
        b.buf.extend_from_slice(&1u16.to_le_bytes());
        b.buf.extend_from_slice(&TAG_IMAGE_WIDTH.to_le_bytes());
        b.buf.extend_from_slice(&TY_SHORT.to_le_bytes());
        b.buf.extend_from_slice(&1u32.to_le_bytes());
        b.buf.extend_from_slice(&100u32.to_le_bytes());
        b.buf.extend_from_slice(&ifd0_off.to_le_bytes()); // next == self
        let data = b.finish(ifd0_off);

        let mut walker = walk(&data);
        // The cycle is on the "next IFD" chain, which the walker tolerates (breaks the loop
        // rather than erroring the whole walk) -- it should still return cleanly with no
        // embedded JPEGs found.
        let jpegs = walker
            .find_embedded_jpegs()
            .expect("walk tolerates a next-IFD cycle");
        assert!(jpegs.is_empty());
    }

    #[test]
    fn truncated_ifd_entry_count_is_an_error() {
        // Claims 5 entries but the buffer ends immediately after the count field.
        let mut data = vec![b'I', b'I', 42, 0, 8, 0, 0, 0];
        data.extend_from_slice(&5u16.to_le_bytes());
        let mut walker = walk(&data);
        assert!(matches!(
            walker.find_embedded_jpegs(),
            Err(IfdError::TruncatedIfd) | Err(IfdError::OffsetOutOfBounds(_))
        ));
    }

    #[test]
    fn ignores_multi_strip_subifd_as_out_of_scope() {
        // A SubIFD with 2 strips is the full-resolution raw image data, not a single-shot JPEG
        // preview -- `strip_jpeg` should decline it (count != 1), not misinterpret strip 0 as a
        // whole JPEG.
        let mut b = FileBuilder::new();
        let jpeg_off = b.append_bytes(FAKE_JPEG);
        let sub_ifd_off = {
            let ifd_off = b.offset();
            b.buf.extend_from_slice(&4u16.to_le_bytes());
            // NewSubfileType = 0 (main image)
            b.buf.extend_from_slice(&TAG_NEW_SUBFILE_TYPE.to_le_bytes());
            b.buf.extend_from_slice(&TY_LONG.to_le_bytes());
            b.buf.extend_from_slice(&1u32.to_le_bytes());
            b.buf.extend_from_slice(&0u32.to_le_bytes());
            // Compression = 7
            b.buf.extend_from_slice(&TAG_COMPRESSION.to_le_bytes());
            b.buf.extend_from_slice(&TY_SHORT.to_le_bytes());
            b.buf.extend_from_slice(&1u32.to_le_bytes());
            b.buf.extend_from_slice(&7u32.to_le_bytes());
            // StripOffsets, count = 2 (external array; contents irrelevant, walker should bail
            // before reading them since count != 1).
            b.buf.extend_from_slice(&TAG_STRIP_OFFSETS.to_le_bytes());
            b.buf.extend_from_slice(&TY_LONG.to_le_bytes());
            b.buf.extend_from_slice(&2u32.to_le_bytes());
            b.buf.extend_from_slice(&jpeg_off.to_le_bytes());
            // StripByteCounts, count = 2
            b.buf
                .extend_from_slice(&TAG_STRIP_BYTE_COUNTS.to_le_bytes());
            b.buf.extend_from_slice(&TY_LONG.to_le_bytes());
            b.buf.extend_from_slice(&2u32.to_le_bytes());
            b.buf.extend_from_slice(&jpeg_off.to_le_bytes());
            b.buf.extend_from_slice(&0u32.to_le_bytes()); // next
            ifd_off
        };
        let (ifd0_off, _) = b.append_ifd(&[(TAG_SUB_IFDS, TY_LONG, 1, sub_ifd_off)], 0);
        let data = b.finish(ifd0_off);

        let mut walker = walk(&data);
        let jpegs = walker.find_embedded_jpegs().expect("walk");
        assert!(jpegs.is_empty());
    }
}
