//! Decoder for Nikon's embedded lens-correction blob (#410, ADR-0410).
//!
//! Z-series bodies write their own distortion and vignette correction coefficients into TIFF tag
//! 0xC7D5 of a SubIFD (`nicti_cornea::embedded::Walker::find_nikon_lens_info` locates and reads it,
//! the same split as `dng`: the decoder hands over raw bytes, this crate interprets them). The
//! payload is not encrypted: `"Nikon\0"`, a 2-byte version, 2 reserved bytes, then an inner TIFF
//! header and one IFD whose offsets are relative to that inner header (the same shape as the
//! MakerNote, which is why the walker's `Nikon\0` handling looks familiar). Entry 0x05 is the
//! distortion block and 0x06 the vignette block; each block is
//!
//! ```text
//! 0x00 char[4]  version      0x04 u8  flag (0 no lens, 1 on, 2 off, 3 on/required)
//! 0x10 u32 n                 0x14 n x SRATIONAL (i32 numerator, i32 denominator)
//! ```
//!
//! Layout provenance: ExifTool's public `NEFInfo`/`DistortionInfo`/`VignetteInfo` tag tables and the
//! pixls.us reverse-engineering threads. No code was copied. This module only *decodes* the
//! numbers; what they mean (the polynomial form) is `nicti_iris::nikon`'s concern, since that part
//! is not pinned against real files yet. Lateral CA (entry 0x07) is deliberately not decoded: its
//! layout is not documented anywhere citable (ADR-0410).

use crate::{LensCorrection, LensModel, LensSource, Vignette, Warp};

/// `"Nikon\0"` + 2 version + 2 reserved + the 8-byte inner TIFF header.
const NIKON_BLOB_HEADER_LEN: usize = 18;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ByteOrder {
    Little,
    Big,
}

const ENTRY_DISTORTION: u16 = 0x05;
const ENTRY_VIGNETTE: u16 = 0x06;
/// Offset of the coefficient count inside a block, and of the first coefficient after it.
const BLOCK_COUNT_AT: usize = 0x10;
const BLOCK_COEFFS_AT: usize = 0x14;
/// Real blocks carry 4 (distortion) or 8 (vignette) coefficients; this bounds a corrupt count.
const MAX_COEFFS: usize = 16;
/// An IFD this long is corrupt (the real one has about 9 entries).
const MAX_ENTRIES: usize = 64;

/// How the body says the correction should be treated (`DistortionCorrection` in ExifTool).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LensFlag {
    NoLens,
    OnOptional,
    Off,
    OnRequired,
    Unknown(u8),
}

impl LensFlag {
    fn from_byte(b: u8) -> Self {
        match b {
            0 => Self::NoLens,
            1 => Self::OnOptional,
            2 => Self::Off,
            3 => Self::OnRequired,
            other => Self::Unknown(other),
        }
    }

    /// Whether the camera's own setting asks for the correction to be applied.
    pub fn applies(self) -> bool {
        matches!(self, Self::OnOptional | Self::OnRequired)
    }
}

