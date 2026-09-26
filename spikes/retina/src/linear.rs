//! `retina dump-linear`: hands demosaiced-but-uncorrected linear camera RGB (plus the metadata
//! needed to color-correct it) to `spikes/calico` (#38/ADR-0021), so calico's color pipeline can
//! be developed and tested without depending on retina's LibRaw FFI or vendored submodule
//! directly. LibRaw's demosaic is a stand-in for this hand-off only -- the demosaic algorithm
//! itself is #40's decision, not this ticket's.
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
use serde::Serialize;

use crate::libraw_ffi::LibRawHandle;

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

/// Decodes `path` with LibRaw, runs the WB/matrix/gamma-free demosaic, and writes the TIFF +
/// JSON sidecar into `out_dir` (created if missing), named after `path`'s file stem.
pub fn dump_linear(path: &Path, out_dir: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(out_dir)?;
    let data = fs::read(path)?;

    let mut handle = LibRawHandle::new();
    handle
        .decode(&data)
        .map_err(|e| anyhow::anyhow!("decode {}: {e}", path.display()))?;
    handle
        .process_linear()
        .map_err(|e| anyhow::anyhow!("process_linear {}: {e}", path.display()))?;

    let meta = handle.metadata();
    let linear = handle.linear_metadata();
    let image = handle.linear_image()?;

    let width = meta.iwidth as u32;
    let height = meta.iheight as u32;
    let expected_len = width as usize * height as usize * 4;
    anyhow::ensure!(
        image.len() == expected_len,
        "linear_image length {} != iwidth*iheight*4 ({expected_len}) for {}",
        image.len(),
        path.display()
    );

    // Drop the 4th (G2) channel -- calico's pipeline works in 3-channel camera RGB, matching
    // every DNG-spec matrix/HueSatMap operation downstream (all defined over R/G/B).
    let mut buf: ImageBuffer<Rgb<u16>, Vec<u16>> = ImageBuffer::new(width, height);
    for (i, px) in buf.pixels_mut().enumerate() {
        let base = i * 4;
        px.0 = [image[base], image[base + 1], image[base + 2]];
    }

    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("output");
    let tiff_path: PathBuf = out_dir.join(format!("{stem}.linear.tiff"));
    let json_path: PathBuf = out_dir.join(format!("{stem}.meta.json"));

    buf.save(&tiff_path)
        .map_err(|e| anyhow::anyhow!("write {}: {e}", tiff_path.display()))?;

    let sidecar = LinearMeta {
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
    };
    fs::write(&json_path, serde_json::to_string_pretty(&sidecar)?)?;

    eprintln!(
        "wrote {} ({width}x{height}) and {}",
        tiff_path.display(),
        json_path.display()
    );
    Ok(())
}
