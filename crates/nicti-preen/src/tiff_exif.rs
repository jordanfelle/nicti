//! EXIF for TIFF output (#337). A TIFF file *is* a TIFF, so unlike JPEG/PNG there is no APP1
//! segment or `eXIf` chunk to drop the EXIF block into: its IFD0 tags (Make, Model, Artist,
//! Copyright, ...) belong in the image's own IFD, and the Exif and GPS sub-IFDs hang off it
//! through tags 34665 and 34853.
//!
//! The source of truth stays `metadata::build_exif`: its payload is turned into a TIFF block
//! (`exif_tiff_bytes`, the same bytes the PNG `eXIf` chunk carries), and this module parses that
//! block (bounded, byte-order aware) into raw entries that the `tiff` encoder writes back out, so a
//! TIFF carries exactly the EXIF a JPEG does. Entries the image directory owns (size, strips,
//! compression, resolution, ICC, XMP) are never copied, and the Interoperability and MakerNote
//! sub-structures are left out.

use std::borrow::Cow;

use tiff::tags::Type;

/// Tags that describe the image's own pixels or that the TIFF writer sets itself; copying the
/// EXIF block's own values for these would duplicate or contradict them.
const IMAGE_OWNED: &[u16] = &[
    254, 255, 256, 257, 258, 259, 262, 266, 273, 277, 278, 279, 282, 283, 284, 296, 317, 320, 338,
    339, 700, 34665, 34675, 34853, 40965,
];
/// Exif-IFD tags a TIFF must not carry (exiftool `-validate`: "not allowed in TIFF"): the pixel
/// size and component layout live in the image directory, not the Exif one.
const EXIF_NOT_IN_TIFF: &[u16] = &[0x9101, 0xA002, 0xA003];
/// `FlashpixVersion` (UNDEFINED x4, "0100"): required in a TIFF's Exif IFD.
const FLASHPIX_VERSION: u16 = 0xA000;
const EXIF_IFD_POINTER: u16 = 34665;
const GPS_IFD_POINTER: u16 = 34853;
/// More than this many entries in one IFD is not a real EXIF block.
const MAX_ENTRIES: usize = 512;
/// A single value larger than this is dropped (a thumbnail or a maker note, never a camera tag).
const MAX_VALUE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub tag: u16,
    /// TIFF field type code (1 BYTE .. 12 DOUBLE).
    pub ty: u16,
    pub count: u32,
    /// The value in the *platform's native* byte order, as the `tiff` encoder expects.
    pub data: Vec<u8>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ExifEntries {
    /// Entries for the image's own IFD (never including the sub-IFD pointers).
    pub ifd0: Vec<Entry>,
    pub exif: Vec<Entry>,
    pub gps: Vec<Entry>,
}

/// Bytes per element, and the width to byte-swap in, for a TIFF field type. `None` for a type this
/// writer does not carry (IFD, 64-bit and the like).
fn type_info(ty: u16) -> Option<(usize, usize)> {
    Some(match ty {
        1 | 2 | 6 | 7 => (1, 1),
        3 | 8 => (2, 2),
        4 | 9 | 11 => (4, 4),
        5 | 10 => (8, 4),
        12 => (8, 8),
        _ => return None,
    })
}

struct Reader<'a> {
    b: &'a [u8],
    le: bool,
}

