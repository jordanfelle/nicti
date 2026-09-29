//! Adobe `.dcp` camera-profile parser. A DCP is a TIFF-structured file (the same IFD container
//! DNG itself uses) whose IFD0 carries the DNG-spec "Camera Profile" private tags -- this reads
//! that container generically (a minimal single-IFD TIFF reader, not a full TIFF library: DCPs
//! never have sub-IFDs or strips/tiles) and pulls out the tags calico's `pipeline.rs` needs.
//!
//! Tag IDs and semantics are from the DNG specification (Adobe DNG Specification 1.6.0.0,
//! section 6, "Camera Profile Tags"). ADR-0038 records the never-bundle-real-DCPs licensing
//! stance (ADR-0018) this parser exists under -- it reads DCPs already installed on the user's
//! own machine, never a file this repo ships.

use std::collections::HashMap;
use std::io::{Cursor, Read, Seek, SeekFrom};

use byteorder::{BigEndian, ByteOrder, LittleEndian, ReadBytesExt};
use thiserror::Error;

use crate::huesatmap::HueSatMap;
use crate::math::Mat3;

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
    #[error("malformed profile: {0}")]
    Malformed(&'static str),
}

// DNG spec Camera Profile tag IDs.
/// Every tag [`DcpProfile::parse`] reads. `read_ifd` decodes only these: a hostile file can list
/// tens of thousands of entries, and decoding each unknown one would copy its value bytes for
/// nothing.
const KNOWN_TAGS: [u16; 17] = [
    50708, 50721, 50722, 50778, 50779, 50936, 50937, 50938, 50939, 50940, 50964, 50965, 50981,
    50982, 51107, 51108, 51109,
];

/// Largest axis / total cell count accepted for a HueSatMap or LookTable. Real tables are at most
/// 90x30x1 or 90x16x16; the bound keeps the 3D texture within every adapter's limit (2048) and
/// the upload small.
const MAX_TABLE_AXIS: usize = 256;
const MAX_TABLE_CELLS: usize = 1 << 20;

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
// Real files carry 50981/50982 (ProfileLookTableDims/Data); the spike's 51958/51959 were wrong
// and silently found no LookTable.
const TAG_PROFILE_LOOK_TABLE_DIMS: u16 = 50981;
const TAG_PROFILE_LOOK_TABLE_DATA: u16 = 50982;
const TAG_UNIQUE_CAMERA_MODEL: u16 = 50708;
const TAG_PROFILE_TONE_CURVE: u16 = 50940;
const TAG_BASELINE_EXPOSURE_OFFSET: u16 = 51109;
const TAG_PROFILE_HUE_SAT_MAP_ENCODING: u16 = 51107;
const TAG_PROFILE_LOOK_TABLE_ENCODING: u16 = 51108;

/// `ProfileHueSatMapEncoding`/`ProfileLookTableEncoding` (DNG 1.4+, spec section 6.3.7): which
/// representation a HueSatMap/LookTable's HSV coordinates are defined in. A missing tag means
/// `Linear` (the spec's default) -- there is no "gamma 1.8" encoding in the spec at all, contrary
/// to an earlier draft of this parser that assumed one unconditionally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableEncoding {
    Linear,
    Srgb,
}

fn table_encoding(tags: &HashMap<u16, TagValue>, tag: u16) -> TableEncoding {
    match tags.get(&tag) {
        Some(TagValue::Longs(v)) if v.first() == Some(&1) => TableEncoding::Srgb,
        _ => TableEncoding::Linear,
    }
}

