//! DNG-embedded lens correction (#428): `WarpRectilinear` (opcode 1) and `FixVignetteRadial`
//! (opcode 3) out of an `OpcodeList3` blob, as pure data.
//!
//! The byte layout follows the DNG 1.6 spec (all fields big-endian): a `u32` opcode count, then
//! per opcode `id, version, flags, size` (`u32` each) followed by `size` parameter bytes. Ported
//! in shape from LightCraft's `raw/src/opcodes.rs` (storytold/lightcraft@265248c, MIT OR
//! Apache-2.0, see `docs/licensing.md`), narrowed to the two lens opcodes and hardened: the blob
//! comes from an untrusted file, so every count is bounded and every float must be finite.
//!
//! Coordinates: the optical centre is normalised over the opcode's area (0..1 across width and
//! height); the normalised radius `r` of a pixel is its distance from that centre divided by the
//! distance from the centre to the *farthest corner* of the area (spec: `m`).

use crate::{LensModel, Vignette, Warp};

/// Hard cap on opcodes parsed from one list; a real list holds a handful.
const MAX_OPCODES: u32 = 256;
const OP_WARP_RECTILINEAR: u32 = 1;
const OP_FIX_VIGNETTE_RADIAL: u32 = 3;

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn u32(&mut self) -> Option<u32> {
        let end = self.pos.checked_add(4)?;
        let v = u32::from_be_bytes(self.data.get(self.pos..end)?.try_into().ok()?);
        self.pos = end;
        Some(v)
    }

    /// A finite `f64`; NaN/inf reject the whole opcode (a poisoned coefficient would NaN every
    /// pixel of the frame).
    fn f64(&mut self) -> Option<f64> {
        let end = self.pos.checked_add(8)?;
        let v = f64::from_be_bytes(self.data.get(self.pos..end)?.try_into().ok()?);
        self.pos = end;
        v.is_finite().then_some(v)
    }
}

fn parse_warp(params: &[u8]) -> Option<Warp> {
    let mut r = Reader {
        data: params,
        pos: 0,
    };
    let planes = r.u32()?;
    if !(1..=4).contains(&planes) {
        return None;
    }
    let mut out = Vec::with_capacity(planes as usize);
    for _ in 0..planes {
        let mut p = [0.0; 6];
        for v in &mut p {
            *v = r.f64()?;
        }
        out.push(p);
    }
    let center = [r.f64()?, r.f64()?];
    Some(Warp {
        planes: out,
        center,
    })
}

fn parse_vignette(params: &[u8]) -> Option<Vignette> {
    let mut r = Reader {
        data: params,
        pos: 0,
    };
    let mut k = [0.0; 5];
    for v in &mut k {
        *v = r.f64()?;
    }
    let center = [r.f64()?, r.f64()?];
    Some(Vignette { k, center })
}

/// Parse an `OpcodeList3` blob, keeping the first well-formed warp and the first well-formed
/// vignette opcode. Everything else (and anything malformed or truncated) is skipped; returns
/// `None` when the list carries no lens correction at all.
pub fn parse_opcode_list3(data: &[u8]) -> Option<LensModel> {
    let mut r = Reader { data, pos: 0 };
    let count = r.u32()?.min(MAX_OPCODES);
    let mut model = LensModel::default();
    for _ in 0..count {
        let (Some(id), Some(_version), Some(_flags), Some(size)) =
            (r.u32(), r.u32(), r.u32(), r.u32())
        else {
            break;
        };
        let Some(end) = r.pos.checked_add(size as usize) else {
            break;
        };
        let Some(params) = data.get(r.pos..end) else {
            break;
        };
        r.pos = end;
        match id {
            OP_WARP_RECTILINEAR if model.warp.is_none() => model.warp = parse_warp(params),
            OP_FIX_VIGNETTE_RADIAL if model.vignette.is_none() => {
                model.vignette = parse_vignette(params)
            }
            _ => {}
        }
    }
    (model.warp.is_some() || model.vignette.is_some()).then_some(model)
}

/// The DNG-embedded provider: the correction is whatever the file's own `OpcodeList3` says.
pub struct DngEmbedded;

impl DngEmbedded {
    pub const ID: &'static str = "nicti.lens.dng-embedded";
}