impl Reader<'_> {
    fn u16(&self, at: usize) -> Option<u16> {
        let s: [u8; 2] = self.b.get(at..at.checked_add(2)?)?.try_into().ok()?;
        Some(if self.le {
            u16::from_le_bytes(s)
        } else {
            u16::from_be_bytes(s)
        })
    }

    fn u32(&self, at: usize) -> Option<u32> {
        let s: [u8; 4] = self.b.get(at..at.checked_add(4)?)?.try_into().ok()?;
        Some(if self.le {
            u32::from_le_bytes(s)
        } else {
            u32::from_be_bytes(s)
        })
    }

    /// Reads one IFD: its entries, with values converted to native byte order. Malformed entries
    /// are skipped, not fatal.
    fn ifd(&self, offset: usize) -> Vec<Entry> {
        let Some(n) = self.u16(offset) else {
            return Vec::new();
        };
        let n = usize::from(n).min(MAX_ENTRIES);
        let mut out = Vec::new();
        for i in 0..n {
            let at = offset + 2 + i * 12;
            let (Some(tag), Some(ty), Some(count)) =
                (self.u16(at), self.u16(at + 2), self.u32(at + 4))
            else {
                break;
            };
            let Some((size, swap)) = type_info(ty) else {
                continue;
            };
            let Some(len) = (count as usize).checked_mul(size) else {
                continue;
            };
            if count == 0 || len > MAX_VALUE_BYTES {
                continue;
            }
            let raw = if len <= 4 {
                self.b.get(at + 8..at + 8 + len)
            } else {
                match self.u32(at + 8) {
                    Some(off) => (off as usize)
                        .checked_add(len)
                        .and_then(|end| self.b.get(off as usize..end)),
                    None => None,
                }
            };
            let Some(raw) = raw else { continue };
            let mut data = raw.to_vec();
            if self.le != cfg!(target_endian = "little") && swap > 1 {
                for chunk in data.chunks_exact_mut(swap) {
                    chunk.reverse();
                }
            }
            out.push(Entry {
                tag,
                ty,
                count,
                data,
            });
        }
        out
    }
}

/// Parses an EXIF TIFF block (`II*\0` or `MM\0*`, as in a PNG `eXIf` chunk) into the entries a TIFF
/// file needs. A block that is not TIFF-shaped yields no entries.
pub fn parse_exif_block(block: &[u8]) -> ExifEntries {
    let le = match block.get(..4) {
        Some(b"II*\0") => true,
        Some(b"MM\0*") => false,
        _ => return ExifEntries::default(),
    };
    let r = Reader { b: block, le };
    let Some(ifd0_at) = r.u32(4) else {
        return ExifEntries::default();
    };
    let ifd0_all = r.ifd(ifd0_at as usize);
    let pointer = |tag: u16| {
        ifd0_all
            .iter()
            .find(|e| e.tag == tag && e.ty == 4 && e.count == 1)
            .and_then(|e| e.data.as_slice().try_into().ok())
            .map(u32::from_ne_bytes)
    };
    let sub = |tag: u16| {
        pointer(tag)
            .map(|off| r.ifd(off as usize))
            .unwrap_or_default()
    };
    let mut ifd0: Vec<Entry> = ifd0_all
        .iter()
        .filter(|e| !IMAGE_OWNED.contains(&e.tag))
        .cloned()
        .collect();
    dedupe(&mut ifd0);
    let mut exif = sub(EXIF_IFD_POINTER);
    exif.retain(|e| {
        e.tag != EXIF_IFD_POINTER
            && e.tag != GPS_IFD_POINTER
            && e.tag != 40965
            && !EXIF_NOT_IN_TIFF.contains(&e.tag)
    });
    if !exif.is_empty() && !exif.iter().any(|e| e.tag == FLASHPIX_VERSION) {
        exif.push(Entry {
            tag: FLASHPIX_VERSION,
            ty: 7,
            count: 4,
            data: b"0100".to_vec(),
        });
    }
    dedupe(&mut exif);
    let mut gps = sub(GPS_IFD_POINTER);
    dedupe(&mut gps);
    ExifEntries { ifd0, exif, gps }
}

/// A TIFF directory is sorted by tag with no repeats; keep the first of any duplicate.
fn dedupe(entries: &mut Vec<Entry>) {
    entries.sort_by_key(|e| e.tag);
    entries.dedup_by_key(|e| e.tag);
}

