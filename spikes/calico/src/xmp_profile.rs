//! Adobe Camera Raw "Look" `.xmp` profiles (e.g. the user's own installed "Adobe Vivid" preset
//! under `...\CameraRaw\Settings\Adobe\Profiles\`).
//!
//! **How the container actually works** (confirmed against the real installed profiles, ADR-0021):
//! `crs:LookTable="<32-hex-digit ID>"` is *not* the table data -- it's the table's own MD5
//! fingerprint. The actual payload is a second attribute on the same `rdf:Description`,
//! `crs:Table_<ID>="<encoded>"`. That encoded string is the DNG SDK's `dng_big_table` wire format
//! (`dng_big_table.cpp`'s `ASCIItoBinary`/`DecodeFromBinary`, read as a spec reference only, never
//! vendored -- see the follow-up issue this module's own history traces): a Z85-like base85
//! variant (own alphabet, adjusted so it round-trips cleanly through XML) decodes to a 4-byte
//! little-endian uncompressed-size prefix followed by a zlib-compressed stream, which itself
//! contains a small tagged record (`dng_look_table::GetStream`'s layout, all little-endian):
//! `u32 magic(=0) | u32 version(1|2) | u32 hueDiv | u32 satDiv | u32 valDiv | (f32,f32,f32) *
//! hueDiv*satDiv*valDiv | u32 encoding | [f64 minAmount, f64 maxAmount if version==2] | [u32 flags
//! if bytes remain]`. The delta triples land directly in calico's own `HueSatMap` layout (value
//! outermost, hue middle, saturation innermost -- same order `dcp.rs`'s `ProfileLookTableData`
//! uses, per the DNG spec).
//!
//! **Correctness check**: the DNG SDK's own `RecomputeFingerprint` derives the `crs:LookTable` ID
//! by MD5-hashing a canonical re-serialization of the decoded fields (`PutStream(forFingerprint =
//! true)`, not a hash of the raw compressed bytes) -- so [`parse`] recomputes that same
//! re-serialization and compares it against the ID in the file. A wrong decode can't produce the
//! right hash, which is the strongest proof of correctness available without an LRC render.
//! Verified against all six real Adobe Raw profiles (Color/Landscape/Monochrome/Neutral/Portrait/
//! Vivid) during this research pass -- all six decode and fingerprint-match; no real file or
//! decoded table content is committed here (ADR-0003).
//!
//! What calico still doesn't apply from a Look profile: `crs:Clarity2012`, the
//! `crs:ToneCurvePV2012` point sequence, and any `crs:RGBTable`-based look (a separate
//! `dng_rgb_table` container this module doesn't decode). [`LookProfile::unsupported_settings`]
//! surfaces these instead of silently dropping them.

use flate2::read::ZlibDecoder;
use std::io::Read;
use thiserror::Error;

use crate::dcp::TableEncoding;
use crate::huesatmap::HueSatMap;

#[derive(Debug, Error)]
pub enum LookProfileError {
    #[error("xml parse error: {0}")]
    Xml(#[from] roxmltree::Error),
    #[error("no crs:LookTable property found in this .xmp")]
    NoLookTableProperty,
    #[error("crs:LookTable references ID {0}, but no matching crs:Table_{0} attribute exists")]
    TableIdNotFound(String),
    #[error("big-table decode failed: {0}")]
    BigTable(#[from] BigTableError),
    #[error(
        "decoded look table's recomputed fingerprint ({computed}) does not match its \
         crs:LookTable ID ({expected}) -- decode is wrong somewhere"
    )]
    FingerprintMismatch { expected: String, computed: String },
}

#[derive(Debug, Error)]
pub enum BigTableError {
    #[error("encoded table too short to contain a size prefix")]
    TooShort,
    #[error("zlib decompression failed: {0}")]
    Zlib(#[from] std::io::Error),
    #[error("decompressed size {actual} does not match the declared size {declared}")]
    SizeMismatch { declared: u32, actual: usize },
    #[error(
        "declared uncompressed size {declared} exceeds the {MAX_DECOMPRESSED_SIZE}-byte cap \
         (dng_big_table.h's kMaxCompressedBigTableDecodedSize)"
    )]
    DeclaredSizeTooLarge { declared: u32 },
    #[error(
        "decompressed output exceeded the {MAX_DECOMPRESSED_SIZE}-byte cap before matching its \
         declared size -- refusing to keep inflating (possible zip bomb)"
    )]
    DecompressedSizeExceededCap,
    #[error("stream too short: expected at least {needed} bytes, has {have}")]
    StreamTooShort { needed: usize, have: usize },
    #[error("unrecognized big-table magic {0} (expected 0 for a Look table)")]
    UnrecognizedMagic(u32),
    #[error("unrecognized look-table version {0} (expected 1 or 2)")]
    UnrecognizedVersion(u32),
    #[error("dimensions {hue}x{sat}x{val} exceed the DNG spec's per-axis/total sample limits")]
    DimensionsOutOfRange { hue: u32, sat: u32, val: u32 },
    #[error("unrecognized look-table encoding {0} (expected 0=Linear or 1=sRGB)")]
    UnrecognizedEncoding(u32),
}