impl nicti_claw::Module for DngEmbedded {
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

impl crate::LensCorrection for DngEmbedded {
    fn model(&self, source: &crate::LensSource<'_>) -> Option<LensModel> {
        parse_opcode_list3(source.dng_opcode_list3?)
    }
}

/// Serialise a model back to an `OpcodeList3` blob. Test fixtures and round-trip checks only.
pub fn write_opcode_list3(model: &LensModel) -> Vec<u8> {
    fn put(out: &mut Vec<u8>, id: u32, params: &[u8]) {
        for v in [id, 0x0103_0000, 0, params.len() as u32] {
            out.extend_from_slice(&v.to_be_bytes());
        }
        out.extend_from_slice(params);
    }
    let mut body = Vec::new();
    let mut count = 0u32;
    if let Some(w) = &model.warp {
        let mut p = (w.planes.len() as u32).to_be_bytes().to_vec();
        for plane in &w.planes {
            for v in plane {
                p.extend_from_slice(&v.to_be_bytes());
            }
        }
        for v in w.center {
            p.extend_from_slice(&v.to_be_bytes());
        }
        put(&mut body, OP_WARP_RECTILINEAR, &p);
        count += 1;
    }
    if let Some(v) = &model.vignette {
        let mut p = Vec::new();
        for x in v.k.iter().chain(v.center.iter()) {
            p.extend_from_slice(&x.to_be_bytes());
        }
        put(&mut body, OP_FIX_VIGNETTE_RADIAL, &p);
        count += 1;
    }
    let mut out = count.to_be_bytes().to_vec();
    out.extend_from_slice(&body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> LensModel {
        LensModel {
            warp: Some(Warp {
                planes: vec![
                    [1.0, 0.01, -0.002, 0.0, 0.0001, -0.0002],
                    [1.001, 0.011, -0.002, 0.0, 0.0001, -0.0002],
                    [0.999, 0.009, -0.002, 0.0, 0.0001, -0.0002],
                ],
                center: [0.5, 0.49],
            }),
            vignette: Some(Vignette {
                k: [0.3, -0.1, 0.05, 0.0, 0.0],
                center: [0.5, 0.5],
            }),
        }
    }

    #[test]
    fn round_trips_warp_and_vignette() {
        let m = sample();
        assert_eq!(parse_opcode_list3(&write_opcode_list3(&m)), Some(m));
    }

    #[test]
    fn empty_and_garbage_lists_carry_no_lens() {
        assert_eq!(parse_opcode_list3(&[]), None);
        assert_eq!(parse_opcode_list3(&0u32.to_be_bytes()), None);
        assert_eq!(parse_opcode_list3(&[0xff; 7]), None);
    }

    #[test]
    fn truncated_list_never_panics_and_keeps_what_was_complete() {
        let blob = write_opcode_list3(&sample());
        for cut in 0..blob.len() {
            let _ = parse_opcode_list3(&blob[..cut]);
        }
        // Cutting inside the second opcode keeps the first (warp) one.
        let warp_only = write_opcode_list3(&LensModel {
            vignette: None,
            ..sample()
        });
        let got = parse_opcode_list3(&blob[..warp_only.len() + 6]).unwrap();
        assert!(got.warp.is_some() && got.vignette.is_none());
    }

    #[test]
    fn non_finite_coefficient_drops_just_that_opcode() {
        let mut m = sample();
        m.warp.as_mut().unwrap().planes[0][1] = f64::NAN;
        let got = parse_opcode_list3(&write_opcode_list3(&m)).unwrap();
        assert!(got.warp.is_none());
        assert!(got.vignette.is_some());
    }

    #[test]
    fn oversized_plane_count_and_size_are_rejected() {
        // count=1, id=1, ver, flags, size=4, params = plane count 99.
        let mut blob = 1u32.to_be_bytes().to_vec();
        for v in [1u32, 0, 0, 4, 99] {
            blob.extend_from_slice(&v.to_be_bytes());
        }
        assert_eq!(parse_opcode_list3(&blob), None);
        // A size field far beyond the buffer must not overflow or read out of bounds.
        let mut blob = 1u32.to_be_bytes().to_vec();
        for v in [1u32, 0, 0, u32::MAX] {
            blob.extend_from_slice(&v.to_be_bytes());
        }
        assert_eq!(parse_opcode_list3(&blob), None);
    }

    #[test]
    fn unrelated_opcodes_are_skipped() {
        // An opcode 9 (GainMap) with 3 junk bytes, then a real vignette.
        let vig = write_opcode_list3(&LensModel {
            warp: None,
            ..sample()
        });
        let mut blob = 2u32.to_be_bytes().to_vec();
        for v in [9u32, 0, 0, 3] {
            blob.extend_from_slice(&v.to_be_bytes());
        }
        blob.extend_from_slice(&[1, 2, 3]);
        blob.extend_from_slice(&vig[4..]);
        let got = parse_opcode_list3(&blob).unwrap();
        assert!(got.warp.is_none() && got.vignette.is_some());
    }
}