/// An entry as a `tiff` encoder value. The field type is an associated const there, so one
/// instantiation per type code.
pub struct Raw<'a, const TY: u16> {
    pub count: usize,
    pub data: &'a [u8],
}

const fn field_type(ty: u16) -> Type {
    match ty {
        1 => Type::BYTE,
        2 => Type::ASCII,
        3 => Type::SHORT,
        4 => Type::LONG,
        5 => Type::RATIONAL,
        6 => Type::SBYTE,
        7 => Type::UNDEFINED,
        8 => Type::SSHORT,
        9 => Type::SLONG,
        10 => Type::SRATIONAL,
        11 => Type::FLOAT,
        _ => Type::DOUBLE,
    }
}

const fn elem_len(ty: u16) -> u8 {
    match ty {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 => 4,
        _ => 8,
    }
}

impl<const TY: u16> tiff::encoder::TiffValue for Raw<'_, TY> {
    const BYTE_LEN: u8 = elem_len(TY);
    const FIELD_TYPE: Type = field_type(TY);

    fn count(&self) -> usize {
        self.count
    }

    fn data(&self) -> Cow<'_, [u8]> {
        // An out-of-line value must start on a word boundary (TIFF 6.0): pad an odd length with a
        // zero byte, which the entry's count does not include. Values of 4 bytes or fewer are
        // inline and need no pad.
        if self.data.len() > 4 && self.data.len() % 2 == 1 {
            let mut padded = self.data.to_vec();
            padded.push(0);
            Cow::Owned(padded)
        } else {
            Cow::Borrowed(self.data)
        }
    }
}