/// DNG SDK's own limits (`dng_big_table.h`'s `kMaxHueSamples`/`kMaxSatSamples`/`kMaxValSamples`/
/// `kMaxTotalSamples`) -- a decode bug (e.g. misreading the size prefix) could otherwise walk off
/// into a huge bogus allocation before the fingerprint check ever gets a chance to reject it.
const MAX_HUE_SAMPLES: u32 = 360;
const MAX_SAT_SAMPLES: u32 = 256;
const MAX_VAL_SAMPLES: u32 = 256;
const MAX_TOTAL_SAMPLES: u32 = 36 * 32 * 16;

/// `dng_big_table.h`'s `kMaxCompressedBigTableDecodedSize`: the SDK rejects a declared
/// uncompressed size over this *before* allocating, and bounds the actual `zlib` output to it via
/// a fixed-size destination buffer. `big_table_decode` enforces the same cap on both the declared
/// size and the actual decompressed byte count, so a crafted small zlib stream with a very high
/// compression ratio can't inflate to an unbounded allocation (a "zip bomb") before either check
/// has a chance to reject it.
const MAX_DECOMPRESSED_SIZE: u32 = 128 * 1024 * 1024;

#[derive(Debug)]
pub struct LookProfile {
    pub name: String,
    pub look_table: HueSatMap,
    pub encoding: TableEncoding,
    /// Adobe Look settings this parser found but calico's pipeline doesn't apply -- e.g.
    /// `crs:Clarity2012`, `crs:ToneCurvePV2012`. Callers should warn rather than silently drop
    /// them.
    pub unsupported_settings: Vec<String>,
}

/// Base85-variant alphabet from `dng_big_table.cpp`'s `ASCIItoBinary` (a Z85-like scheme with an
/// alphabet adjusted to round-trip cleanly through XML) -- decode table indexed by
/// `byte - 32` for ASCII 32..=127; `0xFF` marks a byte that isn't part of the alphabet (skipped,
/// same as the SDK: whitespace and a handful of XML-unfriendly punctuation characters).
#[rustfmt::skip]
const BASE85_DECODE: [u8; 96] = [
    0xFF, 0x44, 0xFF, 0x54, 0x53, 0x52, 0xFF, 0x49, 0x4B, 0x4C, 0x46, 0x41, 0xFF, 0x3F, 0x3E, 0x45,
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x40, 0xFF, 0xFF, 0x42, 0xFF, 0x47,
    0x51, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2A, 0x2B, 0x2C, 0x2D, 0x2E, 0x2F, 0x30, 0x31, 0x32,
    0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3A, 0x3B, 0x3C, 0x3D, 0x4D, 0xFF, 0x4E, 0x43, 0xFF,
    0x48, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18,
    0x19, 0x1A, 0x1B, 0x1C, 0x1D, 0x1E, 0x1F, 0x20, 0x21, 0x22, 0x23, 0x4F, 0x4A, 0x50, 0xFF, 0xFF,
];

/// Decodes `dng_big_table`'s base85-variant text encoding to bytes (`ASCIItoBinary`). Bytes
/// outside the alphabet (including whitespace) are skipped, matching the SDK.
fn base85_decode(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() * 4 / 5);
    let mut phase: u32 = 0;
    let mut value: u32 = 0;

    for byte in s.bytes() {
        if !(32..=127).contains(&byte) {
            continue;
        }
        let d = BASE85_DECODE[(byte - 32) as usize];
        if d > 85 {
            continue;
        }
        phase += 1;
        match phase {
            1 => value = d as u32,
            2 => value = value.wrapping_add((d as u32).wrapping_mul(85)),
            3 => value = value.wrapping_add((d as u32).wrapping_mul(85 * 85)),
            4 => value = value.wrapping_add((d as u32).wrapping_mul(85 * 85 * 85)),
            _ => {
                // `85^4` (52,200,625) times a digit up to 84 exceeds u32::MAX -- the SDK relies on
                // plain uint32 wraparound here (well-defined in C++), so this needs `wrapping_mul`
                // too, not just the outer `wrapping_add`.
                value = value.wrapping_add((d as u32).wrapping_mul(85 * 85 * 85 * 85));
                out.extend_from_slice(&value.to_le_bytes());
                phase = 0;
            }
        }
    }

    // Trailing partial group: the SDK writes bytes 2/1/0 of `value` (in that order, into the same
    // three buffer slots) whenever 2, 3, or 4 leftover characters were seen -- net effect is just
    // the low `phase - 1` little-endian bytes of `value`.
    if phase > 1 {
        let bytes = value.to_le_bytes();
        out.extend_from_slice(&bytes[..(phase as usize - 1)]);
    }

    out
}

/// Decodes a `dng_big_table`-encoded attribute value to its uncompressed byte stream
/// (`DecodeFromString` + `DecodeFromBinary`'s compressed path).
fn big_table_decode(encoded: &str) -> Result<Vec<u8>, BigTableError> {
    let binary = base85_decode(encoded);
    if binary.len() < 5 {
        return Err(BigTableError::TooShort);
    }

    let declared_size = u32::from_le_bytes(binary[0..4].try_into().unwrap());

    if declared_size > MAX_DECOMPRESSED_SIZE {
        return Err(BigTableError::DeclaredSizeTooLarge {
            declared: declared_size,
        });
    }

    // Bound the actual decompressed byte count too, not just the declared size -- a crafted zlib
    // stream can decompress to far more than it declares (or a very high ratio to a huge amount),
    // so `read_to_end` alone would keep inflating into an unbounded `Vec` before the size check
    // below ever runs. Reading in capped chunks lets us bail out mid-stream instead.
    let mut decoder = ZlibDecoder::new(&binary[4..]);
    let mut out = Vec::new();
    let cap = MAX_DECOMPRESSED_SIZE as usize;
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let n = decoder.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        if out.len() + n > cap {
            return Err(BigTableError::DecompressedSizeExceededCap);
        }
        out.extend_from_slice(&chunk[..n]);
    }

    if out.len() != declared_size as usize {
        return Err(BigTableError::SizeMismatch {
            declared: declared_size,
            actual: out.len(),
        });
    }

    Ok(out)
}