/// DNG's `LightSource` enum values relevant here (spec section 6.3.7's calibration illuminants).
/// Not exhaustive -- only the values real DCPs commonly use.
pub fn light_source_to_cct(value: u16) -> f64 {
    match value {
        1 => 5500.0,  // Daylight
        3 => 2850.0,  // Tungsten (incandescent)
        4 => 5500.0,  // Flash
        9 => 5500.0,  // Fine weather
        10 => 6500.0, // Cloudy
        11 => 7500.0, // Shade
        12 => 6500.0, // Daylight fluorescent (D 5700-7100K)
        13 => 5000.0, // Day white fluorescent (N 4600-5500K)
        14 => 4150.0, // Cool white fluorescent (W 3800-4500K)
        15 => 3500.0, // White fluorescent (WW 3250-3800K)
        17 => 2856.0, // Standard Light A
        18 => 4874.0, // Standard Light B
        19 => 6774.0, // Standard Light C
        20 => 5503.0, // D55
        21 => 6504.0, // D65
        22 => 7504.0, // D75
        23 => 5003.0, // D50
        24 => 3200.0, // ISO studio tungsten
        // A raw Kelvin value packed directly is rare in practice for DCPs (they use the named
        // enum), so an unrecognized value defaults to D65 rather than panicking -- a wrong
        // default is a mild color-temperature error, never a failure.
        _ => 6504.0,
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
    // 42 is plain TIFF; 0x4352 ("RC") is the DNG Camera Profile magic -- every real Adobe `.dcp`
    // starts `IIRC`, which the spike (no real DCP available then) rejected as not-TIFF.
    if magic != 42 && magic != 0x4352 {
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
    // Entries may legally overlap in a corrupt or hostile file (many tags pointing at one big
    // blob), so total decoded bytes are budgeted against the file size, not just each entry
    // against it -- otherwise N entries x one large region allocates N copies.
    let budget = data.len().saturating_mul(2).saturating_add(64 * 1024);
    let mut decoded_bytes = 0usize;
    for _ in 0..entry_count {
        let tag = read_u16(&mut cursor, little_endian)?;
        let type_raw = read_u16(&mut cursor, little_endian)?;
        let count = read_u32(&mut cursor, little_endian)? as usize;
        let value_offset_pos = cursor.position();
        let Some(ty) = TagType::from_u16(type_raw) else {
            cursor.seek(SeekFrom::Start(value_offset_pos + 4))?;
            continue;
        };
        if !KNOWN_TAGS.contains(&tag) {
            cursor.seek(SeekFrom::Start(value_offset_pos + 4))?;
            continue;
        }

        let total_bytes = ty.size().saturating_mul(count);
        decoded_bytes = decoded_bytes.saturating_add(total_bytes);
        if decoded_bytes > budget {
            return Err(DcpError::Malformed(
                "tag values overlap or exceed the file size",
            ));
        }
        let value_bytes = if total_bytes <= 4 {
            let mut buf = [0u8; 4];
            cursor.read_exact(&mut buf)?;
            buf[..total_bytes].to_vec()
        } else {
            let offset = read_u32(&mut cursor, little_endian)?;
            let start = offset as usize;
            let end = start.saturating_add(total_bytes);
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
    /// `UniqueCameraModel` (tag 50708), e.g. `"NIKON Z 8"` -- the key for matching a profile to
    /// a frame's camera make/model. Empty when absent.
    pub unique_camera_model: String,
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
    /// `ProfileHueSatMapEncoding`; defaults to `Linear` when absent, per spec.
    pub hue_sat_map_encoding: TableEncoding,
    /// `ProfileLookTableEncoding`; defaults to `Linear` when absent, per spec.
    pub look_table_encoding: TableEncoding,
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

        // The raw bytes here are copied straight from the file in on-disk order (no reordering),
        // so `HueSatMap::index`'s formula is what determines which (hue, sat, val) grid point
        // each entry lands on. That formula matches the DNG SDK's actual storage order (value
        // outermost, hue middle, saturation innermost, per `dng_hue_sat_map::SetDivisions`) --
        // checked against real Adobe profiles by the `#[ignore]`d `real_adobe_profiles` test.
        let hue_sat_map = |dims_tag: u16, data_tag: u16| -> Option<HueSatMap> {
            let TagValue::Longs(dims) = tags.get(&dims_tag)? else {
                return None;
            };
            if dims.len() != 3 {
                return None;
            }
            let (hue_div, sat_div, val_div) =
                (dims[0] as usize, dims[1] as usize, dims[2] as usize);
            if hue_div == 0 || sat_div == 0 {
                return None;
            }
            // Bound each axis and the total: also keeps the GPU 3D texture within adapter limits.
            if hue_div > MAX_TABLE_AXIS
                || sat_div > MAX_TABLE_AXIS
                || val_div > MAX_TABLE_AXIS
                || hue_div
                    .saturating_mul(sat_div)
                    .saturating_mul(val_div.max(1))
                    > MAX_TABLE_CELLS
            {
                return None;
            }
            let TagValue::Floats(values) = tags.get(&data_tag)? else {
                return None;
            };
            let expected = hue_div
                .checked_mul(sat_div)?
                .checked_mul(val_div.max(1))?
                .checked_mul(3)?;
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
                let points: Vec<(f64, f64)> = values
                    .chunks_exact(2)
                    .map(|c| (c[0] as f64, c[1] as f64))
                    .collect();
                // `ToneCurve::new` asserts strictly-increasing x -- a malformed/adversarial DCP
                // could otherwise panic the whole parse. Drop to the ACR default curve instead of
                // trusting untrusted file content to satisfy that invariant.
                if points.windows(2).all(|w| w[1].0 > w[0].0) {
                    Some(points)
                } else {
                    None
                }
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
        // Both are inverted at solve time; a singular or non-finite one (a zero denominator
        // decodes to 0.0) is a corrupt profile, refused here rather than crashing a render.
        if crate::math::mat_try_invert(&color_matrix1).is_none()
            || crate::math::mat_try_invert(&color_matrix2).is_none()
        {
            return Err(DcpError::Malformed("ColorMatrix is singular or not finite"));
        }
        let finite = |m: Option<Mat3>| m.filter(|m| m.iter().flatten().all(|v| v.is_finite()));

        let unique_camera_model = match tags.get(&TAG_UNIQUE_CAMERA_MODEL) {
            Some(TagValue::Ascii(s)) => s.clone(),
            _ => String::new(),
        };

        Ok(DcpProfile {
            name,
            unique_camera_model,
            illuminant1_cct: illuminant1,
            illuminant2_cct: illuminant2,
            color_matrix1,
            color_matrix2,
            forward_matrix1: finite(optional_matrix3(TAG_FORWARD_MATRIX1)),
            forward_matrix2: finite(optional_matrix3(TAG_FORWARD_MATRIX2)),
            hue_sat_map1: hue_sat_map(TAG_PROFILE_HUE_SAT_MAP_DIMS, TAG_PROFILE_HUE_SAT_MAP_DATA1),
            hue_sat_map2: hue_sat_map(TAG_PROFILE_HUE_SAT_MAP_DIMS, TAG_PROFILE_HUE_SAT_MAP_DATA2),
            look_table: hue_sat_map(TAG_PROFILE_LOOK_TABLE_DIMS, TAG_PROFILE_LOOK_TABLE_DATA),
            tone_curve_points,
            baseline_exposure_offset,
            hue_sat_map_encoding: table_encoding(&tags, TAG_PROFILE_HUE_SAT_MAP_ENCODING),
            look_table_encoding: table_encoding(&tags, TAG_PROFILE_LOOK_TABLE_ENCODING),
        })
    }
}

/// Test support: a byte-level `.dcp` writer, so tests here and in downstream crates can build
/// profiles without ever touching (or bundling) a real Adobe file (ADR-0018). Not part of the
/// supported API.
#[doc(hidden)]
pub mod testing {
    use super::*;

    /// A profile for `model` (`UniqueCameraModel`) named `name`, identity `ColorMatrix1`, an
    /// optional single-cell HueSatMap whose value scale is `hue_sat_value_scale` (so it visibly
    /// brightens or darkens), and an optional LookTable value scale. `real_magic` writes the
    /// `IIRC` header every real Adobe profile has, instead of plain TIFF's 42.
    pub fn synthetic_dcp_bytes(
        model: &str,
        name: &str,
        hue_sat_value_scale: Option<f32>,
        look_value_scale: Option<f32>,
        real_magic: bool,
    ) -> Vec<u8> {
        synthetic_dcp_with_matrix(
            model,
            name,
            hue_sat_value_scale,
            look_value_scale,
            real_magic,
            [1, 0, 0, 0, 1, 0, 0, 0, 1],
        )
    }

    /// As [`synthetic_dcp_bytes`] with an explicit `ColorMatrix1` (row-major numerators over 1).
    pub fn synthetic_dcp_with_matrix(
        model: &str,
        name: &str,
        hue_sat_value_scale: Option<f32>,
        look_value_scale: Option<f32>,
        real_magic: bool,
        color_matrix: [i32; 9],
    ) -> Vec<u8> {
        // (tag, tiff type, count, payload)
        let mut entries: Vec<(u16, u16, u32, Vec<u8>)> = Vec::new();
        let ascii = |s: &str| {
            let mut b = s.as_bytes().to_vec();
            b.push(0);
            b
        };
        let floats = |v: &[f32]| v.iter().flat_map(|f| f.to_le_bytes()).collect::<Vec<u8>>();
        let longs = |v: &[u32]| v.iter().flat_map(|f| f.to_le_bytes()).collect::<Vec<u8>>();

        entries.push((
            TAG_UNIQUE_CAMERA_MODEL,
            2,
            model.len() as u32 + 1,
            ascii(model),
        ));
        entries.push((TAG_PROFILE_NAME, 2, name.len() as u32 + 1, ascii(name)));
        entries.push((
            TAG_CALIBRATION_ILLUMINANT1,
            3,
            1,
            17u16.to_le_bytes().to_vec(),
        ));
        let identity: Vec<u8> = color_matrix
            .iter()
            .flat_map(|n| [n.to_le_bytes(), 1i32.to_le_bytes()].concat())
            .collect();
        entries.push((TAG_COLOR_MATRIX1, 10, 9, identity));
        if let Some(scale) = hue_sat_value_scale {
            entries.push((TAG_PROFILE_HUE_SAT_MAP_DIMS, 4, 3, longs(&[1, 1, 1])));
            entries.push((
                TAG_PROFILE_HUE_SAT_MAP_DATA1,
                11,
                3,
                floats(&[0.0, 1.0, scale]),
            ));
        }
        if let Some(scale) = look_value_scale {
            entries.push((TAG_PROFILE_LOOK_TABLE_DIMS, 4, 3, longs(&[1, 1, 1])));
            entries.push((
                TAG_PROFILE_LOOK_TABLE_DATA,
                11,
                3,
                floats(&[0.0, 1.0, scale]),
            ));
        }
        assemble(entries, real_magic)
    }

    /// Lays out `(tag, tiff type, count, payload)` entries as a little-endian TIFF-structured file
    /// (values of 4 bytes or fewer inline, the rest external). Exposed so tests can craft
    /// deliberately malformed files.
    pub fn assemble(mut entries: Vec<(u16, u16, u32, Vec<u8>)>, real_magic: bool) -> Vec<u8> {
        entries.sort_by_key(|e| e.0);

        let mut buf = Vec::new();
        buf.extend_from_slice(b"II");
        buf.extend_from_slice(&(if real_magic { 0x4352u16 } else { 42 }).to_le_bytes());
        buf.extend_from_slice(&8u32.to_le_bytes());
        buf.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        let mut external = Vec::new();
        let external_base = 8 + 2 + entries.len() * 12 + 4;
        for (tag, ty, count, payload) in &entries {
            buf.extend_from_slice(&tag.to_le_bytes());
            buf.extend_from_slice(&ty.to_le_bytes());
            buf.extend_from_slice(&count.to_le_bytes());
            if payload.len() <= 4 {
                let mut inline = payload.clone();
                inline.resize(4, 0);
                buf.extend_from_slice(&inline);
            } else {
                buf.extend_from_slice(&((external_base + external.len()) as u32).to_le_bytes());
                external.extend_from_slice(payload);
                if external.len() % 2 == 1 {
                    external.push(0); // word-align, as TIFF requires
                }
            }
        }
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&external);
        buf
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
    fn accepts_the_real_iirc_header_and_the_real_lookup_table_tags() {
        // Regression for the two spike bugs a real Adobe file exposed: magic 0x4352, and the
        // LookTable tag ids 50981/50982.
        for real_magic in [false, true] {
            let bytes =
                testing::synthetic_dcp_bytes("NIKON Z 8", "T", Some(1.5), Some(0.5), real_magic);
            let p = DcpProfile::parse(&bytes).unwrap();
            assert_eq!(p.unique_camera_model, "NIKON Z 8");
            assert_eq!(p.name, "T");
            let hsm = p.hue_sat_map1.expect("HueSatMap");
            assert_eq!(hsm.data, vec![[0.0, 1.0, 1.5]]);
            let look = p.look_table.expect("LookTable");
            assert_eq!(look.data, vec![[0.0, 1.0, 0.5]]);
        }
    }

    #[test]
    fn hostile_tag_counts_do_not_panic_or_over_allocate() {
        // A count that claims far more data than the file holds must be skipped, not trusted.
        let mut bytes = testing::synthetic_dcp_bytes("X", "T", None, None, true);
        // Corrupt the first entry's count (offset 8 + 2 + 4) to u32::MAX.
        bytes[14..18].copy_from_slice(&u32::MAX.to_le_bytes());
        let _ = DcpProfile::parse(&bytes);
        // Truncations at every length must never panic either.
        let good = testing::synthetic_dcp_bytes("X", "T", Some(2.0), Some(2.0), true);
        for n in 0..good.len() {
            let _ = DcpProfile::parse(&good[..n]);
        }
    }

    #[test]
    fn overlapping_tag_values_cannot_amplify_memory() {
        // Six known float tags all claiming the same 1 MiB blob: 6 MiB of decoded values from a
        // ~1 MiB file. Each entry passes the "within the file" check on its own.
        let blob = vec![0u8; 1 << 20];
        let count = (blob.len() / 4) as u32;
        let external_base = 8 + 2 + 6 * 12 + 4;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"II");
        bytes.extend_from_slice(&0x4352u16.to_le_bytes());
        bytes.extend_from_slice(&8u32.to_le_bytes());
        bytes.extend_from_slice(&6u16.to_le_bytes());
        for tag in [50938u16, 50939, 50940, 50964, 50965, 50982] {
            bytes.extend_from_slice(&tag.to_le_bytes());
            bytes.extend_from_slice(&11u16.to_le_bytes());
            bytes.extend_from_slice(&count.to_le_bytes());
            bytes.extend_from_slice(&(external_base as u32).to_le_bytes());
        }
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&blob);
        assert!(matches!(
            DcpProfile::parse(&bytes),
            Err(DcpError::Malformed(_))
        ));
    }

    #[test]
    fn unknown_tags_are_never_decoded() {
        // Ten thousand unknown-tag entries pointing at one big blob: skipped, so parsing a file
        // that is otherwise a valid profile still succeeds and stays cheap.
        let good = testing::synthetic_dcp_bytes("X", "T", None, None, true);
        assert!(DcpProfile::parse(&good).is_ok());
        let mut entries = vec![(50721u16, 10u16, 9u32, vec![0u8; 72])];
        entries.clear();
        let blob_tag = |t: u16| (t, 11u16, 1u32, 1.0f32.to_le_bytes().to_vec());
        for t in 1000..1100u16 {
            entries.push(blob_tag(t));
        }
        // No ColorMatrix1 -> MissingTag, not an allocation blow-up.
        let bytes = testing::assemble(entries, true);
        assert!(matches!(
            DcpProfile::parse(&bytes),
            Err(DcpError::MissingTag(..))
        ));
    }

    #[test]
    fn a_singular_or_zero_color_matrix_is_refused_not_a_later_panic() {
        for m in [
            [0; 9],
            [1, 0, 0, 0, 1, 0, 0, 0, 0],
            [1, 2, 3, 2, 4, 6, 1, 1, 1],
        ] {
            let bytes = testing::synthetic_dcp_with_matrix("X", "T", None, None, true, m);
            assert!(
                matches!(DcpProfile::parse(&bytes), Err(DcpError::Malformed(_))),
                "matrix {m:?} should be rejected"
            );
        }
    }

    #[test]
    fn oversized_table_axes_are_dropped() {
        // 1 x 4096 x 1: tiny file, but a 4096-wide 3D texture exceeds common adapter limits.
        let dims: Vec<u8> = [1u32, 4096, 1]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let data: Vec<u8> = (0..4096 * 3).flat_map(|_| 1.0f32.to_le_bytes()).collect();
        let base = testing::synthetic_dcp_bytes("X", "T", None, None, true);
        assert!(DcpProfile::parse(&base).is_ok());
        let mut entries = vec![
            (
                50721u16,
                10u16,
                9u32,
                [1, 0, 0, 0, 1, 0, 0, 0, 1i32]
                    .iter()
                    .flat_map(|n| [n.to_le_bytes(), 1i32.to_le_bytes()].concat())
                    .collect(),
            ),
            (50937, 4, 3, dims),
            (50938, 11, 4096 * 3, data),
        ];
        entries.sort_by_key(|e| e.0);
        let p = DcpProfile::parse(&testing::assemble(entries, true)).unwrap();
        assert!(
            p.hue_sat_map1.is_none(),
            "an oversized table must be dropped"
        );
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
        assert_eq!(light_source_to_cct(20), 5503.0);
        assert_eq!(light_source_to_cct(21), 6504.0);
    }

    /// Real Adobe profiles (never bundled, ADR-0018 -- read from a local install). Run with
    /// `cargo test -p nicti-calico -- --ignored real_adobe_profiles`; set `NICTI_DCP_DIR` to the
    /// `CameraProfiles` folder if it isn't the default WSL path. Pins the two things the spike got
    /// wrong for lack of a real file: the `IIRC` magic and the LookTable tag ids.
    #[test]
    #[ignore = "needs a local Adobe Camera Raw profile install"]
    fn real_adobe_profiles() {
        let dir = std::env::var("NICTI_DCP_DIR")
            .unwrap_or_else(|_| "/mnt/c/ProgramData/Adobe/CameraRaw/CameraProfiles".into());
        let read = |rel: &str| {
            let path = std::path::Path::new(&dir).join(rel);
            let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            DcpProfile::parse(&bytes).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        };

        let std_prof = read("Adobe Standard/NIKON Z 8 Adobe Standard.dcp");
        assert_eq!(std_prof.unique_camera_model, "NIKON Z 8");
        assert!(std_prof.forward_matrix1.is_some() && std_prof.forward_matrix2.is_some());
        let hsm = std_prof.hue_sat_map1.as_ref().expect("HueSatMap");
        assert_eq!(
            (hsm.hue_divisions, hsm.sat_divisions, hsm.val_divisions),
            (90, 30, 1)
        );
        assert!(std_prof.hue_sat_map2.is_some());
        let look = std_prof
            .look_table
            .as_ref()
            .expect("LookTable (tags 50981/50982)");
        assert_eq!(
            (look.hue_divisions, look.sat_divisions, look.val_divisions),
            (36, 8, 16)
        );
        assert_eq!(std_prof.look_table_encoding, TableEncoding::Linear);

        let land = read("Camera/Nikon Z 8/NIKON Z 8 Camera Landscape.dcp");
        assert!(land.hue_sat_map1.is_none());
        let look = land.look_table.as_ref().expect("LookTable");
        assert_eq!(
            (look.hue_divisions, look.sat_divisions, look.val_divisions),
            (90, 16, 16)
        );
        assert_eq!(land.look_table_encoding, TableEncoding::Srgb);
        assert!((land.baseline_exposure_offset - -0.2).abs() < 1e-9);
        assert!(land
            .tone_curve_points
            .as_ref()
            .is_some_and(|p| p.len() == 127));
    }
}
