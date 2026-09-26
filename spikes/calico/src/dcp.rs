//! Adobe `.dcp` camera-profile parser. A DCP is a TIFF-structured file (the same IFD container
//! DNG itself uses) whose IFD0 carries the DNG-spec "Camera Profile" private tags -- this reads
//! that container generically (a minimal single-IFD TIFF reader, not a full TIFF library: DCPs
//! never have sub-IFDs or strips/tiles) and pulls out the tags calico's `pipeline.rs` needs.
//!
//! Tag IDs and semantics are from the DNG specification (Adobe DNG Specification 1.6.0.0,
//! section 6, "Camera Profile Tags"). ADR-0021 records the never-bundle-real-DCPs licensing
//! stance (ADR-0003) this parser exists under -- it reads DCPs already installed on the user's
//! own machine, never a file this repo ships.

use std::collections::HashMap;
use std::io::{Cursor, Read, Seek, SeekFrom};

use byteorder::{BigEndian, ByteOrder, LittleEndian, ReadBytesExt};
use thiserror::Error;

use crate::huesatmap::HueSatMap;
use crate::matrix::Mat3;

#[derive(Debug, Error)]
pub enum DcpError {
    #[error("not a TIFF-structured file (bad byte-order marker or magic number)")]
    NotTiff,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("required tag {0:#06x} ({1}) missing")]
    MissingTag(u16, &'static str),
    #[error("tag {0:#06x}: expected {1} values, found {2}")]
    WrongCount(u16, usize, usize),
}

// DNG spec Camera Profile tag IDs.
const TAG_PROFILE_NAME: u16 = 50936;
const TAG_CALIBRATION_ILLUMINANT1: u16 = 50778;
const TAG_CALIBRATION_ILLUMINANT2: u16 = 50779;
const TAG_COLOR_MATRIX1: u16 = 50721;
const TAG_COLOR_MATRIX2: u16 = 50722;
const TAG_FORWARD_MATRIX1: u16 = 50964;
const TAG_FORWARD_MATRIX2: u16 = 50965;
const TAG_PROFILE_HUE_SAT_MAP_DIMS: u16 = 50937;
const TAG_PROFILE_HUE_SAT_MAP_DATA1: u16 = 50938;
const TAG_PROFILE_HUE_SAT_MAP_DATA2: u16 = 50939;
const TAG_PROFILE_LOOK_TABLE_DIMS: u16 = 51958;
const TAG_PROFILE_LOOK_TABLE_DATA: u16 = 51959;
const TAG_PROFILE_TONE_CURVE: u16 = 50940;
const TAG_BASELINE_EXPOSURE_OFFSET: u16 = 51109;