struct StreamReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> StreamReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn need(&self, n: usize) -> Result<(), BigTableError> {
        if self.pos + n > self.data.len() {
            return Err(BigTableError::StreamTooShort {
                needed: self.pos + n,
                have: self.data.len(),
            });
        }
        Ok(())
    }

    fn u32(&mut self) -> Result<u32, BigTableError> {
        self.need(4)?;
        let v = u32::from_le_bytes(self.data[self.pos..self.pos + 4].try_into().unwrap());
        self.pos += 4;
        Ok(v)
    }

    fn f32(&mut self) -> Result<f32, BigTableError> {
        self.need(4)?;
        let v = f32::from_le_bytes(self.data[self.pos..self.pos + 4].try_into().unwrap());
        self.pos += 4;
        Ok(v)
    }

    fn f64(&mut self) -> Result<f64, BigTableError> {
        self.need(8)?;
        let v = f64::from_le_bytes(self.data[self.pos..self.pos + 8].try_into().unwrap());
        self.pos += 8;
        Ok(v)
    }

    fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }
}

struct DecodedLookTable {
    look_table: HueSatMap,
    encoding: TableEncoding,
    min_amount: f64,
    max_amount: f64,
    flags: u32,
}

/// Parses `dng_look_table::GetStream`'s layout out of an already-decompressed big-table stream.
fn parse_look_table_stream(raw: &[u8]) -> Result<DecodedLookTable, BigTableError> {
    let mut r = StreamReader::new(raw);

    let magic = r.u32()?;
    if magic != 0 {
        return Err(BigTableError::UnrecognizedMagic(magic));
    }

    let version = r.u32()?;
    if version != 1 && version != 2 {
        return Err(BigTableError::UnrecognizedVersion(version));
    }

    let hue_divisions = r.u32()?;
    let sat_divisions = r.u32()?;
    let val_divisions = r.u32()?;

    let total = hue_divisions
        .checked_mul(sat_divisions)
        .and_then(|v| v.checked_mul(val_divisions));

    if !(1..=MAX_HUE_SAMPLES).contains(&hue_divisions)
        || !(1..=MAX_SAT_SAMPLES).contains(&sat_divisions)
        || !(1..=MAX_VAL_SAMPLES).contains(&val_divisions)
        || total.is_none_or(|t| t > MAX_TOTAL_SAMPLES)
    {
        return Err(BigTableError::DimensionsOutOfRange {
            hue: hue_divisions,
            sat: sat_divisions,
            val: val_divisions,
        });
    }

    let count = total.unwrap() as usize;
    let mut data = Vec::with_capacity(count);
    for _ in 0..count {
        let hue_shift = r.f32()?;
        let sat_scale = r.f32()?;
        let val_scale = r.f32()?;
        data.push([hue_shift, sat_scale, val_scale]);
    }

    let encoding_raw = r.u32()?;
    let encoding = match encoding_raw {
        0 => TableEncoding::Linear,
        1 => TableEncoding::Srgb,
        other => return Err(BigTableError::UnrecognizedEncoding(other)),
    };

    let (min_amount, max_amount) = if version != 1 {
        (r.f64()?, r.f64()?)
    } else {
        (1.0, 1.0)
    };

    let flags = if r.remaining() >= 4 { r.u32()? } else { 0 };

    Ok(DecodedLookTable {
        look_table: HueSatMap {
            hue_divisions: hue_divisions as usize,
            sat_divisions: sat_divisions as usize,
            val_divisions: val_divisions as usize,
            data,
        },
        encoding,
        min_amount,
        max_amount,
        flags,
    })
}

/// Recomputes `dng_look_table::PutStream(forFingerprint = true)`'s canonical re-serialization and
/// MD5-hashes it (`dng_big_table::ComputeFingerprint`'s `dng_md5_printer_le_stream`), returning
/// the result as the same uppercase-hex form `crs:LookTable`'s ID uses.
fn recompute_fingerprint(decoded: &DecodedLookTable) -> String {
    let mut buf = Vec::new();
    buf.extend_from_slice(&0u32.to_le_bytes()); // btt_LookTable

    // `dng_look_table::PutStream` (the SDK function this mirrors) always re-derives the version
    // tag from the current amount fields, discarding whatever version was on the wire when read --
    // there is no stored "version" state anywhere in the SDK's own data model, only this
    // computed-at-serialization-time value. So this must infer version the same way, not use
    // whatever the source stream's version field said.
    let version: u32 = if decoded.min_amount != 1.0 || decoded.max_amount != 1.0 {
        2
    } else {
        1
    };
    buf.extend_from_slice(&version.to_le_bytes());

    buf.extend_from_slice(&(decoded.look_table.hue_divisions as u32).to_le_bytes());
    buf.extend_from_slice(&(decoded.look_table.sat_divisions as u32).to_le_bytes());
    buf.extend_from_slice(&(decoded.look_table.val_divisions as u32).to_le_bytes());

    for entry in &decoded.look_table.data {
        for component in entry {
            buf.extend_from_slice(&component.to_le_bytes());
        }
    }

    let encoding_raw: u32 = match decoded.encoding {
        TableEncoding::Linear => 0,
        TableEncoding::Srgb => 1,
    };
    buf.extend_from_slice(&encoding_raw.to_le_bytes());

    if version != 1 {
        buf.extend_from_slice(&decoded.min_amount.to_le_bytes());
        buf.extend_from_slice(&decoded.max_amount.to_le_bytes());
    }

    if decoded.flags != 0 {
        buf.extend_from_slice(&decoded.flags.to_le_bytes());
    }

    let digest = md5(&buf);
    digest
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<String>()
}