/// One decoded correction block: the raw coefficients in file order, highest power first.
#[derive(Debug, Clone, PartialEq)]
pub struct LensBlock {
    pub version: [u8; 4],
    pub flag: LensFlag,
    pub coefficients: Vec<f64>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct NikonLensInfo {
    pub distortion: Option<LensBlock>,
    pub vignette: Option<LensBlock>,
}

fn u16_at(bo: ByteOrder, b: &[u8], at: usize) -> Option<u16> {
    let s: [u8; 2] = b.get(at..at.checked_add(2)?)?.try_into().ok()?;
    Some(match bo {
        ByteOrder::Little => u16::from_le_bytes(s),
        ByteOrder::Big => u16::from_be_bytes(s),
    })
}

fn u32_at(bo: ByteOrder, b: &[u8], at: usize) -> Option<u32> {
    let s: [u8; 4] = b.get(at..at.checked_add(4)?)?.try_into().ok()?;
    Some(match bo {
        ByteOrder::Little => u32::from_le_bytes(s),
        ByteOrder::Big => u32::from_be_bytes(s),
    })
}

fn type_size(field_type: u16) -> usize {
    match field_type {
        3 | 8 => 2,
        4 | 9 | 11 => 4,
        5 | 10 | 12 => 8,
        _ => 1,
    }
}

/// Decodes the blob read by `Walker::find_nikon_lens_info`. `None` when it isn't a recognisable
/// Nikon lens blob or carries neither block; a single bad block is dropped on its own.
pub fn parse(blob: &[u8]) -> Option<NikonLensInfo> {
    if blob.len() < NIKON_BLOB_HEADER_LEN || &blob[0..6] != b"Nikon\0" {
        return None;
    }
    let base = 10usize;
    let bo = match &blob[base..base + 2] {
        b"II" => ByteOrder::Little,
        b"MM" => ByteOrder::Big,
        _ => return None,
    };
    if u16_at(bo, blob, base + 2)? != 42 {
        return None;
    }
    let ifd_at = base.checked_add(u32_at(bo, blob, base + 4)? as usize)?;
    let count = u16_at(bo, blob, ifd_at)? as usize;
    if count > MAX_ENTRIES {
        return None;
    }
    let mut info = NikonLensInfo::default();
    for i in 0..count {
        let e = ifd_at.checked_add(2 + i * 12)?;
        let (Some(tag), Some(field_type), Some(n)) = (
            u16_at(bo, blob, e),
            u16_at(bo, blob, e + 2),
            u32_at(bo, blob, e + 4),
        ) else {
            break;
        };
        if tag != ENTRY_DISTORTION && tag != ENTRY_VIGNETTE {
            continue;
        }
        let Some(len) = type_size(field_type).checked_mul(n as usize) else {
            continue;
        };
        let start = if len <= 4 {
            e + 8
        } else {
            let Some(off) = u32_at(bo, blob, e + 8) else {
                continue;
            };
            let Some(s) = base.checked_add(off as usize) else {
                continue;
            };
            s
        };
        let Some(data) = start.checked_add(len).and_then(|end| blob.get(start..end)) else {
            continue;
        };
        let block = parse_block(bo, data);
        // A later corrupt duplicate must not erase an earlier good block.
        if block.is_some() {
            match tag {
                ENTRY_DISTORTION => info.distortion = block,
                _ => info.vignette = block,
            }
        }
    }
    (info.distortion.is_some() || info.vignette.is_some()).then_some(info)
}

fn parse_block(bo: ByteOrder, data: &[u8]) -> Option<LensBlock> {
    let version: [u8; 4] = data.get(0..4)?.try_into().ok()?;
    let flag = LensFlag::from_byte(*data.get(4)?);
    let n = u32_at(bo, data, BLOCK_COUNT_AT)? as usize;
    if n == 0 || n > MAX_COEFFS {
        return None;
    }
    let mut coefficients = Vec::with_capacity(n);
    for i in 0..n {
        let at = BLOCK_COEFFS_AT + i * 8;
        let num = u32_at(bo, data, at)? as i32;
        let den = u32_at(bo, data, at + 4)? as i32;
        if den == 0 {
            return None;
        }
        let v = num as f64 / den as f64;
        if !v.is_finite() {
            return None;
        }
        coefficients.push(v);
    }
    Some(LensBlock {
        version,
        flag,
        coefficients,
    })
}

/// Coefficients beyond this are a corrupt or hostile file (real ones are well under 1).
const MAX_COEFFICIENT: f64 = 10.0;
/// Largest accepted refit error (distortion scale / vignette gain): a profile the even-power basis
/// cannot represent is dropped rather than rendered wrongly.
const MAX_FIT_RESIDUAL: f64 = 0.01;
/// Real distortion is a few percent; beyond this the source sample runs far off the frame.
const MAX_DISTORTION_SCALE_DEVIATION: f64 = 0.5;
/// The correction is defined over the normalised radius 0..=1 (the recorded image's farthest
/// corner is 1); the refit samples that range.
const FIT_SAMPLES: usize = 64;
/// The vignette model must stay positive and bounded everywhere it is evaluated.
const MAX_VIGNETTE_GAIN: f64 = 16.0;

/// Nikon's polynomials are written highest power first: `poly(r) = 1 + c[0] r^n + c[1] r^(n-1) +
/// ... + c[n-1] r` (Horner form, `n = c.len()`).
///
/// **Unverified semantics (ADR-0410):** this is the reading ART documents. The pixls.us threads
/// read the distortion block as even powers only, and neither has been checked on a Z8 against
/// Adobe DNG Converter's `WarpRectilinear` for the same file. That check is the parity follow-up;
/// until it lands the provider is opt-in (`LensParams::nikon_profile`).
fn horner(coefficients: &[f64], r: f64) -> f64 {
    coefficients.iter().fold(0.0, |acc, c| (acc + c) * r) + 1.0
}

/// Least-squares fit of `target(r) ~ sum_j a[j] * r^(2 (j + first_power))` over `r` in 0..=1, by
/// normal equations and Gaussian elimination with partial pivoting. `None` on a singular system or
/// a non-finite result.
fn fit_even<const N: usize>(first_power: usize, target: impl Fn(f64) -> f64) -> Option<[f64; N]> {
    let mut a = [[0.0f64; N]; N];
    let mut b = [0.0f64; N];
    for i in 0..FIT_SAMPLES {
        let r = (i as f64 + 0.5) / FIT_SAMPLES as f64;
        let y = target(r);
        if !y.is_finite() {
            return None;
        }
        let basis: [f64; N] = std::array::from_fn(|j| r.powi(2 * (j + first_power) as i32));
        for j in 0..N {
            b[j] += basis[j] * y;
            for k in 0..N {
                a[j][k] += basis[j] * basis[k];
            }
        }
    }
    for col in 0..N {
        let pivot = (col..N).max_by(|&x, &y| a[x][col].abs().total_cmp(&a[y][col].abs()))?;
        if a[pivot][col].abs() < 1e-14 {
            return None;
        }
        a.swap(col, pivot);
        b.swap(col, pivot);
        let pivot_row = a[col];
        for row in col + 1..N {
            let f = a[row][col] / pivot_row[col];
            for (dst, src) in a[row][col..].iter_mut().zip(&pivot_row[col..]) {
                *dst -= f * src;
            }
            b[row] -= f * b[col];
        }
    }
    let mut x = [0.0f64; N];
    for row in (0..N).rev() {
        let tail: f64 = (row + 1..N).map(|k| a[row][k] * x[k]).sum();
        x[row] = (b[row] - tail) / a[row][row];
    }
    x.iter().all(|v| v.is_finite()).then_some(x)
}

/// The refit must track the target everywhere in 0..=1 (checked between the fit's own sample
/// points too) and stay within `max_abs`.
fn within_bounds(
    fitted: impl Fn(f64) -> f64,
    target: impl Fn(f64) -> f64,
    max_residual: f64,
    max_abs: f64,
) -> bool {
    (0..=4 * FIT_SAMPLES).all(|i| {
        let r = i as f64 / (4 * FIT_SAMPLES) as f64;
        let f = fitted(r);
        f.is_finite() && (f - target(r)).abs() <= max_residual && (f - 1.0).abs() <= max_abs
    })
}

fn sane(block: &LensBlock) -> bool {
    block.flag.applies()
        && block
            .coefficients
            .iter()
            .all(|c| c.abs() <= MAX_COEFFICIENT)
}

/// Distortion block -> the shared `Warp` (`source = f(r^2) * dest`, `f` a cubic in `r^2`).
fn warp_from(block: &LensBlock) -> Option<Warp> {
    if !sane(block) {
        return None;
    }
    let target = |r: f64| horner(&block.coefficients, r);
    let [k0, k1, k2, k3] = fit_even::<4>(0, target)?;
    let fitted = |r: f64| {
        let r2 = r * r;
        k0 + r2 * (k1 + r2 * (k2 + r2 * k3))
    };
    if !within_bounds(
        fitted,
        target,
        MAX_FIT_RESIDUAL,
        MAX_DISTORTION_SCALE_DEVIATION,
    ) {
        return None;
    }
    Some(Warp {
        planes: vec![[k0, k1, k2, k3, 0.0, 0.0]],
        center: [0.5, 0.5],
    })
}

/// Vignette block -> the shared `Vignette`. The block's polynomial is a falloff to divide by, so
/// the correction gain is `1 / sqrt(poly(r))`; that is refit onto `1 + k0 r^2 + ... + k4 r^10`.
fn vignette_from(block: &LensBlock) -> Option<Vignette> {
    if !sane(block) {
        return None;
    }
    // A non-positive falloff anywhere in 0..=1 has no sensible inverse; drop rather than clamp.
    for i in 0..=FIT_SAMPLES {
        let p = horner(&block.coefficients, i as f64 / FIT_SAMPLES as f64);
        if p.is_nan() || p <= 1.0 / (MAX_VIGNETTE_GAIN * MAX_VIGNETTE_GAIN) {
            return None;
        }
    }
    let target = |r: f64| 1.0 / horner(&block.coefficients, r).sqrt();
    let k = fit_even::<5>(1, |r| target(r) - 1.0)?;
    let v = Vignette {
        k,
        center: [0.5, 0.5],
    };
    let fitted = |r: f64| v.gain(r * r);
    if !within_bounds(fitted, target, MAX_FIT_RESIDUAL, MAX_VIGNETTE_GAIN - 1.0) {
        return None;
    }
    Some(Vignette {
        k,
        center: [0.5, 0.5],
    })
}

/// Nikon-embedded provider (#410): the correction is what the NEF's own 0xC7D5 blob says.
pub struct NikonEmbedded;

impl NikonEmbedded {
    pub const ID: &'static str = "nicti.lens.nikon-embedded";
}

impl nicti_claw::Module for NikonEmbedded {
    fn id(&self) -> &str {
        Self::ID
    }

