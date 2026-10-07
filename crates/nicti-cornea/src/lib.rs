//! RAW decoder extension point (ADR-0019 §7/§8) plus its real implementation (#41): LibRaw-backed
//! decode → demosaic (WB/color-matrix/gamma disabled) → linear camera RGB, the first stage of
//! ADR-0044's baked prefix. #40's own demosaic/NR algorithm comparison lives in `spikes/rods`, not
//! here; `decode_linear` always uses LibRaw's own demosaic as a placeholder for whichever
//! algorithm #40 settles on (mirrors what `spikes/retina dump-linear` already did before this
//! promotion). Camera→working-space color correction (#38/#42) is a separate `ColorProfile`
//! implementation's job, not this crate's.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use nicti_claw::{Module, Registry};
#[cfg(feature = "libraw")]
use serde_json::Value;

// Gated behind the `libraw` feature (off by default): `LibRawHandle` calls into the vendored
// LibRaw C++ archive `build.rs` compiles, and `nicti-lair` depends on this crate for its
// pure-Rust `embedded` module alone -- it must never need `vendor/LibRaw` checked out, a C++
// compiler, or a link against `libretina_libraw.a` just to build. See build.rs's own doc comment
// for the other half of this gate.
#[cfg(feature = "libraw")]
mod libraw_ffi;

#[cfg(feature = "libraw")]
pub use libraw_ffi::{
    DecodedMetadata, DemosaicQuality, LibRawError as FfiError, LibRawHandle, LinearMetadata,
};

pub mod embedded;

/// A RAW decoder backend.
pub trait RawDecoder: Module {
    /// Decodes `path` and runs LibRaw's demosaic with white balance, camera-color-matrix, and
    /// gamma all disabled (see `crates/nicti-cornea/shim.h`'s `retina_libraw_process_linear` doc
    /// comment) — the black/white-level-scaled, demosaiced-but-uncorrected linear camera RGB a
    /// `ColorProfile` implementation (#38/#42) needs to reach a working-space image.
    fn decode_linear(&self, path: &Path) -> Result<LinearFrame, DecodeError>;
}

/// Registry of RAW decoder modules, keyed by namespaced id (e.g. `"nicti.decoder.libraw"`).
pub type DecoderRegistry = Registry<dyn RawDecoder>;

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("reading {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[cfg(feature = "libraw")]
    #[error("decoding {path:?}: {source}")]
    Decode {
        path: PathBuf,
        #[source]
        source: FfiError,
    },
    #[error(
        "linear_image length {actual} != iwidth*iheight*4 ({expected}) for {path:?} -- \
         LibRaw's imgdata.image allocation didn't match its own reported dimensions"
    )]
    UnexpectedImageLength {
        path: PathBuf,
        expected: usize,
        actual: usize,
    },
}

