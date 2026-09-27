//! A minimal TIFF/EXIF/Nikon-MakerNote reader, extending `spikes/sniff/src/ifd.rs`'s walker
//! (#28/#29) with the tags #33's grouping signals actually need: capture time (with millisecond
//! sub-second precision), Nikon's shutter count and continuous-release bit, camera serial number,
//! orientation, and the Nikon MakerNote PreviewIFD's embedded JPEG (T0, per ADR-0017) for
//! perceptual-hash/embedding signals.
//!
//! Sniff's walker covers IFD0/next-IFD-chain/SubIFDs/the-MakerNote-preview generally, for finding
//! *any* embedded JPEG across DNG/NEF alike; this reader is narrower and Nikon-specific -- it only
//! needs the one MakerNote-embedded preview plus a handful of named tags, so it re-implements the
//! walk rather than depending on the `sniff` spike crate directly (spikes don't depend on other
//! spikes, per CLAUDE.md's package-map note).
//!
//! **Verified against 37 real Z8 NEFs** (`H:\Photos\Furries\Socials\2025\2025-12-27`, exiftool
//! cross-check + each file's own XMP sidecar, see `tests/real_nef_cross_check.rs` -- gated on
//! `NICTI_TEST_REAL_NEF_DIR`, not run in CI, which has neither the env var nor the files):
//! `ShutterCount`/`SerialNumber` come through unencrypted on this body (tag 0x00A7 direct int32u,
//! no key derivation needed) and match `aux:ImageNumber`/`aux:SerialNumber` exactly;
//! `DateTimeOriginal`+`SubSecTimeOriginal` combine to match `exif:DateTimeOriginal`'s millisecond
//! value to within 1ms on every file (LRC's own XMP-writing rounds its decimal-fraction expansion
//! slightly differently than this parser's literal `"0.<digits>"` interpretation on some files --
//! confirmed against exiftool's independent parse, which agrees with this parser, not LRC's XMP;
//! irrelevant at grouping resolution either way). **Unverified**: whether other Nikon
//! bodies (D7500/D3400) encrypt these tags -- some Nikon DSLRs do, using a key derived from the
//! serial number and a separate counter tag; this reader doesn't implement that decryption, and a
//! future real-file test against a D-series body should confirm whether it's needed before trusting
//! `shutter_count`/`serial` on non-Z-series input.

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

pub const TAG_ORIENTATION: u16 = 0x0112;
pub const TAG_EXIF_IFD: u16 = 0x8769;
pub const TAG_MAKER_NOTE: u16 = 0x927C;
pub const TAG_DATE_TIME_ORIGINAL: u16 = 0x9003;
pub const TAG_SUBSEC_TIME_ORIGINAL: u16 = 0x9291;
pub const TAG_NIKON_SERIAL_NUMBER: u16 = 0x001D;
pub const TAG_NIKON_SHOOTING_MODE: u16 = 0x0089;
pub const TAG_NIKON_SHUTTER_COUNT: u16 = 0x00A7;
pub const TAG_NIKON_PREVIEW_IFD: u16 = 0x0011;
pub const TAG_JPEG_IF_OFFSET: u16 = 0x0201;
pub const TAG_JPEG_IF_LENGTH: u16 = 0x0202;

/// Nikon's `ShootingMode` (0x0089) bit 0: continuous-release (vs. single-frame).
const SHOOTING_MODE_CONTINUOUS_BIT: u16 = 0x0001;

#[derive(Debug, thiserror::Error)]
pub enum NefError {
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
    #[error("no DateTimeOriginal tag found")]
    MissingCaptureTime,
    #[error("DateTimeOriginal did not match the expected \"YYYY:MM:DD HH:MM:SS\" format: {0}")]
    BadCaptureTime(String),
    #[error("I/O error reading source: {0}")]
    Io(String),
}

impl From<std::io::Error> for NefError {
    fn from(e: std::io::Error) -> Self {
        NefError::Io(e.to_string())
    }
}