/// Writes `e` into `dir` as its own type.
pub fn write_entry<W, K>(
    dir: &mut tiff::encoder::DirectoryEncoder<'_, W, K>,
    e: &Entry,
) -> tiff::TiffResult<()>
where
    W: std::io::Write + std::io::Seek,
    K: tiff::encoder::TiffKind,
{
    let tag = tiff::tags::Tag::Unknown(e.tag);
    let (count, data) = (e.count as usize, e.data.as_slice());
    macro_rules! go {
        ($($ty:literal),*) => {
            match e.ty {
                $($ty => dir.write_tag(tag, Raw::<$ty> { count, data }),)*
                _ => Ok(()),
            }
        };
    }
    go!(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny EXIF block: IFD0 {Make (ASCII, out of line), Orientation, ExifIFD ptr},
    /// Exif IFD {ISO (SHORT inline), FNumber (RATIONAL)}.
    fn block(le: bool) -> Vec<u8> {
        let w16 = |v: u16| if le { v.to_le_bytes() } else { v.to_be_bytes() };
        let w32 = |v: u32| if le { v.to_le_bytes() } else { v.to_be_bytes() };
        let mut b = Vec::new();
        b.extend_from_slice(if le { b"II*\0" } else { b"MM\0*" });
        b.extend_from_slice(&w32(8));
        // IFD0 at 8: 3 entries = 2 + 36 + 4 = 42 bytes -> ends at 50.
        b.extend_from_slice(&w16(3));
        let entry = |tag: u16, ty: u16, count: u32, val: [u8; 4]| {
            let mut e = Vec::new();
            e.extend_from_slice(&w16(tag));
            e.extend_from_slice(&w16(ty));
            e.extend_from_slice(&w32(count));
            e.extend_from_slice(&val);
            e
        };
        b.extend(entry(271, 2, 6, w32(50))); // Make "Nikon\0" at 50
        b.extend(entry(274, 3, 1, [w16(1)[0], w16(1)[1], 0, 0])); // Orientation
        b.extend(entry(34665, 4, 1, w32(56))); // ExifIFD at 56
        b.extend_from_slice(&w32(0));
        assert_eq!(b.len(), 50);
        b.extend_from_slice(b"Nikon\0");
        // Exif IFD at 56: 2 entries = 2 + 24 + 4 = 30 -> ends at 86; FNumber data at 86.
        b.extend_from_slice(&w16(2));
        b.extend(entry(33434, 5, 1, w32(86))); // ExposureTime? (rational, out of line)
        b.extend(entry(34855, 3, 1, [w16(400)[0], w16(400)[1], 0, 0])); // ISO 400
        b.extend_from_slice(&w32(0));
        assert_eq!(b.len(), 86);
        b.extend_from_slice(&w32(1));
        b.extend_from_slice(&w32(250));
        b
    }

    #[test]
    fn parses_either_byte_order_to_native_values() {
        for le in [true, false] {
            let e = parse_exif_block(&block(le));
            let tags: Vec<u16> = e.ifd0.iter().map(|x| x.tag).collect();
            assert_eq!(
                tags,
                [271, 274],
                "le={le}: Orientation is carried, the pointer is not"
            );
            assert_eq!(e.ifd0[0].data, b"Nikon\0");
            let iso = e.exif.iter().find(|x| x.tag == 34855).unwrap();
            assert_eq!(u16::from_ne_bytes(iso.data[..2].try_into().unwrap()), 400);
            let exp = e.exif.iter().find(|x| x.tag == 33434).unwrap();
            assert_eq!(u32::from_ne_bytes(exp.data[..4].try_into().unwrap()), 1);
            assert_eq!(u32::from_ne_bytes(exp.data[4..8].try_into().unwrap()), 250);
            assert!(e.gps.is_empty());
        }
    }

    #[test]
    fn garbage_and_truncation_yield_nothing_not_a_panic() {
        assert_eq!(parse_exif_block(b""), ExifEntries::default());
        assert_eq!(parse_exif_block(b"JUNKJUNKJUNK"), ExifEntries::default());
        let full = block(true);
        for cut in 0..full.len() {
            let _ = parse_exif_block(&full[..cut]);
        }
        // An IFD pointing far outside the block, and a huge count, are skipped.
        let mut bad = full.clone();
        bad[4..8].copy_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
        assert_eq!(parse_exif_block(&bad), ExifEntries::default());
        let mut huge = full;
        huge[18..22].copy_from_slice(&u32::MAX.to_le_bytes());
        let e = parse_exif_block(&huge);
        assert!(e.ifd0.iter().all(|x| x.tag != 271));
    }

    #[test]
    fn image_owned_tags_are_never_copied() {
        let mut b = block(true);
        // Turn Make's tag into ImageWidth (256).
        b[10..12].copy_from_slice(&256u16.to_le_bytes());
        assert!(parse_exif_block(&b)
            .ifd0
            .iter()
            .all(|e| e.tag != 256 && e.tag != 271));
    }

    #[test]
    fn a_tiff_exif_ifd_drops_tags_a_tiff_forbids_and_gains_flashpix_version() {
        let mut b = block(true);
        // Rewrite the Exif IFD's ISO entry (at 58 + 12) as ExifImageWidth (0xA002).
        b[70..72].copy_from_slice(&0xA002u16.to_le_bytes());
        let e = parse_exif_block(&b);
        let tags: Vec<u16> = e.exif.iter().map(|x| x.tag).collect();
        assert_eq!(tags, [33434, 0xA000], "{tags:?}");
        assert_eq!(e.exif[1].data, b"0100");
    }

    #[test]
    fn odd_out_of_line_values_are_padded_to_a_word_boundary_and_inline_ones_are_not() {
        use tiff::encoder::TiffValue;
        let odd = Raw::<2> {
            count: 5,
            data: b"abcde",
        };
        assert_eq!(odd.data().len(), 6);
        assert_eq!(odd.count(), 5);
        let inline = Raw::<2> {
            count: 3,
            data: b"abc",
        };
        assert_eq!(inline.data().len(), 3);
        let even = Raw::<2> {
            count: 6,
            data: b"abcdef",
        };
        assert_eq!(even.data().len(), 6);
    }
}