/// DNG's `LightSource` enum values relevant here (spec section 6.3.7's calibration illuminants).
/// Not exhaustive -- only the values real DCPs commonly use.
pub fn light_source_to_cct(value: u16) -> f64 {
    match value {
        17 => 2856.0, // Standard Light A
        18 => 4874.0, // Standard Light B
        19 => 6774.0, // Standard Light C
        20 => 6504.0, // D65
        21 => 6500.0, // Daylight
        23 => 5500.0, // Fine Weather / D55
        // A raw Kelvin value packed directly is rare in practice for DCPs (they use the named
        // enum), so an unrecognized value defaults to D65 rather than panicking -- a wrong
        // default here shows up immediately in `calico compare`'s numbers, not silently.
        other => {
            eprintln!("warning: unrecognized CalibrationIlluminant value {other}, assuming D65");
            6504.0
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TagType {
    Byte,
    Ascii,
    Short,
    Long,
    Rational,
    SByte,
    Undefined,
    SShort,
    SLong,
    SRational,
    Float,
    Double,
}

impl TagType {
    fn from_u16(v: u16) -> Option<Self> {
        Some(match v {
            1 => TagType::Byte,
            2 => TagType::Ascii,
            3 => TagType::Short,
            4 => TagType::Long,
            5 => TagType::Rational,
            6 => TagType::SByte,
            7 => TagType::Undefined,
            8 => TagType::SShort,
            9 => TagType::SLong,
            10 => TagType::SRational,
            11 => TagType::Float,
            12 => TagType::Double,
            _ => return None,
        })
    }

    fn size(self) -> usize {
        match self {
            TagType::Byte | TagType::Ascii | TagType::SByte | TagType::Undefined => 1,
            TagType::Short | TagType::SShort => 2,
            TagType::Long | TagType::SLong | TagType::Float => 4,
            TagType::Rational | TagType::SRational | TagType::Double => 8,
        }
    }
}

#[derive(Debug, Clone)]
enum TagValue {
    Ascii(String),
    Longs(Vec<u32>),
    SRationals(Vec<f64>),
    Floats(Vec<f32>),
}

/// Reads every IFD0 entry into a tag-id -> value map, resolving external values via the
/// value/offset field per the TIFF6 rule (values <= 4 bytes are stored inline; longer values are
/// read from `offset`).
fn read_ifd(data: &[u8]) -> Result<HashMap<u16, TagValue>, DcpError> {
    if data.len() < 8 {
        return Err(DcpError::NotTiff);
    }
    let little_endian = match &data[0..2] {
        b"II" => true,
        b"MM" => false,
        _ => return Err(DcpError::NotTiff),
    };
    let magic = if little_endian {
        LittleEndian::read_u16(&data[2..4])
    } else {
        BigEndian::read_u16(&data[2..4])
    };
    if magic != 42 {
        return Err(DcpError::NotTiff);
    }
    let ifd0_offset = if little_endian {
        LittleEndian::read_u32(&data[4..8])
    } else {
        BigEndian::read_u32(&data[4..8])
    } as usize;

    let mut cursor = Cursor::new(data);
    cursor.seek(SeekFrom::Start(ifd0_offset as u64))?;
    let entry_count = read_u16(&mut cursor, little_endian)?;

    let mut tags = HashMap::new();
    for _ in 0..entry_count {
        let tag = read_u16(&mut cursor, little_endian)?;
        let type_raw = read_u16(&mut cursor, little_endian)?;
        let count = read_u32(&mut cursor, little_endian)? as usize;
        let value_offset_pos = cursor.position();
        let Some(ty) = TagType::from_u16(type_raw) else {
            cursor.seek(SeekFrom::Start(value_offset_pos + 4))?;
            continue;
        };

        let total_bytes = ty.size() * count;
        let value_bytes = if total_bytes <= 4 {
            let mut buf = [0u8; 4];
            cursor.read_exact(&mut buf)?;
            buf[..total_bytes].to_vec()
        } else {
            let offset = read_u32(&mut cursor, little_endian)?;
            let start = offset as usize;
            let end = start + total_bytes;
            if end > data.len() {
                cursor.seek(SeekFrom::Start(value_offset_pos + 4))?;
                continue;
            }
            data[start..end].to_vec()
        };
        cursor.seek(SeekFrom::Start(value_offset_pos + 4))?;

        if let Some(value) = decode_value(ty, count, &value_bytes, little_endian) {
            tags.insert(tag, value);
        }
    }
    Ok(tags)
}

fn decode_value(ty: TagType, count: usize, bytes: &[u8], le: bool) -> Option<TagValue> {
    match ty {
        TagType::Ascii => {
            let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
            Some(TagValue::Ascii(
                String::from_utf8_lossy(&bytes[..end]).into_owned(),
            ))
        }
        TagType::Long => Some(TagValue::Longs(
            (0..count)
                .map(|i| read_u32_slice(&bytes[i * 4..i * 4 + 4], le))
                .collect(),
        )),
        TagType::Short => Some(TagValue::Longs(
            (0..count)
                .map(|i| read_u16_slice(&bytes[i * 2..i * 2 + 2], le) as u32)
                .collect(),
        )),
        TagType::SRational => Some(TagValue::SRationals(
            (0..count)
                .map(|i| {
                    let off = i * 8;
                    let num = read_i32_slice(&bytes[off..off + 4], le) as f64;
                    let den = read_i32_slice(&bytes[off + 4..off + 8], le) as f64;
                    if den == 0.0 {
                        0.0
                    } else {
                        num / den
                    }
                })
                .collect(),
        )),
        TagType::Float => Some(TagValue::Floats(
            (0..count)
                .map(|i| read_f32_slice(&bytes[i * 4..i * 4 + 4], le))
                .collect(),
        )),
        _ => None,
    }
}

fn read_u16(cursor: &mut Cursor<&[u8]>, le: bool) -> std::io::Result<u16> {
    if le {
        cursor.read_u16::<LittleEndian>()
    } else {
        cursor.read_u16::<BigEndian>()
    }
}
fn read_u32(cursor: &mut Cursor<&[u8]>, le: bool) -> std::io::Result<u32> {
    if le {
        cursor.read_u32::<LittleEndian>()
    } else {
        cursor.read_u32::<BigEndian>()
    }
}
fn read_u16_slice(b: &[u8], le: bool) -> u16 {
    if le {
        LittleEndian::read_u16(b)
    } else {
        BigEndian::read_u16(b)
    }
}
fn read_u32_slice(b: &[u8], le: bool) -> u32 {
    if le {
        LittleEndian::read_u32(b)
    } else {
        BigEndian::read_u32(b)
    }
}
fn read_i32_slice(b: &[u8], le: bool) -> i32 {
    if le {
        LittleEndian::read_i32(b)
    } else {
        BigEndian::read_i32(b)
    }
}
fn read_f32_slice(b: &[u8], le: bool) -> f32 {
    if le {
        LittleEndian::read_f32(b)
    } else {
        BigEndian::read_f32(b)
    }
}

#[derive(Debug)]
pub struct DcpProfile {
    pub name: String,
    pub illuminant1_cct: f64,
    pub illuminant2_cct: f64,
    pub color_matrix1: Mat3,
    pub color_matrix2: Mat3,
    pub forward_matrix1: Option<Mat3>,
    pub forward_matrix2: Option<Mat3>,
    pub hue_sat_map1: Option<HueSatMap>,
    pub hue_sat_map2: Option<HueSatMap>,
    pub look_table: Option<HueSatMap>,
    /// `(x, y)` control points in [0,1], or `None` if the profile has no `ProfileToneCurve`
    /// (callers should fall back to `ToneCurve::acr_default()`).
    pub tone_curve_points: Option<Vec<(f64, f64)>>,
    /// EV offset applied before the tone curve, if the profile specifies one.
    pub baseline_exposure_offset: f64,
}

impl DcpProfile {
    pub fn parse(data: &[u8]) -> Result<Self, DcpError> {
        let tags = read_ifd(data)?;

        let matrix3 = |tag: u16| -> Result<Mat3, DcpError> {
            let TagValue::SRationals(values) = tags
                .get(&tag)
                .ok_or(DcpError::MissingTag(tag, "ColorMatrix"))?
            else {
                return Err(DcpError::MissingTag(tag, "ColorMatrix (wrong type)"));
            };
            if values.len() != 9 {
                return Err(DcpError::WrongCount(tag, 9, values.len()));
            }
            Ok([
                [values[0], values[1], values[2]],
                [values[3], values[4], values[5]],
                [values[6], values[7], values[8]],
            ])
        };
        let optional_matrix3 = |tag: u16| -> Option<Mat3> {
            let TagValue::SRationals(values) = tags.get(&tag)? else {
                return None;
            };
            if values.len() != 9 {
                return None;
            }
            Some([
                [values[0], values[1], values[2]],
                [values[3], values[4], values[5]],
                [values[6], values[7], values[8]],
            ])
        };

        let illuminant1 = match tags.get(&TAG_CALIBRATION_ILLUMINANT1) {
            Some(TagValue::Longs(v)) if !v.is_empty() => light_source_to_cct(v[0] as u16),
            _ => 2856.0, // DNG spec default when absent: Standard Light A.
        };
        let illuminant2 = match tags.get(&TAG_CALIBRATION_ILLUMINANT2) {
            Some(TagValue::Longs(v)) if !v.is_empty() => light_source_to_cct(v[0] as u16),
            _ => 6504.0,
        };

        let name = match tags.get(&TAG_PROFILE_NAME) {
            Some(TagValue::Ascii(s)) => s.clone(),
            _ => String::from("(unnamed)"),
        };

        // UNVERIFIED against a real DCP, per ADR-0021/ADR-0003: this assumes the DNG spec's
        // ProfileHueSatMapData/ProfileLookTableData tables are stored hue-major/sat-mid/val-minor
        // (hue slowest-varying), matching `HueSatMap::index`'s `(v*sat+s)*hue+h` layout -- read
        // from memory, not cross-checked against a real file, since none can exist in this
        // sandbox. If it's actually the reverse nesting, every entry gets silently assigned to
        // the wrong (hue, sat, val) grid point without any parse error to catch it. The
        // reference-machine pass (docs/research/calico-color-pipeline.md) is the first point
        // this can be checked for real -- verify with a known, distinctive real profile (e.g. one
        // whose behavior at a specific hue/sat is visually obvious) before trusting this ADR's
        // measured ΔE numbers as reflecting THIS table's stage rather than a mis-indexed one.
        let hue_sat_map = |dims_tag: u16, data_tag: u16| -> Option<HueSatMap> {
            let TagValue::Longs(dims) = tags.get(&dims_tag)? else {
                return None;
            };
            if dims.len() != 3 {
                return None;
            }
            let (hue_div, sat_div, val_div) =
                (dims[0] as usize, dims[1] as usize, dims[2] as usize);
            let TagValue::Floats(values) = tags.get(&data_tag)? else {
                return None;
            };
            let expected = hue_div * sat_div * val_div.max(1) * 3;
            if values.len() != expected {
                return None;
            }
            #[allow(clippy::chunks_exact_to_as_chunks)] // `as_chunks` needs a `const N` generic
            let data = values.chunks_exact(3).map(|c| [c[0], c[1], c[2]]).collect();
            Some(HueSatMap {
                hue_divisions: hue_div,
                sat_divisions: sat_div,
                val_divisions: val_div.max(1),
                data,
            })
        };

        let tone_curve_points = match tags.get(&TAG_PROFILE_TONE_CURVE) {
            Some(TagValue::Floats(values)) if values.len() >= 4 && values.len() % 2 == 0 => {
                #[allow(clippy::chunks_exact_to_as_chunks)]
                let points = values
                    .chunks_exact(2)
                    .map(|c| (c[0] as f64, c[1] as f64))
                    .collect();
                Some(points)
            }
            _ => None,
        };

        let baseline_exposure_offset = match tags.get(&TAG_BASELINE_EXPOSURE_OFFSET) {
            Some(TagValue::SRationals(v)) if !v.is_empty() => v[0],
            _ => 0.0,
        };

        // Single-illuminant profiles (ColorMatrix1 only, no ColorMatrix2) fall back to using
        // ColorMatrix1 for both -- interpolation between two identical matrices is that same
        // matrix regardless of the CCT-derived weight, so this degrades gracefully rather than
        // needing a separate "single illuminant" code path. Computed once into a local so this
        // fallback can't silently break if the struct literal's field order below is ever
        // reordered (an `unwrap_or_else` calling `matrix3(TAG_COLOR_MATRIX1)` a second time would
        // rely on struct-literal fields evaluating in source order, an easy-to-break invariant).
        let color_matrix1 = matrix3(TAG_COLOR_MATRIX1)?;
        let color_matrix2 = matrix3(TAG_COLOR_MATRIX2).unwrap_or(color_matrix1);

        Ok(DcpProfile {
            name,
            illuminant1_cct: illuminant1,
            illuminant2_cct: illuminant2,
            color_matrix1,
            color_matrix2,
            forward_matrix1: optional_matrix3(TAG_FORWARD_MATRIX1),
            forward_matrix2: optional_matrix3(TAG_FORWARD_MATRIX2),
            hue_sat_map1: hue_sat_map(TAG_PROFILE_HUE_SAT_MAP_DIMS, TAG_PROFILE_HUE_SAT_MAP_DATA1),
            hue_sat_map2: hue_sat_map(TAG_PROFILE_HUE_SAT_MAP_DIMS, TAG_PROFILE_HUE_SAT_MAP_DATA2),
            look_table: hue_sat_map(TAG_PROFILE_LOOK_TABLE_DIMS, TAG_PROFILE_LOOK_TABLE_DATA),
            tone_curve_points,
            baseline_exposure_offset,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-builds a minimal synthetic DCP (IFD0 with ColorMatrix1/2 + CalibrationIlluminant1/2 +
    /// ProfileName only) byte-for-byte, little-endian, to exercise the parser without ever
    /// touching a real Adobe profile file.
    fn build_synthetic_dcp() -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"II");
        buf.extend_from_slice(&42u16.to_le_bytes());
        buf.extend_from_slice(&8u32.to_le_bytes()); // IFD0 at offset 8

        // 3 entries: CalibrationIlluminant1 (SHORT), ColorMatrix1 (SRATIONAL x9), ProfileName (ASCII).
        let entry_count: u16 = 3;
        let header_len = 8;
        let ifd_len = 2 + entry_count as usize * 12 + 4;
        let mut external = Vec::new();

        buf.extend_from_slice(&entry_count.to_le_bytes());

        // CalibrationIlluminant1 = 17 (Standard Light A), SHORT, count 1 -> fits inline.
        buf.extend_from_slice(&TAG_CALIBRATION_ILLUMINANT1.to_le_bytes());
        buf.extend_from_slice(&3u16.to_le_bytes()); // SHORT
        buf.extend_from_slice(&1u32.to_le_bytes());
        buf.extend_from_slice(&17u16.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes()); // pad to 4 bytes

        // ColorMatrix1: identity, 9 SRATIONALs -> external.
        let matrix_offset = header_len + ifd_len + external.len();
        for (num, den) in [
            (1i32, 1i32),
            (0, 1),
            (0, 1),
            (0, 1),
            (1, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (1, 1),
        ] {
            external.extend_from_slice(&num.to_le_bytes());
            external.extend_from_slice(&den.to_le_bytes());
        }
        buf.extend_from_slice(&TAG_COLOR_MATRIX1.to_le_bytes());
        buf.extend_from_slice(&10u16.to_le_bytes()); // SRATIONAL
        buf.extend_from_slice(&9u32.to_le_bytes());
        buf.extend_from_slice(&(matrix_offset as u32).to_le_bytes());

        // ProfileName: ASCII "Test Profile\0".
        let name = b"Test Profile\0";
        let name_offset = header_len + ifd_len + external.len();
        external.extend_from_slice(name);
        buf.extend_from_slice(&TAG_PROFILE_NAME.to_le_bytes());
        buf.extend_from_slice(&2u16.to_le_bytes()); // ASCII
        buf.extend_from_slice(&(name.len() as u32).to_le_bytes());
        buf.extend_from_slice(&(name_offset as u32).to_le_bytes());

        buf.extend_from_slice(&0u32.to_le_bytes()); // next IFD offset = 0
        buf.extend_from_slice(&external);
        buf
    }

    #[test]
    fn parses_synthetic_dcp() {
        let bytes = build_synthetic_dcp();
        let profile = DcpProfile::parse(&bytes).unwrap();
        assert_eq!(profile.name, "Test Profile");
        assert_eq!(profile.illuminant1_cct, 2856.0);
        #[allow(clippy::needless_range_loop)]
        for i in 0..3 {
            for j in 0..3 {
                let expected = if i == j { 1.0 } else { 0.0 };
                assert!((profile.color_matrix1[i][j] - expected).abs() < 1e-9);
            }
        }
    }

    #[test]
    fn rejects_non_tiff_data() {
        let err = DcpProfile::parse(b"not a tiff file at all").unwrap_err();
        assert!(matches!(err, DcpError::NotTiff));
    }

    #[test]
    fn light_source_lookup_known_values() {
        assert_eq!(light_source_to_cct(17), 2856.0);
        assert_eq!(light_source_to_cct(20), 6504.0);
    }
}