/// Capture time to millisecond precision, as a calendar timestamp (no timezone -- comparisons
/// within one shoot on one camera never cross a DST/offset change, so this stays a plain
/// wall-clock value rather than pulling in a timezone-aware date/time dependency).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CaptureTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    pub millis: u16,
}

impl CaptureTime {
    /// A day ordinal (365-day years, no leap-day correction -- documented limitation, wrong only
    /// for a burst spanning a leap-year Feb 29 -> Mar 1 boundary at exactly midnight) as an exact
    /// integer, kept separate from the sub-day fractional-second part so that combining the two
    /// same-day timestamps this method actually needs to compare never routes through a single
    /// huge float (computing "seconds since year 0" directly loses millisecond precision to f64
    /// rounding at that magnitude -- caught by this module's own `gap_seconds_across_a_minute_
    /// boundary` test, which wants exact millisecond gaps, not `~1e-6`-off ones).
    fn day_ordinal(&self) -> i64 {
        const CUMULATIVE_DAYS: [i64; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
        let month_days = CUMULATIVE_DAYS[(self.month.saturating_sub(1) as usize).min(11)];
        self.year as i64 * 365 + month_days + (self.day.saturating_sub(1)) as i64
    }

    fn seconds_of_day(&self) -> f64 {
        self.hour as f64 * 3600.0
            + self.minute as f64 * 60.0
            + self.second as f64
            + self.millis as f64 / 1000.0
    }

    /// Gap in seconds between two capture times, always >= 0.
    pub fn gap_seconds(&self, other: &CaptureTime) -> f64 {
        let day_diff = (other.day_ordinal() - self.day_ordinal()) as f64 * 86400.0;
        (day_diff + other.seconds_of_day() - self.seconds_of_day()).abs()
    }
}

fn parse_capture_time(date_time: &str, subsec: Option<&str>) -> Result<CaptureTime, NefError> {
    // "YYYY:MM:DD HH:MM:SS", exactly 19 bytes, ASCII digits and separators only.
    let bytes = date_time.as_bytes();
    if bytes.len() < 19 {
        return Err(NefError::BadCaptureTime(date_time.to_string()));
    }
    let field = |r: std::ops::Range<usize>| -> Result<u32, NefError> {
        std::str::from_utf8(&bytes[r])
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .ok_or_else(|| NefError::BadCaptureTime(date_time.to_string()))
    };
    let year = field(0..4)?;
    let month = field(5..7)?;
    let day = field(8..10)?;
    let hour = field(11..13)?;
    let minute = field(14..16)?;
    let second = field(17..19)?;

    // SubSecTimeOriginal is an arbitrary-length decimal-digit string representing the fractional
    // second, e.g. "52" == 0.52s == 520ms -- confirmed against real files' own XMP sidecars
    // (`exif:DateTimeOriginal`'s millisecond value), not assumed from the EXIF spec's prose alone.
    let millis = match subsec {
        Some(s) if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) => {
            let frac: f64 = format!("0.{s}").parse().unwrap_or(0.0);
            (frac * 1000.0).round() as u16
        }
        _ => 0,
    };

    Ok(CaptureTime {
        year: year as u16,
        month: month as u8,
        day: day as u8,
        hour: hour as u8,
        minute: minute as u8,
        second: second as u8,
        millis,
    })
}

/// Nikon's `ShootingMode` continuous-release bit, decoded from the raw tag value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShootingMode {
    pub continuous: bool,
    pub raw: u16,
}

#[derive(Debug, Clone)]
pub struct EmbeddedJpeg {
    pub file_offset: u64,
    pub byte_len: u64,
}

#[derive(Debug, Clone)]
pub struct NefMeta {
    pub capture_time: CaptureTime,
    pub orientation: Option<u16>,
    pub serial: Option<String>,
    pub shutter_count: Option<u32>,
    pub shooting_mode: Option<ShootingMode>,
    /// The Nikon MakerNote PreviewIFD's embedded JPEG (T0, per ADR-0017) -- `None` if this file
    /// has no Nikon MakerNote or no PreviewIFD entry (an unsupported camera, not necessarily an
    /// error for the caller).
    pub preview: Option<EmbeddedJpeg>,
}

