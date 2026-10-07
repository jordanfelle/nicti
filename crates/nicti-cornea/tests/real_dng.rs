//! DNG lens-opcode plumbing end to end (#428): LibRaw reads `OpcodeList3` (tag 51022) out of the
//! file, the shim hands the bytes across the FFI, and `nicti_iris` parses what arrived.
//!
//! Two tests:
//! - `a_synthetic_dng_round_trips_its_lens_opcodes_through_libraw` builds a tiny uncompressed CFA
//!   DNG in a temp dir (no binary fixture in a public repo) with a known profile injected, decodes
//!   it through the real shim and checks the profile that comes out. It runs whenever the `libraw`
//!   feature does.
//! - `real_dngs_decode_and_any_lens_opcodes_parse` runs over a user-supplied directory
//!   (`NICTI_TEST_REAL_DNG_DIR`), skipped when unset, like the repo's other real-file tests.
#![cfg(feature = "libraw")]

use std::path::PathBuf;

use nicti_cornea::{LibRawDecoder, RawDecoder};
use nicti_iris::dng::{parse_opcode_list3, write_opcode_list3};
use nicti_iris::{LensModel, Vignette, Warp};

fn known_profile() -> LensModel {
    LensModel {
        warp: Some(Warp {
            planes: vec![
                [1.0010, 0.012, -0.004, 0.0, 0.0003, -0.0002],
                [1.0, 0.010, -0.004, 0.0, 0.0003, -0.0002],
                [0.9990, 0.008, -0.004, 0.0, 0.0003, -0.0002],
            ],
            center: [0.52, 0.48],
        }),
        vignette: Some(Vignette {
            k: [0.35, -0.12, 0.04, 0.0, 0.0],
            center: [0.5, 0.5],
        }),
    }
}

enum Val {
    /// Fits in the 4-byte value field.
    Inline([u8; 4]),
    /// Laid out after the IFD; the entry holds its offset.
    Out(Vec<u8>),
    /// The offset of the pixel strip, known only once everything else is laid out.
    StripOffset,
}

/// Little-endian TIFF/DNG writer, just enough for one uncompressed 16-bit RGGB image in IFD0.
#[derive(Default)]
struct Dng {
    entries: Vec<(u16, u16, u32, Val)>,
}

impl Dng {
    fn put(&mut self, tag: u16, ty: u16, count: u32, bytes: Vec<u8>) {
        let val = if bytes.len() <= 4 {
            let mut inline = [0u8; 4];
            inline[..bytes.len()].copy_from_slice(&bytes);
            Val::Inline(inline)
        } else {
            Val::Out(bytes)
        };
        self.entries.push((tag, ty, count, val));
    }

    fn short(&mut self, tag: u16, v: &[u16]) {
        let bytes = v.iter().flat_map(|x| x.to_le_bytes()).collect();
        self.put(tag, 3, v.len() as u32, bytes);
    }

    fn long(&mut self, tag: u16, v: &[u32]) {
        let bytes = v.iter().flat_map(|x| x.to_le_bytes()).collect();
        self.put(tag, 4, v.len() as u32, bytes);
    }

    fn raw(&mut self, tag: u16, ty: u16, v: &[u8]) {
        self.put(tag, ty, v.len() as u32, v.to_vec());
    }

    fn rationals(&mut self, tag: u16, ty: u16, v: &[(i32, i32)]) {
        let bytes = v
            .iter()
            .flat_map(|(n, d)| [n.to_le_bytes(), d.to_le_bytes()].concat())
            .collect();
        self.put(tag, ty, v.len() as u32, bytes);
    }

    fn strip_offsets(&mut self) {
        self.entries.push((0x0111, 4, 1, Val::StripOffset));
    }

    /// Serialises the file: header, IFD0 (sorted by tag), out-of-line values, then `strip`.
    fn finish(mut self, strip: &[u8]) -> Vec<u8> {
        self.entries.sort_by_key(|e| e.0);
        let ifd_len = 2 + self.entries.len() * 12 + 4;
        let mut cursor = (8 + ifd_len) as u32;
        let mut offsets = Vec::new();
        for (_, _, _, val) in &self.entries {
            if let Val::Out(bytes) = val {
                offsets.push(cursor);
                cursor += (bytes.len() + bytes.len() % 2) as u32;
            }
        }
        let strip_off = cursor;
        let mut out = b"II*\0".to_vec();
        out.extend_from_slice(&8u32.to_le_bytes());
        out.extend_from_slice(&(self.entries.len() as u16).to_le_bytes());
        let mut next_out = offsets.iter();
        for (tag, ty, count, val) in &self.entries {
            out.extend_from_slice(&tag.to_le_bytes());
            out.extend_from_slice(&ty.to_le_bytes());
            out.extend_from_slice(&count.to_le_bytes());
            let value = match val {
                Val::Inline(b) => *b,
                Val::Out(_) => next_out.next().unwrap().to_le_bytes(),
                Val::StripOffset => strip_off.to_le_bytes(),
            };
            out.extend_from_slice(&value);
        }
        out.extend_from_slice(&0u32.to_le_bytes());
        for (_, _, _, val) in &self.entries {
            if let Val::Out(bytes) = val {
                out.extend_from_slice(bytes);
                if bytes.len() % 2 == 1 {
                    out.push(0);
                }
            }
        }
        assert_eq!(out.len() as u32, strip_off);
        out.extend_from_slice(strip);
        out
    }
}

