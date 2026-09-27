//! `retina dump-linear`: writes `nicti-cornea`'s [`LinearFrame`] (demosaiced-but-uncorrected
//! linear camera RGB plus the metadata needed to color-correct it) to a TIFF + JSON sidecar pair,
//! so `spikes/calico` (#38/ADR-0038) can develop/test its color pipeline without depending on
//! `nicti-cornea`'s LibRaw FFI/vendored submodule directly. LibRaw's demosaic is a stand-in for
//! this hand-off only -- the demosaic algorithm itself is #40's decision, not this ticket's.
//!
//! The actual decode/demosaic step is `nicti-cornea`'s `RawDecoder::decode_linear` (promoted out
//! of this function in #41) -- this module is now just the TIFF/JSON serialization glue calico's
//! file-based tooling still expects.
//!
//! Output per input file:
//! - `<stem>.linear.tiff`: 16-bit RGB TIFF, values are LibRaw's black-subtracted, linearly
//!   white/black-scaled, demosaiced samples with no white balance, no color matrix, and no gamma
//!   applied (see shim.h's `retina_libraw_process_linear` doc comment).
//! - `<stem>.meta.json`: the [`LinearMeta`] sidecar calico needs to turn that into a color-correct
//!   image.

use std::fs;
use std::path::{Path, PathBuf};

use image::{ImageBuffer, Rgb};
use nicti_cornea::{LibRawDecoder, LinearFrame, RawDecoder};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct LinearMeta {
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
}

impl From<&LinearFrame> for LinearMeta {
    fn from(frame: &LinearFrame) -> Self {
        LinearMeta {
            make: frame.make.clone(),
            model: frame.model.clone(),
            width: frame.width,
            height: frame.height,
            black: frame.black,
            maximum: frame.maximum,
            cam_mul: frame.cam_mul,
            pre_mul: frame.pre_mul,
            cam_xyz: frame.cam_xyz,
            cblack: frame.cblack,
        }
    }
}

/// Decodes `path` via `nicti-cornea`'s `LibRawDecoder`, and writes the TIFF + JSON sidecar into
/// `out_dir` (created if missing), named after `path`'s file stem.
pub fn dump_linear(path: &Path, out_dir: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(out_dir)?;

    let frame = LibRawDecoder
        .decode_linear(path)
        .map_err(|e| anyhow::anyhow!("decode_linear {}: {e}", path.display()))?;

    // frame.pixels is already 3 u16 samples/pixel (R,G,B), row-major -- matches ImageBuffer's own
    // layout directly, no per-pixel channel selection needed here anymore.
    let buf: ImageBuffer<Rgb<u16>, Vec<u16>> =
        ImageBuffer::from_vec(frame.width, frame.height, frame.pixels.clone())
            .ok_or_else(|| anyhow::anyhow!("frame pixel buffer doesn't match width*height*3"))?;

    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("output");
    let tiff_path: PathBuf = out_dir.join(format!("{stem}.linear.tiff"));
    let json_path: PathBuf = out_dir.join(format!("{stem}.meta.json"));

    buf.save(&tiff_path)
        .map_err(|e| anyhow::anyhow!("write {}: {e}", tiff_path.display()))?;

    let sidecar = LinearMeta::from(&frame);
    fs::write(&json_path, serde_json::to_string_pretty(&sidecar)?)?;

    eprintln!(
        "wrote {} ({}x{}) and {}",
        tiff_path.display(),
        frame.width,
        frame.height,
        json_path.display()
    );
    Ok(())
}