/// Minimal MD5 (RFC 1321) -- only used to self-check a Look table's decode against its own
/// `crs:LookTable` fingerprint; not exposed outside this module and not a general-purpose hash.
fn md5(input: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613,
        0xfd469501, 0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193,
        0xa679438e, 0x49b40821, 0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d,
        0x02441453, 0xd8a1e681, 0xe7d3fbc8, 0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed,
        0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, 0xfffa3942, 0x8771f681, 0x6d9d6122,
        0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, 0x289b7ec6, 0xeaa127fa,
        0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, 0xf4292244,
        0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb,
        0xeb86d391,
    ];

    let mut a0: u32 = 0x67452301;
    let mut b0: u32 = 0xefcdab89;
    let mut c0: u32 = 0x98badcfe;
    let mut d0: u32 = 0x10325476;

    let mut msg = input.to_vec();
    let orig_len_bits = (input.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&orig_len_bits.to_le_bytes());

    #[allow(clippy::chunks_exact_to_as_chunks)] // `as_chunks` isn't stable on this repo's MSRV.
    for chunk in msg.chunks_exact(64) {
        let mut m = [0u32; 16];
        for (i, word) in m.iter_mut().enumerate() {
            *word = u32::from_le_bytes(chunk[i * 4..i * 4 + 4].try_into().unwrap());
        }

        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);

        for i in 0..64 {
            let (f, g) = match i {
                0..=15 => ((b & c) | (!b & d), i),
                16..=31 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };

            let f = f.wrapping_add(a).wrapping_add(K[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(S[i]));
        }

        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }

    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&a0.to_le_bytes());
    out[4..8].copy_from_slice(&b0.to_le_bytes());
    out[8..12].copy_from_slice(&c0.to_le_bytes());
    out[12..16].copy_from_slice(&d0.to_le_bytes());
    out
}

/// Reads `<rdf:Alt><rdf:li xml:lang="...">text</rdf:li>...</rdf:Alt>` (an XMP "language
/// alternative" element, used for `crs:Name`) and returns the `x-default` entry, or the first
/// entry if none is tagged `x-default`.
fn read_alt_text(node: roxmltree::Node) -> Option<String> {
    let alt = node.children().find(|n| n.tag_name().name() == "Alt")?;
    let mut first: Option<String> = None;
    for li in alt.children().filter(|n| n.tag_name().name() == "li") {
        let text = li.text().unwrap_or("").to_string();
        if li.attribute("lang") == Some("x-default") {
            return Some(text);
        }
        if first.is_none() {
            first = Some(text);
        }
    }
    first
}

