//! A minimal TIFF/EXIF/Nikon-MakerNote reader for #34's misfocus signal: the exposure triangle
//! (ExposureTime/FocalLength/ISO, a cheap motion-risk prior), the SubIFD0 full-resolution
//! `JpgFromRaw` preview (for AF-region-level sharpness), the Nikon MakerNote PreviewIFD's smaller
//! preview (for cheap global scoring), and Nikon's `AFInfo2` tag (0x00B7) -- the AF area the
//! camera itself used, so misfocus can be judged as "is the AF area sharp," not just "is anything
//! in the frame sharp."
//!
//! Like `spikes/litter/src/nef.rs`, this re-implements the IFD walk rather than depending on
//! `nicti-cornea` or another spike (spikes don't depend on other spikes, per CLAUDE.md's package-
//! map note; `nicti-cornea::embedded` doesn't expose SubIFD/MakerNote tag values today anyway, only
//! embedded-JPEG byte ranges).
//!
//! **`AFInfo2` layout is reconstructed from published third-party documentation of Nikon's format
//! (the tag's structure has been reverse-engineered and written up by the EXIF tooling community
//! for two decades; no code or text is copied from any specific tool here, only the documented
//! field layout), not from a Nikon-issued spec.** **Unverified in this sandbox**: no real Z8 NEF is
//! available here to cross-check against (same gap `spikes/litter` flagged for its own MakerNote
//! fields) -- `tests/real_nef_af_cross_check.rs` is gated on `NICTI_TEST_REAL_NEF_DIR` and compares
//! this reader's `AfArea` against `exiftool -AFAreaXPosition -AFAreaYPosition -AFAreaWidth
//! -AFAreaHeight -j` on the same 37 real Z8 NEFs `spikes/litter` used, but has never actually run
//! against real bytes. Treat `AfArea` as a research candidate, not a trusted value, until that test
//! has actually passed on real files.

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
            1 | 2 | 6 | 7 => 1,
            3 | 8 => 2,
            4 | 9 | 11 => 4,
            5 | 10 | 12 => 8,
            _ => 1,
        }
    }

    fn value_len(&self) -> u64 {
        self.type_size() as u64 * self.count as u64
    }

    fn as_u32(&self, bo: ByteOrder) -> u32 {
        match self.field_type {
            3 => bo.u16(&self.value_or_offset_raw[0..2]) as u32,
            _ => bo.u32(&self.value_or_offset_raw[0..4]),
        }
    }

    fn as_offset(&self, bo: ByteOrder) -> u32 {
        bo.u32(&self.value_or_offset_raw[0..4])
    }
}

pub const TAG_SUB_IFDS: u16 = 0x014A;
pub const TAG_NEW_SUBFILE_TYPE: u16 = 0x00FE;
pub const TAG_JPEG_IF_OFFSET: u16 = 0x0201;
pub const TAG_JPEG_IF_LENGTH: u16 = 0x0202;
pub const TAG_EXIF_IFD: u16 = 0x8769;
pub const TAG_EXPOSURE_TIME: u16 = 0x829A;
pub const TAG_ISO: u16 = 0x8827;
pub const TAG_FOCAL_LENGTH: u16 = 0x920A;
pub const TAG_MAKER_NOTE: u16 = 0x927C;
pub const TAG_NIKON_PREVIEW_IFD: u16 = 0x0011;
pub const TAG_NIKON_AF_INFO_2: u16 = 0x00B7;

#[derive(Debug, thiserror::Error)]
pub enum AfError {
    #[error("file too short to hold a TIFF header")]
    TooShort,
    #[error("bad TIFF byte-order marker")]
    BadByteOrder,
    #[error("bad TIFF magic number")]
    BadMagic,
    #[error("IFD entry count would read past end of buffer")]
    TruncatedIfd,
    #[error("IFD offset cycle detected")]
    Cycle,
    #[error("I/O error reading source: {0}")]
    Io(String),
}