/// Demosaiced (WB/color-matrix/gamma-free) linear camera RGB, plus the metadata needed to
/// interpret it colorimetrically (as-shot and daylight-calibration white-balance multipliers,
/// camera-to-XYZ fallback matrix, per-channel black). Produced by [`RawDecoder::decode_linear`];
/// consumed by a `ColorProfile` implementation (#38/#42) to reach a working-space image. This is
/// the in-memory equivalent of `spikes/retina dump-linear`'s TIFF+JSON sidecar pair (kept for
/// `spikes/calico`'s own file-based tooling, see `spikes/calico/src/linear_input.rs`) — same
/// fields, no round-trip through disk required for a production caller.
#[derive(Debug, Clone)]
pub struct LinearFrame {
    pub make: String,
    pub model: String,
    pub width: u32,
    pub height: u32,
    pub black: u32,
    pub maximum: u32,
    /// As-shot white-balance multipliers (LibRaw's `cam_mul`), R/G/B/G2.
    pub cam_mul: [f32; 4],
    /// LibRaw's own daylight-calibration multipliers (`pre_mul`), R/G/B/G2.
    pub pre_mul: [f32; 4],
    /// LibRaw's camera->XYZ matrix, row-major 4x3 (unused rows zero) -- a fallback for cameras
    /// with no DCP/ColorMatrix of calico's own.
    pub cam_xyz: [f32; 12],
    /// Per-channel black-level additions beyond the single `black` scalar, R/G/B/G2.
    pub cblack: [u32; 4],
    /// Row-major, 3 u16 samples/pixel (R, G, B — LibRaw's G2 channel is dropped, matching every
    /// DNG-spec matrix/HueSatMap operation downstream, all defined over R/G/B). Length is
    /// `width * height * 3`.
    pub pixels: Vec<u16>,
    /// The raw DNG `OpcodeList3` blob (tag 51022) when the file is a DNG that carries one, `None`
    /// for every NEF and for DNGs without opcodes. Parsed by `nicti_iris::dng`, applied by the
    /// Tapetum lens stage (#428); LibRaw itself applies none of it.
    pub dng_opcode_list3: Option<Vec<u8>>,
    /// Nikon's raw lens-correction blob (TIFF tag 0xC7D5 in a SubIFD) when the file is a Z-series
    /// NEF that carries one, `None` otherwise. Located by `embedded::Walker::find_nikon_lens_info`,
    /// decoded by `nicti_iris::nikon`, applied by the Tapetum lens stage when the user opts in (#410).
    pub nikon_lens_info: Option<Vec<u8>>,
}

/// The production `RawDecoder`: LibRaw (vendored `yogthos/LibRaw#nikon-he-decoder`, see
/// `vendor/LibRaw` and `build.rs`), the only candidate #37/ADR-0019 found that decodes the real
/// library's Nikon HE/HE* files.
///
/// **UNVERIFIED against a real file in this sandbox** (no NEF/DNG fixture exists anywhere in this
/// repo, and this promotion's own review pass had none to test against either): `decode_linear`'s
/// logic is carried over unchanged from `spikes/retina`'s own `dump_linear` (#37/#38's own
/// research already exercised that exact code path against real HE/HE*/Lossless files pulled from
/// the live library, see `docs/decisions/raw-decoder.md`), but this promotion itself only checked
/// that the moved code compiles, lints clean, and passes unit tests that don't reach the real FFI
/// decode path (the missing-file I/O-error case is the only `decode_linear` test that exists).
/// Treat the decode/demosaic/channel-drop path as re-verified-by-inspection, not re-tested, until
/// it's actually run against a real file.
#[cfg(feature = "libraw")]
pub struct LibRawDecoder;

#[cfg(feature = "libraw")]
impl Module for LibRawDecoder {
    fn id(&self) -> &str {
        "nicti.decoder.libraw"
    }

    fn schema_version(&self) -> u32 {
        1
    }

    fn migrate_params(&self, _from_version: u32, params: Value) -> Option<Value> {
        Some(params)
    }
}