/// Attempts to parse a `.xmp` Look profile's embedded LookTable. On any decode failure, callers
/// should fall back to measuring against the base DCP profile (Adobe Standard/Color) rather than
/// guessing at a partially-decoded table.
pub fn parse(xmp_text: &str) -> Result<LookProfile, LookProfileError> {
    let doc = roxmltree::Document::parse(xmp_text)?;

    // Two passes rather than one interleaved pass: `crs:LookTable` (the ID) and its matching
    // `crs:Table_<id>` (the payload) are ordinary XML attributes with no ordering guarantee --
    // XMP/RDF allows a subject's properties to be split across more than one `rdf:Description`
    // about the same `rdf:about`, and even within one node, attribute order in the source text
    // isn't guaranteed to put `LookTable` before `Table_<id>`. Matching them as they're seen in a
    // single pass would let an unrelated `LookTable` value encountered later silently overwrite
    // `table_id` after the right payload was already captured. Finding the ID first and only then
    // searching for its payload removes that ordering dependency entirely.
    let table_id = doc.descendants().find_map(|node| {
        node.attributes()
            .find(|attr| attr.name() == "LookTable")
            .map(|attr| attr.value().to_string())
    });

    let mut name = String::from("(unnamed look)");
    let mut table_payload: Option<String> = None;
    let mut unsupported_settings = Vec::new();

    for node in doc.descendants() {
        for attr in node.attributes() {
            match attr.name() {
                name if name.starts_with("Table_") => {
                    if Some(&name["Table_".len()..]) == table_id.as_deref() {
                        table_payload = Some(attr.value().to_string());
                    }
                }
                "Clarity2012" if attr.value() != "0" => {
                    unsupported_settings.push(format!("Clarity2012={}", attr.value()));
                }
                // `crs:RGBTable` is a `dng_big_table` subclass exactly like `crs:LookTable` --
                // written as `crs:RGBTable="<id>"` + a matching `crs:Table_<id>` payload
                // attribute (`dng_big_table::WriteToXMP`/`ReadFromXMP` are generic over the
                // property name), never as an XML element. The `"RGBTable"` element-tag match
                // below is kept as a defensive fallback in case some producer writes it that way
                // instead, but this attribute case is the one that actually matches the SDK.
                "RGBTable" => {
                    unsupported_settings.push(format!("RGBTable={}", attr.value()));
                }
                _ => {}
            }
        }
        match node.tag_name().name() {
            "Name" => {
                if let Some(text) = read_alt_text(node) {
                    if !text.is_empty() {
                        name = text;
                    }
                }
            }
            "ToneCurvePV2012" => {
                unsupported_settings.push("ToneCurvePV2012".to_string());
            }
            "RGBTable" => {
                unsupported_settings.push("RGBTable".to_string());
            }
            _ => {}
        }
    }

    // A LookTable ID with no matching Table_<id> payload attribute is a distinct, more specific
    // failure than "no property at all" -- surface it as such rather than folding it into
    // `NoLookTableProperty`.
    let table_id = table_id.ok_or(LookProfileError::NoLookTableProperty)?;
    let payload =
        table_payload.ok_or_else(|| LookProfileError::TableIdNotFound(table_id.clone()))?;

    let raw = big_table_decode(&payload)?;
    let decoded = parse_look_table_stream(&raw)?;

    let computed_fingerprint = recompute_fingerprint(&decoded);
    if !computed_fingerprint.eq_ignore_ascii_case(&table_id) {
        return Err(LookProfileError::FingerprintMismatch {
            expected: table_id,
            computed: computed_fingerprint,
        });
    }

    Ok(LookProfile {
        name,
        look_table: decoded.look_table,
        encoding: decoded.encoding,
        unsupported_settings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write;

    /// Mirrors `dng_big_table.cpp`'s `BinaryToASCII` encode table (the inverse of
    /// `BASE85_DECODE`), for building synthetic fixtures.
    const BASE85_ENCODE: [u8; 85] =
        *b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ.-:+=^!/*?`'|()[]{}@%$#";

    fn base85_encode(data: &[u8]) -> String {
        let mut out = String::new();
        for chunk in data.chunks(4) {
            let mut buf = [0u8; 4];
            buf[..chunk.len()].copy_from_slice(chunk);
            let mut value = u32::from_le_bytes(buf);
            let mut digits = [0u8; 5];
            for d in digits.iter_mut() {
                *d = (value % 85) as u8;
                value /= 85;
            }
            let n = chunk.len() + 1;
            for &d in digits.iter().take(n) {
                out.push(BASE85_ENCODE[d as usize] as char);
            }
        }
        out
    }

    fn encode_look_table_stream(
        hue: u32,
        sat: u32,
        val: u32,
        data: &[[f32; 3]],
        encoding: u32,
        version2: Option<(f64, f64)>,
        flags: Option<u32>,
    ) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&0u32.to_le_bytes());
        let version: u32 = if version2.is_some() { 2 } else { 1 };
        buf.extend_from_slice(&version.to_le_bytes());
        buf.extend_from_slice(&hue.to_le_bytes());
        buf.extend_from_slice(&sat.to_le_bytes());
        buf.extend_from_slice(&val.to_le_bytes());
        for entry in data {
            for c in entry {
                buf.extend_from_slice(&c.to_le_bytes());
            }
        }
        buf.extend_from_slice(&encoding.to_le_bytes());
        if let Some((min_amt, max_amt)) = version2 {
            buf.extend_from_slice(&min_amt.to_le_bytes());
            buf.extend_from_slice(&max_amt.to_le_bytes());
        }
        if let Some(flags) = flags {
            buf.extend_from_slice(&flags.to_le_bytes());
        }
        buf
    }

    fn big_table_encode(raw: &[u8]) -> String {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(raw).unwrap();
        let compressed = encoder.finish().unwrap();

        let mut binary = Vec::new();
        binary.extend_from_slice(&(raw.len() as u32).to_le_bytes());
        binary.extend_from_slice(&compressed);

        base85_encode(&binary)
    }

    fn make_xmp(look_table_id: &str, table_attr: &str, name_el: &str) -> String {
        format!(
            r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/" crs:LookTable="{look_table_id}" {table_attr}>{name_el}</rdf:Description></rdf:RDF></x:xmpmeta>"#
        )
    }

    fn synthetic_hue_sat_map(hue: u32, sat: u32, val: u32) -> Vec<[f32; 3]> {
        let mut data = Vec::new();
        for v in 0..val {
            for h in 0..hue {
                for s in 0..sat {
                    data.push([
                        (h as f32) * 0.5 - 3.0,
                        1.0 + (s as f32) * 0.01,
                        1.0 + (v as f32) * 0.02,
                    ]);
                }
            }
        }
        data
    }

    fn synthetic_valid_xmp(name: &str) -> String {
        let hue = 4;
        let sat = 2;
        let val = 2;
        let data = synthetic_hue_sat_map(hue, sat, val);
        let stream = encode_look_table_stream(hue, sat, val, &data, 0, None, None);
        let encoded = big_table_encode(&stream);

        let fake = DecodedLookTable {
            look_table: HueSatMap {
                hue_divisions: hue as usize,
                sat_divisions: sat as usize,
                val_divisions: val as usize,
                data,
            },
            encoding: TableEncoding::Linear,
            min_amount: 1.0,
            max_amount: 1.0,
            flags: 0,
        };
        let id = recompute_fingerprint(&fake);

        make_xmp(
            &id,
            &format!(r#"crs:Table_{id}="{encoded}""#),
            &format!(
                r#"<crs:Name><rdf:Alt><rdf:li xml:lang="x-default">{name}</rdf:li></rdf:Alt></crs:Name>"#
            ),
        )
    }

    #[test]
    fn synthetic_round_trip_decodes_and_fingerprint_matches() {
        let xmp = synthetic_valid_xmp("Test Look");
        let look = parse(&xmp).expect("should decode");
        assert_eq!(look.name, "Test Look");
        assert_eq!(look.look_table.hue_divisions, 4);
        assert_eq!(look.look_table.sat_divisions, 2);
        assert_eq!(look.look_table.val_divisions, 2);
        assert_eq!(look.encoding, TableEncoding::Linear);
        assert!(look.unsupported_settings.is_empty());
    }

    #[test]
    fn version2_amount_fields_round_trip() {
        let hue = 2;
        let sat = 2;
        let val = 2;
        let data = synthetic_hue_sat_map(hue, sat, val);
        let stream = encode_look_table_stream(hue, sat, val, &data, 1, Some((0.0, 1.5)), None);
        let encoded = big_table_encode(&stream);

        let fake = DecodedLookTable {
            look_table: HueSatMap {
                hue_divisions: hue as usize,
                sat_divisions: sat as usize,
                val_divisions: val as usize,
                data,
            },
            encoding: TableEncoding::Srgb,
            min_amount: 0.0,
            max_amount: 1.5,
            flags: 0,
        };
        let id = recompute_fingerprint(&fake);

        let xmp = make_xmp(&id, &format!(r#"crs:Table_{id}="{encoded}""#), "");
        let look = parse(&xmp).expect("should decode");
        assert_eq!(look.encoding, TableEncoding::Srgb);
    }

    #[test]
    fn trailing_flags_field_is_read_and_included_in_fingerprint() {
        let hue = 2;
        let sat = 2;
        let val = 2;
        let data = synthetic_hue_sat_map(hue, sat, val);
        let stream = encode_look_table_stream(hue, sat, val, &data, 0, None, Some(1));
        let encoded = big_table_encode(&stream);

        let fake = DecodedLookTable {
            look_table: HueSatMap {
                hue_divisions: hue as usize,
                sat_divisions: sat as usize,
                val_divisions: val as usize,
                data,
            },
            encoding: TableEncoding::Linear,
            min_amount: 1.0,
            max_amount: 1.0,
            flags: 1,
        };
        let id = recompute_fingerprint(&fake);

        let xmp = make_xmp(&id, &format!(r#"crs:Table_{id}="{encoded}""#), "");
        parse(&xmp).expect("should decode with trailing flags field");
    }

    #[test]
    fn no_look_table_property_is_reported_clearly() {
        let xmp = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description crs:Name="Adobe Vivid" xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"/></rdf:RDF></x:xmpmeta>"#;
        let err = parse(xmp).unwrap_err();
        assert!(matches!(err, LookProfileError::NoLookTableProperty));
    }

    #[test]
    fn malformed_xml_is_reported_as_xml_error() {
        let err = parse("<not valid xml").unwrap_err();
        assert!(matches!(err, LookProfileError::Xml(_)));
    }

    #[test]
    fn look_table_id_with_no_matching_table_attribute_is_reported_clearly() {
        let xmp = make_xmp("DEADBEEFDEADBEEFDEADBEEFDEADBEEF", "", "");
        let err = parse(&xmp).unwrap_err();
        assert!(
            matches!(err, LookProfileError::TableIdNotFound(ref id) if id == "DEADBEEFDEADBEEFDEADBEEFDEADBEEF")
        );
    }

    #[test]
    fn fingerprint_mismatch_is_detected() {
        let hue = 2;
        let sat = 2;
        let val = 2;
        let data = synthetic_hue_sat_map(hue, sat, val);
        let stream = encode_look_table_stream(hue, sat, val, &data, 0, None, None);
        let encoded = big_table_encode(&stream);

        // Deliberately wrong ID.
        let wrong_id = "00000000000000000000000000000000";
        let xmp = make_xmp(
            wrong_id,
            &format!(r#"crs:Table_{wrong_id}="{encoded}""#),
            "",
        );
        let err = parse(&xmp).unwrap_err();
        assert!(matches!(err, LookProfileError::FingerprintMismatch { .. }));
    }

    #[test]
    fn bad_magic_is_reported() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&1u32.to_le_bytes()); // wrong magic
        stream.extend_from_slice(&1u32.to_le_bytes());
        let encoded = big_table_encode(&stream);
        let id = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let xmp = make_xmp(id, &format!(r#"crs:Table_{id}="{encoded}""#), "");
        let err = parse(&xmp).unwrap_err();
        assert!(matches!(
            err,
            LookProfileError::BigTable(BigTableError::UnrecognizedMagic(1))
        ));
    }

    #[test]
    fn truncated_stream_is_reported() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&0u32.to_le_bytes());
        stream.extend_from_slice(&1u32.to_le_bytes());
        stream.extend_from_slice(&4u32.to_le_bytes()); // hueDivisions, then truncated
        let encoded = big_table_encode(&stream);
        let id = "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";
        let xmp = make_xmp(id, &format!(r#"crs:Table_{id}="{encoded}""#), "");
        let err = parse(&xmp).unwrap_err();
        assert!(matches!(
            err,
            LookProfileError::BigTable(BigTableError::StreamTooShort { .. })
        ));
    }

    #[test]
    fn out_of_range_dimensions_are_rejected() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&0u32.to_le_bytes());
        stream.extend_from_slice(&1u32.to_le_bytes());
        stream.extend_from_slice(&1000u32.to_le_bytes()); // hueDivisions way over the DNG limit
        stream.extend_from_slice(&1u32.to_le_bytes());
        stream.extend_from_slice(&1u32.to_le_bytes());
        let encoded = big_table_encode(&stream);
        let id = "CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC";
        let xmp = make_xmp(id, &format!(r#"crs:Table_{id}="{encoded}""#), "");
        let err = parse(&xmp).unwrap_err();
        assert!(matches!(
            err,
            LookProfileError::BigTable(BigTableError::DimensionsOutOfRange { .. })
        ));
    }

    #[test]
    fn table_attribute_before_look_table_id_in_source_order_still_matches() {
        // crs:Table_<id> written *before* crs:LookTable on the same node -- attribute order in
        // the source text isn't guaranteed, so this must still resolve correctly rather than
        // silently missing the payload (the two-pass id-then-payload lookup in `parse` exists for
        // exactly this).
        let hue = 2;
        let sat = 2;
        let val = 2;
        let data = synthetic_hue_sat_map(hue, sat, val);
        let stream = encode_look_table_stream(hue, sat, val, &data, 0, None, None);
        let encoded = big_table_encode(&stream);

        let fake = DecodedLookTable {
            look_table: HueSatMap {
                hue_divisions: hue as usize,
                sat_divisions: sat as usize,
                val_divisions: val as usize,
                data,
            },
            encoding: TableEncoding::Linear,
            min_amount: 1.0,
            max_amount: 1.0,
            flags: 0,
        };
        let id = recompute_fingerprint(&fake);

        let xmp = format!(
            r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/" crs:Table_{id}="{encoded}" crs:LookTable="{id}"/></rdf:RDF></x:xmpmeta>"#
        );
        parse(&xmp).expect("should decode regardless of attribute order");
    }

    #[test]
    fn declared_size_over_cap_is_rejected_before_decompressing() {
        let mut binary = Vec::new();
        binary.extend_from_slice(&(MAX_DECOMPRESSED_SIZE + 1).to_le_bytes());
        binary.extend_from_slice(&[0u8; 8]); // any bytes -- never reached
        let encoded = base85_encode(&binary);
        let err = big_table_decode(&encoded).unwrap_err();
        assert!(matches!(err, BigTableError::DeclaredSizeTooLarge { .. }));
    }

    /// A crafted zlib stream that lies about its declared size (small) but actually decompresses
    /// to more than the cap -- proves `big_table_decode` bails out mid-stream instead of fully
    /// materializing an unbounded allocation (the zip-bomb DoS this cap exists to prevent).
    #[test]
    fn decompressed_output_over_cap_is_rejected_mid_stream() {
        let huge_zeros = vec![0u8; MAX_DECOMPRESSED_SIZE as usize + 4096];

        let mut zlib_encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        zlib_encoder.write_all(&huge_zeros).unwrap();
        let compressed = zlib_encoder.finish().unwrap();

        let mut binary = Vec::new();
        binary.extend_from_slice(&100u32.to_le_bytes()); // lies -- actual output is far bigger
        binary.extend_from_slice(&compressed);
        let encoded = base85_encode(&binary);

        let err = big_table_decode(&encoded).unwrap_err();
        assert!(matches!(err, BigTableError::DecompressedSizeExceededCap));
    }

    #[test]
    fn unsupported_clarity_and_tone_curve_settings_are_surfaced() {
        let hue = 2;
        let sat = 2;
        let val = 2;
        let data = synthetic_hue_sat_map(hue, sat, val);
        let stream = encode_look_table_stream(hue, sat, val, &data, 0, None, None);
        let encoded = big_table_encode(&stream);

        let fake = DecodedLookTable {
            look_table: HueSatMap {
                hue_divisions: hue as usize,
                sat_divisions: sat as usize,
                val_divisions: val as usize,
                data,
            },
            encoding: TableEncoding::Linear,
            min_amount: 1.0,
            max_amount: 1.0,
            flags: 0,
        };
        let id = recompute_fingerprint(&fake);

        let xmp = format!(
            r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/" crs:LookTable="{id}" crs:Table_{id}="{encoded}" crs:Clarity2012="+10"><crs:ToneCurvePV2012><rdf:Seq><rdf:li>0, 0</rdf:li></rdf:Seq></crs:ToneCurvePV2012></rdf:Description></rdf:RDF></x:xmpmeta>"#
        );
        let look = parse(&xmp).expect("should decode despite unsupported settings");
        assert!(look
            .unsupported_settings
            .iter()
            .any(|s| s.starts_with("Clarity2012")));
        assert!(look
            .unsupported_settings
            .iter()
            .any(|s| s == "ToneCurvePV2012"));
    }

    #[test]
    fn rgbtable_attribute_is_surfaced_as_unsupported() {
        // crs:RGBTable is a dng_big_table subclass exactly like crs:LookTable -- written as an
        // attribute (`crs:RGBTable="<id>"` + a matching `crs:Table_<id>` payload attribute), not
        // as an XML element. This exercises that real-world representation, not the element-tag
        // fallback.
        let hue = 2;
        let sat = 2;
        let val = 2;
        let data = synthetic_hue_sat_map(hue, sat, val);
        let stream = encode_look_table_stream(hue, sat, val, &data, 0, None, None);
        let encoded = big_table_encode(&stream);

        let fake = DecodedLookTable {
            look_table: HueSatMap {
                hue_divisions: hue as usize,
                sat_divisions: sat as usize,
                val_divisions: val as usize,
                data,
            },
            encoding: TableEncoding::Linear,
            min_amount: 1.0,
            max_amount: 1.0,
            flags: 0,
        };
        let id = recompute_fingerprint(&fake);

        let xmp = format!(
            r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/" crs:LookTable="{id}" crs:Table_{id}="{encoded}" crs:RGBTable="SOMEOTHERID"/></rdf:RDF></x:xmpmeta>"#
        );
        let look = parse(&xmp).expect("should decode despite an unsupported RGBTable attribute");
        assert!(look
            .unsupported_settings
            .iter()
            .any(|s| s == "RGBTable=SOMEOTHERID"));
    }

    /// Local-only proof against the user's own real, installed Adobe Raw "Look" profiles -- never
    /// runs in CI (ADR-0003: no real Adobe `.xmp` is ever committed here). Set
    /// `NICTI_LOOK_XMP_DIR` to a directory of `.xmp` Look profiles (e.g. the Windows
    /// `...\CameraRaw\Settings\Adobe\Profiles\Adobe Raw` folder, reachable from WSL under
    /// `/mnt/c/...`) and run with `cargo test -p calico -- --ignored`. Verified during this
    /// research pass against all six real Adobe Raw profiles (Color, Landscape, Monochrome,
    /// Neutral, Portrait, Vivid) -- every one decodes and fingerprint-matches.
    #[test]
    #[ignore]
    fn real_installed_look_profiles_decode_and_fingerprint_match() {
        let dir = match std::env::var("NICTI_LOOK_XMP_DIR") {
            Ok(dir) => dir,
            Err(_) => {
                panic!("set NICTI_LOOK_XMP_DIR to a directory of real Adobe Look .xmp profiles")
            }
        };

        let mut checked = 0;
        for entry in std::fs::read_dir(&dir).expect("read NICTI_LOOK_XMP_DIR") {
            let entry = entry.expect("dir entry");
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("xmp") {
                continue;
            }
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
            match parse(&text) {
                Ok(look) => {
                    eprintln!(
                        "{}: decoded '{}' ({}x{}x{}), unsupported: {:?}",
                        path.display(),
                        look.name,
                        look.look_table.hue_divisions,
                        look.look_table.sat_divisions,
                        look.look_table.val_divisions,
                        look.unsupported_settings
                    );
                    checked += 1;
                }
                Err(e) => panic!("{}: failed to decode: {e}", path.display()),
            }
        }
        assert!(checked > 0, "no .xmp files found under {dir}");
    }

    #[test]
    fn md5_matches_known_vectors() {
        // RFC 1321 test vectors.
        assert_eq!(
            md5(b""),
            [
                0xd4, 0x1d, 0x8c, 0xd9, 0x8f, 0x00, 0xb2, 0x04, 0xe9, 0x80, 0x09, 0x98, 0xec, 0xf8,
                0x42, 0x7e
            ]
        );
        assert_eq!(
            md5(b"abc"),
            [
                0x90, 0x01, 0x50, 0x98, 0x3c, 0xd2, 0x4f, 0xb0, 0xd6, 0x96, 0x3f, 0x7d, 0x28, 0xe1,
                0x7f, 0x72
            ]
        );
        assert_eq!(
            md5(b"message digest"),
            [
                0xf9, 0x6b, 0x69, 0x7d, 0x7c, 0xb7, 0x93, 0x8d, 0x52, 0x5a, 0x2f, 0x31, 0xaa, 0xf1,
                0x61, 0xd0
            ]
        );
        assert_eq!(
            md5(b"abcdefghijklmnopqrstuvwxyz"),
            [
                0xc3, 0xfc, 0xd3, 0xd7, 0x61, 0x92, 0xe4, 0x00, 0x7d, 0xfb, 0x49, 0x6c, 0xca, 0x67,
                0xe1, 0x3b
            ]
        );
        // 80 bytes -- exercises the multi-block path (padding + carrying a0/b0/c0/d0 across
        // chunks), unlike every vector above (all <= 55 bytes, single-block only).
        assert_eq!(
            md5(
                b"12345678901234567890123456789012345678901234567890123456789012345678901234567890"
            ),
            [
                0x57, 0xed, 0xf4, 0xa2, 0x2b, 0xe3, 0xc9, 0x55, 0xac, 0x49, 0xda, 0x2e, 0x21, 0x07,
                0xb6, 0x7a
            ]
        );
    }
}