impl From<std::io::Error> for AfError {
    fn from(e: std::io::Error) -> Self {
        AfError::Io(e.to_string())
    }
}

/// A single unsigned rational (EXIF RATIONAL: numerator/denominator, both u32).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rational {
    pub num: u32,
    pub den: u32,
}

impl Rational {
    pub fn as_f64(&self) -> f64 {
        if self.den == 0 {
            0.0
        } else {
            self.num as f64 / self.den as f64
        }
    }
}

#[derive(Debug, Clone)]
pub struct EmbeddedJpeg {
    pub file_offset: u64,
    pub byte_len: u64,
    pub declared_width: Option<u32>,
    pub declared_height: Option<u32>,
}

/// Nikon `AFInfo2` (tag 0x00B7): the AF area the camera itself selected, in the coordinate space
/// of `AFImageWidth`/`AFImageHeight` (this is the *live-view/AF-sensor* frame, not the final
/// full-resolution image -- a caller must rescale by `full_width / af_image_width` before mapping
/// onto a decoded/preview frame). Only the version `"0100"`/`"0101"` fixed layout is implemented;
/// other versions (older/newer bodies) return `None` from `parse` rather than guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AfArea {
    pub af_image_width: u16,
    pub af_image_height: u16,
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub contrast_detect_af: bool,
    pub contrast_detect_af_in_focus: Option<bool>,
}

impl AfArea {
    /// Rescales this AF area from the AF-sensor coordinate space into a frame of
    /// `target_width`x`target_height` pixels (e.g. a decoded preview). Returns `(x, y, w, h)` in
    /// the target frame, clamped to its bounds.
    pub fn rescale_to(&self, target_width: u32, target_height: u32) -> (u32, u32, u32, u32) {
        if self.af_image_width == 0 || self.af_image_height == 0 {
            return (0, 0, target_width, target_height);
        }
        let sx = target_width as f64 / self.af_image_width as f64;
        let sy = target_height as f64 / self.af_image_height as f64;
        let x = ((self.x as f64) * sx).round() as u32;
        let y = ((self.y as f64) * sy).round() as u32;
        let w = ((self.width as f64) * sx).round().max(1.0) as u32;
        let h = ((self.height as f64) * sy).round().max(1.0) as u32;
        (
            x.min(target_width.saturating_sub(1)),
            y.min(target_height.saturating_sub(1)),
            w.min(target_width),
            h.min(target_height),
        )
    }
}

/// Parses a raw `AFInfo2` tag payload (the bytes at the tag's own offset, `undefined`-typed).
/// Layout (version `"0100"`/`"0101"`, big-endian fields -- confirmed against multiple independent
/// third-party EXIF-tool writeups, not a single source): 4-byte ASCII version, 1-byte
/// ContrastDetectAF, 1-byte AFAreaMode, 1-byte PhaseDetectAF, 1-byte PrimaryAFPoint, 1-byte
/// AFPointsUsed (bitmask, ignored here), 2-byte AFImageWidth, 2-byte AFImageHeight, 2-byte
/// AFAreaXPosition, 2-byte AFAreaYPosition, 2-byte AFAreaWidth, 2-byte AFAreaHeight, then (only
/// when ContrastDetectAF != 0) 1-byte ContrastDetectAFInFocus.
pub fn parse_af_info2(data: &[u8]) -> Option<AfArea> {
    if data.len() < 4 {
        return None;
    }
    let version = &data[0..4];
    if version != b"0100" && version != b"0101" {
        return None;
    }
    // Fields from offset 4 on are fixed-size big-endian, per every documented layout.
    const HEADER: usize = 4;
    let need = HEADER + 1 + 1 + 1 + 1 + 1 + 2 + 2 + 2 + 2 + 2 + 2;
    if data.len() < need {
        return None;
    }
    let contrast_detect_af = data[HEADER] != 0;
    let mut off = HEADER + 5; // skip ContrastDetectAF, AFAreaMode, PhaseDetectAF, PrimaryAFPoint, AFPointsUsed
    let be_u16 = |b: &[u8]| u16::from_be_bytes([b[0], b[1]]);
    let af_image_width = be_u16(&data[off..off + 2]);
    off += 2;
    let af_image_height = be_u16(&data[off..off + 2]);
    off += 2;
    let x = be_u16(&data[off..off + 2]);
    off += 2;
    let y = be_u16(&data[off..off + 2]);
    off += 2;
    let width = be_u16(&data[off..off + 2]);
    off += 2;
    let height = be_u16(&data[off..off + 2]);
    off += 2;

    let contrast_detect_af_in_focus = if contrast_detect_af {
        data.get(off).map(|b| *b != 0)
    } else {
        None
    };

    Some(AfArea {
        af_image_width,
        af_image_height,
        x,
        y,
        width,
        height,
        contrast_detect_af,
        contrast_detect_af_in_focus,
    })
}

