//! Common decoded-frame shape both backends (LibRaw, rawler) produce, so `compare` can diff them
//! independent of which decoder made either one. This is also the shape ADR-0018 proposes for
//! `nicti-decode::RawDecoder`'s eventual decode method -- not implemented there yet, that's #41's
//! promotion, not this spike's.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawFrame {
    pub make: String,
    pub model: String,
    /// Raw `NEFCompression` MakerNote tag value (Nikon-specific; 0 for non-Nikon/rawler-DNG
    /// paths that don't expose it the same way).
    pub nef_compression: u16,
    pub compression_label: String,
    pub raw_width: u32,
    pub raw_height: u32,
    pub top_margin: u32,
    pub left_margin: u32,
    pub filters: u32,
    pub colors: i32,
    pub black: u32,
    pub maximum: u32,
    pub cam_mul: [f32; 4],
    /// blake3 of the still-mosaiced Bayer plane (post black-level-as-decoded, i.e. exactly what
    /// the decoder handed back -- *not* black-subtracted/normalized). This is what `compare`
    /// diffs, but it's an *exact-match* indicator only -- **a mismatch here is not itself a
    /// correctness failure for the Lossless/D7500 cross-check.** LibRaw and rawler decode
    /// Lossless NEF with a real, characterized, one-directional ±1 LSB rounding difference (see
    /// `docs/adr/0018-raw-decoder.md`'s Correctness section), so their hashes never match on real
    /// files -- use `diff`'s per-pixel histogram (max abs diff ≤ 1) as the actual correctness
    /// check, not hash equality.
    pub cfa_hash: String,
    pub cfa_len: usize,
}

impl RawFrame {
    pub fn from_libraw(meta: &crate::libraw_ffi::DecodedMetadata, cfa: &[u16]) -> Self {
        RawFrame {
            make: meta.make.clone(),
            model: meta.model.clone(),
            nef_compression: meta.nef_compression,
            compression_label: crate::compression::label(meta.nef_compression).to_string(),
            raw_width: meta.raw_width as u32,
            raw_height: meta.raw_height as u32,
            top_margin: meta.top_margin as u32,
            left_margin: meta.left_margin as u32,
            filters: meta.filters,
            colors: meta.colors,
            black: meta.black,
            maximum: meta.maximum,
            cam_mul: meta.cam_mul,
            cfa_hash: hash_cfa(cfa),
            cfa_len: cfa.len(),
        }
    }
}

/// blake3 over the raw `u16` samples' little-endian bytes -- stable across platforms regardless
/// of host endianness (this repo's reference machine is x86_64, but this is cheap insurance).
pub fn hash_cfa(cfa: &[u16]) -> String {
    let mut hasher = blake3::Hasher::new();
    for sample in cfa {
        hasher.update(&sample.to_le_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_deterministic_and_endian_stable() {
        let a = hash_cfa(&[1, 2, 3, 65535]);
        let b = hash_cfa(&[1, 2, 3, 65535]);
        assert_eq!(a, b);
        let c = hash_cfa(&[1, 2, 3, 65534]);
        assert_ne!(a, c);
    }
}