const MAX_IFDS_VISITED: usize = 512;

pub struct NefReader<S: ByteSource> {
    source: S,
    bo: ByteOrder,
    ifd0_off: u32,
    visited: HashSet<u64>,
    ifds_visited: usize,
    len: Option<u64>,
}

impl<S: ByteSource> NefReader<S> {
    pub fn new(mut source: S) -> Result<Self, NefError> {
        let header = source.read_at(0, 8)?;
        if header.len() < 8 {
            return Err(NefError::TooShort);
        }
        let bo = match &header[0..2] {
            b"II" => ByteOrder::Little,
            b"MM" => ByteOrder::Big,
            _ => return Err(NefError::BadByteOrder),
        };
        if bo.u16(&header[2..4]) != 42 {
            return Err(NefError::BadMagic);
        }
        let ifd0_off = bo.u32(&header[4..8]);
        let len = source.len_hint();
        Ok(NefReader {
            source,
            bo,
            ifd0_off,
            visited: HashSet::new(),
            ifds_visited: 0,
            len,
        })
    }

    pub fn read_range(&mut self, offset: u64, len: usize) -> Result<Vec<u8>, NefError> {
        Ok(self.source.read_at(offset, len)?)
    }

    fn read_ifd(&mut self, base: u64, offset: u32) -> Result<(Vec<IfdEntry>, u32), NefError> {
        let abs = base + offset as u64;
        if !self.visited.insert(abs) {
            return Err(NefError::Cycle);
        }
        self.ifds_visited += 1;
        if self.ifds_visited > MAX_IFDS_VISITED {
            return Err(NefError::TruncatedIfd);
        }
        let count_bytes = self.source.read_at(abs, 2)?;
        if count_bytes.len() < 2 {
            return Err(NefError::TruncatedIfd);
        }
        let count = self.bo.u16(&count_bytes) as usize;
        let body_len = count
            .checked_mul(12)
            .and_then(|n| n.checked_add(4))
            .ok_or(NefError::TruncatedIfd)?;
        let body = self.source.read_at(abs + 2, body_len)?;
        if body.len() < body_len {
            return Err(NefError::TruncatedIfd);
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

    fn read_ascii(&mut self, entry: &IfdEntry, base: u64) -> Result<String, NefError> {
        let len = entry.value_len() as usize;
        let bytes = if len <= 4 {
            entry.value_or_offset_raw[..len.min(4)].to_vec()
        } else {
            let off = base + entry.as_offset(self.bo) as u64;
            self.source.read_at(off, len)?
        };
        let s = String::from_utf8_lossy(&bytes);
        Ok(s.trim_end_matches('\0').to_string())
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

    /// Reads IFD0, the ExifIFD's `DateTimeOriginal`/`SubSecTimeOriginal`, and (if present) the
    /// Nikon MakerNote's serial/shooting-mode/shutter-count and PreviewIFD JPEG.
    pub fn read_meta(&mut self) -> Result<NefMeta, NefError> {
        let (ifd0, _next) = self.read_ifd(0, self.ifd0_off)?;
        let orientation = Self::find(&ifd0, TAG_ORIENTATION).map(|e| e.as_u32(self.bo) as u16);

        let mut capture_time = None;
        let mut serial = None;
        let mut shutter_count = None;
        let mut shooting_mode = None;
        let mut preview = None;

        if let Some(exif_entry) = Self::find(&ifd0, TAG_EXIF_IFD) {
            let exif_off = exif_entry.as_offset(self.bo);
            if let Ok((exif_entries, _)) = self.read_ifd(0, exif_off) {
                if let Some(dto) = Self::find(&exif_entries, TAG_DATE_TIME_ORIGINAL).cloned() {
                    let date_time = self.read_ascii(&dto, 0)?;
                    let subsec = match Self::find(&exif_entries, TAG_SUBSEC_TIME_ORIGINAL).cloned()
                    {
                        Some(e) => Some(self.read_ascii(&e, 0)?),
                        None => None,
                    };
                    capture_time = Some(parse_capture_time(&date_time, subsec.as_deref())?);
                }

                if let Some(mn) = Self::find(&exif_entries, TAG_MAKER_NOTE).cloned() {
                    if let Ok(nikon) = self.read_nikon_maker_note(&mn) {
                        serial = nikon.serial;
                        shutter_count = nikon.shutter_count;
                        shooting_mode = nikon.shooting_mode;
                        preview = nikon.preview;
                    }
                }
            }
        }

        let capture_time = capture_time.ok_or(NefError::MissingCaptureTime)?;
        Ok(NefMeta {
            capture_time,
            orientation,
            serial,
            shutter_count,
            shooting_mode,
            preview,
        })
    }

    /// Nikon MakerNote layout (same quirk `spikes/sniff/src/ifd.rs` documents): `"Nikon\0"` (6
    /// bytes), 2-byte format version, 2 reserved bytes, then a second, embedded TIFF header --
    /// every offset inside the MakerNote's own IFD tree is relative to that inner header's start,
    /// not the file's.
    fn read_nikon_maker_note(&mut self, mn_entry: &IfdEntry) -> Result<NikonMeta, NefError> {
        let mn_off = mn_entry.as_offset(self.bo) as u64;
        let mn_data = self.source.read_at(mn_off, 18)?;
        if mn_data.len() < 18 || &mn_data[0..6] != b"Nikon\0" {
            return Ok(NikonMeta::default());
        }
        let inner_header = &mn_data[10..18];
        let inner_bo = match &inner_header[0..2] {
            b"II" => ByteOrder::Little,
            b"MM" => ByteOrder::Big,
            _ => return Ok(NikonMeta::default()),
        };
        if inner_bo.u16(&inner_header[2..4]) != 42 {
            return Ok(NikonMeta::default());
        }
        let maker_base = mn_off + 10;
        let ifd_off = inner_bo.u32(&inner_header[4..8]);

        let saved_bo = self.bo;
        self.bo = inner_bo;
        let result = self.read_nikon_ifd(maker_base, ifd_off);
        self.bo = saved_bo;
        result
    }

    fn read_nikon_ifd(&mut self, maker_base: u64, ifd_off: u32) -> Result<NikonMeta, NefError> {
        let (entries, _) = match self.read_ifd(maker_base, ifd_off) {
            Ok(v) => v,
            Err(_) => return Ok(NikonMeta::default()),
        };

        let serial = match Self::find(&entries, TAG_NIKON_SERIAL_NUMBER).cloned() {
            Some(e) => Some(self.read_ascii(&e, maker_base)?),
            None => None,
        };
        let shutter_count =
            Self::find(&entries, TAG_NIKON_SHUTTER_COUNT).map(|e| e.as_u32(self.bo));
        let shooting_mode = Self::find(&entries, TAG_NIKON_SHOOTING_MODE).map(|e| {
            let raw = e.as_u32(self.bo) as u16;
            ShootingMode {
                continuous: raw & SHOOTING_MODE_CONTINUOUS_BIT != 0,
                raw,
            }
        });

        let preview = match Self::find(&entries, TAG_NIKON_PREVIEW_IFD).cloned() {
            Some(preview_entry) => {
                let preview_off = preview_entry.as_offset(self.bo);
                match self.read_ifd(maker_base, preview_off) {
                    Ok((preview_ifd, _)) => self.jpeg_pair(&preview_ifd, maker_base),
                    Err(_) => None,
                }
            }
            None => None,
        };

        Ok(NikonMeta {
            serial,
            shutter_count,
            shooting_mode,
            preview,
        })
    }

    fn jpeg_pair(&self, entries: &[IfdEntry], base: u64) -> Option<EmbeddedJpeg> {
        let off = Self::find(entries, TAG_JPEG_IF_OFFSET)?;
        let len = Self::find(entries, TAG_JPEG_IF_LENGTH)?;
        let file_offset = base + off.as_offset(self.bo) as u64;
        let byte_len = len.as_u32(self.bo) as u64;
        if !self.in_bounds(file_offset) || byte_len < 2 {
            return None;
        }
        Some(EmbeddedJpeg {
            file_offset,
            byte_len: self.clamp_len(file_offset, byte_len),
        })
    }
}

#[derive(Default)]
struct NikonMeta {
    serial: Option<String>,
    shutter_count: Option<u32>,
    shooting_mode: Option<ShootingMode>,
    preview: Option<EmbeddedJpeg>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::SliceSource;

    #[test]
    fn parses_capture_time_with_subsec() {
        let t = parse_capture_time("2025:12:27 19:48:32", Some("52")).unwrap();
        assert_eq!(
            t,
            CaptureTime {
                year: 2025,
                month: 12,
                day: 27,
                hour: 19,
                minute: 48,
                second: 32,
                millis: 520,
            }
        );
    }

    #[test]
    fn parses_capture_time_without_subsec() {
        let t = parse_capture_time("2025:12:27 19:48:32", None).unwrap();
        assert_eq!(t.millis, 0);
    }

    #[test]
    fn rejects_malformed_date_time() {
        assert!(matches!(
            parse_capture_time("not-a-date", None),
            Err(NefError::BadCaptureTime(_))
        ));
    }

    #[test]
    fn gap_seconds_across_a_minute_boundary() {
        let a = parse_capture_time("2025:12:27 19:48:59", Some("900")).unwrap();
        let b = parse_capture_time("2025:12:27 19:49:01", Some("100")).unwrap();
        let gap = a.gap_seconds(&b);
        assert!((gap - 1.2).abs() < 1e-6, "expected ~1.2s, got {gap}");
    }

    /// Builds a minimal little-endian TIFF fixture: IFD0 with Orientation + ExifIFD pointer;
    /// ExifIFD with DateTimeOriginal + SubSecTimeOriginal + a Nikon MakerNote pointer; a Nikon
    /// MakerNote with SerialNumber/ShootingMode/ShutterCount/PreviewIFD (JPEG pair).
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
        fn append_ascii(&mut self, s: &str) -> u32 {
            self.append_bytes(format!("{s}\0").as_bytes())
        }
        /// entries: (tag, type, count, value). Values needing an external buffer must already be
        /// appended and their offset passed as `value`.
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

    const TY_ASCII: u16 = 2;
    const TY_SHORT: u16 = 3;
    const TY_LONG: u16 = 4;
    const FAKE_JPEG: &[u8] = b"\xFF\xD8FAKEDATA\xFF\xD9";

    #[test]
    fn reads_full_real_shaped_fixture() {
        let mut b = FileBuilder::new();

        // Nikon MakerNote: "Nikon\0" + 2 version + 2 reserved, then an inner TIFF header whose
        // own offsets are relative to *its own* start (`maker_base`), matching the real
        // MakerNote quirk `read_nikon_maker_note`'s doc comment describes. Everything the
        // MakerNote's IFDs point at (the serial-number string, the PreviewIFD, its JPEG bytes)
        // is appended *after* `maker_base` here, in file order, so every offset is a plain
        // forward reference computed from `b.offset()` at the moment it's known -- no negative
        // offset, no need for `sniff`'s own patch-after-the-fact approach.
        let mn_off = b.offset();
        b.buf.extend_from_slice(b"Nikon\0");
        b.buf.extend_from_slice(&[0x02, 0x10, 0x00, 0x00]);
        let inner_header_off = b.offset();
        let maker_base = inner_header_off as u64;
        b.buf.extend_from_slice(&[b'I', b'I', 42, 0]);
        // The inner header's own IFD-offset field (patched below once `mn_ifd`'s real position
        // is known -- unlike sniff's own MakerNote test, this fixture doesn't hardcode "the IFD
        // is always at relative offset 8," since here the serial string/PreviewIFD/JPEG bytes
        // are written *before* `mn_ifd` itself, not after it).
        let mn_ifd_offset_field = b.offset();
        b.buf.extend_from_slice(&0u32.to_le_bytes());

        let serial_off = b.append_ascii("3037771");
        let jpeg_off = b.append_bytes(FAKE_JPEG);
        let preview_ifd_off = b.append_ifd(
            &[
                (TAG_JPEG_IF_OFFSET, TY_LONG, 1, jpeg_off - maker_base as u32),
                (TAG_JPEG_IF_LENGTH, TY_LONG, 1, FAKE_JPEG.len() as u32),
            ],
            0,
        );
        let mn_ifd_off = b.append_ifd(
            &[
                (
                    TAG_NIKON_SERIAL_NUMBER,
                    TY_ASCII,
                    8,
                    serial_off - maker_base as u32,
                ),
                (TAG_NIKON_SHOOTING_MODE, TY_SHORT, 1, 0x0001),
                (TAG_NIKON_SHUTTER_COUNT, TY_LONG, 1, 633179),
                (
                    TAG_NIKON_PREVIEW_IFD,
                    TY_LONG,
                    1,
                    preview_ifd_off - maker_base as u32,
                ),
            ],
            0,
        );
        b.patch_u32(mn_ifd_offset_field, mn_ifd_off - maker_base as u32);

        let date_time_off = b.append_ascii("2025:12:27 19:48:32");
        // "52\0", 3 bytes, fits inline per the TIFF spec (total byte length <= 4) -- stored
        // directly in the entry's own 4-byte value field, not as an external-buffer offset (real
        // Nikon files store SubSecTimeOriginal this way too, being 2-3 ASCII digits + NUL).
        let subsec_inline = u32::from_le_bytes([b'5', b'2', 0, 0]);
        let exif_ifd_off = b.append_ifd(
            &[
                (TAG_DATE_TIME_ORIGINAL, TY_ASCII, 20, date_time_off),
                (TAG_SUBSEC_TIME_ORIGINAL, TY_ASCII, 3, subsec_inline),
                (TAG_MAKER_NOTE, 7, 1, mn_off),
            ],
            0,
        );
        let ifd0_off = b.append_ifd(
            &[
                (TAG_ORIENTATION, TY_SHORT, 1, 6),
                (TAG_EXIF_IFD, TY_LONG, 1, exif_ifd_off),
            ],
            0,
        );
        let data = b.finish(ifd0_off);

        let mut reader = NefReader::new(SliceSource::new(&data)).unwrap();
        let meta = reader.read_meta().unwrap();

        assert_eq!(meta.orientation, Some(6));
        assert_eq!(meta.serial.as_deref(), Some("3037771"));
        assert_eq!(meta.shutter_count, Some(633179));
        assert!(meta.shooting_mode.unwrap().continuous);
        assert_eq!(
            meta.capture_time,
            CaptureTime {
                year: 2025,
                month: 12,
                day: 27,
                hour: 19,
                minute: 48,
                second: 32,
                millis: 520,
            }
        );
        let preview = meta.preview.expect("preview jpeg found");
        assert_eq!(preview.file_offset, jpeg_off as u64);
        assert_eq!(preview.byte_len, FAKE_JPEG.len() as u64);

        let bytes = reader
            .read_range(preview.file_offset, preview.byte_len as usize)
            .unwrap();
        assert_eq!(bytes, FAKE_JPEG);
    }

    #[test]
    fn missing_date_time_original_is_an_error() {
        let mut b = FileBuilder::new();
        let ifd0_off = b.append_ifd(&[(TAG_ORIENTATION, TY_SHORT, 1, 1)], 0);
        let data = b.finish(ifd0_off);
        let mut reader = NefReader::new(SliceSource::new(&data)).unwrap();
        assert!(matches!(
            reader.read_meta(),
            Err(NefError::MissingCaptureTime)
        ));
    }

    #[test]
    fn rejects_too_short_buffer() {
        assert!(matches!(
            NefReader::new(SliceSource::new(&[0u8; 4])),
            Err(NefError::TooShort)
        ));
    }
}