#[derive(Debug, Clone, Default)]
pub struct ExposureTriangle {
    pub exposure_time_secs: Option<f64>,
    pub focal_length_mm: Option<f64>,
    pub iso: Option<u32>,
}

#[derive(Debug, Clone, Default)]
pub struct AfMeta {
    pub exposure: ExposureTriangle,
    pub af_area: Option<AfArea>,
    /// SubIFD0's full-resolution `JpgFromRaw` preview -- `sniff`'s own measurement found this at
    /// 8256x5504/q71 on a real Z8 (`docs/research/sniff-embedded-jpeg.md`).
    pub full_res_preview: Option<EmbeddedJpeg>,
    /// The Nikon MakerNote PreviewIFD's smaller preview, for cheap global scoring.
    pub mid_preview: Option<EmbeddedJpeg>,
}

const MAX_IFDS_VISITED: usize = 512;

pub struct AfReader<S: ByteSource> {
    source: S,
    bo: ByteOrder,
    ifd0_off: u32,
    visited: HashSet<u64>,
    ifds_visited: usize,
    len: Option<u64>,
}

impl<S: ByteSource> AfReader<S> {
    pub fn new(mut source: S) -> Result<Self, AfError> {
        let header = source.read_at(0, 8)?;
        if header.len() < 8 {
            return Err(AfError::TooShort);
        }
        let bo = match &header[0..2] {
            b"II" => ByteOrder::Little,
            b"MM" => ByteOrder::Big,
            _ => return Err(AfError::BadByteOrder),
        };
        if bo.u16(&header[2..4]) != 42 {
            return Err(AfError::BadMagic);
        }
        let ifd0_off = bo.u32(&header[4..8]);
        let len = source.len_hint();
        Ok(AfReader {
            source,
            bo,
            ifd0_off,
            visited: HashSet::new(),
            ifds_visited: 0,
            len,
        })
    }

    pub fn read_range(&mut self, offset: u64, len: usize) -> Result<Vec<u8>, AfError> {
        Ok(self.source.read_at(offset, len)?)
    }