const W: u32 = 128;
const H: u32 = 96;

/// A 128x96 uncompressed 16-bit RGGB DNG whose `OpcodeList3` is `opcodes` (when given).
fn synthetic_dng(opcodes: Option<&[u8]>) -> Vec<u8> {
    let mut pixels = Vec::with_capacity((W * H * 2) as usize);
    for y in 0..H {
        for x in 0..W {
            // A smooth gradient with a little per-CFA-site offset, well inside 16 bits.
            let v = 4000 + (x * 200 + y * 120) as u16 + ((x & 1) + (y & 1) * 2) as u16 * 50;
            pixels.extend_from_slice(&v.to_le_bytes());
        }
    }
    let mut d = Dng::default();
    d.long(0x00FE, &[0]); // NewSubfileType: the full-resolution image
    d.long(0x0100, &[W]);
    d.long(0x0101, &[H]);
    d.short(0x0102, &[16]); // BitsPerSample
    d.short(0x0103, &[1]); // Compression: none
    d.short(0x0106, &[32803]); // Photometric: CFA
    d.raw(0x010F, 2, b"Test\0"); // Make
    d.raw(0x0110, 2, b"Synthetic\0"); // Model
    d.strip_offsets();
    d.short(0x0115, &[1]); // SamplesPerPixel
    d.long(0x0116, &[H]); // RowsPerStrip
    d.long(0x0117, &[W * H * 2]); // StripByteCounts
    d.short(0x828D, &[2, 2]); // CFARepeatPatternDim
    d.raw(0x828E, 1, &[0, 1, 1, 2]); // CFAPattern: RGGB
    d.raw(0xC612, 1, &[1, 4, 0, 0]); // DNGVersion
    d.raw(0xC614, 2, b"Test Synthetic\0"); // UniqueCameraModel
    d.long(0xC61D, &[65535]); // WhiteLevel
    d.rationals(
        0xC621, // ColorMatrix1 (XYZ -> camera)
        10,
        &[
            (6, 10),
            (2, 10),
            (1, 10),
            (2, 10),
            (7, 10),
            (1, 10),
            (1, 10),
            (1, 10),
            (8, 10),
        ],
    );
    d.rationals(0xC628, 5, &[(1, 2), (1, 1), (3, 5)]); // AsShotNeutral
    d.short(0xC65A, &[21]); // CalibrationIlluminant1: D65
    if let Some(blob) = opcodes {
        d.raw(0xC74E, 7, blob); // OpcodeList3
    }
    d.finish(&pixels)
}

#[test]
fn a_synthetic_dng_round_trips_its_lens_opcodes_through_libraw() {
    let dir = tempfile::tempdir().unwrap();
    let with = dir.path().join("with.dng");
    let without = dir.path().join("without.dng");
    std::fs::write(
        &with,
        synthetic_dng(Some(&write_opcode_list3(&known_profile()))),
    )
    .unwrap();
    std::fs::write(&without, synthetic_dng(None)).unwrap();

    let frame = LibRawDecoder
        .decode_linear(&with)
        .expect("LibRaw decodes the synthetic DNG");
    assert_eq!((frame.width, frame.height), (W, H));
    let blob = frame
        .dng_opcode_list3
        .as_deref()
        .expect("the opcode bytes cross the FFI");
    assert_eq!(parse_opcode_list3(blob), Some(known_profile()));

    // The same file without the tag: decodes identically and reports no profile.
    let plain = LibRawDecoder.decode_linear(&without).unwrap();
    assert!(plain.dng_opcode_list3.is_none());
    assert_eq!(
        plain.pixels, frame.pixels,
        "opcodes change no decoded pixel"
    );
}

fn dng_files() -> Option<Vec<PathBuf>> {
    let dir = PathBuf::from(std::env::var_os("NICTI_TEST_REAL_DNG_DIR")?);
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("dng")))
        .collect();
    files.sort();
    Some(files)
}

#[test]
fn real_dngs_decode_and_any_lens_opcodes_parse() {
    let Some(files) = dng_files() else {
        eprintln!("NICTI_TEST_REAL_DNG_DIR not set, skipping");
        return;
    };
    assert!(
        !files.is_empty(),
        "NICTI_TEST_REAL_DNG_DIR holds no .dng files"
    );
    for path in files {
        let frame = LibRawDecoder
            .decode_linear(&path)
            .unwrap_or_else(|e| panic!("decoding {}: {e}", path.display()));
        assert_eq!(
            frame.pixels.len(),
            frame.width as usize * frame.height as usize * 3
        );
        let model = frame
            .dng_opcode_list3
            .as_deref()
            .and_then(parse_opcode_list3);
        eprintln!(
            "{}: {}x{}, opcode list {} bytes, (warp, vignette) = {:?}",
            path.display(),
            frame.width,
            frame.height,
            frame.dng_opcode_list3.as_ref().map_or(0, Vec::len),
            model
                .as_ref()
                .map(|m| (m.warp.is_some(), m.vignette.is_some()))
        );
    }
}