    fn schema_version(&self) -> u32 {
        1
    }

    fn migrate_params(
        &self,
        _from_version: u32,
        params: serde_json::Value,
    ) -> Option<serde_json::Value> {
        Some(params)
    }
}

impl LensCorrection for NikonEmbedded {
    fn model(&self, source: &LensSource<'_>) -> Option<LensModel> {
        let info = parse(source.nikon_lens_info?)?;
        let model = LensModel {
            warp: info.distortion.as_ref().and_then(warp_from),
            vignette: info.vignette.as_ref().and_then(vignette_from),
        };
        (model.warp.is_some() || model.vignette.is_some()).then_some(model)
    }
}

/// Test/fixture writer: assembles a big-endian blob with the given blocks, the inverse of
/// [`parse`]. `(flag, coefficients)` per block; rationals use a fixed denominator of 1_000_000.
pub fn write_blob(distortion: Option<(u8, &[f64])>, vignette: Option<(u8, &[f64])>) -> Vec<u8> {
    write_blob_with(true, distortion, vignette)
}

/// [`write_blob`] with a choice of byte order for the inner TIFF header and block fields.
pub fn write_blob_with(
    big: bool,
    distortion: Option<(u8, &[f64])>,
    vignette: Option<(u8, &[f64])>,
) -> Vec<u8> {
    let u16b = |v: u16| {
        if big {
            v.to_be_bytes()
        } else {
            v.to_le_bytes()
        }
    };
    let u32b = |v: u32| {
        if big {
            v.to_be_bytes()
        } else {
            v.to_le_bytes()
        }
    };
    let i32b = |v: i32| {
        if big {
            v.to_be_bytes()
        } else {
            v.to_le_bytes()
        }
    };
    let block = |flag: u8, coeffs: &[f64]| -> Vec<u8> {
        let mut b = vec![0u8; BLOCK_COEFFS_AT];
        b[0..4].copy_from_slice(b"0100");
        b[4] = flag;
        b[BLOCK_COUNT_AT..BLOCK_COUNT_AT + 4].copy_from_slice(&u32b(coeffs.len() as u32));
        for c in coeffs {
            b.extend_from_slice(&i32b((c * 1e6).round() as i32));
            b.extend_from_slice(&i32b(1_000_000));
        }
        b
    };
    let blocks: Vec<(u16, Vec<u8>)> = [(ENTRY_DISTORTION, distortion), (ENTRY_VIGNETTE, vignette)]
        .into_iter()
        .filter_map(|(tag, b)| b.map(|(f, c)| (tag, block(f, c))))
        .collect();

    let mut out = Vec::new();
    out.extend_from_slice(b"Nikon\0\x02\x00\x00\x00");
    out.extend_from_slice(if big { b"MM" } else { b"II" });
    out.extend_from_slice(&u16b(42));
    out.extend_from_slice(&u32b(8));
    // IFD at inner offset 8: count, entries, next-IFD pointer.
    let ifd_len = 2 + blocks.len() * 12 + 4;
    let mut data_at = 8 + ifd_len;
    out.extend_from_slice(&u16b(blocks.len() as u16));
    for (tag, b) in &blocks {
        out.extend_from_slice(&u16b(*tag));
        out.extend_from_slice(&u16b(7)); // UNDEFINED
        out.extend_from_slice(&u32b(b.len() as u32));
        out.extend_from_slice(&u32b(data_at as u32));
        data_at += b.len();
    }
    out.extend_from_slice(&u32b(0));
    for (_, b) in &blocks {
        out.extend_from_slice(b);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIST: [f64; 4] = [0.01, -0.02, 0.03, -0.04];
    const VIG: [f64; 8] = [0.1, 0.0, -0.2, 0.0, 0.3, 0.0, -0.4, 0.0];

    #[test]
    fn round_trips_both_blocks() {
        let blob = write_blob(Some((1, &DIST)), Some((3, &VIG)));
        let info = parse(&blob).expect("parses");
        let d = info.distortion.expect("distortion");
        assert_eq!(&d.version, b"0100");
        assert_eq!(d.flag, LensFlag::OnOptional);
        assert_eq!(d.coefficients, DIST);
        let v = info.vignette.expect("vignette");
        assert_eq!(v.flag, LensFlag::OnRequired);
        assert_eq!(v.coefficients, VIG);
    }

    #[test]
    fn a_single_block_is_enough() {
        let info = parse(&write_blob(None, Some((1, &VIG)))).expect("parses");
        assert!(info.distortion.is_none() && info.vignette.is_some());
    }

    #[test]
    fn flag_semantics() {
        assert!(LensFlag::OnOptional.applies() && LensFlag::OnRequired.applies());
        assert!(!LensFlag::Off.applies() && !LensFlag::NoLens.applies());
        assert!(!LensFlag::Unknown(9).applies());
    }

    #[test]
    fn rejects_garbage_and_empty() {
        assert!(parse(&[]).is_none());
        assert!(parse(b"not a nikon blob at all, definitely").is_none());
        assert!(parse(&write_blob(None, None)).is_none());
    }

    #[test]
    fn truncation_at_every_byte_never_panics() {
        let blob = write_blob(Some((1, &DIST)), Some((1, &VIG)));
        for cut in 0..blob.len() {
            // A cut inside a block drops that block; it must never invent data.
            if let Some(i) = parse(&blob[..cut]) {
                for b in [i.distortion, i.vignette].into_iter().flatten() {
                    assert!(b.coefficients.iter().all(|c| c.is_finite()));
                }
            }
        }
    }

    #[test]
    fn zero_denominator_drops_only_that_block() {
        let mut blob = write_blob(Some((1, &DIST)), Some((1, &VIG)));
        // Distortion block is first: inner base 10, IFD at +8 (count, 2 entries, next pointer).
        let dist_at = 10 + 8 + 2 + 2 * 12 + 4 + BLOCK_COEFFS_AT + 4;
        blob[dist_at..dist_at + 4].copy_from_slice(&0i32.to_be_bytes());
        let info = parse(&blob).expect("vignette survives");
        assert!(info.distortion.is_none());
        assert!(info.vignette.is_some());
    }

    #[test]
    fn oversized_coefficient_count_is_rejected() {
        let mut blob = write_blob(Some((1, &DIST)), None);
        let n_at = 10 + 8 + 2 + 12 + 4 + BLOCK_COUNT_AT;
        blob[n_at..n_at + 4].copy_from_slice(&1_000_000u32.to_be_bytes());
        assert!(parse(&blob).is_none());
    }

    #[test]
    fn entry_pointing_past_the_end_is_dropped() {
        let mut blob = write_blob(Some((1, &DIST)), None);
        // First entry's value offset field: base 10 + IFD at 8 + count 2 + tag/type/count 8.
        let off_at = 10 + 8 + 2 + 8;
        blob[off_at..off_at + 4].copy_from_slice(&0x00FF_FFFFu32.to_be_bytes());
        assert!(parse(&blob).is_none());
    }

    fn source(blob: &[u8]) -> LensSource<'_> {
        LensSource {
            nikon_lens_info: Some(blob),
            ..LensSource::default()
        }
    }

    #[test]
    fn even_distortion_refits_exactly() {
        // poly = 1 + 0.02 r^2 - 0.01 r^4 (odd slots zero): inside the fit basis, so the refit
        // must reproduce it to numerical precision.
        let blob = write_blob(Some((1, &[-0.01, 0.0, 0.02, 0.0])), None);
        let w = NikonEmbedded.model(&source(&blob)).unwrap().warp.unwrap();
        let [k0, k1, k2, k3, ..] = w.planes[0];
        assert!(
            (k0 - 1.0).abs() < 1e-6 && (k1 - 0.02).abs() < 1e-6,
            "{k0} {k1}"
        );
        assert!((k2 + 0.01).abs() < 1e-6 && k3.abs() < 1e-6, "{k2} {k3}");
    }

    #[test]
    fn odd_distortion_terms_refit_within_a_small_residual() {
        let c = [0.004, -0.012, 0.03, 0.002];
        let blob = write_blob(Some((1, &c)), None);
        let w = NikonEmbedded.model(&source(&blob)).unwrap().warp.unwrap();
        let [k0, k1, k2, k3, ..] = w.planes[0];
        let worst = (1..=100)
            .map(|i| {
                let r = i as f64 / 100.0;
                let fit = k0 + k1 * r * r + k2 * r.powi(4) + k3 * r.powi(6);
                (fit - horner(&c, r)).abs()
            })
            .fold(0.0, f64::max);
        assert!(worst < 2e-3, "worst residual {worst}");
    }

    #[test]
    fn vignette_gain_inverts_the_falloff() {
        let c = [0.0, 0.0, 0.0, 0.0, 0.1, 0.0, 0.2, 0.0];
        let blob = write_blob(None, Some((1, &c)));
        let v = NikonEmbedded
            .model(&source(&blob))
            .unwrap()
            .vignette
            .unwrap();
        for r in [0.0f64, 0.3, 0.6, 0.9, 1.0] {
            let want = 1.0 / horner(&c, r).sqrt();
            assert!(
                (v.gain(r * r) - want).abs() < 1e-3,
                "r={r}: {} vs {want}",
                v.gain(r * r)
            );
        }
    }

    #[test]
    fn flag_off_or_no_lens_yields_nothing() {
        for flag in [0u8, 2] {
            let blob = write_blob(
                Some((flag, &[0.01, 0.0, 0.0, 0.0])),
                Some((flag, &[0.1; 8])),
            );
            assert!(NikonEmbedded.model(&source(&blob)).is_none(), "flag {flag}");
        }
    }

    #[test]
    fn hostile_coefficients_and_non_positive_falloff_are_dropped() {
        let blob = write_blob(Some((1, &[500.0, 0.0, 0.0, 0.0])), None);
        assert!(NikonEmbedded.model(&source(&blob)).is_none());
        // poly(1) = 1 - 2 < 0: no inverse.
        let blob = write_blob(None, Some((1, &[-2.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])));
        assert!(NikonEmbedded.model(&source(&blob)).is_none());
    }

    #[test]
    fn no_blob_no_model() {
        assert!(NikonEmbedded.model(&LensSource::default()).is_none());
    }

    #[test]
    fn little_endian_blob_round_trips() {
        let blob = write_blob_with(false, Some((1, &DIST)), Some((3, &VIG)));
        assert_eq!(&blob[10..12], b"II");
        let info = parse(&blob).expect("parses");
        assert_eq!(info.distortion.unwrap().coefficients, DIST);
        assert_eq!(info.vignette.unwrap().coefficients, VIG);
    }

    #[test]
    fn a_corrupt_duplicate_entry_does_not_erase_a_good_block() {
        let mut blob = write_blob(Some((1, &DIST)), Some((1, &VIG)));
        // Retag the vignette entry (second) as a second distortion entry, then zero its count so
        // that block fails to parse: the first, good distortion block must survive.
        let second_entry = 10 + 8 + 2 + 12;
        blob[second_entry..second_entry + 2].copy_from_slice(&ENTRY_DISTORTION.to_be_bytes());
        let vig_block = 10 + 8 + 2 + 2 * 12 + 4 + BLOCK_COEFFS_AT + DIST.len() * 8;
        blob[vig_block + BLOCK_COUNT_AT..vig_block + BLOCK_COUNT_AT + 4]
            .copy_from_slice(&0u32.to_be_bytes());
        let info = parse(&blob).expect("good block survives");
        assert_eq!(info.distortion.unwrap().coefficients, DIST);
    }

    #[test]
    fn a_profile_the_even_basis_cannot_represent_is_dropped() {
        // A large linear (odd) term: no even polynomial tracks it within the residual bound.
        let blob = write_blob(Some((1, &[0.0, 0.0, 0.0, 0.4])), None);
        assert!(NikonEmbedded.model(&source(&blob)).is_none());
        // And an absurdly large radial scale is rejected outright.
        let blob = write_blob(Some((1, &[9.0, 0.0, 0.0, 0.0])), None);
        assert!(NikonEmbedded.model(&source(&blob)).is_none());
    }
}