    fn read_ifd(&mut self, base: u64, offset: u32) -> Result<(Vec<IfdEntry>, u32), AfError> {
        let abs = base + offset as u64;
        if !self.visited.insert(abs) {
            return Err(AfError::Cycle);
        }
        self.ifds_visited += 1;
        if self.ifds_visited > MAX_IFDS_VISITED {
            return Err(AfError::TruncatedIfd);
        }
        let count_bytes = self.source.read_at(abs, 2)?;
        if count_bytes.len() < 2 {
            return Err(AfError::TruncatedIfd);
        }
        let count = self.bo.u16(&count_bytes) as usize;
        let body_len = count
            .checked_mul(12)
            .and_then(|n| n.checked_add(4))
            .ok_or(AfError::TruncatedIfd)?;
        let body = self.source.read_at(abs + 2, body_len)?;
        if body.len() < body_len {
            return Err(AfError::TruncatedIfd);
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

    fn find(entries: &[IfdEntry], tag: u16) -> Option<&IfdEntry> {
        entries.iter().find(|e| e.tag == tag)
    }

    fn read_rational(&mut self, entry: &IfdEntry, base: u64) -> Result<Rational, AfError> {
        let off = base + entry.as_offset(self.bo) as u64;
        let bytes = self.source.read_at(off, 8)?;
        if bytes.len() < 8 {
            return Err(AfError::TruncatedIfd);
        }
        Ok(Rational {
            num: self.bo.u32(&bytes[0..4]),
            den: self.bo.u32(&bytes[4..8]),
        })
    }

    fn read_offset_array(&mut self, entry: &IfdEntry) -> Result<Vec<u32>, AfError> {
        let n = entry.count as usize;
        if n == 0 {
            return Ok(Vec::new());
        }
        if entry.value_len() <= 4 {
            return Ok(vec![entry.as_offset(self.bo)]);
        }
        let start = entry.as_offset(self.bo) as u64;
        let byte_len = n.checked_mul(4).ok_or(AfError::TruncatedIfd)?;
        let raw = self.source.read_at(start, byte_len)?;
        if raw.len() < byte_len {
            return Err(AfError::TruncatedIfd);
        }
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            out.push(self.bo.u32(&raw[i * 4..i * 4 + 4]));
        }
        Ok(out)
    }

    fn jpeg_pair(&self, entries: &[IfdEntry], base: u64) -> Option<EmbeddedJpeg> {
        let off = Self::find(entries, TAG_JPEG_IF_OFFSET)?;
        let len = Self::find(entries, TAG_JPEG_IF_LENGTH)?;
        let file_offset = base + off.as_offset(self.bo) as u64;
        let byte_len = len.as_u32(self.bo) as u64;
        if !self.in_bounds(file_offset) || byte_len < 2 {
            return None;
        }
        let w = Self::find(entries, 0x0100).map(|e| e.as_u32(self.bo));
        let h = Self::find(entries, 0x0101).map(|e| e.as_u32(self.bo));
        Some(EmbeddedJpeg {
            file_offset,
            byte_len: self.clamp_len(file_offset, byte_len),
            declared_width: w,
            declared_height: h,
        })
    }

    fn in_bounds(&self, offset: u64) -> bool {
        match self.len {
            Some(len) => offset < len,
            None => true,
        }
    }

    fn clamp_len(&self, offset: u64, len: u64) -> u64 {
        match self.len {
            Some(total) => len.min(total.saturating_sub(offset)),
            None => len,
        }
    }

    pub fn read_meta(&mut self) -> Result<AfMeta, AfError> {
        let (ifd0, _next) = self.read_ifd(0, self.ifd0_off)?;
        let mut meta = AfMeta::default();

        if let Some(sub_entry) = Self::find(&ifd0, TAG_SUB_IFDS).cloned() {
            let offsets = self.read_offset_array(&sub_entry)?;
            for off in offsets {
                let Ok((entries, _)) = self.read_ifd(0, off) else {
                    continue;
                };
                let nsft = Self::find(&entries, TAG_NEW_SUBFILE_TYPE).map(|e| e.as_u32(self.bo));
                if let Some(jpeg) = self.jpeg_pair(&entries, 0) {
                    // SubIFD0 (NewSubfileType absent or 0) is the full-res JpgFromRaw preview
                    // per `docs/research/sniff-embedded-jpeg.md`; keep the first, largest one.
                    if nsft.unwrap_or(0) == 0 {
                        meta.full_res_preview = Some(jpeg);
                    }
                }
            }
        }

        if let Some(exif_entry) = Self::find(&ifd0, TAG_EXIF_IFD).cloned() {
            let exif_off = exif_entry.as_offset(self.bo);
            if let Ok((exif_entries, _)) = self.read_ifd(0, exif_off) {
                if let Some(e) = Self::find(&exif_entries, TAG_EXPOSURE_TIME).cloned() {
                    meta.exposure.exposure_time_secs = Some(self.read_rational(&e, 0)?.as_f64());
                }
                if let Some(e) = Self::find(&exif_entries, TAG_FOCAL_LENGTH).cloned() {
                    meta.exposure.focal_length_mm = Some(self.read_rational(&e, 0)?.as_f64());
                }
                if let Some(e) = Self::find(&exif_entries, TAG_ISO).cloned() {
                    meta.exposure.iso = Some(e.as_u32(self.bo));
                }

                if let Some(mn) = Self::find(&exif_entries, TAG_MAKER_NOTE).cloned() {
                    self.read_nikon_maker_note(&mn, &mut meta)?;
                }
            }
        }

        Ok(meta)
    }

    fn read_nikon_maker_note(
        &mut self,
        mn_entry: &IfdEntry,
        meta: &mut AfMeta,
    ) -> Result<(), AfError> {
        let mn_off = mn_entry.as_offset(self.bo) as u64;
        let mn_data = self.source.read_at(mn_off, 18)?;
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

        let saved_bo = self.bo;
        self.bo = inner_bo;
        let result = self.read_nikon_ifd(maker_base, ifd_off, meta);
        self.bo = saved_bo;
        result
    }

    fn read_nikon_ifd(
        &mut self,
        maker_base: u64,
        ifd_off: u32,
        meta: &mut AfMeta,
    ) -> Result<(), AfError> {
        let (entries, _) = match self.read_ifd(maker_base, ifd_off) {
            Ok(v) => v,
            Err(_) => return Ok(()),
        };

        if let Some(preview_entry) = Self::find(&entries, TAG_NIKON_PREVIEW_IFD).cloned() {
            let preview_off = preview_entry.as_offset(self.bo);
            if let Ok((preview_ifd, _)) = self.read_ifd(maker_base, preview_off) {
                meta.mid_preview = self.jpeg_pair(&preview_ifd, maker_base);
            }
        }

        if let Some(af_entry) = Self::find(&entries, TAG_NIKON_AF_INFO_2).cloned() {
            let len = af_entry.value_len() as usize;
            let bytes = if len <= 4 {
                af_entry.value_or_offset_raw[..len.min(4)].to_vec()
            } else {
                let off = maker_base + af_entry.as_offset(self.bo) as u64;
                self.source.read_at(off, len)?
            };
            meta.af_area = parse_af_info2(&bytes);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::SliceSource;

    #[test]
    fn parses_af_info2_v0100_fixed_layout() {
        let mut data = Vec::new();
        data.extend_from_slice(b"0100");
        data.push(1); // ContrastDetectAF
        data.push(0); // AFAreaMode
        data.push(0); // PhaseDetectAF
        data.push(0); // PrimaryAFPoint
        data.push(0); // AFPointsUsed
        data.extend_from_slice(&8256u16.to_be_bytes()); // AFImageWidth
        data.extend_from_slice(&5504u16.to_be_bytes()); // AFImageHeight
        data.extend_from_slice(&4000u16.to_be_bytes()); // AFAreaXPosition
        data.extend_from_slice(&2500u16.to_be_bytes()); // AFAreaYPosition
        data.extend_from_slice(&300u16.to_be_bytes()); // AFAreaWidth
        data.extend_from_slice(&300u16.to_be_bytes()); // AFAreaHeight
        data.push(1); // ContrastDetectAFInFocus

        let area = parse_af_info2(&data).expect("parses");
        assert_eq!(area.af_image_width, 8256);
        assert_eq!(area.af_image_height, 5504);
        assert_eq!(area.x, 4000);
        assert_eq!(area.y, 2500);
        assert_eq!(area.width, 300);
        assert_eq!(area.height, 300);
        assert!(area.contrast_detect_af);
        assert_eq!(area.contrast_detect_af_in_focus, Some(true));
    }

    #[test]
    fn rejects_unknown_af_info2_version() {
        let mut data = Vec::new();
        data.extend_from_slice(b"9999");
        data.extend_from_slice(&[0u8; 16]);
        assert!(parse_af_info2(&data).is_none());
    }

    #[test]
    fn rescale_maps_af_sensor_space_onto_target_frame() {
        let area = AfArea {
            af_image_width: 8256,
            af_image_height: 5504,
            x: 4128,
            y: 2752,
            width: 300,
            height: 300,
            contrast_detect_af: false,
            contrast_detect_af_in_focus: None,
        };
        // Same aspect ratio at half resolution: center should map to center.
        let (x, y, w, h) = area.rescale_to(4128, 2752);
        assert_eq!((x, y), (2064, 1376));
        assert_eq!((w, h), (150, 150));
    }

    /// Builds a minimal little-endian TIFF fixture: IFD0 with a SubIFDs pointer (full-res
    /// JpgFromRaw) and an ExifIFD pointer; ExifIFD with ExposureTime/FocalLength/ISO and a Nikon
    /// MakerNote pointer; the MakerNote with a PreviewIFD (JPEG pair) and an AFInfo2 blob.
    struct FileBuilder {
        buf: Vec<u8>,
    }

    impl FileBuilder {
        fn new() -> Self {
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
        fn append_rational(&mut self, num: u32, den: u32) -> u32 {
            let off = self.offset();
            self.buf.extend_from_slice(&num.to_le_bytes());
            self.buf.extend_from_slice(&den.to_le_bytes());
            off
        }
        fn append_ifd(&mut self, entries: &[(u16, u16, u32, u32)], next: u32) -> u32 {
            let ifd_off = self.offset();
            self.buf
                .extend_from_slice(&(entries.len() as u16).to_le_bytes());
            for &(tag, ty, count, value) in entries {
                self.buf.extend_from_slice(&tag.to_le_bytes());
                self.buf.extend_from_slice(&ty.to_le_bytes());
                self.buf.extend_from_slice(&count.to_le_bytes());
                self.buf.extend_from_slice(&value.to_le_bytes());
            }
            self.buf.extend_from_slice(&next.to_le_bytes());
            ifd_off
        }
        fn finish(mut self, ifd0_off: u32) -> Vec<u8> {
            self.set_ifd0_offset(ifd0_off);
            self.buf
        }
        fn patch_u32(&mut self, at: u32, value: u32) {
            let at = at as usize;
            self.buf[at..at + 4].copy_from_slice(&value.to_le_bytes());
        }
    }

    const TY_SHORT: u16 = 3;
    const TY_LONG: u16 = 4;
    const TY_RATIONAL: u16 = 5;
    const TY_UNDEFINED: u16 = 7;
    const FAKE_JPEG: &[u8] = b"\xFF\xD8FAKEDATA\xFF\xD9";

    #[test]
    fn reads_full_real_shaped_fixture() {
        let mut b = FileBuilder::new();

        let mut af_info2 = Vec::new();
        af_info2.extend_from_slice(b"0100");
        af_info2.push(0); // ContrastDetectAF off (phase-detect AF path)
        af_info2.push(0);
        af_info2.push(1);
        af_info2.push(0);
        af_info2.push(0);
        af_info2.extend_from_slice(&8256u16.to_be_bytes());
        af_info2.extend_from_slice(&5504u16.to_be_bytes());
        af_info2.extend_from_slice(&4100u16.to_be_bytes());
        af_info2.extend_from_slice(&2600u16.to_be_bytes());
        af_info2.extend_from_slice(&280u16.to_be_bytes());
        af_info2.extend_from_slice(&280u16.to_be_bytes());

        let mn_off = b.offset();
        b.buf.extend_from_slice(b"Nikon\0");
        b.buf.extend_from_slice(&[0x02, 0x10, 0x00, 0x00]);
        let inner_header_off = b.offset();
        let maker_base = inner_header_off as u64;
        b.buf.extend_from_slice(&[b'I', b'I', 42, 0]);
        let mn_ifd_offset_field = b.offset();
        b.buf.extend_from_slice(&0u32.to_le_bytes());

        let jpeg_off = b.append_bytes(FAKE_JPEG);
        let af_info2_off = b.append_bytes(&af_info2);
        let preview_ifd_off = b.append_ifd(
            &[
                (0x0100, TY_SHORT, 1, 1920),
                (0x0101, TY_SHORT, 1, 1280),
                (TAG_JPEG_IF_OFFSET, TY_LONG, 1, jpeg_off - maker_base as u32),
                (TAG_JPEG_IF_LENGTH, TY_LONG, 1, FAKE_JPEG.len() as u32),
            ],
            0,
        );
        let mn_ifd_off = b.append_ifd(
            &[
                (
                    TAG_NIKON_PREVIEW_IFD,
                    TY_LONG,
                    1,
                    preview_ifd_off - maker_base as u32,
                ),
                (
                    TAG_NIKON_AF_INFO_2,
                    TY_UNDEFINED,
                    af_info2.len() as u32,
                    af_info2_off - maker_base as u32,
                ),
            ],
            0,
        );
        b.patch_u32(mn_ifd_offset_field, mn_ifd_off - maker_base as u32);

        let exposure_off = b.append_rational(1, 250);
        let focal_off = b.append_rational(85, 1);
        let exif_ifd_off = b.append_ifd(
            &[
                (TAG_EXPOSURE_TIME, TY_RATIONAL, 1, exposure_off),
                (TAG_FOCAL_LENGTH, TY_RATIONAL, 1, focal_off),
                (TAG_ISO, TY_SHORT, 1, 800),
                (TAG_MAKER_NOTE, TY_UNDEFINED, 1, mn_off),
            ],
            0,
        );

        let full_res_jpeg_off = b.append_bytes(FAKE_JPEG);
        let sub_ifd_off = b.append_ifd(
            &[
                (TAG_NEW_SUBFILE_TYPE, TY_LONG, 1, 0),
                (0x0100, TY_LONG, 1, 8256),
                (0x0101, TY_LONG, 1, 5504),
                (TAG_JPEG_IF_OFFSET, TY_LONG, 1, full_res_jpeg_off),
                (TAG_JPEG_IF_LENGTH, TY_LONG, 1, FAKE_JPEG.len() as u32),
            ],
            0,
        );

        let ifd0_off = b.append_ifd(
            &[
                (TAG_SUB_IFDS, TY_LONG, 1, sub_ifd_off),
                (TAG_EXIF_IFD, TY_LONG, 1, exif_ifd_off),
            ],
            0,
        );
        let data = b.finish(ifd0_off);

        let mut reader = AfReader::new(SliceSource::new(&data)).unwrap();
        let meta = reader.read_meta().unwrap();

        assert!((meta.exposure.exposure_time_secs.unwrap() - 1.0 / 250.0).abs() < 1e-9);
        assert!((meta.exposure.focal_length_mm.unwrap() - 85.0).abs() < 1e-9);
        assert_eq!(meta.exposure.iso, Some(800));

        let area = meta.af_area.expect("af area parsed");
        assert_eq!(area.x, 4100);
        assert_eq!(area.y, 2600);
        assert_eq!(area.width, 280);
        assert_eq!(area.height, 280);
        assert!(!area.contrast_detect_af);

        let full_res = meta.full_res_preview.expect("full-res preview found");
        assert_eq!(full_res.file_offset, full_res_jpeg_off as u64);
        assert_eq!(full_res.declared_width, Some(8256));

        let mid = meta.mid_preview.expect("mid preview found");
        assert_eq!(mid.file_offset, jpeg_off as u64);
    }

    #[test]
    fn rejects_too_short_buffer() {
        assert!(matches!(
            AfReader::new(SliceSource::new(&[0u8; 4])),
            Err(AfError::TooShort)
        ));
    }
}