#[cfg(feature = "libraw")]
impl RawDecoder for LibRawDecoder {
    fn decode_linear(&self, path: &Path) -> Result<LinearFrame, DecodeError> {
        let data = std::fs::read(path).map_err(|source| DecodeError::Io {
            path: path.to_path_buf(),
            source,
        })?;

        let mut handle = LibRawHandle::new();
        handle.decode(&data).map_err(|source| DecodeError::Decode {
            path: path.to_path_buf(),
            source,
        })?;

        // Captured *before* `process_linear()`, not after: LibRaw's `scale_colors()` (run inside
        // `dcraw_process()`) can mutate `imgdata.color.maximum` as part of applying the
        // black-level correction, so a post-process read of `black`/`maximum` doesn't necessarily
        // reflect the sensor-native values this struct's doc comment promises.
        let meta = handle.metadata();

        handle
            .process_linear()
            .map_err(|source| DecodeError::Decode {
                path: path.to_path_buf(),
                source,
            })?;

        let linear = handle.linear_metadata();
        let dng_opcode_list3 = handle.dng_opcode_list3();
        // A missing or unreadable profile is a normal outcome, never a decode failure.
        let nikon_lens_info = embedded::Walker::new(embedded::SliceSource::new(&data))
            .ok()
            .and_then(|mut w| w.find_nikon_lens_info().ok().flatten());
        let image = handle
            .linear_image()
            .map_err(|source| DecodeError::Decode {
                path: path.to_path_buf(),
                source,
            })?;

        let width = meta.iwidth as u32;
        let height = meta.iheight as u32;
        let expected_len = width as usize * height as usize * 4;
        if image.len() != expected_len {
            return Err(DecodeError::UnexpectedImageLength {
                path: path.to_path_buf(),
                expected: expected_len,
                actual: image.len(),
            });
        }

        // Drop the 4th (G2) channel -- see this struct's own doc comment on `pixels`.
        let mut pixels = Vec::with_capacity(width as usize * height as usize * 3);
        for px in image.as_chunks::<4>().0 {
            pixels.extend_from_slice(&px[..3]);
        }

        Ok(LinearFrame {
            make: meta.make.clone(),
            model: meta.model.clone(),
            width,
            height,
            black: meta.black,
            maximum: meta.maximum,
            cam_mul: meta.cam_mul,
            pre_mul: linear.pre_mul,
            cam_xyz: linear.cam_xyz,
            cblack: linear.cblack,
            pixels,
            dng_opcode_list3,
            nikon_lens_info,
        })
    }
}

/// True for the file extensions `LibRawDecoder` can actually be pointed at (Nikon NEF, DNG) --
/// v1's target scope per ADR-0015, not every extension LibRaw itself recognizes.
pub fn is_supported_raw(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(OsStr::to_str)
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("nef") | Some("dng")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use nicti_claw::Descriptor;
    use serde_json::Value;
    use std::sync::Arc;

    struct Dummy;

    impl Module for Dummy {
        fn id(&self) -> &str {
            "nicti.decoder.dummy"
        }

        fn schema_version(&self) -> u32 {
            1
        }

        fn migrate_params(&self, _from_version: u32, params: Value) -> Option<Value> {
            Some(params)
        }
    }

    impl RawDecoder for Dummy {
        fn decode_linear(&self, path: &Path) -> Result<LinearFrame, DecodeError> {
            Err(DecodeError::Io {
                path: path.to_path_buf(),
                source: std::io::Error::new(std::io::ErrorKind::NotFound, "dummy"),
            })
        }
    }

    fn make_dummy() -> Arc<dyn RawDecoder> {
        Arc::new(Dummy)
    }

    #[test]
    fn dummy_registers_and_resolves_as_trait_object() {
        let mut registry: DecoderRegistry = Registry::new();
        registry
            .register(
                Descriptor {
                    id: "nicti.decoder.dummy",
                    schema_version: 1,
                },
                make_dummy,
            )
            .expect("registration should succeed");

        let resolved = registry
            .get("nicti.decoder.dummy")
            .expect("dummy decoder is registered");
        assert_eq!(resolved.id(), "nicti.decoder.dummy");
    }

    #[cfg(feature = "libraw")]
    #[test]
    fn libraw_decoder_reports_module_identity() {
        let decoder = LibRawDecoder;
        assert_eq!(decoder.id(), "nicti.decoder.libraw");
        assert_eq!(decoder.schema_version(), 1);
    }

    #[cfg(feature = "libraw")]
    #[test]
    fn decode_linear_reports_io_error_for_missing_file() {
        let decoder = LibRawDecoder;
        let err = decoder
            .decode_linear(Path::new("/nonexistent/does-not-exist.nef"))
            .expect_err("missing file should error, not panic");
        assert!(matches!(err, DecodeError::Io { .. }));
    }

    #[test]
    fn is_supported_raw_matches_nef_and_dng_case_insensitively() {
        assert!(is_supported_raw(Path::new("photo.NEF")));
        assert!(is_supported_raw(Path::new("photo.dng")));
        assert!(!is_supported_raw(Path::new("photo.jpg")));
        assert!(!is_supported_raw(Path::new("photo")));
    }
}
